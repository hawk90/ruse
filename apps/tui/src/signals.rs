//! Fatal-signal handling (#857). SIGTERM and SIGHUP (a closed terminal window) must not leave the terminal
//! raw or lose unsaved work. The handler does only async-signal-safe work: it records the signal in an
//! atomic and raises SIGWINCH at its own process. crossterm's event source already wakes on SIGWINCH
//! (it reports a resize), so a blocked `event::read`/`event::poll` returns; the session loop then sees
//! [`pending`] and ends. The caller journals every modified buffer (`crate::recover::rescue_on_exit`), the
//! terminal guard restores the terminal on the way out, and [`reraise`] exits with the conventional
//! killed-by-signal status.

#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};

#[cfg(unix)]
static PENDING: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    // Async-signal-safe only: an atomic store, getpid and kill. The first signal wins.
    let _ = PENDING.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    // SAFETY: getpid and kill are async-signal-safe and have no memory preconditions.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGWINCH);
    }
}

/// Install the SIGTERM / SIGHUP handler. Call after crossterm's event source exists (any `event::poll`),
/// so its SIGWINCH registration is in place to turn the wake-up into an event.
pub fn install() {
    #[cfg(unix)]
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction is a valid starting value on every unix libc; the handler is an
        // `extern "C" fn(c_int)` that does only async-signal-safe work, and every pointer is valid.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// The fatal signal received, if any.
pub fn pending() -> Option<i32> {
    #[cfg(unix)]
    {
        match PENDING.load(Ordering::SeqCst) {
            0 => None,
            sig => Some(sig),
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// End the process the way signal `sig` would have: restore its default action and raise it, so the
/// parent sees a killed-by-signal status (a shell reports 128 + `sig`). Falls back to exiting 128 + `sig`.
pub fn reraise(sig: i32) -> ! {
    #[cfg(unix)]
    // SAFETY: plain libc calls on this process with valid arguments; the default action terminates it.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(sig);
    }
    std::process::exit(128 + sig)
}
