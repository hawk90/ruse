//! The per-transaction recovery journal (F-008 #4, RFC-0023 slice 2, D-064): where journals live, the
//! [`Writer`] that turns a document's drained change log into [`rj01`] records, the crash-path rescue, and
//! the open-time read ([`Journals::offer`]).
//!
//! * **Location** (RFC §3). `$XDG_STATE_HOME/ruse/journal/` (else `~/.local/state/ruse/journal/`), the
//!   directory `0700` and every file `0600`. A file's journal is `<basename>.<16 hex of sha256(canonical
//!   path)>.rj`, the basename shortened so the name fits `NAME_MAX` even once archived (#1196). The unnamed
//!   start buffer's is `unnamed.<pid>.<start ms>.rj`, fixed when the process starts, so a later `:cd` does
//!   not move it (#1192). Archives go to `journal/archive/<name>.<unix ms>`.
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

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ruse_core::{ChangeDrain, Document};

use super::encoding::FileFormat;
use super::rj01::{self, Base, Header, Owner};
use super::rjs0;
use super::{Available, Recovery};

/// Sync after this long without a change (Vim's `updatetime` default).
pub const IDLE: Duration = Duration::from_millis(4000);
/// Sync once this many changes are buffered (Vim's `updatecount` default).
pub const SYNC_EVERY: usize = 200;
/// Compaction threshold: the edit payload since the last Checkpoint must pass this and the Checkpoint's size.
pub const COMPACT_AFTER: u64 = 1 << 20;
/// Buffered records past this are dropped for a Checkpoint of the current text, so a journal that keeps
/// failing to write never holds more than this in memory (INV-FAIL-BOUNDED).
const PENDING_CAP: usize = 8 << 20;
/// The longest file name the common filesystems accept.
const NAME_MAX: usize = 255;
/// Room kept for the archive suffix (`.<unix ms>`, 14 bytes) on top of a journal name.
const ARCHIVE_SUFFIX: usize = 14;
/// The extension and hash part of a journal name: `.<16 hex>.rj`.
const HASH_SUFFIX: usize = 1 + 16 + 3;
/// Room for the `.<pid>` of a second session's journal ([`claim`]).
const PID_SUFFIX: usize = 1 + 10;

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// This process as a journal owner: pid, host name, and its start time as the system reports it
/// ([`process_start_ms`]), so a later session can tell this process from another that reuses its pid. When
/// the system does not say, the time of the first call stands in (the session asks at startup, so the
/// unnamed buffer's journal name is fixed then); such a journal is never taken for a live one.
pub fn owner() -> &'static Owner {
    static OWNER: OnceLock<Owner> = OnceLock::new();
    OWNER.get_or_init(|| {
        let pid = std::process::id();
        Owner {
            pid,
            host: host_name(),
            start_ms: process_start_ms(pid).unwrap_or_else(now_ms),
        }
    })
}

/// How far apart two readings of one process's start time may be: Linux derives it from the boot time,
/// which is kept in whole seconds and moves when the clock is stepped.
const START_SLACK_MS: u64 = 2000;

/// The start time of process `pid`, Unix milliseconds, as the system reports it: `/proc/<pid>/stat` and
/// the boot time on Linux, `proc_pidinfo` on macOS. `None` when it cannot be read.
pub fn process_start_ms(pid: u32) -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name (field 2) may hold spaces and parentheses: count fields after the last `)`.
        let rest = &stat[stat.rfind(')')? + 1..];
        let ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
        let btime: u64 = fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if hz <= 0 {
            return None;
        }
        Some(btime * 1000 + ticks * 1000 / hz as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let pid = libc::c_int::try_from(pid).ok()?;
        // SAFETY: plain data, fully written by proc_pidinfo on success.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a valid buffer of `size` bytes for the PROC_PIDTBSDINFO flavor.
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if n != size {
            return None;
        }
        Some(info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Whether `o` is another ruse process still running on this host (RFC-0023 §7): same host name, its pid
/// answers `kill(pid, 0)`, and that process started when the Header says. A start time that cannot be read
/// counts as not running, so a reused pid or an unknown answer offers recovery rather than claim the file
/// is in use. This process is never "another".
pub fn is_live_other(o: &Owner) -> bool {
    if o == owner() || o.host != owner().host || !alive(o.pid) {
        return false;
    }
    process_start_ms(o.pid).is_some_and(|s| s.abs_diff(o.start_ms) <= START_SLACK_MS)
}

/// The owner recorded in the journal at `path`'s Header, when it has an intact one. Reads only a bounded
/// prefix: the Header comes first and is small.
pub fn read_owner(path: &Path) -> Option<Owner> {
    match read_head(path)? {
        Ok((rj01::Record::Header(h), _)) => Some(h.owner),
        _ => None,
    }
}

/// The first record of the journal at `path`, from a bounded prefix; `None` when it cannot be read.
fn read_head(path: &Path) -> Option<Result<(rj01::Record, usize), rj01::Stop>> {
    let f = open_regular(path, OpenOptions::new().read(true)).ok()?;
    let mut raw = Vec::new();
    f.take(HEADER_MAX).read_to_end(&mut raw).ok()?;
    Some(rj01::read_record(&raw, 0))
}

/// The most of a journal [`read_owner`] reads: a Header holds a host name and a path.
const HEADER_MAX: u64 = 64 << 10;

/// The journal this process may write for a document whose journal is `journal`: that file, unless a live
/// other session owns it (#1193), a newer ruse wrote it (kept for that version, RFC-0023 §7), or it is a
/// regular file this process cannot read (mode 000, another user's: it may hold a crash's unsaved work, and
/// the open reported it as kept), in which case this process's own `<name>.<pid>.rj` beside it. Anything
/// else at the path that is not a regular file (a FIFO, a device) keeps the name, and the write fails
/// visibly (#869).
pub fn claim(journal: &Path) -> PathBuf {
    match read_head(journal) {
        Some(Ok((rj01::Record::Header(h), _))) if is_live_other(&h.owner) => {
            alternate(journal, owner().pid)
        }
        Some(Err(rj01::Stop::NewerMajor(_))) => alternate(journal, owner().pid),
        None if fs::symlink_metadata(journal).is_ok_and(|m| m.is_file()) => {
            alternate(journal, owner().pid)
        }
        _ => journal.to_path_buf(),
    }
}

/// `<stem>.<pid>.rj` for the journal `<stem>.rj`.
fn alternate(journal: &Path, pid: u32) -> PathBuf {
    let name = journal
        .file_name()
        .map_or_else(Vec::new, |n| path_bytes(Path::new(n)));
    let stem = name.strip_suffix(b".rj").unwrap_or(&name);
    let mut alt = stem.to_vec();
    alt.extend_from_slice(format!(".{pid}.rj").as_bytes());
    journal.with_file_name(os_name(alt))
}

fn host_name() -> Vec<u8> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a valid writable buffer of the given length.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return buf[..n].to_vec();
        }
    }
    Vec::new()
}

