//! Durable save (F-008 acceptance #1: "Saved" reflects an fsync, not just a `write()`).
//!
//! **Replace by rename** (the normal path): write a uniquely named sibling temp file
//! (`.<name>.ruse-tmp-<rand>`, created `O_EXCL`), give it the original's owner, mode and extended
//! attributes, `fsync` it, `rename` it over the target (atomic on a POSIX filesystem), then `fsync` the
//! containing directory (`.` for a bare relative name) so the rename itself is durable. A crash at any point
//! leaves EITHER the old file or the fully-written new one, never a truncated target (the anti-pattern D-005
//! fixed). The temp is a sibling so the rename stays on one filesystem.
//!
//! **Write in place** (Neovim's `'backupcopy'` `auto`): a rename would give the path a NEW inode, which is
//! wrong when the file has other hard links (they would keep the old bytes), when its owner/group cannot be
//! reproduced on a new file, when an extended attribute (on Linux this includes a POSIX ACL) cannot be copied,
//! or when the directory is not writable. Then the existing file is overwritten and `fsync`ed. That is
//! durable but not atomic: a crash or an error during the write can leave the file partly written, and an
//! error says so ([`SaveError::Partial`]).
//!
//! **Refusals**: a FIFO, socket, device or directory is never replaced (#907); an existing file that is not
//! writable is refused with nvim's `E505` unless the write is forced (`:w!`).
//!
//! **Honesty** (#936): every fsync's result counts. If the directory cannot be fsynced the file has been
//! replaced but the save is NOT reported durable ([`SaveError::NotDurable`]), so the caller keeps the buffer
//! modified and its recovery journal. A filesystem that does not support a directory fsync (`EINVAL`,
//! `ENOTSUP`, `EOPNOTSUPP`, `EISDIR`) is not a failure: the save counts, with
//! [`Method::RenamedDirSyncUnsupported`] and a report note that the rename's durability is not proven.
//!
//! Directory fsync, ownership and xattr handling are Unix-only; elsewhere the file fsync + rename still give
//! atomic replacement. Known gap: macOS ACLs (not exposed as xattrs) are not carried over by the rename path.
//!
//! **Write policy** (#619, [`write_with`]): `'backupcopy'` picks the method (`yes` in place, `no` rename even
//! over hard links, `breaksymlink`/`breakhardlink`), `'fsync'` the syncs, and [`super::backup`] makes the
//! backups just before the file is replaced (a write backup guards an in-place write).
//!
//! **Symlinks**: a symlinked target is written THROUGH — the link is resolved and the file it points at is
//! saved, so the symlink itself survives.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Symlink hops followed before giving up (Linux's `MAXSYMLINKS`); guards a link cycle.
const MAX_SYMLINK_HOPS: usize = 40;

/// The longest file name the common filesystems accept (`NAME_MAX`); the temp name stays within it.
const NAME_MAX: usize = 255;

/// The temp name: `.` + name + `.ruse-tmp-` + the writer's pid + `-` + 8 hex digits. The pid lets a later
/// save tell a crashed save's leftover from a live one's (#1197).
const TEMP_TAG: &str = ".ruse-tmp-";
const TEMP_RAND_LEN: usize = 8;
/// Room for the pid (up to 10 digits) and its `-`.
const TEMP_PID_LEN: usize = 11;

/// Fresh temp names tried before giving up when each one already exists.
const TEMP_ATTEMPTS: u32 = 16;

/// How a successful save reached the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// A new inode was renamed over the target (atomic).
    Renamed,
    /// Renamed, but the filesystem does not support a directory fsync (`EINVAL`/`ENOTSUP`/...), so the
    /// rename's durability is not proven. The save counts (the bytes are fsynced in the file); the message
    /// says what was not proven, as SQLite and PostgreSQL treat an unsupported directory fsync.
    RenamedDirSyncUnsupported,
    /// The existing file was overwritten (keeps hard links, owner, ACLs and xattrs; not atomic).
    InPlace,
}

impl Method {
    /// The suffix the write report carries for this method: empty unless something was not proven.
    pub fn note(self) -> &'static str {
        match self {
            Method::RenamedDirSyncUnsupported => {
                " (directory sync not supported here; rename durability not proven)"
            }
            Method::Renamed | Method::InPlace => "",
        }
    }
}

/// Why a save did not complete. `Display` is the status-line text.
#[derive(Debug)]
pub enum SaveError {
    /// `E505`: the existing file is not writable and the write was not forced.
    ReadOnly(PathBuf),
    /// The target is a FIFO, socket, device or directory (#907); nothing was written.
    NotRegular(PathBuf),
    /// The save failed before the target was touched: the file on disk is unchanged.
    Untouched(io::Error),
    /// An in-place write failed after it began: the file may hold part of the new bytes.
    Partial { path: PathBuf, err: io::Error },
    /// The new bytes are in the file, but an fsync that makes them durable failed (`E667`).
    NotDurable { path: PathBuf, err: io::Error },
    /// The backup the write needs could not be made (`E509`/`E510`); nothing was written.
    Backup(super::backup::BackupError),
    /// The write failed and its write backup is kept: `note` names it.
    Kept { err: Box<SaveError>, note: String },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::ReadOnly(p) => {
                write!(
                    f,
                    "E505: \"{}\" is read-only (add ! to override)",
                    p.display()
                )
            }
            SaveError::NotRegular(p) => {
                write!(f, "write failed: {}: is not a regular file", p.display())
            }
            SaveError::Untouched(e) => write!(f, "write failed: {e}"),
            SaveError::Partial { path, err } => write!(
                f,
                "E514: Write error, \"{}\" may be partly written: {err}",
                path.display()
            ),
            SaveError::NotDurable { path, err } => write!(
                f,
                "E667: Fsync failed: \"{}\": {err} (written but not proven durable)",
                path.display()
            ),
            SaveError::Backup(e) => write!(f, "{e}"),
            SaveError::Kept { err, note } => write!(f, "{err}; {note}"),
        }
    }
}

impl std::error::Error for SaveError {}

impl SaveError {
    /// The `io::ErrorKind` this failure is.
    fn kind(&self) -> io::ErrorKind {
        match self {
            SaveError::Kept { err, .. } => err.kind(),
            SaveError::Backup(b) => b.err.kind(),
            SaveError::ReadOnly(_) => io::ErrorKind::PermissionDenied,
            SaveError::NotRegular(_) => io::ErrorKind::InvalidInput,
            SaveError::Untouched(err)
            | SaveError::Partial { err, .. }
            | SaveError::NotDurable { err, .. } => err.kind(),
        }
    }
}

