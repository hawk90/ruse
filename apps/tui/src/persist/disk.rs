//! External-change detection (RFC-0023 §8, design doc §5, #938): whether a file on disk still holds what
//! ruse last read from it or saved to it. A stat (size, modification time, inode, device) is the cheap
//! hint; when it differs, the file's bytes are hashed and compared, so a `touch` or an mtime that moved
//! without a change never counts. The modification time alone is never trusted.

use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The parts of a file's metadata that change when it is written or replaced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stat {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    dev: u64,
}

fn stat(path: &Path) -> Option<Stat> {
    let m = fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(Stat {
        len: m.len(),
        modified: m.modified().ok(),
        #[cfg(unix)]
        ino: m.ino(),
        #[cfg(unix)]
        dev: m.dev(),
    })
}

/// A content hash for change detection (not a security boundary): SipHash with fixed keys, so equal
/// bytes give equal hashes within the process.
fn hash(bytes: &[u8]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// What ruse last read from or saved to a file, and whether the file still holds it.
#[derive(Clone, Debug)]
pub struct Seen {
    /// The hash of the bytes ruse read or wrote.
    hash: u64,
    /// The stat the last check saw (the read or save first).
    last: Option<Stat>,
    changed: bool,
}

impl Seen {
    /// `path` as ruse just read or wrote it: `bytes` are the file's bytes, as they are on disk.
    #[must_use]
    pub fn of(path: &Path, bytes: &[u8]) -> Seen {
        Seen {
            hash: hash(bytes),
            last: stat(path),
            changed: false,
        }
    }

    /// Whether the file differed from what ruse read or saved at the last [`Seen::check`].
    #[must_use]
    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Look at `path` again; returns whether it now differs from what ruse read or saved. Only a stat
    /// that moved reads and hashes the file; a file that is gone or cannot be read differs.
    pub fn check(&mut self, path: &Path) -> bool {
        let now = stat(path);
        if now == self.last {
            return self.changed;
        }
        self.last = now;
        self.changed = match now {
            None => true,
            Some(_) => crate::persist::read_file(path).map_or(true, |b| hash(&b) != self.hash),
        };
        self.changed
    }
}

/// What this process last read from or wrote to each file, keyed by canonical path. Every read of a
/// buffer's file and every save records here ([`saw`]), as every save clears the journal by path
/// ([`super::journal::clear`]), so no call site needs the session's file registry.
fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, Seen>> {
    static R: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Seen>>> =
        std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

fn key(path: &Path) -> PathBuf {
    super::journal::canonical(path)
}

/// Why a save without `!` was refused: the file changed on disk since ruse read or saved it (nvim asks
/// with this warning; ruse refuses and takes `!` as the answer, design doc §5).
pub const CHANGED_SINCE_READ: &str =
    "WARNING: The file has been changed since reading it!!! (add ! to write anyway)";

/// Record that ruse just read `bytes` from `path`, or wrote them to it.
pub fn saw(path: &Path, bytes: &[u8]) {
    let seen = Seen::of(path, bytes);
    if let Ok(mut r) = registry().lock() {
        r.insert(key(path), seen);
    }
}

/// [`Seen::check`] for `path`; `false` for a file ruse has not read or written.
pub fn check(path: &Path) -> bool {
    let k = key(path);
    registry()
        .lock()
        .ok()
        .and_then(|mut r| r.get_mut(&k).map(|s| s.check(path)))
        .unwrap_or(false)
}

/// Whether a save without `!` to `path` would overwrite a change another program made: [`check`], for
/// a file that still exists. A file that is gone is no conflict (nvim writes it again without asking).
pub fn conflicts(path: &Path) -> bool {
    check(path) && fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry follows a read, a check and a save of one path.
    #[test]
    fn the_registry_tracks_reads_and_saves() {
        let dir = std::env::temp_dir().join(format!("ruse-disk-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("g.txt");
        assert!(!check(&f), "never seen");
        fs::write(&f, b"one\n").unwrap();
        saw(&f, b"one\n");
        assert!(!check(&f));
        fs::write(&f, b"other\n").unwrap();
        assert!(check(&f));
        // ruse saves over it: what it wrote is what it saw.
        assert!(conflicts(&f));
        fs::write(&f, b"mine!!\n").unwrap();
        saw(&f, b"mine!!\n");
        assert!(!check(&f));
        fs::remove_file(&f).unwrap();
        assert!(
            check(&f) && !conflicts(&f),
            "gone: changed, but a save may write it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_counts_and_a_touch_does_not() {
        let dir = std::env::temp_dir().join(format!("ruse-disk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f.txt");
        fs::write(&f, b"one\n").unwrap();
        let mut seen = Seen::of(&f, b"one\n");
        assert!(!seen.check(&f), "unchanged");
        // The same bytes written again (a new mtime, maybe a new inode): not a change.
        fs::remove_file(&f).unwrap();
        fs::write(&f, b"one\n").unwrap();
        assert!(!seen.check(&f), "same content");
        // A different length: a stat within one mtime tick of the last still moves.
        fs::write(&f, b"two!\n").unwrap();
        assert!(seen.check(&f) && seen.changed());
        assert!(seen.check(&f), "stays changed");
        // Back to what ruse read: no longer differs.
        fs::write(&f, b"one\n").unwrap();
        assert!(!seen.check(&f));
        fs::remove_file(&f).unwrap();
        assert!(seen.check(&f), "a file that is gone differs");
        let _ = fs::remove_dir_all(&dir);
    }
}
