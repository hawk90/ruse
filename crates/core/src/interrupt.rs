//! The user-interrupt checkpoint (scheduler.md §8 `checkpoint`, nvim's `got_int` / `os_breakcheck`).
//!
//! A long-running loop (a `:g` pass, a `:s` scan, a macro or `:normal` replay, a counted repeat) calls
//! [`breakcheck`] at a natural step and stops when it returns `true`. `breakcheck` polls the frontend's
//! hook ([`set_hook`]) at most once per [`POLL_INTERVAL`]: the TUI's hook reads the typeahead without
//! blocking and reports whether the interrupt key (`CTRL-C`, or `C-g` under Emacs) arrived. Once seen, the
//! interrupt stays raised ([`interrupted`]) until the frontend takes it ([`take`]) after the command
//! unwound, so every nested loop on the way out stops too.
//!
//! The latch is per thread, not process-global editor state (INV-NO-GLOBAL-STATE): the editor runs on one
//! thread, and the latch belongs to whichever thread polls the terminal. Tests on other threads never see
//! each other's interrupts. With no hook installed (the core alone, the headless harness) `breakcheck` only
//! reports an interrupt raised with [`raise`].

use std::cell::Cell;
use std::time::{Duration, Instant};

/// The longest a loop runs between two polls of the hook: an interrupt is noticed within about this long
/// of the key reaching the terminal, plus one loop step.
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);

thread_local! {
    static GOT_INT: Cell<bool> = const { Cell::new(false) };
    static HOOK: Cell<Option<fn() -> bool>> = const { Cell::new(None) };
    static LAST_POLL: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Install (or with `None` remove) this thread's interrupt hook: a non-blocking check that returns `true`
/// when the user asked to interrupt.
pub fn set_hook(hook: Option<fn() -> bool>) {
    HOOK.with(|h| h.set(hook));
    LAST_POLL.with(|l| l.set(None));
}

/// The checkpoint a long-running loop calls once per step: polls the hook when [`POLL_INTERVAL`] has passed
/// since the last poll, and returns whether an interrupt is raised.
pub fn breakcheck() -> bool {
    if interrupted() {
        return true;
    }
    let Some(hook) = HOOK.with(Cell::get) else {
        return false;
    };
    let now = Instant::now();
    let due = LAST_POLL.with(|l| {
        l.get()
            .is_none_or(|t| now.duration_since(t) >= POLL_INTERVAL)
    });
    if !due {
        return false;
    }
    LAST_POLL.with(|l| l.set(Some(now)));
    if hook() {
        raise();
    }
    interrupted()
}

/// Whether an interrupt is raised, without polling.
#[must_use]
pub fn interrupted() -> bool {
    GOT_INT.with(Cell::get)
}

/// Raise the interrupt (the hook saw the key, or a caller wants the running loops to stop).
pub fn raise() {
    GOT_INT.with(|g| g.set(true));
}

/// Clear the interrupt, returning whether one was raised. The frontend calls this once the interrupted
/// command has unwound.
pub fn take() -> bool {
    GOT_INT.with(|g| g.replace(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_latches_until_taken() {
        assert!(!breakcheck());
        raise();
        assert!(breakcheck());
        assert!(interrupted());
        assert!(take());
        assert!(!interrupted());
        assert!(!take());
    }

    #[test]
    fn the_hook_is_polled_at_most_once_per_interval() {
        thread_local!(static CALLS: Cell<u32> = const { Cell::new(0) });
        fn hook() -> bool {
            CALLS.with(|c| c.set(c.get() + 1));
            false
        }
        set_hook(Some(hook));
        for _ in 0..1000 {
            assert!(!breakcheck());
        }
        // The first call polls; the rest fall inside the same interval (the loop takes far less than 20 ms).
        assert!(CALLS.with(Cell::get) <= 2, "{}", CALLS.with(Cell::get));
        set_hook(None);
    }

    #[test]
    fn a_hook_that_sees_the_key_raises_the_interrupt() {
        fn hook() -> bool {
            true
        }
        set_hook(Some(hook));
        assert!(breakcheck());
        set_hook(None);
        assert!(interrupted(), "the interrupt outlives the hook until taken");
        assert!(take());
    }
}