impl From<SaveError> for io::Error {
    fn from(e: SaveError) -> io::Error {
        io::Error::new(e.kind(), e.to_string())
    }
}

/// The steps a test can make fail, for the failures a real filesystem will not produce on demand.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    SyncFile,
    OpenDir,
    SyncDir,
    InPlaceWrite,
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<(Step, Option<i32>)>> = const { std::cell::Cell::new(None) };
    static SYNCED_DIRS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Make `step` fail on this thread with a real I/O error (`EIO`) until cleared with `None` (test seam).
#[cfg(test)]
pub(crate) fn inject(step: Option<Step>) {
    FAULT.with(|f| f.set(step.map(|s| (s, None))));
}

/// Make `step` fail on this thread with the OS error `errno` until cleared with `inject(None)`.
#[cfg(test)]
pub(crate) fn inject_errno(step: Step, errno: i32) {
    FAULT.with(|f| f.set(Some((step, Some(errno)))));
}

/// The directories this thread fsynced, oldest first, draining the log (test seam).
#[cfg(test)]
pub(crate) fn take_synced_dirs() -> Vec<PathBuf> {
    SYNCED_DIRS.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

macro_rules! fault {
    ($step:ident) => {
        #[cfg(test)]
        if let Some((Step::$step, errno)) = FAULT.with(|f| f.get()) {
            return Err(match errno {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::other(concat!("injected ", stringify!($step), " failure")),
            }
            .into());
        }
    };
}

/// Follow `path` through any chain of symlinks to the file a write should replace. A relative link is
/// resolved against the link's own directory. A dangling link resolves to its (missing) destination, which
/// the save then creates — the same thing writing through the link would do.
fn resolve_write_target(path: &Path) -> io::Result<PathBuf> {
    let mut p = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                let link = fs::read_link(&p)?;
                p = match p.parent() {
                    Some(dir) if link.is_relative() => dir.join(link),
                    _ => link,
                };
            }
            _ => return Ok(p),
        }
    }
    Err(io::Error::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// The directory whose entry a rename changes: the parent, or `.` for a bare relative name (#865:
/// `Path::new("foo").parent()` is `Some("")`).
fn containing_dir(target: &Path) -> PathBuf {
    match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// A random-enough 32-bit value without a crate: std's per-process random hash keys, salted with a
/// counter and the clock. Uniqueness is enforced by `O_EXCL`, not by this.
fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish() as u32
}

/// `.<name>.ruse-tmp-<pid>-<rand>` next to `target`, with `<name>` shortened so the whole stays within
/// `NAME_MAX` (#868: a name near the limit could not be saved).
fn temp_path(target: &Path, rand: u32) -> PathBuf {
    let mut tmp = temp_prefix(target);
    tmp.push(format!("{}-{rand:08x}", std::process::id()));
    target.with_file_name(tmp)
}

/// `.<name>.ruse-tmp-` of `target`'s temp names, `<name>` shortened as [`temp_path`] does.
fn temp_prefix(target: &Path) -> std::ffi::OsString {
    let room = NAME_MAX - 1 - TEMP_TAG.len() - TEMP_PID_LEN - TEMP_RAND_LEN;
    let name = target.file_name().unwrap_or_default();
    #[cfg(unix)]
    let stem = {
        use std::os::unix::ffi::OsStrExt;
        let b = name.as_bytes();
        std::ffi::OsStr::from_bytes(&b[..b.len().min(room)]).to_os_string()
    };
    #[cfg(not(unix))]
    let stem = {
        let s = name.to_string_lossy();
        let mut end = s.len().min(room);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        std::ffi::OsString::from(&s[..end])
    };
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(stem);
    tmp.push(TEMP_TAG);
    tmp
}

/// The writer's pid in a temp name of `target` (`prefix` = [`temp_prefix`]): `None` unless the name is
/// exactly `<prefix><digits>-<8 hex digits>`.
fn temp_writer(name: &[u8], prefix: &[u8]) -> Option<u32> {
    let rest = name.strip_prefix(prefix)?;
    let dash = rest.iter().position(|&b| b == b'-')?;
    let (pid, rand) = (&rest[..dash], &rest[dash + 1..]);
    let digits = !pid.is_empty() && pid.len() < TEMP_PID_LEN && pid.iter().all(u8::is_ascii_digit);
    let hex = rand.len() == TEMP_RAND_LEN
        && rand
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b));
    if !(digits && hex) {
        return None;
    }
    std::str::from_utf8(pid).ok()?.parse().ok()
}

/// Remove the temp files a crashed save of `target` left beside it (#1197): only names of exactly
/// `target`'s temp pattern ([`temp_writer`]), only regular files (never a symlink or anything else), and
/// only when their writer is not running (a pid of this host that `kill(pid, 0)` says is gone). A live
/// writer's temp, a name that merely looks alike, and an old temp without a pid are left alone. Done once
/// per target per process, so a save does not list a large directory every time.
fn sweep_stale_temps(target: &Path) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SWEPT: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    {
        let Ok(mut swept) = SWEPT.lock() else {
            return;
        };
        if !swept
            .get_or_insert_with(HashSet::new)
            .insert(target.to_path_buf())
        {
            return;
        }
    }
    let prefix = super::journal::path_bytes(Path::new(&temp_prefix(target)));
    let me = std::process::id();
    let Ok(entries) = fs::read_dir(containing_dir(target)) else {
        return;
    };
    for e in entries.flatten() {
        let name = super::journal::path_bytes(Path::new(&e.file_name()));
        let Some(pid) = temp_writer(&name, &prefix) else {
            continue;
        };
        let is_file = fs::symlink_metadata(e.path()).is_ok_and(|m| m.file_type().is_file());
        if pid != me && is_file && !super::journal::alive(pid) {
            crate::log::stale_temp_removed(&e.path());
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Create a fresh temp next to `target`, exclusively (`O_EXCL`), so it never follows a planted symlink and
/// never truncates another save's temp or a user's file (#868). Owner-only (`0600`) when it will receive an
/// existing file's mode, so the new bytes never sit in a wider-mode file.
fn create_temp(target: &Path, owner_only: bool) -> io::Result<(PathBuf, File)> {
    let mut last = None;
    for _ in 0..TEMP_ATTEMPTS {
        let tmp = temp_path(target, random_u32());
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if owner_only {
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        }
        #[cfg(not(unix))]
        let _ = owner_only;
        match opts.open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no free temp name")))
}

/// Whether the current user may write `path` (`access(W_OK)`).
fn writable(path: &Path, meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let _ = meta;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return true; // cannot ask; let the write itself report
        };
        // SAFETY: `c` is a valid NUL-terminated path.
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        !meta.permissions().readonly()
    }
}

/// Durably replace `target` with `bytes`, forced (no `E505` check). For callers with no user-facing
/// `!` (the recovery journal).
pub fn save(target: &Path, bytes: &[u8]) -> io::Result<()> {
    write(target, bytes, true).map(|_| ()).map_err(Into::into)
}

/// Durably write `bytes` to `path` (through any symlink): by rename where that keeps the file's identity,
/// else in place. `force` (`:w!`) writes a file the user may not write (`E505` otherwise). Returns only after
/// every fsync succeeded; see [`SaveError`] for what each failure leaves on disk.
pub fn write(path: &Path, bytes: &[u8], force: bool) -> Result<Method, SaveError> {
    write_with(path, bytes, &Opts::new(force)).map(|w| w.method)
}

/// How a write reaches the file (`'backupcopy'`): `Auto` renames a new file over it unless that would lose
/// its identity (hard links, owner, xattrs) or the directory is not writable; `Yes` always overwrites it in
/// place; `No` always renames, even over a hard-linked file (whose other names keep the old text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Copy {
    #[default]
    Auto,
    Yes,
    No,
}

