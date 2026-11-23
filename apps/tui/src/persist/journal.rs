//! The per-transaction recovery journal (F-008 #4, RFC-0023 slice 2, D-064): where journals live, the
//! [`Writer`] that turns a document's drained change log into [`rj01`] records, the crash-path rescue, and
//! the open-time read ([`Journals::offer`]).
//!
//! * **Location** (RFC §3). `$XDG_STATE_HOME/ruse/journal/` (else `~/.local/state/ruse/journal/`), the
//!   directory `0700` and every file `0600`. A file's journal is `<basename>.<16 hex of sha256(canonical
//!   path)>.rj`, the basename shortened so the name fits `NAME_MAX` even once archived (#1196). The unnamed
//!   start buffer's is `unnamed.<pid>.<start ms>.rj`, fixed when the process starts, so a later `:cd` does
//!   not move it (#1192). Archives go to `journal/archive/<name>@<unix ms>` ([`ARCHIVE_MARK`]), the name
//!   shortened to fit `NAME_MAX` when it must be (a long legacy sidecar's).
//! * **Writes** (RFC §4). After each command the session drains the document's change log into the writer's
//!   in-memory buffer; the buffer is written and `fdatasync`ed after [`IDLE`] without a change, after
//!   [`SYNC_EVERY`] changes, or before a risky operation, and each sync ends with a Marker. A failed write
//!   keeps the records and retries on the next trigger. A journal starts as Header + Checkpoint of the text
//!   the first change applied to (or, above [`rj01::LARGE_FILE`], Header + the disk text's length and hash,
//!   RFC §5); it is rewritten as Header + Checkpoint of the current text (temp file, fsync, rename) when the
//!   log overflowed, when the edits since the last Checkpoint pass 1 MiB and the Checkpoint's size
//!   (compaction), or when the file on disk is not the one this writer left.
//! * **Lifecycle** (RFC §6, #862, #1191). A clean buffer has no journal: the writer deletes its journal when
//!   the buffer becomes clean (saved, undone to the save, `:e!`), and the session deletes them on a normal
//!   quit and on `:bd!`. A crash path never deletes; it writes a Checkpoint of the exact current text.
//! * **Ownership** (RFC §7, #1193). Only a journal whose Header names this process is ever deleted. A journal
//!   whose owner is another ruse still running on this host ([`is_live_other`]) makes the file
//!   [`Recovery::InUse`]; a session that edits it anyway writes `<name>.<pid>.rj` beside it ([`claim`]),
//!   which a later open also considers ([`Journals::candidates`]). Recovering adopts a journal as this
//!   process's own ([`Journals::adopt`]); editing the disk file or discarding moves it to the archive.
//! * **Legacy** (RFC §3). An `RJS0` sidecar `<file>.ruse-journal` from an older ruse is offered once and
//!   then moved to the archive ([`super::rjs0`]). Nothing writes one any more.
//! * **Retention** (RFC §9, #934). [`super::retention`] sweeps the archive at startup, once a day.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ruse_core::{ChangeDrain, Document};

use super::encoding::FileFormat;
use super::rj01::{self, Base, Header, Owner};
use super::rjs0;
use super::{Available, Recovery};

mod clock;
mod naming;
mod offer;
mod owner;
mod rescue;
mod writer;

pub use clock::*;
pub use naming::*;
pub use owner::*;
pub use rescue::*;
pub use writer::*;

/// Sync after this long without a change (Vim's `updatetime` default).
pub const IDLE: Duration = Duration::from_millis(4000);

/// Sync once this many changes are buffered (Vim's `updatecount` default).
pub const SYNC_EVERY: usize = 200;

/// How often the event loop looks again while a large document's base hash runs (#1203).
const HASH_POLL: Duration = Duration::from_millis(20);

/// Compaction threshold: the edit payload since the last Checkpoint must pass this and the Checkpoint's size.
pub const COMPACT_AFTER: u64 = 1 << 20;

/// Buffered records past this are dropped for a Checkpoint of the current text, so a journal that keeps
/// failing to write never holds more than this in memory (INV-FAIL-BOUNDED).
const PENDING_CAP: usize = 8 << 20;

