//! Crash recovery + panic policy (D-040 / stability §6, §8). A core panic is an invariant violation; before
//! it unwinds we write every modified buffer's exact text to its recovery journal (in the state directory,
//! `crate::persist::journal`) so a bug never silently loses work. A fatal signal or an I/O error that ends the session takes the same path (#857).
//! We do NOT `catch_unwind`-swallow (STAB-6) nor `panic=abort` (STAB-5): the previous hook still runs
//! (prints + unwinds). The hook restores the terminal first, so the printed message lands on the main
//! screen and stays visible (#866); `TermGuard`'s drop then finds nothing left to restore.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};

/// The latest state of one buffer that has somewhere to be recovered to: a file-backed buffer or the
/// session's unnamed start buffer (`path` `None`).
pub struct Entry {
    /// The buffer's file, for the log.
    pub path: Option<PathBuf>,
    /// Its journal, and the canonical document path and file format the journal's Header names.
    pub journal: PathBuf,
    pub canonical: Option<PathBuf>,
    pub format: crate::persist::encoding::FileFormat,
    /// A shared handle to the buffer's bytes (`Document::text_arc`): publishing it is a refcount bump.
    pub bytes: Arc<Vec<u8>>,
    pub modified: bool,
    /// The buffer's revision and current undo node, for the Checkpoint.
    pub revision: u64,
    pub node: u64,
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
            .map(|e| (e.path.clone(), recover_write(e)))
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

/// The recovery decision for one snapshot: write the journal only when the buffer is modified, and report
/// the outcome so the caller can log a structured event. Writes into the same journal the main loop syncs
/// (`crate::persist::journal`, F-008): a Checkpoint of the EXACT latest text after its last intact record,
/// so records the session had not synced yet are covered too. Unit-testable without installing a global
/// hook or panicking.
fn recover_write(e: &Entry) -> Recovery {
    if !e.modified {
        return Recovery::Clean;
    }
    match crate::persist::journal::rescue(
        &e.journal,
        e.canonical.as_deref(),
        e.format,
        &e.bytes,
        e.revision,
        e.node,
    ) {
        Ok(()) => Recovery::Written(e.journal.clone()),
        Err(err) => Recovery::Failed(err.to_string()),
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

/// Install the panic hook: journal every modified buffer, log a structured event with the panic's message
/// and location, restore the terminal, then chain to the previous hook (which prints and unwinds). Never
/// swallows the panic.
pub fn install_hook() {
    install_hook_with(|| crate::terminal::guard::restore(true));
}

/// A panic's message and `file:line:col` location, as the log event carries them (#866). A payload that is
/// not a string is named the way the default hook names it.
fn describe(info: &std::panic::PanicHookInfo<'_>) -> (String, String) {
    let message = info.payload_as_str().unwrap_or("Box<dyn Any>").to_owned();
    let location = info
        .location()
        .map_or_else(String::new, ToString::to_string);
    (message, location)
}

/// [`install_hook`] with the terminal restore passed in, so a test can observe its order against the
/// previous hook. The restore runs BEFORE the previous hook prints (#866): the message used to be printed
/// on the alternate screen and wiped when the guard left it during the unwind.
fn install_hook_with(restore: fn()) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (message, location) = describe(info);
        match rescue_all() {
            Some(outcomes) => {
                let mut saved = 0usize;
                for (path, r) in &outcomes {
                    log_outcome("panic", path.as_deref(), r);
                    saved += usize::from(matches!(r, Recovery::Written(_)));
                }
                tracing::error!(event = "panic", recovered = saved, %location, "core panic: {message}");
            }
            None => {
                tracing::error!(event = "panic", %location, "core panic; buffer store busy: {message}")
            }
        }
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::journal::Journals;

    /// A journal directory of the test's own, never the user's state directory.
    fn journals(tag: &str) -> (Journals, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ruse-recover-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Journals::in_state_home(&dir.join("state")), dir)
    }

    fn entry(j: &Journals, doc: &Path, bytes: &[u8], modified: bool) -> Entry {
        Entry {
            path: Some(doc.to_path_buf()),
            journal: j.path_for(doc),
            canonical: Some(crate::persist::journal::canonical(doc)),
            format: Default::default(),
            bytes: Arc::new(bytes.to_vec()),
            modified,
            revision: 3,
            node: 2,
        }
    }

    #[test]
    fn clean_buffer_appends_nothing() {
        let (j, dir) = journals("clean");
        let doc = dir.join("x.txt");
        // An unmodified buffer must not write a journal at all.
        assert!(matches!(
            recover_write(&entry(&j, &doc, b"x", false)),
            Recovery::Clean
        ));
        assert!(!j.path_for(&doc).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modified_buffer_writes_a_replayable_journal() {
        let (j, dir) = journals("one");
        let doc = dir.join("note.txt");
        match recover_write(&entry(&j, &doc, b"unsaved work", true)) {
            Recovery::Written(p) => assert_eq!(p, j.path_for(&doc)),
            other => panic!("expected Written, got {other:?}"),
        }
        assert_eq!(j.replay(Some(&doc)).as_deref(), Some(&b"unsaved work"[..]));
        // A second rescue appends a newer Checkpoint; the exact latest text is what replays.
        recover_write(&entry(&j, &doc, b"later work", true));
        assert_eq!(j.replay(Some(&doc)).as_deref(), Some(&b"later work"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serialises the tests that use the process-global store.
    static STORE_TEST: Mutex<()> = Mutex::new(());

    /// #897: the installed panic hook itself journals the store before the panic unwinds. There is no way to
    /// make the shipped binary panic on purpose (and no production trigger should exist), so this is the
    /// panic path's coverage, below the process boundary.
    #[test]
    fn panic_hook_journals_modified_buffers_then_unwinds() {
        let _serial = STORE_TEST.lock().unwrap_or_else(PoisonError::into_inner);
        let (j, dir) = journals("hook");
        let doc = dir.join("p.txt");
        sync(vec![entry(&j, &doc, b"work at the panic\n", true)]);
        install_hook();
        // A panicking thread, not `catch_unwind` (STAB-6): the hook runs, the thread unwinds, join reports it.
        let unwound = std::thread::spawn(|| panic!("injected test panic")).join();
        drop(std::panic::take_hook()); // back to the default hook for the rest of the run
        sync(Vec::new());
        assert!(unwound.is_err(), "the hook chains on and the panic unwinds");
        assert_eq!(
            j.replay(Some(&doc)).as_deref(),
            Some(&b"work at the panic\n"[..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #866: the hook restores the terminal before the previous hook prints the message, and it reads the
    /// panic's message and location for the log event.
    #[test]
    fn panic_hook_restores_the_terminal_before_the_message_prints() {
        static ORDER: Mutex<Vec<String>> = Mutex::new(Vec::new());
        /// Hooks are process-wide: record only this test's own panicking thread.
        fn push(s: String) {
            if std::thread::current().name() == Some("panic-866") {
                ORDER.lock().unwrap_or_else(PoisonError::into_inner).push(s);
            }
        }
        let _serial = STORE_TEST.lock().unwrap_or_else(PoisonError::into_inner);
        sync(Vec::new());
        ORDER.lock().unwrap_or_else(PoisonError::into_inner).clear();
        // The "previous" hook stands for the default one that prints the message.
        std::panic::set_hook(Box::new(|info| {
            let (message, location) = describe(info);
            push(format!("print {message} @ {location}"));
        }));
        install_hook_with(|| push("restore".into()));
        let line = line!() + 3; // the `panic!` line below
        let unwound = std::thread::Builder::new()
            .name("panic-866".into())
            .spawn(|| panic!("injected {}", 866))
            .unwrap()
            .join();
        drop(std::panic::take_hook());
        assert!(unwound.is_err());
        let order = ORDER.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(order.len(), 2, "{order:?}");
        assert_eq!(order[0], "restore", "restored first: {order:?}");
        let want = format!("print injected 866 @ {}:{line}:", file!());
        assert!(order[1].starts_with(&want), "{order:?} vs {want}");
    }

    /// #856: a crash journals every modified buffer in the store, not only the focused one, and skips the
    /// clean ones.
    #[test]
    fn rescue_journals_every_modified_buffer() {
        let _serial = STORE_TEST.lock().unwrap_or_else(PoisonError::into_inner);
        let (j, dir) = journals("all");
        let (a, b, c) = (dir.join("a.txt"), dir.join("b.txt"), dir.join("c.txt"));
        sync(vec![
            entry(&j, &a, b"edited a\n", true),
            entry(&j, &b, b"edited b\n", true),
            entry(&j, &c, b"saved c\n", false),
        ]);
        let outcomes = rescue_all().expect("store is free");
        sync(Vec::new());
        assert_eq!(outcomes.len(), 3);
        assert_eq!(j.replay(Some(&a)).as_deref(), Some(&b"edited a\n"[..]));
        assert_eq!(j.replay(Some(&b)).as_deref(), Some(&b"edited b\n"[..]));
        assert!(!j.path_for(&c).exists(), "a clean buffer is not journaled");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
