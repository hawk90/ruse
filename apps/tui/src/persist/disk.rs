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

/// How a file on disk compares with what ruse last read from it or saved to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// It holds the bytes ruse read or saved (or ruse never read or wrote it).
    Same,
    /// It holds other bytes, or exists but cannot be read (nvim's W11/W12).
    Changed,
    /// It is gone: deleted, renamed away, or a dangling link (nvim's E211).
    Gone,
}

/// What ruse last read from or saved to a file, and whether the file still holds it.
#[derive(Clone, Debug)]
pub struct Seen {
    /// The hash of the bytes ruse read or wrote.
    hash: u64,
    /// The stat the last check saw (the read or save first).
    last: Option<Stat>,
    state: State,
}

impl Seen {
    /// `path` as ruse just read or wrote it: `bytes` are the file's bytes, as they are on disk.
    #[must_use]
    pub fn of(path: &Path, bytes: &[u8]) -> Seen {
        Seen {
            hash: hash(bytes),
            last: stat(path),
            state: State::Same,
        }
    }

    /// How the file compared at the last [`Seen::check`].
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Look at `path` again and compare it with what ruse read or saved. Only a stat that moved reads and
    /// hashes the file. A file that comes back after it was gone is compared by content again, so one
    /// restored as ruse saw it is the same (nvim forgets a deleted file's timestamp and never notices).
    pub fn check(&mut self, path: &Path) -> State {
        let now = stat(path);
        if now == self.last {
            return self.state;
        }
        self.last = now;
        self.state = match now {
            None => State::Gone,
            Some(_) => match crate::persist::read_file(path) {
                Ok(b) if hash(&b) == self.hash => State::Same,
                _ => State::Changed,
            },
        };
        self.state
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

/// [`Seen::check`] for `path`; [`State::Same`] for a file ruse has not read or written.
pub fn check(path: &Path) -> State {
    let k = key(path);
    registry()
        .lock()
        .ok()
        .and_then(|mut r| r.get_mut(&k).map(|s| s.check(path)))
        .unwrap_or(State::Same)
}

/// Whether a save without `!` to `path` would overwrite a change another program made. A file that is
/// gone is no conflict (nvim writes it again without asking); a dangling link that stands where it was
/// still is.
pub fn conflicts(path: &Path) -> bool {
    match check(path) {
        State::Same => false,
        State::Changed => true,
        State::Gone => fs::symlink_metadata(path).is_ok(),
    }
}

/// Record that ruse appended to `path` (`:w >>` to the buffer's own file), given what [`check`] said
/// just before: a file that held what ruse saw is now what ruse made it, as after a save (nvim does not
/// count its own append as a change). A file another program had changed first stays changed.
pub fn appended(path: &Path, before: State) {
    if before != State::Same {
        return;
    }
    let known = registry().lock().is_ok_and(|r| r.contains_key(&key(path)));
    if known {
        if let Ok(bytes) = crate::persist::read_file(path) {
            saw(path, &bytes);
        }
    }
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
        assert_eq!(check(&f), State::Same, "never seen");
        fs::write(&f, b"one\n").unwrap();
        saw(&f, b"one\n");
        assert_eq!(check(&f), State::Same);
        fs::write(&f, b"other\n").unwrap();
        assert_eq!(check(&f), State::Changed);
        // ruse saves over it: what it wrote is what it saw.
        assert!(conflicts(&f));
        fs::write(&f, b"mine!!\n").unwrap();
        saw(&f, b"mine!!\n");
        assert_eq!(check(&f), State::Same);
        fs::remove_file(&f).unwrap();
        assert!(
            check(&f) == State::Gone && !conflicts(&f),
            "gone, and a save may write it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// ruse's own append keeps the file "as ruse saw it"; an append over another program's change does not
    /// hide that change.
    #[test]
    fn an_own_append_is_not_a_change() {
        let dir = std::env::temp_dir().join(format!("ruse-disk-app-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        fs::write(&f, b"one\n").unwrap();
        saw(&f, b"one\n");
        let before = check(&f);
        fs::write(&f, b"one\none\n").unwrap(); // the append
        appended(&f, before);
        assert_eq!(check(&f), State::Same, "own append");
        fs::write(&f, b"theirs\n").unwrap();
        let before = check(&f);
        fs::write(&f, b"theirs\none\n").unwrap();
        appended(&f, before);
        assert_eq!(check(&f), State::Changed, "their change stays visible");
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
        assert_eq!(seen.check(&f), State::Same, "unchanged");
        // The same bytes written again (a new mtime, maybe a new inode): not a change.
        fs::remove_file(&f).unwrap();
        fs::write(&f, b"one\n").unwrap();
        assert_eq!(seen.check(&f), State::Same, "same content");
        // A different length: a stat within one mtime tick of the last still moves.
        fs::write(&f, b"two!\n").unwrap();
        assert!(seen.check(&f) == State::Changed && seen.state() == State::Changed);
        assert_eq!(seen.check(&f), State::Changed, "stays changed");
        // Back to what ruse read: no longer differs.
        fs::write(&f, b"one\n").unwrap();
        assert_eq!(seen.check(&f), State::Same);
        fs::remove_file(&f).unwrap();
        assert_eq!(seen.check(&f), State::Gone, "a file that is gone");
        // It comes back: compared by content again.
        fs::write(&f, b"one\n").unwrap();
        assert_eq!(seen.check(&f), State::Same, "restored as read");
        fs::remove_file(&f).unwrap();
        assert_eq!(seen.check(&f), State::Gone);
        fs::write(&f, b"three\n").unwrap();
        assert_eq!(seen.check(&f), State::Changed, "back with other bytes");
        let _ = fs::remove_dir_all(&dir);
    }
}