/// A path's raw bytes, as the journal Header stores them.
pub fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

/// The canonical absolute path a journal is keyed by: symlinks resolved, and for a file that does not
/// exist yet, its canonical directory joined with its name. Falls back to the plain absolute path.
pub fn canonical(doc: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(doc) {
        return c;
    }
    let abs = std::path::absolute(doc).unwrap_or_else(|_| doc.to_path_buf());
    match (abs.parent(), abs.file_name()) {
        (Some(dir), Some(name)) => fs::canonicalize(dir).map_or(abs.clone(), |d| d.join(name)),
        _ => abs,
    }
}

/// Shorten `stem` (at a character boundary when it is UTF-8) to at most `room` bytes.
fn shorten(stem: &[u8], room: usize) -> &[u8] {
    if stem.len() <= room {
        return stem;
    }
    let mut end = room;
    if std::str::from_utf8(stem).is_ok() {
        while end > 0 && (stem[end] & 0xC0) == 0x80 {
            end -= 1;
        }
    }
    &stem[..end]
}

#[cfg(unix)]
fn os_name(bytes: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn os_name(bytes: Vec<u8>) -> std::ffi::OsString {
    String::from_utf8_lossy(&bytes).into_owned().into()
}

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
        let hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
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

    /// Write `text` as `doc`'s whole journal with `who` as its owner (tests: a crashed or a live session).
    pub fn write_as(&self, doc: Option<&Path>, text: &[u8], who: &Owner) -> io::Result<PathBuf> {
        let journal = self.path_of(doc);
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

    /// Move `path` into the archive as `<name>.<unix ms>`; the archived path. A move across filesystems (a
    /// legacy sidecar in the project tree) copies and then removes.
    pub fn archive(&self, path: &Path) -> io::Result<PathBuf> {
        let dir = self.archive_dir();
        make_private_dir(&dir)?;
        let name = path
            .file_name()
            .map_or_else(Vec::new, |n| path_bytes(Path::new(n)));
        // A fresh name: a rename would replace an archive made in the same millisecond.
        let mut ms = now_ms();
        let to = loop {
            let suffix = format!(".{ms}");
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
        Ok(to)
    }

    /// The newest unnamed-buffer journal left by a ruse that is no longer running, for a pathless start.
    fn orphaned_unnamed(&self) -> Option<PathBuf> {
        let me = owner().pid;
        fs::read_dir(&self.dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let pid: u32 = name
                    .strip_prefix("unnamed.")?
                    .split('.')
                    .next()?
                    .parse()
                    .ok()?;
                let mtime = e.metadata().ok()?.modified().ok()?;
                let gone = read_owner(&e.path())
                    .map_or(!alive(pid), |o| o != *owner() && !is_live_other(&o));
                (name.ends_with(".rj") && pid != me && gone).then(|| (mtime, e.path()))
            })
            .max()
            .map(|(_, p)| p)
    }

    /// The journals of `doc`: its own name first, then every `<name>.<pid>.rj` a second session wrote
    /// beside it ([`claim`]), newest first.
    pub fn candidates(&self, doc: &Path) -> Vec<PathBuf> {
        let primary = self.path_for(doc);
        let mut out = vec![primary.clone()];
        let Some(stem) = primary
            .file_name()
            .map(|n| path_bytes(Path::new(n)))
            .and_then(|n| n.strip_suffix(b".rj").map(<[u8]>::to_vec))
        else {
            return out;
        };
        let mut alts: Vec<(SystemTime, PathBuf)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = path_bytes(Path::new(&e.file_name()));
                let pid = name.strip_prefix(stem.as_slice())?.strip_prefix(b".")?;
                let pid = pid.strip_suffix(b".rj")?;
                (!pid.is_empty() && pid.iter().all(u8::is_ascii_digit)).then_some(())?;
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((mtime, e.path()))
            })
            .collect();
        alts.sort_by_key(|a| std::cmp::Reverse(a.0));
        out.extend(alts.into_iter().map(|(_, p)| p));
        out
    }

    /// What the journals of `doc` (or, for `None`, an orphaned unnamed buffer's) offer at open time, given
    /// the buffer's `disk` text (the file decoded with its detected format; empty for no file). A journal
    /// whose owner is still running on this host makes the file [`Recovery::InUse`]; that wins over a
    /// crashed one beside it, which stays for a later open. A journal replaying to the disk text offers
    /// nothing; a journal that cannot be used says why.
    pub fn offer(&self, doc: Option<&Path>, disk: &[u8]) -> Recovery {
        let journals = match doc {
            Some(d) => self.candidates(d),
            None => self.orphaned_unnamed().into_iter().collect(),
        };
        // A journal to recover wins over one that only gives a note (a newer ruse's beside this one's).
        let (mut available, mut note) = (None, None);
        for journal in journals {
            if let Some(o) = read_owner(&journal).filter(is_live_other) {
                return Recovery::InUse { owner: o, journal };
            }
            if available.is_some() {
                continue;
            }
            match self.offer_one(doc, &journal, disk) {
                Recovery::None => {}
                r @ Recovery::Available(_) => available = Some(r),
                r => {
                    note.get_or_insert(r);
                }
            }
        }
        available
            .or(note)
            .unwrap_or_else(|| self.offer_legacy(doc, disk))
    }

    /// The offer of the one journal at `journal` (its owner is not a live other session).
    fn offer_one(&self, doc: Option<&Path>, journal: &Path, disk: &[u8]) -> Recovery {
        let raw = match read_journal(journal) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Recovery::None,
            Err(e) => {
                return Recovery::Unreplayable {
                    reason: unreadable_note(journal, &e),
                }
            }
        };
        let r = rj01::replay(&raw, Some(disk));
        match r.unreplayable {
            Some(rj01::Unreplayable::NewerMajor(v)) => {
                return Recovery::Unreplayable {
                    reason: format!(
                    "recovery journal {} is from a newer ruse (format {v}.x); kept, not offered",
                    journal.display()
                ),
                }
            }
            Some(rj01::Unreplayable::DiskChanged { changes }) => {
                let kept = self.archive(journal).map_or_else(
                    |e| format!("not archived: {e}"),
                    |p| format!("archived as {}", p.display()),
                );
                return Recovery::Unreplayable {
                    reason: format!(
                        "the file changed on disk since the journal began; the {changes} changes \
                         cannot be replayed (journal {kept})"
                    ),
                };
            }
            None => {}
        }
        match r.text {
            Some(text) if text != disk => Recovery::Available(Box::new(Available {
                recovered: text,
                base: r.header.as_ref().map(|h| h.base),
                changes: Some(r.changes),
                last_change: r.last_change_ms,
                up_to: r.last_marker_ms,
                lost_tail: r.lost_tail,
                stale: doc.and_then(|d| stale_since(d, journal)),
                owner: r.header.map(|h| h.owner),
                journal: journal.to_path_buf(),
                legacy: false,
            })),
            _ => Recovery::None,
        }
    }

    /// A sidecar from an older ruse.
    fn offer_legacy(&self, doc: Option<&Path>, disk: &[u8]) -> Recovery {
        let sidecar = rjs0::sidecar_path(doc);
        match read_journal(&sidecar) {
            Ok(raw) => match rjs0::last_snapshot(&raw) {
                Some(text) if text != disk => Recovery::Available(Box::new(Available {
                    recovered: text.to_vec(),
                    base: None,
                    changes: None,
                    last_change: None,
                    up_to: None,
                    lost_tail: false,
                    stale: doc.and_then(|d| stale_since(d, &sidecar)),
                    owner: None,
                    journal: sidecar,
                    legacy: true,
                })),
                // Nothing in it to recover: archive it so it stops sitting in the project tree.
                Some(_) => {
                    let _ = self.archive(&sidecar);
                    Recovery::None
                }
                None => Recovery::None,
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Recovery::None,
            Err(e) => Recovery::Unreplayable {
                reason: unreadable_note(&sidecar, &e),
            },
        }
    }

    /// `r` at the prompt (RFC-0023 §7): this process takes the recovered text over as its own journal at
    /// once, at the name it will write (`doc`'s, or this process's unnamed one), so a crash, `:q!` or a
    /// normal quit of this session finds a journal it owns. A journal of another name it came from (a
    /// legacy sidecar, an orphaned unnamed one, a second session's `<name>.<pid>.rj`) is archived. Nothing
    /// is lost when a step fails: the old journal is moved only once the new one is written.
    pub fn adopt(
        &self,
        offer: &Available,
        doc: Option<&Path>,
        fmt: FileFormat,
    ) -> io::Result<PathBuf> {
        let canon = doc.map(canonical);
        let target = claim(&self.path_of(doc));
        let written = rescue(&target, canon.as_deref(), fmt, &offer.recovered, 0, 0)?;
        if written != offer.journal {
            self.archive(&offer.journal)?;
        }
        Ok(written)
    }
}

