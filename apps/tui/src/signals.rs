//! Fatal-signal handling (#857). SIGTERM, SIGHUP (a closed terminal window) and an external SIGINT must not
//! leave the terminal raw or lose unsaved work. Ctrl-C never raises SIGINT here: raw mode turns it into a
//! key (#897; nvim also treats an external SIGINT to its TUI as deadly). The handler does only
//! async-signal-safe work: it records the signal in an atomic, raises SIGWINCH at its own process, and wakes
//! the watchdog thread through a pipe. crossterm's event source already wakes on SIGWINCH (it reports a
//! resize), so a blocked `event::read`/`event::poll` returns; the session loop then sees [`pending`] and
//! ends. The caller journals every modified buffer (`crate::recover::rescue_on_exit`), the terminal guard
//! restores the terminal on the way out, and [`reraise`] exits with the conventional killed-by-signal status.
//!
//! The watchdog (#897) covers a main thread that does not get back to the loop: after a hang-up crossterm
//! 0.29 spins forever inside `event::read` on a TTY read that returns EOF / EIO (its read loop only stops
//! on `WouldBlock`), and a long-running command can hold the loop too. If the exit path has not started
//! [`GRACE`] after the signal, the watchdog journals every modified buffer itself, restores what it can of
//! the terminal and re-raises.

#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

#[cfg(unix)]
static PENDING: AtomicI32 = AtomicI32::new(0);

/// The write end of the watchdog's wake-up pipe, or -1 before [`install`].
#[cfg(unix)]
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

/// Held by whichever of the normal exit path and the watchdog ends the process, so only one does.
static EXIT: Mutex<()> = Mutex::new(());

/// How long the watchdog waits for the session loop to notice a signal before ending the process itself.
#[cfg(unix)]
const GRACE: std::time::Duration = std::time::Duration::from_millis(1000);

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    // Async-signal-safe only: atomics, getpid, kill and write. The first signal wins.
    let _ = PENDING.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    // SAFETY: getpid, kill and write are async-signal-safe; the buffer is a valid one-byte local.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGWINCH);
        let fd = WAKE_FD.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte = 1u8;
            libc::write(fd, std::ptr::addr_of!(byte).cast(), 1);
        }
    }
}

/// Install the SIGTERM / SIGHUP / SIGINT handler and its watchdog (once per process). Call after
/// crossterm's event source exists (any `event::poll`), so its SIGWINCH registration is in place to turn
/// the wake-up into an event.
pub fn install() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            start_watchdog();
            for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
                // SAFETY: a zeroed sigaction is a valid starting value on every unix libc; the handler is
                // an `extern "C" fn(c_int)` that does only async-signal-safe work, and every pointer is valid.
                unsafe {
                    let mut sa: libc::sigaction = std::mem::zeroed();
                    sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                    sa.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut sa.sa_mask);
                    libc::sigaction(sig, &sa, std::ptr::null_mut());
                }
            }
        });
    }
}

/// Spawn the watchdog thread on a fresh close-on-exec pipe. Without a pipe there is no watchdog, and the
/// handler still works as before.
#[cfg(unix)]
fn start_watchdog() {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a valid two-element out-array; fcntl only sets flags on the fds just created.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC); // never leak into spawned children
        }
    }
    let [read_fd, write_fd] = fds;
    let spawned = std::thread::Builder::new()
        .name("ruse-signal-watchdog".into())
        .spawn(move || {
            let mut byte = 0u8;
            // SAFETY: a blocking one-byte read into a valid local from the pipe this thread owns.
            while unsafe { libc::read(read_fd, std::ptr::addr_of_mut!(byte).cast(), 1) } != 1 {}
            std::thread::sleep(GRACE);
            let Some(sig) = pending() else { return };
            // The exit path holds EXIT once it has started; then it finishes the job.
            let Ok(_exit) = EXIT.try_lock() else { return };
            crate::recover::rescue_on_exit("signal_watchdog");
            restore_terminal();
            reraise(sig);
        });
    if spawned.is_ok() {
        WAKE_FD.store(write_fd, Ordering::SeqCst);
    }
}

/// Best-effort terminal teardown from outside the session (the guard lives on the stuck main thread): pop the
/// modes the guard pushed, leave the alt screen, show the cursor, leave raw mode, with a raw write since the
/// stuck thread may hold the stdout lock. After a hang-up there is no terminal to restore.
#[cfg(unix)]
fn restore_terminal() {
    crate::terminal::guard::restore_raw();
}

/// Mark the process as exiting through the normal path, so the watchdog stands down. Hold the guard until
/// the process ends.
pub fn begin_exit() -> MutexGuard<'static, ()> {
    EXIT.lock().unwrap_or_else(PoisonError::into_inner)
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
