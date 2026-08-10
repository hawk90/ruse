//! Crash recovery + panic policy (D-040 / stability §6, §8). A core panic is an invariant violation; before
//! it unwinds we save the unsaved buffer to `<file>.ruse-recovered` (a recovery snapshot) so a bug never
//! silently loses work. We do NOT `catch_unwind`-swallow (STAB-6) nor `panic=abort` (STAB-5): the previous
//! hook still runs (prints + unwinds), and `TermGuard` restores the terminal on the way out.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct Snapshot {
    path: Option<PathBuf>,
    bytes: Vec<u8>,
    modified: bool,
}

static SNAP: OnceLock<Mutex<Snapshot>> = OnceLock::new();

fn cell() -> &'static Mutex<Snapshot> {
    SNAP.get_or_init(|| {
        Mutex::new(Snapshot {
            path: None,
            bytes: Vec::new(),
            modified: false,
        })
    })
}

/// Record the latest buffer state so a panic can recover it. Called after each command.
pub fn update(path: Option<&PathBuf>, bytes: &[u8], modified: bool) {
    if let Ok(mut s) = cell().lock() {
        s.path = path.cloned();
        s.bytes.clear();
        s.bytes.extend_from_slice(bytes);
        s.modified = modified;
    }
}

/// The recovery decision for one snapshot: append a journal frame only when the buffer is modified,
/// and report the outcome so the panic hook can log a structured event. Appends to the same
/// append-only journal (`crate::persist::journal`, F-008) the main loop throttles into, so a panic
/// captures the EXACT latest buffer as one more recoverable record. Pure w.r.t. control flow (only
/// touches the journal file), so it is unit-testable without installing a global hook or panicking.
fn recover_write(path: Option<&Path>, bytes: &[u8], modified: bool) -> Recovery {
    if !modified {
        return Recovery::Clean;
    }
    match crate::persist::journal::append(path, bytes) {
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

/// Install the panic hook: save the recovery snapshot (if modified), log a structured event, then chain to
/// the previous hook (which unwinds). Never swallows the panic.
pub fn install_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(s) = cell().lock() {
            match recover_write(s.path.as_deref(), &s.bytes, s.modified) {
                Recovery::Written(rp) => tracing::error!(
                    event = "panic.recovered",
                    recovery = %rp.display(),
                    bytes = s.bytes.len(),
                    "core panic — unsaved buffer written to recovery file"
                ),
                Recovery::Failed(e) => tracing::error!(
                    event = "panic.recover_failed",
                    error = %e,
                    "core panic — recovery write failed"
                ),
                Recovery::Clean => {
                    tracing::error!(event = "panic", "core panic (no unsaved work)");
                }
            }
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
}