/// When `doc` was modified on disk after its journal at `journal` was last written (#897), its modification
/// time (Unix milliseconds): recovering would replace a newer file.
fn stale_since(doc: &Path, journal: &Path) -> Option<u64> {
    if !is_stale(doc, journal) {
        return None;
    }
    let t = fs::metadata(doc).and_then(|m| m.modified()).ok()?;
    Some(
        t.duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    )
}

/// Whether `pid` is a running process (`kill(pid, 0)`). An unknown answer counts as alive.
pub(crate) fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 checks for existence and permission only; nothing is delivered.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Local wall-clock `HH:MM:SS` of Unix milliseconds `ms`.
pub fn clock(ms: u64) -> String {
    let (_, time) = local(ms);
    time
}

/// Local `YYYY-MM-DD HH:MM:SS` of Unix milliseconds `ms`.
pub fn date_time(ms: u64) -> String {
    let (date, time) = local(ms);
    format!("{date} {time}")
}

/// The local date and time of `ms`; UTC (and marked so) when the local time cannot be had.
fn local(ms: u64) -> (String, String) {
    #[cfg(unix)]
    {
        let t = (ms / 1000) as libc::time_t;
        // SAFETY: `tm` is plain data, fully written by localtime_r on success.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are valid for the call.
        if !unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return (
                format!(
                    "{:04}-{:02}-{:02}",
                    tm.tm_year + 1900,
                    tm.tm_mon + 1,
                    tm.tm_mday
                ),
                format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
            );
        }
    }
    // Days since the epoch to a civil date (Howard Hinnant's algorithm).
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let s = ms / 1000 % 86_400;
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{:02}:{:02}:{:02} UTC", s / 3600, s / 60 % 60, s % 60),
    )
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

/// The crash-path write (panic hook, fatal signal, I/O error exit): put the exact current `text` in the
/// journal at `journal`, or, when a live other session owns that one, in this process's own beside it
/// ([`claim`], #1193); the path written. A journal of this process with an intact Header gets a Checkpoint
/// and a Marker appended after its last intact record (a torn tail is cut first: the failure being rescued
/// may have interrupted a write). Anything else (a crashed session's journal, kept until this session's
/// first sync, or a damaged one) is replaced whole and atomically, so the Header names who wrote the text
/// and a failure part-way leaves the old journal. Synced before it returns.
pub fn rescue(
    journal: &Path,
    canon: Option<&Path>,
    format: FileFormat,
    text: &[u8],
    revision: u64,
    node: u64,
) -> io::Result<PathBuf> {
    let journal = claim(journal);
    if let Some(dir) = journal.parent() {
        make_private_dir(dir)?;
    }
    let mut f = open_regular(
        &journal,
        OpenOptions::new().read(true).write(true).create(true),
    )?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)?;
    let r = rj01::replay(&raw, None);
    let ours = r.header.as_ref().is_some_and(|h| h.owner == *owner());
    if !ours || r.unreplayable.is_some() {
        drop(f);
        write_private(&journal, &whole(canon, format, text, revision, node)?)?;
        return Ok(journal);
    }
    let mut out = Vec::with_capacity(text.len() + 256);
    rj01::encode_checkpoint(&mut out, node, revision, text)?;
    rj01::encode_marker(&mut out, now_ms())?;
    let at = r.last_good as u64;
    f.set_len(at)?;
    io::Seek::seek(&mut f, io::SeekFrom::Start(at))?;
    let written = f.write_all(&out).and_then(|()| f.sync_data());
    if written.is_err() {
        let _ = f.set_len(at);
    }
    written.map(|()| journal)
}

/// Where a [`Writer`]'s journal is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No journal: the buffer has been clean since this writer last looked.
    Idle,
    /// The pending bytes are a whole new journal (Header first), not yet on disk.
    Fresh,
    /// The journal on disk is this writer's, `on_disk` bytes long; pending bytes append to it.
    Live,
}

/// Writes one file buffer's journal from its drained change log (RFC-0023 §4). The session calls
/// [`Writer::absorb`] after every command and [`Writer::flush`] when [`Writer::due`] (or before a risky
/// operation).
#[derive(Debug)]
pub struct Writer {
    /// The journal this writer writes: the document's own name, or this process's `<name>.<pid>.rj` when
    /// a live other session owns that one ([`claim`], #1193).
    journal: PathBuf,
    /// The document's own journal name.
    primary: PathBuf,
    /// The canonical document path (the Header's).
    doc: PathBuf,
    /// The buffer's path as the session last showed it, to notice a rename or `:cd` cheaply.
    seen: PathBuf,
    fmt: FileFormat,
    /// The text at the last drain while there was no journal: the base the next changes apply to.
    base: Option<Arc<Vec<u8>>>,
    state: State,
    /// The next flush replaces the journal with Header + Checkpoint of the current text.
    rewrite: bool,
    pending: Vec<u8>,
    pending_changes: usize,
    last_change: Option<Instant>,
    on_disk: u64,
    since_checkpoint: u64,
    checkpoint_len: u64,
    /// The journal's base is the disk text (RFC-0023 §5): no compaction.
    large: bool,
    seq: u64,
    /// When the last write and sync succeeded, for `:checkhealth` (RFC-0023 §8).
    last_sync: Option<Instant>,
    /// Failed writes since this writer began, and the last one's error.
    failures: u64,
    last_error: Option<String>,
}