/// The longest file name the common filesystems accept.
const NAME_MAX: usize = 255;

/// Room kept for the archive suffix (`@<unix ms>`, 14 bytes) on top of a journal name.
const ARCHIVE_SUFFIX: usize = 14;

/// What separates an archive's name from its time: `<name>@<unix ms>`. Only [`Journals::archive`] writes it,
/// so [`super::retention`] owns every file in the archive named `<stem>@<digits>`, a shortened stem too.
pub const ARCHIVE_MARK: u8 = b'@';

/// The extension and hash part of a journal name: `.<16 hex>.rj`.
const HASH_SUFFIX: usize = 1 + 16 + 3;

/// Room for the `.<pid>` of a second session's journal ([`claim`]).
const PID_SUFFIX: usize = 1 + 10;

/// The journal directory of one state home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Journals {
    dir: PathBuf,
}

/// The journal directory resolved from the environment once per process; `None` when there is no usable
/// state directory (journaling is then off). Under `cfg(test)` it is a per-process temp directory, so a unit
/// test never writes the user's real one.
pub fn current() -> Option<&'static Journals> {
    static CURRENT: OnceLock<Option<Journals>> = OnceLock::new();
    CURRENT
        .get_or_init(|| {
            if cfg!(test) {
                let d =
                    std::env::temp_dir().join(format!("ruse-test-state-{}", std::process::id()));
                return Some(Journals::in_state_home(&d));
            }
            Journals::from_env()
        })
        .as_ref()
}

impl Journals {
    /// The journals under `state_home` (what `$XDG_STATE_HOME` names): `<state_home>/ruse/journal`.
    pub fn in_state_home(state_home: &Path) -> Journals {
        Journals {
            dir: state_home.join("ruse").join("journal"),
        }
    }