/// The write policy of one save: `'backupcopy'`, `'fsync'` and the backups to make (#619).
#[derive(Debug, Clone, Copy)]
pub struct Opts<'a> {
    /// `:w!`: write a file the user may not write, and without the backup that could not be made.
    pub force: bool,
    pub copy: Copy,
    /// `'backupcopy'` `breaksymlink`: replace a symlink with a regular file instead of writing through it.
    pub break_symlink: bool,
    /// `'backupcopy'` `breakhardlink`: rename even over a file with other hard links.
    pub break_hardlink: bool,
    /// `'fsync'`: sync the file and its directory before reporting the write.
    pub fsync: bool,
    pub backup: Option<&'a super::backup::Policy>,
}

impl Opts<'_> {
    /// ruse's default write: `auto`, synced, no backups.
    pub fn new(force: bool) -> Opts<'static> {
        Opts {
            force,
            copy: Copy::Auto,
            break_symlink: false,
            break_hardlink: false,
            fsync: true,
            backup: None,
        }
    }
}

/// A successful write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub method: Method,
    /// The backup kept for the file (`'backup'`, `make-backup-files`).
    pub backup: Option<PathBuf>,
    /// Numbered Emacs backups over `kept-*-versions` that were kept (`delete-old-versions` nil).
    pub excess: usize,
}

impl Written {
    /// The suffix the write report carries: what was not proven, and backups left over.
    pub fn note(&self) -> String {
        let mut n = self.method.note().to_string();
        if self.excess > 0 {
            n.push_str(&format!(" ({} excess backup versions kept)", self.excess));
        }
        n
    }
}

/// [`write`] under a write policy: the `'backupcopy'` method, `'fsync'`, and the backups `opts.backup` asks
/// for, made just before the file is replaced. A backup that cannot be made stops the write (`E509`/`E510`)
/// unless it is forced; a write backup is removed after a successful write and kept (and named) after a
/// failed one.
pub fn write_with(path: &Path, bytes: &[u8], opts: &Opts<'_>) -> Result<Written, SaveError> {
    let link =
        opts.break_symlink && fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    let target = if link {
        path.to_path_buf()
    } else {
        resolve_write_target(path).map_err(SaveError::Untouched)?
    };
    sweep_stale_temps(&target);
    let existing = fs::metadata(&target).ok();
    // A rename would REPLACE a FIFO, socket or device node with a regular file (#907): refuse instead.
    if existing.as_ref().is_some_and(|m| !m.is_file()) {
        return Err(SaveError::NotRegular(target));
    }
    let force = opts.force;
    if let Some(m) = &existing {
        if !force && !writable(&target, m) {
            return Err(SaveError::ReadOnly(path.to_path_buf()));
        }
    }
    let backup = opts.backup;
    let mut copy = opts.copy;
    if let (Some(m), Some(policy)) = (&existing, backup) {
        // Emacs: the save that makes the backup copies or renames by `backup-by-copying*`.
        if let Some(e) = policy.emacs() {
            if policy.wanted(&target, false) {
                copy = if e.copying.in_place(m) {
                    Copy::Yes
                } else {
                    Copy::No
                };
            }
        }
    }
    // The method this write means to use. A rename may still fall back to an in-place write (the owner or
    // an xattr cannot be reproduced, or the directory is not writable); its write backup is then made late.
    let in_place = !link
        && existing.as_ref().is_some_and(|m| match copy {
            Copy::Yes => !(opts.break_hardlink && hard_linked(m)),
            Copy::No => false,
            Copy::Auto => hard_linked(m) && !opts.break_hardlink,
        });
    let mut made = super::backup::Made::default();
    let make = |made: &mut super::backup::Made, in_place: bool| -> Result<(), SaveError> {
        let Some(policy) = backup else {
            return Ok(());
        };
        match super::backup::make(policy, &target, in_place) {
            Ok(m) => *made = m,
            Err(e) if force => crate::log::backup_failed(&target, &e.err),
            Err(e) => return Err(SaveError::Backup(e)),
        }
        Ok(())
    };
    match (&existing, backup) {
        (Some(_), Some(_)) => make(&mut made, in_place)?,
        (None, Some(policy)) => super::backup::patch_new(policy, &target),
        _ => {}
    }
    let result = match &existing {
        Some(m) if in_place => in_place_write(&target, bytes, m, force, opts.fsync),
        _ => match replace(&target, bytes, existing.as_ref(), opts.fsync) {
            Ok(true) => Ok(Method::Renamed),
            Ok(false) => Ok(Method::RenamedDirSyncUnsupported),
            Err(Attempt::Fail(e)) => Err(e),
            Err(Attempt::InPlace) => match &existing {
                Some(m) => {
                    // The rename became an in-place write, which a write backup must guard by a copy.
                    if made.backup.is_none() {
                        make(&mut made, true)?;
                    }
                    in_place_write(&target, bytes, m, force, opts.fsync)
                }
                None => unreachable!("only an existing file falls back to an in-place write"),
            },
        },
    };
    let (backup_path, excess) = (made.backup.clone(), made.excess);
    let keep_note = made.finish(result.is_ok());
    match result {
        Ok(method) => Ok(Written {
            method,
            backup: backup_path.filter(|p| p.exists()),
            excess,
        }),
        Err(e) => Err(match keep_note {
            Some(note) => SaveError::Kept {
                err: Box::new(e),
                note,
            },
            None => e,
        }),
    }
}