/// One journal's health, as `:checkhealth` reports it (RFC-0023 §8, #938).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// The journal file.
    pub journal: PathBuf,
    /// Its size on disk; 0 when this writer has none there.
    pub size: u64,
    /// Records wait to be written.
    pub unsynced: bool,
    /// How long ago the last successful sync was; `None` before the first.
    pub synced_ago: Option<Duration>,
    pub failures: u64,
    /// The last failure's error, while the last write failed.
    pub failing: Option<String>,
}

impl Writer {
    /// A writer for the buffer whose file is `path`, in `journals`.
    pub fn new(journals: &Journals, path: &Path, fmt: FileFormat) -> Writer {
        let doc = canonical(path);
        let primary = journals.path_for_canonical(&doc);
        Writer {
            journal: claim(&primary),
            primary,
            doc,
            seen: path.to_path_buf(),
            fmt,
            base: None,
            state: State::Idle,
            rewrite: false,
            pending: Vec::new(),
            pending_changes: 0,
            last_change: None,
            on_disk: 0,
            since_checkpoint: 0,
            checkpoint_len: 0,
            large: false,
            seq: 0,
            last_sync: None,
            failures: 0,
            last_error: None,
        }
    }

    pub fn journal(&self) -> &Path {
        &self.journal
    }

    /// The journal's health at `now`.
    pub fn health(&self, now: Instant) -> Health {
        Health {
            journal: self.journal.clone(),
            size: if self.state == State::Live {
                self.on_disk
            } else {
                0
            },
            unsynced: self.unsynced(),
            synced_ago: self.last_sync.map(|t| now.saturating_duration_since(t)),
            failures: self.failures,
            failing: self.last_error.clone(),
        }
    }

    pub fn doc(&self) -> &Path {
        &self.doc
    }

    /// Follow the buffer's path: a `:cd` that only respells it changes nothing; a rename (`:saveas`,
    /// `:file`) moves the journal to the new name and records a Meta rename.
    pub fn follow(&mut self, journals: &Journals, path: &Path) {
        if path == self.seen {
            return;
        }
        self.seen = path.to_path_buf();
        let doc = canonical(path);
        if doc == self.doc {
            return;
        }
        let primary = journals.path_for_canonical(&doc);
        let journal = claim(&primary);
        match self.state {
            State::Idle => {}
            State::Fresh => self.rewrite = true,
            State::Live => {
                if fs::rename(&self.journal, &journal).is_ok() {
                    let _ =
                        rj01::encode_meta(&mut self.pending, &rj01::Meta::Rename(path_bytes(&doc)));
                } else {
                    discard_file(&self.journal);
                    self.rewrite = true;
                }
            }
        }
        self.doc = doc;
        self.journal = journal;
        self.primary = primary;
    }

    /// Drain `doc`'s change log into the pending records (`now` is the drain's time). Turns recording on
    /// first, also after `:e!` replaced the document and reset it. A clean buffer drops its journal.
    pub fn absorb(&mut self, doc: &mut Document, fmt: FileFormat, now: Instant) {
        if !doc.is_recording_changes() {
            doc.record_changes(true);
        }
        let drain = doc.drain_changes();
        if !doc.is_modified() {
            self.discard();
            self.base = Some(doc.text_arc());
            return;
        }
        if fmt != self.fmt {
            self.fmt = fmt;
            if self.state != State::Idle && !self.rewrite {
                let _ = rj01::encode_meta(&mut self.pending, &rj01::Meta::Format(fmt));
            }
        }
        match drain {
            ChangeDrain::Checkpoint { .. } => {
                self.rewrite = true;
                if self.state == State::Idle {
                    self.state = State::Fresh;
                }
                self.touch(now, 1);
            }
            ChangeDrain::Changes(entries) if entries.is_empty() => {}
            ChangeDrain::Changes(entries) => {
                if self.state == State::Idle {
                    self.start(entries.first());
                }
                if !self.rewrite {
                    for e in &entries {
                        self.seq += 1;
                        let meta = rj01::EditMeta {
                            seq: self.seq,
                            timestamp_ms: now_ms(),
                        };
                        let before = self.pending.len();
                        if rj01::encode_edit(&mut self.pending, meta, e).is_err() {
                            self.rewrite = true;
                            break;
                        }
                        self.since_checkpoint += (self.pending.len() - before) as u64;
                    }
                }
                self.touch(now, entries.len());
            }
        }
        if self.pending.len() > PENDING_CAP {
            self.rewrite = true;
        }
        if self.rewrite {
            self.pending = Vec::new();
        }
    }

    /// Begin a journal at the first change: Header + the base the change applied to.
    fn start(&mut self, first: Option<&ruse_core::ChangeEntry>) {
        self.state = State::Fresh;
        self.pending.clear();
        self.since_checkpoint = 0;
        let Some(base) = self.base.take() else {
            self.rewrite = true;
            return;
        };
        let (revision, node) = first.map_or((0, 0), |e| {
            let node = match e.kind {
                ruse_core::ChangeKind::Fresh { parent, .. } => parent.0,
                ruse_core::ChangeKind::Navigate { .. } => 0,
            };
            (e.base_revision.0, node)
        });
        self.large = base.len() > rj01::LARGE_FILE;
        let b = if self.large {
            Base::Disk {
                len: base.len() as u64,
                sha256: rj01::sha256(&base),
            }
        } else {
            Base::Checkpoint
        };
        let ok = rj01::encode_header(&mut self.pending, &header(Some(&self.doc), self.fmt, b))
            .is_ok()
            && (self.large
                || rj01::encode_checkpoint(&mut self.pending, node, revision, &base).is_ok());
        if !ok {
            self.rewrite = true;
        }
        self.checkpoint_len = if self.large { 0 } else { base.len() as u64 };
    }

    fn touch(&mut self, now: Instant, n: usize) {
        self.pending_changes += n;
        self.last_change = Some(now);
    }

    /// Whether records wait to be written.
    pub fn unsynced(&self) -> bool {
        self.state != State::Idle && (self.rewrite || !self.pending.is_empty())
    }

    /// Whether the sync policy asks for a flush now: [`IDLE`] since the last change, or [`SYNC_EVERY`]
    /// changes buffered.
    pub fn due(&self, now: Instant) -> bool {
        self.unsynced()
            && (self.pending_changes >= SYNC_EVERY
                || self
                    .last_change
                    .is_none_or(|t| now.saturating_duration_since(t) >= IDLE))
    }

