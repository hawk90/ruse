//! Atomic, durable save (F-008 acceptance #1: "Saved" reflects an fsync, not just a `write()`).
//!
//! Write to a sibling temp file, `fsync` it, `rename` it over the target (atomic on a POSIX
//! filesystem), then `fsync` the containing directory so the rename itself is durable. A crash at
//! any point leaves EITHER the old file or the fully-written new one — never a truncated target
//! (the anti-pattern D-005 fixed). The temp file is a sibling so the rename stays on one filesystem.
//!
//! Directory fsync is Unix-only; on other platforms the file fsync + rename still gives atomic
//! replacement, and durable-directory-metadata is a post-MVP per-platform refinement (ConPTY/NTFS).
//!
//! **Preservation** (persistence-and-recovery.md §3): a symlinked target is written THROUGH — the link is
//! resolved and the file it points at is replaced, so the symlink itself survives; the existing file's
//! permission bits are re-applied to the temp before the rename (an executable script stays executable, a
//! `0600` file never passes through a wider mode). Ownership / ACL / xattr are not yet carried over.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Symlink hops followed before giving up (Linux's `MAXSYMLINKS`); guards a link cycle.
const MAX_SYMLINK_HOPS: usize = 40;

/// The sibling temp path a save writes before renaming over `target`.
fn temp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".ruse-tmp");
    target.with_file_name(name)
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

/// Durably replace `target` with `bytes`. On error the temp file is removed so a failed save never
/// litters. Returns only after the bytes (and, on Unix, the rename) are on stable storage.
pub fn save(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = resolve_write_target(target)?;
    // The existing file's permissions (None for a new file → the platform default, i.e. umask on Unix).
    let perms = fs::metadata(&target).ok().map(|m| m.permissions());
    let tmp = temp_path(&target);
    if let Err(e) = write_and_sync(&tmp, bytes, perms) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        // A directory fsync makes the rename durable. Best-effort: on a filesystem that rejects it
        // the rename is still atomic, just not proven durable — not worth failing the save over.
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

fn write_and_sync(tmp: &Path, bytes: &[u8], perms: Option<fs::Permissions>) -> io::Result<()> {
    // A stale temp from a crashed save is removed, then the temp is created EXCLUSIVELY (O_EXCL), so a
    // pre-planted `<name>.ruse-tmp` symlink can never redirect the write to another file.
    let _ = fs::remove_file(tmp);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if perms.is_some() {
        // Owner-only until the captured mode is applied: the new bytes never sit in a wider-mode file.
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    }
    let mut f = opts.open(tmp)?;
    f.write_all(bytes)?;
    if let Some(p) = perms {
        f.set_permissions(p)?; // fchmod: the original file's mode carries over to the replacement
    }
    f.sync_all()?; // fsync: the bytes are on stable storage before we rename
    Ok(())
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

    #[test]
    fn writes_new_file_durably() {
        let dir = tmpdir();
        let f = dir.join("new.txt");
        save(&f, b"hello").unwrap();
        assert_eq!(read(&f), b"hello");
        assert!(
            !temp_path(&f).exists(),
            "temp file must be gone after a successful save"
        );
    }

    #[test]
    fn replaces_existing_atomically() {
        let dir = tmpdir();
        let f = dir.join("f.txt");
        save(&f, b"v1").unwrap();
        save(&f, b"v2 longer content").unwrap();
        assert_eq!(read(&f), b"v2 longer content");
    }

    #[test]
    fn no_temp_left_behind() {
        let dir = tmpdir();
        let f = dir.join("x");
        save(&f, b"data").unwrap();
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("ruse-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    /// Regression: saving keeps the original file's permission bits (an executable script stayed
    /// executable only by luck before — the temp was created with the default mode and renamed over it).
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

    /// Regression: saving through a symlink writes the link's TARGET and leaves the symlink in place
    /// (previously the rename replaced the link with a regular file and the real file kept stale bytes).
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
        assert!(!temp_path(&real).exists() && !temp_path(&link2).exists());
    }

    /// A pre-existing `<name>.ruse-tmp` symlink (stale, or planted) is never followed: the save replaces
    /// it and the file it pointed at is untouched.
    #[cfg(unix)]
    #[test]
    fn stale_temp_symlink_is_not_followed() {
        let dir = tmpdir();
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        let f = dir.join("f.txt");
        std::os::unix::fs::symlink(&victim, temp_path(&f)).unwrap();
        save(&f, b"data").unwrap();
        assert_eq!(read(&victim), b"keep");
        assert_eq!(read(&f), b"data");
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
}
