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

/// The recovery-file path for a document path (or `.ruse-recovered` for an unnamed buffer).
#[must_use]
pub fn recovery_path(path: Option<&Path>) -> PathBuf {
    match path {
        Some(p) => {
            let mut s = p.as_os_str().to_os_string();
            s.push(".ruse-recovered");
            PathBuf::from(s)
        }
        None => PathBuf::from(".ruse-recovered"),
    }
}

/// The recovery decision for one snapshot: write only when the buffer is modified, and report the outcome so
/// the panic hook can log a structured event. Pure w.r.t. control flow (only touches the recovery file), so
/// it is unit-testable without installing a global hook or actually panicking.
fn recover_write(path: Option<&Path>, bytes: &[u8], modified: bool) -> Recovery {
    if !modified {
        return Recovery::Clean;
    }
    let rp = recovery_path(path);
    match std::fs::write(&rp, bytes) {
        Ok(()) => Recovery::Written(rp),
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
    fn recovery_path_appends_suffix() {
        assert_eq!(
            recovery_path(Some(Path::new("/tmp/a.rs"))),
            PathBuf::from("/tmp/a.rs.ruse-recovered")
        );
        assert_eq!(recovery_path(None), PathBuf::from(".ruse-recovered"));
    }
}