    /// How long until [`Writer::due`] turns true with no further change; `None` when nothing waits.
    pub fn wake_in(&self, now: Instant) -> Option<Duration> {
        if !self.unsynced() {
            return None;
        }
        Some(self.last_change.map_or(Duration::ZERO, |t| {
            IDLE.saturating_sub(now.saturating_duration_since(t))
        }))
    }

    /// Write the pending records and a Marker, then `fdatasync`. On failure the records stay pending and
    /// the next try waits for the next trigger.
    pub fn flush(&mut self, doc: &Document) -> io::Result<()> {
        if !self.unsynced() {
            return Ok(());
        }
        if self.state == State::Live
            && !self.large
            && self.since_checkpoint > COMPACT_AFTER
            && self.since_checkpoint > self.checkpoint_len
        {
            self.rewrite = true;
            self.pending = Vec::new();
        }
        let result = self.write_out(doc);
        match &result {
            Ok(()) => {
                self.pending = Vec::new();
                self.rewrite = false;
                self.state = State::Live;
                self.last_sync = Some(Instant::now());
                self.last_error = None;
            }
            Err(e) => {
                self.last_change = Some(Instant::now());
                self.failures += 1;
                self.last_error = Some(e.to_string());
            }
        }
        self.pending_changes = 0;
        result
    }

    fn write_out(&mut self, doc: &Document) -> io::Result<()> {
        if self.state == State::Live && !self.rewrite {
            if self.append()? {
                return Ok(());
            }
            self.rewrite = true; // the journal on disk is not the one this writer left
        }
        if self.state != State::Live {
            // No journal of this writer on disk yet: a session that started since may own the name now.
            self.journal = claim(&self.primary);
        }
        if let Some(dir) = self.journal.parent() {
            make_private_dir(dir)?;
        }
        if self.rewrite {
            let text = doc.bytes();
            let bytes = whole(
                Some(&self.doc),
                self.fmt,
                text,
                doc.revision().0,
                doc.history().current_seq().0,
            )?;
            write_private(&self.journal, &bytes)?;
            self.large = false;
            self.since_checkpoint = 0;
            self.checkpoint_len = text.len() as u64;
            self.on_disk = bytes.len() as u64;
            return Ok(());
        }
        // A fresh journal: the pending records start with its Header.
        let len = self.pending.len();
        rj01::encode_marker(&mut self.pending, now_ms())?;
        let written = write_private(&self.journal, &self.pending);
        if written.is_err() {
            self.pending.truncate(len); // keep the records, not the Marker, for the retry
        }
        written?;
        self.on_disk = self.pending.len() as u64;
        Ok(())
    }