/// Why the rename path stopped.
enum Attempt {
    /// A rename would lose the file's identity (owner, xattr) or cannot be made (directory not writable):
    /// write in place instead. Nothing was changed.
    InPlace,
    Fail(SaveError),
}

impl From<io::Error> for Attempt {
    fn from(e: io::Error) -> Attempt {
        Attempt::Fail(SaveError::Untouched(e))
    }
}

#[cfg(unix)]
fn hard_linked(m: &fs::Metadata) -> bool {
    std::os::unix::fs::MetadataExt::nlink(m) > 1
}

#[cfg(not(unix))]
fn hard_linked(_: &fs::Metadata) -> bool {
    false
}

/// Rename a filled temp over `target`; `Ok(false)` when the directory fsync is not supported here.
fn replace(
    target: &Path,
    bytes: &[u8],
    existing: Option<&fs::Metadata>,
    sync: bool,
) -> Result<bool, Attempt> {
    let (tmp, mut f) = match create_temp(target, existing.is_some()) {
        Ok(t) => t,
        // The directory is not writable (or read-only): the file itself may still be.
        Err(e)
            if existing.is_some()
                && matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                ) =>
        {
            return Err(Attempt::InPlace)
        }
        Err(e) => return Err(e.into()),
    };
    let filled = fill_temp(&mut f, target, bytes, existing, sync);
    drop(f);
    if let Err(e) = filled.and_then(|()| fs::rename(&tmp, target).map_err(Attempt::from)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if !sync {
        return Ok(true);
    }
    sync_dir(&containing_dir(target)).map_err(Attempt::Fail)
}

