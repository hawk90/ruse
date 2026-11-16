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

#[cfg(test)]
mod fault;
mod temp;
/// Copy a file's extended attributes (on Linux also its POSIX ACL, `system.posix_acl_access`) to the new
/// file. An `Err` means one could not be reproduced, so the caller writes in place instead of losing it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
mod xattr;

#[cfg(test)]
pub(crate) use fault::*;
use temp::*;

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

#[cfg(test)]
mod tests;