    /// Append the pending records and a Marker to this writer's journal on disk. `Ok(false)` when the
    /// journal is not the length this writer left it (deleted, truncated, written by someone else): nothing
    /// is appended, and the caller rewrites it whole instead.
    fn append(&mut self) -> io::Result<bool> {
        let mut f = match open_regular(&self.journal, OpenOptions::new().append(true)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if f.metadata()?.len() != self.on_disk {
            return Ok(false);
        }
        let mut tail = Vec::new();
        rj01::encode_marker(&mut tail, now_ms())?;
        let written = f
            .write_all(&self.pending)
            .and_then(|()| f.write_all(&tail))
            .and_then(|()| f.sync_data());
        if let Err(e) = written {
            let _ = f.set_len(self.on_disk);
            return Err(e);
        }
        self.on_disk += (self.pending.len() + tail.len()) as u64;
        Ok(true)
    }

    /// Drop the journal (the buffer is clean, or its changes were let go) and start over.
    pub fn discard(&mut self) {
        if self.state != State::Idle {
            discard_file(&self.journal);
        }
        self.state = State::Idle;
        self.rewrite = false;
        self.pending = Vec::new();
        self.pending_changes = 0;
        self.last_change = None;
        self.on_disk = 0;
        self.since_checkpoint = 0;
        self.last_error = None;
    }
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
mod tests {
    use super::*;
    use ruse_core::{BytePos, DocumentId, Edit, EditList, Transaction, TransactionOrigin};

    /// A state home of the test's own.
    fn scratch(tag: &str) -> (Journals, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ruse-journal-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (Journals::in_state_home(&dir.join("state")), dir)
    }

    fn opened(text: &[u8]) -> Document {
        let mut d = Document::new(DocumentId(1), text.to_vec());
        d.mark_saved();
        d
    }

    fn insert(d: &mut Document, at: usize, s: &str) {
        let edits = EditList::new(vec![Edit::insert(BytePos(at), s)]).unwrap();
        d.apply(Transaction::new(
            d.revision(),
            edits,
            TransactionOrigin::UserInput,
        ))
        .unwrap();
        d.sync_undo();
    }

    /// The session's per-frame step, with the sync forced.
    fn step(w: &mut Writer, d: &mut Document) {
        w.absorb(d, FileFormat::default(), Instant::now());
        w.flush(d).unwrap();
    }

    #[test]
    fn names_are_basename_hash_rj_and_fit_name_max() {
        let j = Journals::in_state_home(Path::new("/state"));
        let p = j.path_for_canonical(Path::new("/w/a/notes.md"));
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("notes.md.") && name.ends_with(".rj"),
            "{name}"
        );
        assert_eq!(name.len(), "notes.md.".len() + 16 + 3);
        assert_eq!(p.parent().unwrap(), Path::new("/state/ruse/journal"));
        // Two files of one name in different trees do not collide.
        assert_ne!(p, j.path_for_canonical(Path::new("/w/b/notes.md")));
        // #1196: a 255-byte file name still gives a name within NAME_MAX, archive suffix included.
        let long = "é".repeat(127) + "x";
        assert_eq!(long.len(), 255);
        let p = j.path_for_canonical(&Path::new("/w").join(&long));
        let name = p
            .file_name()
            .unwrap()
            .to_str()
            .expect("cut at a character boundary");
        assert!(name.len() + ARCHIVE_SUFFIX <= NAME_MAX, "{}", name.len());
        // The unnamed buffer's name is fixed for the process.
        let u = j.unnamed();
        assert_eq!(u, j.unnamed());
        let pid = std::process::id().to_string();
        assert!(u
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(&format!("unnamed.{pid}.")));
        assert_eq!(j.archive_dir(), Path::new("/state/ruse/journal/archive"));
    }

    /// The writer starts a journal at the first change (Header + Checkpoint of the text it applied to),
    /// appends Edit records, and a replay gives the current text; the journal is owner-only.
    #[test]
    fn writer_journals_changes_and_replays_them() {
        let (j, dir) = scratch("writer");
        let file = dir.join("f.txt");
        fs::write(&file, b"orig\n").unwrap();
        let mut d = opened(b"orig\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        assert!(!w.journal().exists(), "a clean buffer has no journal");
        insert(&mut d, 0, "a");
        step(&mut w, &mut d);
        insert(&mut d, 1, "b");
        d.undo();
        d.redo();
        step(&mut w, &mut d);
        let raw = read_journal(w.journal()).unwrap();
        let r = rj01::replay(&raw, None);
        assert_eq!(r.text.as_deref(), Some(&b"aborig\n"[..]));
        assert_eq!(r.changes, 4, "two edits, an undo and a redo");
        assert!(r.last_marker_ms.is_some());
        assert_eq!(r.header.unwrap().path, path_bytes(&canonical(&file)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(w.journal()), 0o600);
            assert_eq!(mode(j.dir()), 0o700);
        }
        // Undo back to the saved text: the journal goes (#862).
        d.undo();
        d.undo();
        step(&mut w, &mut d);
        assert!(!w.journal().exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sync_policy_waits_for_idle_or_a_change_count() {
        let (j, dir) = scratch("policy");
        let file = dir.join("f.txt");
        let mut d = opened(b"");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        let t0 = Instant::now();
        w.absorb(&mut d, FileFormat::default(), t0);
        insert(&mut d, 0, "x");
        w.absorb(&mut d, FileFormat::default(), t0);
        assert!(w.unsynced());
        assert!(!w.due(t0), "a change alone does not sync");
        assert!(w.due(t0 + IDLE), "idle does");
        assert_eq!(w.wake_in(t0), Some(IDLE));
        for i in 1..SYNC_EVERY {
            insert(&mut d, i, "x");
        }
        w.absorb(&mut d, FileFormat::default(), t0);
        assert!(w.due(t0), "{SYNC_EVERY} changes do");
        w.flush(&d).unwrap();
        assert!(!w.unsynced() && w.wake_in(t0).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A journal deleted or changed under the writer is rewritten whole, never appended to blindly.
    #[test]
    fn a_journal_changed_under_the_writer_is_rewritten() {
        let (j, dir) = scratch("moved");
        let file = dir.join("f.txt");
        let mut d = opened(b"t\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "1");
        step(&mut w, &mut d);
        fs::remove_file(w.journal()).unwrap();
        insert(&mut d, 0, "2");
        step(&mut w, &mut d);
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"21t\n"[..]));
        fs::write(w.journal(), b"junk").unwrap();
        insert(&mut d, 0, "3");
        step(&mut w, &mut d);
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"321t\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A failed write keeps every record and retries; nothing is dropped (#907).
    #[cfg(unix)]
    #[test]
    fn a_failed_write_keeps_the_records_for_the_retry() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions. Root ignores file modes.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (j, dir) = scratch("retry");
        let file = dir.join("f.txt");
        let mut d = opened(b"t\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "1");
        step(&mut w, &mut d);
        fs::set_permissions(w.journal(), fs::Permissions::from_mode(0o400)).unwrap();
        insert(&mut d, 0, "2");
        w.absorb(&mut d, FileFormat::default(), Instant::now());
        assert!(w.flush(&d).is_err());
        assert!(w.unsynced(), "the records stay");
        assert!(
            !w.due(Instant::now()),
            "the retry waits for the next trigger"
        );
        fs::set_permissions(w.journal(), fs::Permissions::from_mode(0o600)).unwrap();
        insert(&mut d, 0, "3");
        step(&mut w, &mut d);
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"321t\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    /// RFC-0023 §4: once the edits since the last Checkpoint pass 1 MiB and the Checkpoint's size, the
    /// journal is rewritten as Header + Checkpoint of the current text.
    #[test]
    fn a_long_edit_stream_is_compacted() {
        let (j, dir) = scratch("compact");
        let file = dir.join("f.txt");
        let mut d = opened(b"small\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        let chunk = "x".repeat(64 << 10);
        for _ in 0..20 {
            insert(&mut d, 0, &chunk);
            d.undo(); // each pair journals the chunk twice; the text stays small
            insert(&mut d, 0, "k");
            step(&mut w, &mut d);
        }
        let raw = read_journal(w.journal()).unwrap();
        assert!(raw.len() < 2 << 20, "compacted: {} bytes", raw.len());
        let r = rj01::replay(&raw, None);
        assert_eq!(r.text.as_deref(), Some(d.bytes()));
        assert!(
            r.changes < 60,
            "the history before the compaction is gone: {}",
            r.changes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A rename moves the journal to the new name with a Meta rename; a `:cd` respelling changes nothing.
    #[test]
    fn a_rename_moves_the_journal() {
        let (j, dir) = scratch("rename");
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        let mut d = opened(b"t\n");
        let mut w = Writer::new(&j, &a, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "1");
        step(&mut w, &mut d);
        let old = w.journal().to_path_buf();
        w.follow(&j, &dir.join(".").join("a.txt"));
        assert_eq!(w.journal(), old);
        w.follow(&j, &b);
        insert(&mut d, 0, "2");
        step(&mut w, &mut d);
        assert!(!old.exists());
        let r = rj01::replay(&read_journal(&j.path_for(&b)).unwrap(), None);
        assert_eq!(r.text.as_deref(), Some(&b"21t\n"[..]));
        assert_eq!(r.header.unwrap().path, path_bytes(&canonical(&b)));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A Checkpoint drain (recording just turned on with the buffer modified, as for a recovered buffer,
    /// or after `:e!`) writes the journal whole.
    #[test]
    fn a_checkpoint_drain_writes_the_whole_text() {
        let (j, dir) = scratch("ckpt");
        let file = dir.join("f.txt");
        let mut d = opened(b"disk\n");
        insert(&mut d, 0, "rec ");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"rec disk\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rescue_cuts_a_torn_tail_and_appends_the_exact_text() {
        let (j, dir) = scratch("rescue");
        let file = dir.join("f.txt");
        let journal = j.write_text(Some(&file), b"one\n").unwrap();
        let mut raw = fs::read(&journal).unwrap();
        let good = raw.len();
        raw.extend_from_slice(b"RJ01torn");
        fs::write(&journal, &raw).unwrap();
        rescue(&journal, Some(&file), FileFormat::default(), b"two\n", 0, 0).unwrap();
        let raw = fs::read(&journal).unwrap();
        let r = rj01::replay(&raw, None);
        assert_eq!(r.text.as_deref(), Some(&b"two\n"[..]));
        assert!(!r.lost_tail);
        assert!(raw.len() > good);
        // A journal with no intact Header is replaced.
        fs::write(&journal, b"garbage").unwrap();
        rescue(
            &journal,
            Some(&file),
            FileFormat::default(),
            b"three\n",
            0,
            0,
        )
        .unwrap();
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"three\n"[..]));
        let _ = fs::remove_dir_all(&dir);
    }

    fn available(r: Recovery) -> Available {
        match r {
            Recovery::Available(a) => *a,
            other => panic!("not offered: {other:?}"),
        }
    }

    fn unreplayable(r: Recovery) -> String {
        match r {
            Recovery::Unreplayable { reason } => reason,
            other => panic!("not a note: {other:?}"),
        }
    }

    /// The open-time offer: nothing for no journal or one that matches the disk; the text otherwise, with
    /// what the prompt shows; a note for a newer major version, which is kept.
    #[test]
    fn the_offer_follows_the_journal() {
        let (j, dir) = scratch("offer");
        let file = dir.join("f.txt");
        fs::write(&file, b"disk\n").unwrap();
        assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
        j.write_crashed(Some(&file), b"disk\n").unwrap();
        assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
        let journal = j.write_crashed(Some(&file), b"work\n").unwrap();
        let a = available(j.offer(Some(&file), b"disk\n"));
        assert_eq!(a.recovered, b"work\n");
        assert_eq!(a.journal, journal);
        assert!(!a.legacy && !a.lost_tail);
        assert_eq!(a.base, Some(Base::Checkpoint));
        assert_eq!(a.changes, Some(0));
        assert_eq!(a.owner.unwrap().pid, GONE_PID);
        assert!(a.up_to.is_some());
        // This process's own journal (left by an earlier buffer of this session) is offered too: it is not
        // another session's.
        j.write_text(Some(&file), b"mine\n").unwrap();
        assert_eq!(
            available(j.offer(Some(&file), b"disk\n")).recovered,
            b"mine\n"
        );
        j.write_crashed(Some(&file), b"work\n").unwrap();
        // A newer major version is kept and named.
        let mut raw = fs::read(&journal).unwrap();
        raw[5] = 2;
        let crc = rj01::crc32c(&raw[..12]);
        raw[12..16].copy_from_slice(&crc.to_le_bytes());
        fs::write(&journal, &raw).unwrap();
        let n = unreplayable(j.offer(Some(&file), b"disk\n"));
        assert!(n.contains("newer ruse"), "{n}");
        assert!(journal.exists());
        // This version never writes over it: its own journal goes beside it.
        assert_ne!(claim(&journal), journal);
        let mut d = opened(b"disk\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "x");
        step(&mut w, &mut d);
        assert_eq!(
            fs::read(&journal).unwrap(),
            raw,
            "the newer journal is kept as it was"
        );
        w.discard();
        let _ = fs::remove_dir_all(&dir);
    }

    /// A journal this process cannot read (mode 000) is reported at open and never written over: its bytes
    /// and mode stay, and this session's records go beside it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_journal_is_kept_and_written_beside() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions. Root reads a mode-000 file.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (j, dir) = scratch("unreadable");
        let file = dir.join("f.txt");
        fs::write(&file, b"disk\n").unwrap();
        let journal = j.write_crashed(Some(&file), b"crash work\n").unwrap();
        let bytes = fs::read(&journal).unwrap();
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o000)).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let n = unreplayable(j.offer(Some(&file), b"disk\n"));
        assert!(n.contains("unreadable"), "{n}");
        let mut d = opened(b"disk\n");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "x");
        step(&mut w, &mut d);
        assert_ne!(w.journal(), journal);
        assert_eq!(w.journal(), alternate(&journal, std::process::id()));
        let wrote = rescue(&journal, None, FileFormat::default(), b"crash\n", 0, 0).unwrap();
        assert_eq!(wrote, w.journal(), "the crash path writes beside it too");
        j.discard(Some(&file));
        j.clear(Some(&file));
        assert_eq!(mode(&journal), 0o000, "the mode is unchanged");
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            fs::read(&journal).unwrap(),
            bytes,
            "the bytes are unchanged"
        );
        let r = rj01::replay(&read_journal(w.journal()).unwrap(), None);
        assert_eq!(
            r.text.as_deref(),
            Some(&b"crash\n"[..]),
            "the new records are beside it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// RFC-0023 §7 / #1193: a journal whose owner is a live process of this host with a matching start time
    /// is in use; the same pid with another start time (a reused pid) is a crash; this process is neither.
    #[test]
    fn a_live_owner_makes_the_file_in_use() {
        let (j, dir) = scratch("inuse");
        let file = dir.join("f.txt");
        fs::write(&file, b"disk\n").unwrap();
        // A live process other than this one: a child that waits.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = process_start_ms(pid);
        let live = Owner {
            pid,
            host: owner().host.clone(),
            start_ms: start.unwrap_or(0),
        };
        j.write_as(Some(&file), b"theirs\n", &live).unwrap();
        if start.is_some() {
            assert!(is_live_other(&live));
            match j.offer(Some(&file), b"disk\n") {
                Recovery::InUse { owner: o, journal } => {
                    assert_eq!(o, live);
                    assert_eq!(journal, j.path_for(&file));
                }
                other => panic!("{other:?}"),
            }
            // This session writes beside it, never over it, and never deletes it.
            let alt = claim(&j.path_for(&file));
            assert_ne!(alt, j.path_for(&file));
            assert!(alt
                .to_string_lossy()
                .ends_with(&format!(".{}.rj", std::process::id())));
            j.discard(Some(&file));
            j.clear(Some(&file));
            assert!(
                j.path_for(&file).exists(),
                "another session's journal stays"
            );
            let mut d = opened(b"disk\n");
            let mut w = Writer::new(&j, &file, FileFormat::default());
            step(&mut w, &mut d);
            insert(&mut d, 0, "x");
            step(&mut w, &mut d);
            assert_eq!(w.journal(), alt);
            assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"theirs\n"[..]));
            let wrote = rescue(
                &j.path_for(&file),
                None,
                FileFormat::default(),
                b"crash\n",
                0,
                0,
            )
            .unwrap();
            assert_eq!(wrote, alt, "the crash path also writes beside it");
            assert_eq!(j.candidates(&file), [j.path_for(&file), alt.clone()]);
            w.discard();
            assert!(!alt.exists());
        }
        // The same pid with another start time is a crash: offered for recovery.
        let reused = Owner {
            start_ms: live.start_ms.wrapping_add(60_000),
            ..live.clone()
        };
        assert!(!is_live_other(&reused));
        j.write_as(Some(&file), b"theirs\n", &reused).unwrap();
        assert_eq!(
            available(j.offer(Some(&file), b"disk\n")).recovered,
            b"theirs\n"
        );
        // Another host's journal cannot be checked: offered.
        let elsewhere = Owner {
            host: b"some-other-host".to_vec(),
            ..live.clone()
        };
        assert!(!is_live_other(&elsewhere));
        assert!(
            !is_live_other(owner()),
            "this process is not another session"
        );
        let _ = child.kill();
        let _ = child.wait();
        // Once it is gone, its journal is a crash.
        assert!(!is_live_other(&live));
        j.write_as(Some(&file), b"theirs\n", &live).unwrap();
        assert!(matches!(
            j.offer(Some(&file), b"disk\n"),
            Recovery::Available(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_process_start_time_is_read_and_stable() {
        let me = std::process::id();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(
            process_start_ms(me).is_some(),
            "readable where the in-use check relies on it"
        );
        if let Some(t) = process_start_ms(me) {
            assert!(t <= now_ms() && now_ms() - t < 24 * 3600 * 1000 * 30);
            assert!(process_start_ms(me).unwrap().abs_diff(t) <= START_SLACK_MS);
            assert!(owner().start_ms.abs_diff(t) <= START_SLACK_MS);
        }
        assert_eq!(process_start_ms(GONE_PID), None);
    }

    /// `r` takes the journal over as this process's own, and archives one of another name it came from.
    #[test]
    fn adopting_a_recovery_owns_the_journal() {
        let (j, dir) = scratch("adopt");
        let file = dir.join("f.txt");
        fs::write(&file, b"disk\n").unwrap();
        j.write_crashed(Some(&file), b"work\n").unwrap();
        let a = available(j.offer(Some(&file), b"disk\n"));
        let p = j.adopt(&a, Some(&file), FileFormat::default()).unwrap();
        assert_eq!(p, j.path_for(&file));
        assert_eq!(read_owner(&p).as_ref(), Some(owner()));
        assert_eq!(j.replay(Some(&file)).as_deref(), Some(&b"work\n"[..]));
        assert!(!j.archive_dir().exists(), "nothing to archive: same name");
        // `:q!` now discards it.
        j.discard(Some(&file));
        assert!(!p.exists());
        // A second session's crashed `<name>.<pid>.rj` is archived once taken over.
        let alt = alternate(&j.path_for(&file), GONE_PID);
        j.write_crashed(Some(&file), b"alt work\n").unwrap();
        fs::rename(j.path_for(&file), &alt).unwrap();
        let a = available(j.offer(Some(&file), b"disk\n"));
        assert_eq!(a.journal, alt);
        j.adopt(&a, Some(&file), FileFormat::default()).unwrap();
        assert!(!alt.exists());
        assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Two archives of one journal in the same millisecond do not replace each other.
    #[test]
    fn archives_never_replace_each_other() {
        let (j, dir) = scratch("archive");
        let file = dir.join("f.txt");
        for _ in 0..3 {
            let p = j.write_crashed(Some(&file), b"x").unwrap();
            j.archive(&p).unwrap();
        }
        assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    /// RFC-0023 §5: a document over 32 MiB journals against its disk text; a changed disk file refuses,
    /// and the journal is archived.
    #[test]
    fn a_large_document_journals_against_the_disk_text() {
        let (j, dir) = scratch("large");
        let file = dir.join("big.txt");
        let disk = vec![b'a'; rj01::LARGE_FILE + 1];
        let mut d = opened(&disk);
        let mut w = Writer::new(&j, &file, FileFormat::default());
        step(&mut w, &mut d);
        insert(&mut d, 0, "+");
        step(&mut w, &mut d);
        let raw = read_journal(w.journal()).unwrap();
        assert!(
            raw.len() < 4096,
            "no Checkpoint of the text: {} bytes",
            raw.len()
        );
        assert!(matches!(
            rj01::replay(&raw, None).header.unwrap().base,
            Base::Disk { .. }
        ));
        // The writer is this process: the journal is offered (it is not another session's).
        let text = available(j.offer(Some(&file), &disk)).recovered;
        assert!(text.len() == disk.len() + 1 && text[0] == b'+');
        let mut changed = disk.clone();
        changed[10] = b'b';
        let n = unreplayable(j.offer(Some(&file), &changed));
        assert!(
            n.contains("changed on disk") && n.contains("1 changes"),
            "{n}"
        );
        assert!(!w.journal().exists(), "archived");
        assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// RFC-0023 §3: a legacy RJS0 sidecar is offered once, then archived on any answer but quit.
    #[test]
    fn a_legacy_sidecar_is_offered_and_archived() {
        let (j, dir) = scratch("legacy");
        let file = dir.join("f.txt");
        fs::write(&file, b"disk\n").unwrap();
        let sidecar = rjs0::sidecar_path(Some(&file));
        let mut raw = rjs0::frame(b"old");
        raw.extend(rjs0::frame(b"legacy work\n"));
        fs::write(&sidecar, &raw).unwrap();
        let a = available(j.offer(Some(&file), b"disk\n"));
        assert_eq!(a.recovered, b"legacy work\n");
        assert!(a.legacy && a.changes.is_none() && a.owner.is_none());
        j.adopt(&a, Some(&file), FileFormat::default()).unwrap();
        assert!(!sidecar.exists());
        assert_eq!(fs::read_dir(j.archive_dir()).unwrap().count(), 1);
        // The recovered text is now this session's RJ01 journal.
        assert_eq!(
            j.replay(Some(&file)).as_deref(),
            Some(&b"legacy work\n"[..])
        );
        j.discard(Some(&file));
        assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
        // A sidecar holding the disk text is archived without a prompt.
        fs::write(&sidecar, rjs0::frame(b"disk\n")).unwrap();
        assert_eq!(j.offer(Some(&file), b"disk\n"), Recovery::None);
        assert!(!sidecar.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// #869: a FIFO where the journal should be is never opened as one: the offer, the crash write and the
    /// writer fail at once instead of blocking, and discard leaves it alone.
    #[cfg(unix)]
    #[test]
    fn a_journal_that_is_not_a_regular_file_is_refused_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;
        let (j, dir) = scratch("fifo");
        let file = dir.join("f.txt");
        j.ensure_dir().unwrap();
        let jp = j.path_for(&file);
        let c = std::ffi::CString::new(jp.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let t = Instant::now();
        let n = unreplayable(j.offer(Some(&file), b""));
        assert!(n.contains("is not a regular file"), "{n}");
        assert!(j.write_text(Some(&file), b"x").is_err());
        let mut d = opened(b"");
        let mut w = Writer::new(&j, &file, FileFormat::default());
        w.absorb(&mut d, FileFormat::default(), t);
        insert(&mut d, 0, "x");
        w.absorb(&mut d, FileFormat::default(), t);
        assert!(w.flush(&d).is_err());
        j.discard(Some(&file));
        w.discard();
        assert!(t.elapsed() < Duration::from_secs(2), "blocked");
        assert!(fs::symlink_metadata(&jp).unwrap().file_type().is_fifo());
        let _ = fs::remove_dir_all(&dir);
    }

    /// #1192: the unnamed buffer's journal is offered to the next pathless start once its writer is gone,
    /// and becomes that session's own on recover.
    #[test]
    fn an_orphaned_unnamed_journal_is_offered_and_adopted() {
        let (j, dir) = scratch("unnamed");
        j.ensure_dir().unwrap();
        // A pid no process has: above every platform's pid range.
        let orphan = j.dir().join("unnamed.2147483646.1.rj");
        let crashed = j.write_crashed(None, b"unnamed work\n").unwrap();
        fs::rename(&crashed, &orphan).unwrap();
        let a = available(j.offer(None, b""));
        assert_eq!(a.recovered, b"unnamed work\n");
        assert_eq!(a.journal, orphan);
        j.adopt(&a, None, FileFormat::default()).unwrap();
        assert!(!orphan.exists());
        assert_eq!(read_owner(&j.unnamed()).as_ref(), Some(owner()));
        // This process's own unnamed journal is never offered to itself.
        assert_eq!(j.offer(None, b""), Recovery::None);
        let _ = fs::remove_dir_all(&dir);
    }
}