/// Write the bytes, carry the original's owner, mode and xattrs over, and fsync.
fn fill_temp(
    f: &mut File,
    target: &Path,
    bytes: &[u8],
    existing: Option<&fs::Metadata>,
    sync: bool,
) -> Result<(), Attempt> {
    f.write_all(bytes)?;
    if let Some(m) = existing {
        // Owner first: a chown clears the set-id bits the mode then restores.
        #[cfg(unix)]
        if !same_owner(f, m)? {
            return Err(Attempt::InPlace);
        }
        f.set_permissions(m.permissions())?; // fchmod: the original file's mode carries over
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
        if xattr::copy(target, f).is_err() {
            return Err(Attempt::InPlace);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    let _ = target;
    if sync {
        fault!(SyncFile);
        f.sync_all()?; // the bytes are on stable storage before the rename makes them the file
    }
    Ok(())
}

/// Give the temp the original's uid/gid; `false` when that is not permitted (another user's file).
#[cfg(unix)]
fn same_owner(f: &File, m: &fs::Metadata) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let t = f.metadata()?;
    if t.uid() == m.uid() && t.gid() == m.gid() {
        return Ok(true);
    }
    // SAFETY: the fd is open for the lifetime of `f`.
    Ok(unsafe { libc::fchown(f.as_raw_fd(), m.uid(), m.gid()) } == 0)
}

/// Whether an error from opening or fsyncing a directory means the filesystem does not support it (some
/// network and FUSE filesystems), rather than that the sync failed. SQLite and PostgreSQL draw the same line.
#[cfg(unix)]
fn dir_sync_unsupported(e: &io::Error) -> bool {
    e.raw_os_error()
        .is_some_and(|c| [libc::EINVAL, libc::ENOTSUP, libc::EOPNOTSUPP, libc::EISDIR].contains(&c))
}

/// fsync `dir` so a rename in it is durable. `Ok(false)`: the filesystem does not support it, so durability
/// is not proven but nothing failed. Any other failure (EIO, ENOSPC, EROFS, EACCES, ...) is `E667` (#936).
fn sync_dir(dir: &Path) -> Result<bool, SaveError> {
    #[cfg(unix)]
    {
        let fail = |err: io::Error| {
            if dir_sync_unsupported(&err) {
                Ok(false)
            } else {
                Err(SaveError::NotDurable {
                    path: dir.to_path_buf(),
                    err,
                })
            }
        };
        let open = || -> io::Result<File> {
            fault!(OpenDir);
            File::open(dir)
        };
        let d = match open() {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
        let sync = || -> io::Result<()> {
            fault!(SyncDir);
            d.sync_all()
        };
        if let Err(e) = sync() {
            return fail(e);
        }
        #[cfg(test)]
        SYNCED_DIRS.with(|s| s.borrow_mut().push(dir.to_path_buf()));
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(true)
}

/// Overwrite the existing `target` in place and fsync it. `force` lets the owner write a file without write
/// permission by adding it for the duration (as Vim's `:w!` does).
fn in_place_write(
    target: &Path,
    bytes: &[u8],
    meta: &fs::Metadata,
    force: bool,
    sync: bool,
) -> Result<Method, SaveError> {
    let mut restore = None;
    let open = |t: &Path| OpenOptions::new().write(true).open(t);
    let mut f = match open(target) {
        Ok(f) => f,
        #[cfg(unix)]
        Err(e) if force && e.kind() == io::ErrorKind::PermissionDenied => {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            fs::set_permissions(target, fs::Permissions::from_mode(mode | 0o200))
                .map_err(|_| SaveError::Untouched(e))?;
            restore = Some(meta.permissions());
            match open(target) {
                Ok(f) => f,
                Err(e) => {
                    let _ = fs::set_permissions(target, meta.permissions());
                    return Err(SaveError::Untouched(e));
                }
            }
        }
        Err(e) => return Err(SaveError::Untouched(e)),
    };
    #[cfg(not(unix))]
    let _ = (meta, force);
    let written = (|| -> io::Result<()> {
        fault!(InPlaceWrite);
        f.write_all(bytes)?;
        f.set_len(bytes.len() as u64)
    })();
    let synced = written.as_ref().ok().filter(|_| sync).map(|_| {
        fault!(SyncFile);
        f.sync_all()
    });
    if let Some(p) = restore {
        let _ = fs::set_permissions(target, p); // best-effort: the bytes are already written
    }
    match (written, synced) {
        (Err(err), _) => Err(SaveError::Partial {
            path: target.to_path_buf(),
            err,
        }),
        (Ok(()), Some(Err(err))) => Err(SaveError::NotDurable {
            path: target.to_path_buf(),
            err,
        }),
        _ => Ok(Method::InPlace),
    }
}

/// Copy a file's extended attributes (on Linux also its POSIX ACL, `system.posix_acl_access`) to the new
/// file. An `Err` means one could not be reproduced, so the caller writes in place instead of losing it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod xattr {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// Attributes the kernel itself attaches to every file a process creates: neither copyable nor lost.
    const KERNEL_MANAGED: &[&[u8]] = &[b"com.apple.provenance"];

    /// Grow-and-retry for the list/get size race (an attribute changing between the two calls).
    const RETRIES: usize = 4;

    #[cfg(target_os = "macos")]
    unsafe fn list(p: *const c_char, buf: *mut c_char, n: usize) -> isize {
        libc::listxattr(p, buf, n, 0)
    }
    #[cfg(not(target_os = "macos"))]
    unsafe fn list(p: *const c_char, buf: *mut c_char, n: usize) -> isize {
        libc::listxattr(p, buf, n)
    }
    #[cfg(target_os = "macos")]
    unsafe fn get(p: *const c_char, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
        libc::getxattr(p, name, buf, n, 0, 0)
    }
    #[cfg(not(target_os = "macos"))]
    unsafe fn get(p: *const c_char, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
        libc::getxattr(p, name, buf, n)
    }
    #[cfg(target_os = "macos")]
    unsafe fn fget(fd: i32, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
        libc::fgetxattr(fd, name, buf, n, 0, 0)
    }
    #[cfg(not(target_os = "macos"))]
    unsafe fn fget(fd: i32, name: *const c_char, buf: *mut c_void, n: usize) -> isize {
        libc::fgetxattr(fd, name, buf, n)
    }
    #[cfg(target_os = "macos")]
    unsafe fn fset(fd: i32, name: *const c_char, val: *const c_void, n: usize) -> i32 {
        libc::fsetxattr(fd, name, val, n, 0, 0)
    }
    #[cfg(not(target_os = "macos"))]
    unsafe fn fset(fd: i32, name: *const c_char, val: *const c_void, n: usize) -> i32 {
        libc::fsetxattr(fd, name, val, n, 0)
    }

    /// Read a variable-size value with the size-query-then-fetch protocol. `None`: the call reported
    /// the attribute (or attributes) unsupported or absent.
    fn read_sized(
        mut call: impl FnMut(*mut c_void, usize) -> isize,
    ) -> io::Result<Option<Vec<u8>>> {
        for _ in 0..RETRIES {
            let n = call(std::ptr::null_mut(), 0);
            if n < 0 {
                return absent_or_err();
            }
            let mut buf = vec![0u8; n as usize];
            let got = call(buf.as_mut_ptr().cast(), buf.len());
            if got >= 0 {
                buf.truncate(got as usize);
                return Ok(Some(buf));
            }
            if io::Error::last_os_error().raw_os_error() != Some(libc::ERANGE) {
                return absent_or_err();
            }
        }
        Err(io::Error::other("extended attributes kept changing"))
    }

    fn absent_or_err() -> io::Result<Option<Vec<u8>>> {
        let e = io::Error::last_os_error();
        #[cfg(target_os = "macos")]
        let absent = [libc::ENOTSUP, libc::ENOATTR];
        #[cfg(not(target_os = "macos"))]
        let absent = [libc::ENOTSUP, libc::ENODATA];
        if e.raw_os_error().is_some_and(|c| absent.contains(&c)) {
            Ok(None)
        } else {
            Err(e)
        }
    }

    pub(super) fn copy(src: &Path, dst: &File) -> io::Result<()> {
        let path = CString::new(src.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let fd = dst.as_raw_fd();
        // SAFETY (all calls below): `path`/`name` are valid NUL-terminated strings, `fd` is open for the
        // lifetime of `dst`, and each buffer pointer comes with its true length.
        let Some(names) = read_sized(|b, n| unsafe { list(path.as_ptr(), b.cast(), n) })? else {
            return Ok(());
        };
        for raw in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
            if KERNEL_MANAGED.contains(&raw) {
                continue;
            }
            let name = CString::new(raw).map_err(io::Error::other)?;
            let Some(value) =
                read_sized(|b, n| unsafe { get(path.as_ptr(), name.as_ptr(), b, n) })?
            else {
                continue; // removed meanwhile
            };
            // Already right (an SELinux label the directory gives every new file): nothing to set.
            if read_sized(|b, n| unsafe { fget(fd, name.as_ptr(), b, n) })?.as_deref()
                == Some(&value[..])
            {
                continue;
            }
            if unsafe { fset(fd, name.as_ptr(), value.as_ptr().cast(), value.len()) } != 0 {
                return Err(io::Error::other(format!(
                    "cannot copy extended attribute {}: {}",
                    String::from_utf8_lossy(raw),
                    io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tmpdir() -> PathBuf {
        // A unique-enough dir under the OS temp root without external crates or Instant/random:
        // the process id + a static counter keep concurrent test cases disjoint.
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "ruse-atomic-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn read(p: &Path) -> Vec<u8> {
        let mut v = Vec::new();
        File::open(p).unwrap().read_to_end(&mut v).unwrap();
        v
    }

    /// The temp files left in `dir`.
    fn temps(dir: &Path) -> Vec<std::ffi::OsString> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n.to_string_lossy().contains(TEMP_TAG))
            .collect()
    }

    #[cfg(unix)]
    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    fn ino(p: &Path) -> u64 {
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(p).unwrap())
    }

    /// Clears an injected fault even when the test panics.
    struct Fault;
    impl Fault {
        fn set(step: Step) -> Fault {
            inject(Some(step));
            Fault
        }
    }
    impl Drop for Fault {
        fn drop(&mut self) {
            inject(None);
        }
    }

    #[test]
    fn writes_new_file_durably() {
        let dir = tmpdir();
        let f = dir.join("new.txt");
        take_synced_dirs();
        assert_eq!(write(&f, b"hello", false).unwrap(), Method::Renamed);
        assert_eq!(read(&f), b"hello");
        assert!(temps(&dir).is_empty(), "temp gone after a successful save");
        #[cfg(unix)]
        assert_eq!(
            take_synced_dirs(),
            vec![dir.clone()],
            "the rename is fsynced"
        );
    }

    #[test]
    fn replaces_existing_atomically() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        save(&f, b"v1").unwrap();
        save(&f, b"v2 longer content").unwrap();
        assert_eq!(read(&f), b"v2 longer content");
        save(&f, b"v3").unwrap();
        assert_eq!(read(&f), b"v3", "a shorter write leaves no old tail");
        assert!(temps(&dir).is_empty());
    }

    /// #865: a bare relative name's directory is `.`, which is fsynced, not skipped as an empty path.
    #[test]
    fn bare_name_fsyncs_the_current_directory() {
        assert_eq!(containing_dir(Path::new("foo")), PathBuf::from("."));
        assert_eq!(containing_dir(Path::new("a/foo")), PathBuf::from("a"));
        assert_eq!(containing_dir(Path::new("/foo")), PathBuf::from("/"));
        #[cfg(unix)]
        {
            take_synced_dirs();
            sync_dir(&containing_dir(Path::new("foo"))).unwrap();
            assert_eq!(take_synced_dirs(), vec![PathBuf::from(".")]);
        }
    }

    /// #936: a failed directory open or fsync is an error, not a silent success: the file has the new bytes
    /// but the save says it is not proven durable.
    #[cfg(unix)]
    #[test]
    fn directory_fsync_failure_is_reported() {
        for step in [Step::OpenDir, Step::SyncDir] {
            let dir = tmpdir();
            let f = dir.join("f.txt");
            fs::write(&f, b"old").unwrap();
            let e = {
                let _g = Fault::set(step);
                write(&f, b"new", false).unwrap_err()
            };
            assert!(
                matches!(&e, SaveError::NotDurable { path, .. } if path == &dir),
                "{step:?}: {e:?}"
            );
            assert!(e.to_string().starts_with("E667: Fsync failed"), "{e}");
            assert_eq!(read(&f), b"new", "{step:?}: the rename already happened");
            assert!(temps(&dir).is_empty());
        }
    }

    /// A filesystem that does not support a directory fsync (EINVAL, ENOTSUP, EOPNOTSUPP, EISDIR from the
    /// open or the fsync) does not fail the save: it succeeds with a method whose note says the rename's
    /// durability is not proven. A real I/O error (EIO, ENOSPC, EROFS, EACCES) stays E667.
    #[cfg(unix)]
    #[test]
    fn unsupported_directory_fsync_is_saved_with_a_note() {
        let codes = [libc::EINVAL, libc::ENOTSUP, libc::EOPNOTSUPP, libc::EISDIR];
        for step in [Step::OpenDir, Step::SyncDir] {
            for code in codes {
                let dir = tmpdir();
                let f = dir.join("f.txt");
                fs::write(&f, b"old").unwrap();
                let m = {
                    let _g = Fault::set(step);
                    inject_errno(step, code);
                    write(&f, b"new", false)
                };
                let m = m.unwrap_or_else(|e| panic!("{step:?} errno {code}: {e}"));
                assert_eq!(m, Method::RenamedDirSyncUnsupported, "{step:?} {code}");
                assert!(m.note().contains("rename durability not proven"));
                assert_eq!(read(&f), b"new");
            }
            for code in [libc::EIO, libc::ENOSPC, libc::EROFS, libc::EACCES] {
                let dir = tmpdir();
                let f = dir.join("f.txt");
                let e = {
                    let _g = Fault::set(step);
                    inject_errno(step, code);
                    write(&f, b"new", false).unwrap_err()
                };
                assert!(
                    matches!(e, SaveError::NotDurable { .. }),
                    "{step:?} {code}: {e:?}"
                );
                assert!(e.to_string().starts_with("E667: "), "{e}");
            }
        }
        assert_eq!(Method::Renamed.note(), "");
        assert_eq!(Method::InPlace.note(), "");
    }

    /// A failed file fsync leaves the target untouched and no temp behind.
    #[test]
    fn file_fsync_failure_leaves_the_target_untouched() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        fs::write(&f, b"old").unwrap();
        let e = {
            let _g = Fault::set(Step::SyncFile);
            write(&f, b"new", false).unwrap_err()
        };
        assert!(matches!(e, SaveError::Untouched(_)), "{e:?}");
        assert_eq!(read(&f), b"old");
        assert!(temps(&dir).is_empty());
    }

    /// #868: concurrent saves of one path each get their own temp, so none fails; the file ends whole.
    #[test]
    fn concurrent_saves_all_succeed() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        let a = vec![b'a'; 4096];
        let b = vec![b'b'; 8192];
        std::thread::scope(|s| {
            for bytes in [&a, &b] {
                let f = &f;
                s.spawn(move || {
                    for _ in 0..100 {
                        save(f, bytes).unwrap();
                    }
                });
            }
        });
        let got = read(&f);
        assert!(got == a || got == b, "a whole version, {} bytes", got.len());
        assert!(temps(&dir).is_empty());
    }

    /// #868: a user's own files that look like temps are never deleted or followed.
    #[cfg(unix)]
    #[test]
    fn files_named_like_temps_are_left_alone() {
        let dir = tmpdir();
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        let f = dir.join("f.txt");
        fs::write(dir.join("f.txt.ruse-tmp"), b"mine").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(".f.txt.ruse-tmp")).unwrap();
        save(&f, b"data").unwrap();
        assert_eq!(read(&dir.join("f.txt.ruse-tmp")), b"mine");
        assert!(fs::symlink_metadata(dir.join(".f.txt.ruse-tmp"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read(&victim), b"keep");
        assert_eq!(read(&f), b"data");
    }

    /// #1197: a save removes the temp files a crashed save of the same file left (writer gone), and nothing
    /// else: not a live writer's, not this process's, not a symlink, not a name that only looks alike.
    #[cfg(unix)]
    #[test]
    fn a_save_sweeps_only_dead_writers_temps() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        fs::write(&f, b"old").unwrap();
        let gone = crate::persist::journal::GONE_PID;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let me = std::process::id();
        let dead = format!(".f.txt.ruse-tmp-{gone}-deadbeef");
        let kept = [
            format!(".f.txt.ruse-tmp-{}-0badf00d", child.id()), // a live writer
            format!(".f.txt.ruse-tmp-{me}-0badf00d"),           // this process
            format!(".f.txt.ruse-tmp-{gone}-deadbee"),          // 7 hex digits
            format!(".f.txt.ruse-tmp-{gone}-deadbeeg"),         // not hex
            format!(".f.txt.ruse-tmp-{gone}-deadbeef.bak"),
            ".f.txt.ruse-tmp-deadbeef".to_string(), // the old name, without a pid
            format!(".g.txt.ruse-tmp-{gone}-deadbeef"), // another file's
            format!("f.txt.ruse-tmp-{gone}-deadbeef"), // not hidden
        ];
        fs::write(dir.join(&dead), b"leftover").unwrap();
        for k in &kept {
            fs::write(dir.join(k), b"keep").unwrap();
        }
        let link = format!(".f.txt.ruse-tmp-{gone}-0000aaaa");
        std::os::unix::fs::symlink(dir.join("f.txt"), dir.join(&link)).unwrap();
        save(&f, b"new").unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !dir.join(&dead).exists(),
            "the dead writer's temp is removed"
        );
        for k in &kept {
            assert_eq!(read(&dir.join(k)), b"keep", "{k} is kept");
        }
        assert!(
            fs::symlink_metadata(dir.join(&link)).is_ok(),
            "a symlink is kept"
        );
        assert_eq!(read(&f), b"new");
        assert_eq!(
            temp_writer(dead.as_bytes(), b".f.txt.ruse-tmp-"),
            Some(gone)
        );
        assert_eq!(
            temp_writer(b".f.txt.ruse-tmp-5-DEADBEEF", b".f.txt.ruse-tmp-"),
            None
        );
        assert_eq!(
            temp_writer(b".f.txt.ruse-tmp--deadbeef", b".f.txt.ruse-tmp-"),
            None
        );
    }

    /// #868: a name at NAME_MAX saves; its temp name is shortened to fit.
    #[test]
    fn longest_name_saves() {
        let dir = tmpdir();
        let f = dir.join("n".repeat(NAME_MAX));
        assert!(temp_path(&f, 0).file_name().unwrap().len() <= NAME_MAX);
        assert!(temp_path(&f, 7)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&format!(".ruse-tmp-{}-00000007", std::process::id())));
        save(&f, b"x").unwrap();
        save(&f, b"y").unwrap();
        assert_eq!(read(&f), b"y");
    }

    /// Regression: saving keeps the original file's permission bits.
    #[cfg(unix)]
    #[test]
    fn preserves_permission_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        for mode in [0o755, 0o600, 0o640] {
            let f = dir.join(format!("m{mode:o}"));
            fs::write(&f, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(&f, fs::Permissions::from_mode(mode)).unwrap();
            save(&f, b"#!/bin/sh\necho hi\n").unwrap();
            let got = fs::metadata(&f).unwrap().permissions().mode() & 0o7777;
            assert_eq!(got, mode, "mode {mode:o} preserved across save");
        }
    }

    /// #864: a hard-linked file is written in place, so every name sees the new bytes (nvim does the same).
    #[cfg(unix)]
    #[test]
    fn hard_link_is_written_in_place() {
        let dir = tmpdir();
        let f = dir.join("f");
        let g = dir.join("g");
        fs::write(&f, b"v1\n").unwrap();
        fs::hard_link(&f, &g).unwrap();
        let before = ino(&f);
        assert_eq!(write(&f, b"v2 longer\n", false).unwrap(), Method::InPlace);
        assert_eq!(read(&g), b"v2 longer\n");
        assert_eq!(ino(&f), before);
        assert_eq!(write(&g, b"v3\n", false).unwrap(), Method::InPlace);
        assert_eq!(read(&f), b"v3\n", "a shorter write truncates the old tail");
        assert!(temps(&dir).is_empty());
    }

    /// #864: a file the user cannot write is refused with E505 unless forced; forced, its mode is kept.
    #[cfg(unix)]
    #[test]
    fn read_only_file_needs_force() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            return;
        }
        let dir = tmpdir();
        let f = dir.join("r");
        fs::write(&f, b"ro\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
        let e = write(&f, b"rox\n", false).unwrap_err();
        assert!(matches!(e, SaveError::ReadOnly(_)), "{e:?}");
        assert!(e.to_string().starts_with("E505: "), "{e}");
        assert_eq!(read(&f), b"ro\n");
        write(&f, b"rox\n", true).unwrap();
        assert_eq!(read(&f), b"rox\n");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o444
        );
        // The same through the in-place path (a hard link), which must add and then drop write permission.
        fs::hard_link(&f, dir.join("r2")).unwrap();
        assert!(write(&f, b"x\n", false).is_err());
        assert_eq!(write(&f, b"roxx\n", true).unwrap(), Method::InPlace);
        assert_eq!(read(&dir.join("r2")), b"roxx\n");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    /// #864: a writable file in a directory that is not writable is written in place (nvim does too) instead
    /// of failing; a NEW file there still fails cleanly.
    #[cfg(unix)]
    #[test]
    fn unwritable_directory_falls_back_to_in_place() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            return;
        }
        let dir = tmpdir();
        let f = dir.join("f");
        fs::write(&f, b"old\n").unwrap();
        let before = ino(&f);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let r = write(&f, b"new\n", false);
        let fresh = write(&dir.join("g"), b"x", false);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.unwrap(), Method::InPlace);
        assert_eq!(read(&f), b"new\n");
        assert_eq!(ino(&f), before);
        assert!(matches!(fresh, Err(SaveError::Untouched(_))), "{fresh:?}");
    }

    /// An in-place write that fails part-way says the file may be partly written.
    #[cfg(unix)]
    #[test]
    fn in_place_failure_is_reported_as_partial() {
        let dir = tmpdir();
        let f = dir.join("f");
        fs::write(&f, b"v1\n").unwrap();
        fs::hard_link(&f, dir.join("g")).unwrap();
        let e = {
            let _g = Fault::set(Step::InPlaceWrite);
            write(&f, b"v2\n", false).unwrap_err()
        };
        assert!(matches!(e, SaveError::Partial { .. }), "{e:?}");
        assert!(e.to_string().contains("may be partly written"), "{e}");
        let e = {
            let _g = Fault::set(Step::SyncFile);
            write(&f, b"v2\n", false).unwrap_err()
        };
        assert!(matches!(e, SaveError::NotDurable { .. }), "{e:?}");
    }

    /// #935: a user extended attribute survives the save (by copying it to the new inode).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn extended_attributes_survive() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tmpdir();
        let f = dir.join("x");
        fs::write(&f, b"x\n").unwrap();
        let c = std::ffi::CString::new(f.as_os_str().as_bytes()).unwrap();
        let name = c"user.ruse-test";
        let val = b"value";
        // SAFETY: valid NUL-terminated strings and a buffer with its true length.
        #[cfg(target_os = "macos")]
        let rc = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), val.as_ptr().cast(), 5, 0, 0) };
        #[cfg(target_os = "linux")]
        let rc = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), val.as_ptr().cast(), 5, 0) };
        if rc != 0 {
            return; // the filesystem has no user xattrs (tmpfs on an old kernel)
        }
        let before = ino(&f);
        // The rename path copies it (an in-place write would keep it trivially).
        assert_eq!(write(&f, b"xy\n", false).unwrap(), Method::Renamed);
        assert_ne!(ino(&f), before);
        assert_eq!(read(&f), b"xy\n");
        let mut buf = [0u8; 16];
        // SAFETY: as above.
        #[cfg(target_os = "macos")]
        let n =
            unsafe { libc::getxattr(c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), 16, 0, 0) };
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::getxattr(c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), 16) };
        assert_eq!(n, 5, "the xattr is still there");
        assert_eq!(&buf[..5], val);
    }

    /// Regression: saving through a symlink writes the link's TARGET and leaves the symlink in place.
    /// Covers a relative link and a two-hop chain.
    #[cfg(unix)]
    #[test]
    fn writes_through_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tmpdir();
        let real = dir.join("real.txt");
        fs::write(&real, b"old").unwrap();
        let link = dir.join("link.txt");
        symlink("real.txt", &link).unwrap(); // relative target
        let link2 = dir.join("link2.txt");
        symlink(&link, &link2).unwrap(); // link → link → file

        save(&link2, b"new").unwrap();
        assert_eq!(read(&real), b"new", "the target file got the bytes");
        for l in [&link, &link2] {
            assert!(
                fs::symlink_metadata(l).unwrap().file_type().is_symlink(),
                "{} is still a symlink",
                l.display()
            );
        }
        assert!(temps(&dir).is_empty());
    }

    /// A symlink cycle is an error, not an infinite loop.
    #[cfg(unix)]
    #[test]
    fn symlink_cycle_is_an_error() {
        let dir = tmpdir();
        let a = dir.join("a");
        let b = dir.join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert!(save(&a, b"x").is_err());
    }

    /// #907: saving over a FIFO, a socket or a directory refuses, rather than renaming a regular file over
    /// the node.
    #[cfg(unix)]
    #[test]
    fn non_regular_target_is_refused_and_kept() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;
        let dir = tmpdir();
        let fifo = dir.join("p");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = save(&fifo, b"x").unwrap_err();
        assert!(e.to_string().contains("is not a regular file"), "{e}");
        assert!(fs::metadata(&fifo).unwrap().file_type().is_fifo());
        let sock = dir.join("s");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let e = write(&sock, b"x", true).unwrap_err();
        assert!(matches!(e, SaveError::NotRegular(_)), "{e:?}");
        assert!(fs::metadata(&sock).unwrap().file_type().is_socket());
        let sub = dir.join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(save(&sub, b"x").is_err());
        assert!(sub.is_dir());
        assert!(temps(&dir).is_empty());
    }

    fn vim_backups(keep: bool, write: bool) -> super::super::backup::Policy {
        super::super::backup::Policy {
            keep,
            write,
            once_per_visit: false,
            skip: String::new(),
            patchmode: String::new(),
            rules: super::super::backup::Rules::Vim {
                dirs: ".".into(),
                ext: "~".into(),
            },
        }
    }

    /// nvim v0.12.6 `'backupcopy'`: `yes` overwrites a hard-linked file in place (both names see the new
    /// text, the backup is a copy); `no` renames, so the other name keeps the old text; `auto,breakhardlink`
    /// renames too.
    #[cfg(unix)]
    #[test]
    fn backupcopy_decides_the_write_method() {
        for (copy, brk, linked_sees_new) in [
            (Copy::Yes, false, true),
            (Copy::No, false, false),
            (Copy::Auto, true, false),
            (Copy::Auto, false, true),
        ] {
            let dir = tmpdir();
            let f = dir.join("h.txt");
            let l = dir.join("l.txt");
            fs::write(&f, b"one\n").unwrap();
            fs::hard_link(&f, &l).unwrap();
            let policy = vim_backups(true, true);
            let opts = Opts {
                copy,
                break_hardlink: brk,
                backup: Some(&policy),
                ..Opts::new(false)
            };
            let w = write_with(&f, b"TWO\n", &opts).unwrap();
            assert_eq!(read(&f), b"TWO\n");
            let want: &[u8] = if linked_sees_new { b"TWO\n" } else { b"one\n" };
            assert_eq!(read(&l), want, "{copy:?} breakhardlink={brk}");
            assert_eq!(read(&dir.join("h.txt~")), b"one\n", "{copy:?}");
            assert_eq!(w.backup.as_deref(), Some(dir.join("h.txt~").as_path()));
            assert!(temps(&dir).is_empty());
        }
    }

    /// `'backupcopy'` `breaksymlink`: the link becomes a regular file and its target keeps the old text.
    #[cfg(unix)]
    #[test]
    fn breaksymlink_replaces_the_link() {
        let dir = tmpdir();
        let t = dir.join("t.txt");
        let s = dir.join("s.txt");
        fs::write(&t, b"one\n").unwrap();
        std::os::unix::fs::symlink("t.txt", &s).unwrap();
        let opts = Opts {
            break_symlink: true,
            ..Opts::new(false)
        };
        write_with(&s, b"TWO\n", &opts).unwrap();
        assert!(fs::symlink_metadata(&s).unwrap().file_type().is_file());
        assert_eq!(read(&s), b"TWO\n");
        assert_eq!(read(&t), b"one\n");
    }

    /// A write backup guards an in-place write and is gone after it; with `'nowritebackup'` there is none.
    #[cfg(unix)]
    #[test]
    fn writebackup_is_removed_after_an_in_place_write() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        fs::write(&f, b"one\n").unwrap();
        let policy = vim_backups(false, true);
        let opts = Opts {
            copy: Copy::Yes,
            backup: Some(&policy),
            ..Opts::new(false)
        };
        let w = write_with(&f, b"two\n", &opts).unwrap();
        assert_eq!(w.backup, None);
        assert!(!dir.join("f.txt~").exists());
        assert_eq!(read(&f), b"two\n");
    }

    /// nvim: a backup that cannot be made stops the write (E510) and leaves the file alone; `:w!` writes
    /// without it.
    #[cfg(unix)]
    #[test]
    fn a_backup_that_cannot_be_made_needs_force() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        fs::write(&f, b"one\n").unwrap();
        let blocker = dir.join("file");
        fs::write(&blocker, b"").unwrap();
        let mut policy = vim_backups(true, true);
        policy.rules = super::super::backup::Rules::Vim {
            dirs: format!("{}/sub", blocker.display()),
            ext: "~".into(),
        };
        let mut opts = Opts {
            backup: Some(&policy),
            ..Opts::new(false)
        };
        let e = write_with(&f, b"two\n", &opts).unwrap_err();
        assert_eq!(
            e.to_string(),
            "E510: Can't make backup file (add ! to override)"
        );
        assert_eq!(read(&f), b"one\n");
        opts.force = true;
        write_with(&f, b"two\n", &opts).unwrap();
        assert_eq!(read(&f), b"two\n");
    }

    /// `'nofsync'`: the write still lands, without the file or directory sync.
    #[cfg(unix)]
    #[test]
    fn nofsync_skips_the_syncs() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        fs::write(&f, b"one\n").unwrap();
        take_synced_dirs();
        let opts = Opts {
            fsync: false,
            ..Opts::new(false)
        };
        write_with(&f, b"two\n", &opts).unwrap();
        assert_eq!(read(&f), b"two\n");
        assert!(take_synced_dirs().is_empty());
    }
}
