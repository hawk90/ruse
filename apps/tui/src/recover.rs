//! Crash recovery + panic policy (D-040 / stability §6, §8). A core panic is an invariant violation; before
//! it unwinds we append every modified buffer to its recovery journal (`<file>.ruse-journal`) so a bug never
//! silently loses work. A fatal signal or an I/O error that ends the session takes the same path (#857).
//! We do NOT `catch_unwind`-swallow (STAB-6) nor `panic=abort` (STAB-5): the previous hook still runs
//! (prints + unwinds), and `TermGuard` restores the terminal on the way out.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

/// The latest state of one buffer that has somewhere to be recovered to: a file-backed buffer (keyed by
/// its path) or the session's unnamed start buffer (`None`, the journal `ruse` with no file replays).
pub struct Entry {
    pub path: Option<PathBuf>,
    /// A shared handle to the buffer's bytes (`Document::text_arc`): publishing it is a refcount bump.
    pub bytes: Arc<Vec<u8>>,
    pub modified: bool,
}

/// Every recoverable buffer's latest state (#856), refreshed by the main loop each frame. Read by the
/// panic hook and the fatal-signal / I/O-error exit path, which journal EVERY modified buffer, not just
/// the focused one. Held only for a `Vec` swap, and read with `try_lock` on crash paths, so a panic
/// can never deadlock on it.
static STORE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Publish the latest state of every recoverable buffer, replacing the previous set (a closed buffer
/// drops out). Called once per frame.
pub fn sync(entries: Vec<Entry>) {
    let old = {
        let mut s = STORE.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *s, entries)
    };
    drop(old); // the last handle to an old snapshot is freed outside the lock
}

/// Journal every modified buffer in the store, returning one outcome per buffer (clean ones included).
/// Safe on a crash path: never blocks on the store, and each journal re-checks its tail. `None` when
/// the store is held, which only a panic inside [`sync`] itself can cause.
fn rescue_all() -> Option<Vec<(Option<PathBuf>, Recovery)>> {
    let guard = match STORE.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    Some(
        guard
            .iter()
            .map(|e| {
                (
                    e.path.clone(),
                    recover_write(e.path.as_deref(), &e.bytes, e.modified),
                )
            })
            .collect(),
    )
}

/// The exit path for a fatal signal or an I/O error that ended the session (#857): journal every
/// modified buffer so the next start offers it back.
pub fn rescue_on_exit(reason: &str) {
    for (path, r) in rescue_all().unwrap_or_default() {
        log_outcome(reason, path.as_deref(), &r);
    }
}

fn log_outcome(reason: &str, path: Option<&Path>, r: &Recovery) {
    let doc = path.map_or_else(|| "[No Name]".into(), |p| p.display().to_string());
    match r {
        Recovery::Written(rp) => tracing::error!(
            event = "recover.written",
            reason,
            doc = %doc,
            recovery = %rp.display(),
            "unsaved buffer written to its recovery journal"
        ),
        Recovery::Failed(e) => tracing::error!(
            event = "recover.failed",
            reason,
            doc = %doc,
            error = %e,
            "recovery journal write failed"
        ),
        Recovery::Clean => {}
    }
}

/// The recovery decision for one snapshot: append a journal frame only when the buffer is modified,
/// and report the outcome so the caller can log a structured event. Appends to the same journal
/// (`crate::persist::journal`, F-008) the main loop throttles into, so a crash captures the EXACT latest
/// buffer as one more recoverable record. Uses the crash-path append, which re-checks the tail in case
/// the failure interrupted an append. Unit-testable without installing a global hook or panicking.
fn recover_write(path: Option<&Path>, bytes: &[u8], modified: bool) -> Recovery {
    if !modified {
        return Recovery::Clean;
    }
    match crate::persist::journal::append_fresh(path, bytes) {
        Ok(()) => Recovery::Written(crate::persist::journal::journal_path(path)),
        Err(e) => Recovery::Failed(e.to_string()),
    }
}

/// Outcome of a recovery attempt.
#[derive(Debug)]
enum Recovery {
    /// Nothing to save — the buffer matched disk.
    Clean,
    /// Unsaved work was written to this recovery file.
    Written(PathBuf),
    /// The buffer was modified but the recovery write failed (message).
    Failed(String),
}

/// Install the panic hook: journal every modified buffer, log a structured event, then chain to the
/// previous hook (which unwinds). Never swallows the panic.
pub fn install_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        match rescue_all() {
            Some(outcomes) => {
                let mut saved = 0usize;
                for (path, r) in &outcomes {
                    log_outcome("panic", path.as_deref(), r);
                    saved += usize::from(matches!(r, Recovery::Written(_)));
                }
                tracing::error!(event = "panic", recovered = saved, "core panic");
            }
            None => tracing::error!(event = "panic", "core panic; buffer store busy"),
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_buffer_appends_nothing() {
        // An unmodified buffer must not write a recovery frame at all.
        assert!(matches!(recover_write(None, b"x", false), Recovery::Clean));
    }

    #[test]
    fn modified_buffer_appends_a_replayable_frame() {
        let dir = std::env::temp_dir().join(format!("ruse-recover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("note.txt");
        let _ = std::fs::remove_file(crate::persist::journal::journal_path(Some(&doc)));
        match recover_write(Some(&doc), b"unsaved work", true) {
            Recovery::Written(_) => {}
            other => panic!("expected Written, got {other:?}"),
        }
        assert_eq!(
            crate::persist::journal::replay(Some(&doc)).as_deref(),
            Some(&b"unsaved work"[..])
        );
    }

    /// #856: a crash journals every modified buffer in the store, not only the focused one, and skips the
    /// clean ones. The store is process-global, so this is the only test that touches it.
    #[test]
    fn rescue_journals_every_modified_buffer() {
        use crate::persist::journal::{journal_path, replay};
        let dir = std::env::temp_dir().join(format!("ruse-recover-all-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.txt"), dir.join("b.txt"), dir.join("c.txt"));
        for p in [&a, &b, &c] {
            let _ = std::fs::remove_file(journal_path(Some(p)));
        }
        let entry = |p: &PathBuf, bytes: &[u8], modified| Entry {
            path: Some(p.clone()),
            bytes: Arc::new(bytes.to_vec()),
            modified,
        };
        sync(vec![
            entry(&a, b"edited a\n", true),
            entry(&b, b"edited b\n", true),
            entry(&c, b"saved c\n", false),
        ]);
        let outcomes = rescue_all().expect("store is free");
        sync(Vec::new());
        assert_eq!(outcomes.len(), 3);
        assert_eq!(replay(Some(&a)).as_deref(), Some(&b"edited a\n"[..]));
        assert_eq!(replay(Some(&b)).as_deref(), Some(&b"edited b\n"[..]));
        assert!(
            !journal_path(Some(&c)).exists(),
            "a clean buffer is not journaled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
