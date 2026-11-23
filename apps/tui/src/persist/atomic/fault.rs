//! The test fault-injection harness: a step to fail and the directories synced. Split from `atomic.rs`
//! (#1395).

use super::PathBuf;

/// The steps a test can make fail, for the failures a real filesystem will not produce on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    SyncFile,
    OpenDir,
    SyncDir,
    InPlaceWrite,
}

thread_local! {
    pub(super) static FAULT: std::cell::Cell<Option<(Step, Option<i32>)>> = const { std::cell::Cell::new(None) };
    pub(super) static SYNCED_DIRS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Make `step` fail on this thread with a real I/O error (`EIO`) until cleared with `None` (test seam).
pub(crate) fn inject(step: Option<Step>) {
    FAULT.with(|f| f.set(step.map(|s| (s, None))));
}

/// Make `step` fail on this thread with the OS error `errno` until cleared with `inject(None)`.
pub(crate) fn inject_errno(step: Step, errno: i32) {
    FAULT.with(|f| f.set(Some((step, Some(errno)))));
}

/// The directories this thread fsynced, oldest first, draining the log (test seam).
pub(crate) fn take_synced_dirs() -> Vec<PathBuf> {
    SYNCED_DIRS.with(|d| std::mem::take(&mut *d.borrow_mut()))
}