    /// `$XDG_STATE_HOME` when it is an absolute path (the XDG spec ignores a relative one), else
    /// `$HOME/.local/state`, the same on macOS (as Neovim's `stdpath("state")`).
    pub fn from_env() -> Option<Journals> {
        let var = |n: &str| {
            std::env::var_os(n)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        var("XDG_STATE_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".local").join("state")))
            .map(|s| Journals::in_state_home(&s))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.dir.join("archive")
    }

    /// The journal of the document at `doc` (any spelling of its path).
    pub fn path_for(&self, doc: &Path) -> PathBuf {
        self.path_for_canonical(&canonical(doc))
    }

    /// The journal of a document by its canonical path: `<basename>.<16 hex>.rj`.
    pub fn path_for_canonical(&self, canon: &Path) -> PathBuf {
        let hash = rj01::sha256(&path_bytes(canon));
        let hex = crate::hex_lower(&hash[..8]);
        let base = canon
            .file_name()
            .map_or_else(Vec::new, |n| path_bytes(Path::new(n)));
        let base = if base.is_empty() {
            b"root".to_vec()
        } else {
            base
        };
        let mut name =
            shorten(&base, NAME_MAX - HASH_SUFFIX - PID_SUFFIX - ARCHIVE_SUFFIX).to_vec();
        name.extend_from_slice(format!(".{hex}.rj").as_bytes());
        self.dir.join(os_name(name))
    }

    /// This process's journal for the unnamed start buffer.
    pub fn unnamed(&self) -> PathBuf {
        let o = owner();
        self.dir
            .join(format!("unnamed.{}.{}.rj", o.pid, o.start_ms))
    }

    /// The journal of a buffer: a file's, or this process's unnamed one.
    pub fn path_of(&self, doc: Option<&Path>) -> PathBuf {
        doc.map_or_else(|| self.unnamed(), |d| self.path_for(d))
    }

    /// Create the journal directory (and its parents) owner-only.
    pub fn ensure_dir(&self) -> io::Result<()> {
        make_private_dir(&self.dir)
    }

    /// The text a journal replays to, for a buffer whose base is a Checkpoint. A convenience over
    /// [`rj01::replay`] for callers that do not have the disk text.
    pub fn replay(&self, doc: Option<&Path>) -> Option<Vec<u8>> {
        let disk = doc.and_then(|d| fs::read(d).ok());
        let raw = read_journal(&self.path_of(doc)).ok()?;
        rj01::replay(&raw, disk.as_deref()).text
    }

    /// Write `text` as a buffer's whole journal (Header + Checkpoint + Marker), as the crash path does.
    pub fn write_text(&self, doc: Option<&Path>, text: &[u8]) -> io::Result<PathBuf> {
        let journal = self.path_of(doc);
        let canon = doc.map(canonical);
        rescue(
            &journal,
            canon.as_deref(),
            FileFormat::default(),
            text,
            0,
            0,
        )
    }

    /// Write `text` as `doc`'s journal left by a session that crashed: its Header names a process of this
    /// host that is not running. For tests that need a journal to recover from.
    pub fn write_crashed(&self, doc: Option<&Path>, text: &[u8]) -> io::Result<PathBuf> {
        let gone = Owner {
            pid: GONE_PID,
            host: owner().host.clone(),
            start_ms: 1,
        };
        self.write_as(doc, text, &gone)
    }

    /// Write `text` as the journal a crashed second session left beside `doc`'s own (`<name>.<pid>.rj`,
    /// [`claim`]), for tests of a file in use by one session after another crashed (#1210).
    pub fn write_crashed_beside(&self, doc: &Path, text: &[u8]) -> io::Result<PathBuf> {
        let gone = Owner {
            pid: GONE_PID,
            host: owner().host.clone(),
            start_ms: 1,
        };
        let journal = alternate(&self.path_for(doc), GONE_PID);
        self.write_at(&journal, Some(doc), text, &gone)
    }

    /// Write `text` as `doc`'s whole journal with `who` as its owner (tests: a crashed or a live session).
    pub fn write_as(&self, doc: Option<&Path>, text: &[u8], who: &Owner) -> io::Result<PathBuf> {
        self.write_at(&self.path_of(doc), doc, text, who)
    }

    fn write_at(
        &self,
        journal: &Path,
        doc: Option<&Path>,
        text: &[u8],
        who: &Owner,
    ) -> io::Result<PathBuf> {
        let journal = journal.to_path_buf();
        let canon = doc.map(canonical);
        if let Some(dir) = journal.parent() {
            make_private_dir(dir)?;
        }
        let mut out = Vec::with_capacity(text.len() + 256);
        let mut h = header(canon.as_deref(), FileFormat::default(), Base::Checkpoint);
        h.owner = who.clone();
        rj01::encode_header(&mut out, &h)?;
        rj01::encode_checkpoint(&mut out, 0, 0, text)?;
        rj01::encode_marker(&mut out, now_ms())?;
        write_private(&journal, &out)?;
        Ok(journal)
    }

    /// Remove `doc`'s journal after a durable save: there is nothing left to recover. Only a journal this
    /// process owns ([`discard_file`]).
    pub fn clear(&self, doc: Option<&Path>) {
        discard_file(&self.path_of(doc));
    }

    /// Remove `doc`'s journal because its changes were let go (#862): a normal quit, `:e!`, `:bd!`. Only a
    /// journal this process owns is removed ([`discard_file`]): another session's, live or crashed, is not
    /// this session's to delete (#1193).
    pub fn discard(&self, doc: Option<&Path>) {
        discard_file(&self.path_of(doc));
    }

    /// Move `path` into the archive as `<name>@<unix ms>` ([`ARCHIVE_MARK`]; `<name>` shortened to fit
    /// `NAME_MAX` when it must be), its modification time set to now (the age [`super::retention`] sweeps
    /// by); the archived path. A move across filesystems (a legacy sidecar in the
    /// project tree) copies and then removes.
    pub fn archive(&self, path: &Path) -> io::Result<PathBuf> {
        let dir = self.archive_dir();
        make_private_dir(&dir)?;
        let name = path
            .file_name()
            .map(|n| path_bytes(Path::new(n)))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| b"journal".to_vec());
        // A fresh name: a rename would replace an archive made in the same millisecond.
        let mut ms = now_ms();
        let to = loop {
            let suffix = format!("{}{ms}", char::from(ARCHIVE_MARK));
            let mut n = shorten(&name, NAME_MAX - suffix.len()).to_vec();
            n.extend_from_slice(suffix.as_bytes());
            let to = dir.join(os_name(n));
            if fs::symlink_metadata(&to).is_err() {
                break to;
            }
            ms += 1;
        };
        if fs::rename(path, &to).is_err() {
            let bytes = read_journal(path)?;
            write_private(&to, &bytes)?;
            fs::remove_file(path)?;
        }
        // Retention counts an archive's age from now, not from the journal's last write (RFC-0023 §9).
        if let Ok(f) = open_regular(&to, OpenOptions::new().write(true)) {
            let _ = f.set_modified(SystemTime::now());
        }
        Ok(to)
    }
}

fn make_private_dir(dir: &Path) -> io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// Open the journal at `path`, refusing anything but a regular file (#869): a FIFO would block the open or
/// the read, and a device such as `/dev/zero` would be read until memory runs out. `O_NONBLOCK` keeps the
/// open itself from blocking on a FIFO; it has no effect on a regular file. The type is checked on the
/// opened descriptor, so a swap between the check and the use cannot slip a FIFO in.
fn open_regular(path: &Path, opts: &mut OpenOptions) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
        opts.mode(0o600);
    }
    let f = opts.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is not a regular file",
        ));
    }
    Ok(f)
}

