//! Atomic, durable save (F-008 acceptance #1: "Saved" reflects an fsync, not just a `write()`).
//!
//! Write to a sibling temp file, `fsync` it, `rename` it over the target (atomic on a POSIX
//! filesystem), then `fsync` the containing directory so the rename itself is durable. A crash at
//! any point leaves EITHER the old file or the fully-written new one — never a truncated target
//! (the anti-pattern D-005 fixed). The temp file is a sibling so the rename stays on one filesystem.
//!
//! Directory fsync is Unix-only; on other platforms the file fsync + rename still gives atomic
//! replacement, and durable-directory-metadata is a post-MVP per-platform refinement (ConPTY/NTFS).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The sibling temp path a save writes before renaming over `target`.
fn temp_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".ruse-tmp");
    target.with_file_name(name)
}

/// Durably replace `target` with `bytes`. On error the temp file is removed so a failed save never
/// litters. Returns only after the bytes (and, on Unix, the rename) are on stable storage.
pub fn save(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(target);
    if let Err(e) = write_and_sync(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, target) {
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

fn write_and_sync(tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = File::create(tmp)?;
    f.write_all(bytes)?;
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
}