/// Read a whole journal (a regular file only).
pub fn read_journal(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = open_regular(path, OpenOptions::new().read(true))?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)?;
    Ok(raw)
}

/// Remove the journal at `path` when this process owns it (its Header names this process). Anything else
/// is left alone: another session's journal (#1193), one without an intact Header, and a path that is not a
/// regular file (unreadable, a FIFO, a device: the user was told it was not offered, so it was never seen).
fn discard_file(path: &Path) {
    if read_owner(path).is_some_and(|o| o == *owner()) {
        let _ = fs::remove_file(path);
    }
}

/// A pid no process has: above every platform's pid range (Linux's `pid_max` is at most 2^22).
pub const GONE_PID: u32 = 2_147_483_646;

/// Durably replace `path` with `bytes` (temp, fsync, rename, directory fsync), owner-only: a missing target
/// is created `0600` first, and the atomic save carries its mode over to the replacement.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if fs::symlink_metadata(path).is_err() {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        match opts.open(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    super::atomic::save(path, bytes)
}

/// Whether `doc` was modified on disk after its journal was last written (#897), so recovering would replace
/// a newer file. nvim warns the same way ("NEWER than swap file!"). False when either time is unknown.
pub fn is_stale(doc: &Path, journal: &Path) -> bool {
    let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    matches!((mtime(doc), mtime(journal)), (Some(file), Some(j)) if file > j)
}

/// The status line when a journal exists but could not be read (#897). The path comes last: a journal in
/// the state directory has a long one, and the reason must stay on screen.
pub fn unreadable_note(journal: &Path, err: &io::Error) -> String {
    format!(
        "recovery journal unreadable ({err}); not offered: {}",
        journal.display()
    )
}

/// The Header this process writes for a document (`canon` `None` for the unnamed buffer).
fn header(canon: Option<&Path>, format: FileFormat, base: Base) -> Header {
    Header {
        owner: owner().clone(),
        path: canon.map(path_bytes).unwrap_or_default(),
        format,
        created_ms: now_ms(),
        base,
    }
}

/// A whole journal holding `text`: Header + Checkpoint + Marker.
fn whole(
    canon: Option<&Path>,
    format: FileFormat,
    text: &[u8],
    revision: u64,
    node: u64,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() + 256);
    rj01::encode_header(&mut out, &header(canon, format, Base::Checkpoint))?;
    rj01::encode_checkpoint(&mut out, node, revision, text)?;
    rj01::encode_marker(&mut out, now_ms())?;
    Ok(out)
}

/// [`Journals::clear`] in this process's journal directory ([`current`]).
pub fn clear(doc: Option<&Path>) {
    if let Some(j) = current() {
        j.clear(doc);
    }
}

/// [`Journals::discard`] in this process's journal directory ([`current`]).
pub fn discard(doc: Option<&Path>) {
    if let Some(j) = current() {
        j.discard(doc);
    }
}

#[cfg(test)]
mod tests;
