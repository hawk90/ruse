//! The terminal guard: enters raw mode + the alternate screen (pushing the capability-appropriate
//! mode set) and restores everything on drop, even on panic (F-010 acceptance #3).

use std::io::{self, Write};

use crossterm::{cursor, queue, terminal};

use crate::caps;
use crate::terminal::capabilities::detect_capabilities;

/// Restores the terminal on drop, even on panic. Owns the capability ledger so the exact set of
/// modes pushed on enter is the exact set reset on exit (F-010 acceptance #3 — no shell corruption).
pub(crate) struct TermGuard {
    ledger: caps::ledger::Ledger,
}
impl TermGuard {
    /// Whether the terminal confirmed DEC synchronized output (mode 2026) at startup — read once
    /// and held (INV-RENDER-PROFILE: the profile is pinned, never re-probed on frame noise). The
    /// render diff fences a repaint in `?2026h`/`l` when this is true so the frame lands atomically.
    pub(crate) fn sync_output(&self) -> bool {
        self.ledger
            .enabled(caps::ledger::Capability::SynchronizedOutput)
    }

    /// Health-check readouts of the pinned ledger (F-030): whether these capabilities were detected.
    pub(crate) fn bracketed_paste(&self) -> bool {
        self.ledger
            .enabled(caps::ledger::Capability::BracketedPaste)
    }

    pub(crate) fn sgr_mouse(&self) -> bool {
        self.ledger.enabled(caps::ledger::Capability::SgrMouse)
    }

    pub(crate) fn focus_events(&self) -> bool {
        self.ledger.enabled(caps::ledger::Capability::FocusEvents)
    }

    /// The inline-graphics protocol detected at startup, pinned like [`Self::sync_output`] (F-031 slice
    /// 3b-2b / D-053). The render loop lowers image blocks to real pixels on a capable terminal, else the
    /// placeholder (INV-CAP-DEGRADE).
    pub(crate) fn graphics(&self) -> caps::ledger::GraphicsProtocol {
        self.ledger.graphics()
    }

    pub(crate) fn enter() -> io::Result<TermGuard> {
        let ledger = enter_with(&mut LiveTerminal)?;
        Ok(TermGuard { ledger })
    }
}

/// The steps of [`TermGuard::enter`], as a seam so the unwind order unit-tests without a terminal (#786).
trait Setup {
    fn raw_mode_on(&mut self) -> io::Result<()>;
    /// Arm [`restore`] with the mode pops `exit`.
    fn arm(&mut self, exit: Vec<u8>);
    fn alt_screen_on(&mut self) -> io::Result<()>;
    /// Probe the terminal (never fails: a failed probe leaves the env/default belief).
    fn probe(&mut self) -> caps::ledger::Ledger;
    /// Push the modes the ledger enables.
    fn push(&mut self, enter: &[u8]) -> io::Result<()>;
    /// Undo whatever is armed — pop the pushed modes, leave the alternate screen, show the cursor, leave
    /// raw mode — and disarm.
    fn restore(&mut self);
}

/// Enter raw mode and the alternate screen, probe, and push the ledger's modes. On an error after raw
/// mode is on, every step already taken is undone and the restore disarmed before the error returns: no
/// guard exists yet, so nothing else would leave the terminal raw-free (#786).
fn enter_with(t: &mut impl Setup) -> io::Result<caps::ledger::Ledger> {
    t.raw_mode_on()?;
    // Armed at once, so a signal or panic from here on (the probe included) still leaves the alternate
    // screen and raw mode; the modes the ledger pushes are added once they are known.
    t.arm(Vec::new());
    let entered = enter_after_raw(t);
    if entered.is_err() {
        t.restore();
    }
    entered
}

fn enter_after_raw(t: &mut impl Setup) -> io::Result<caps::ledger::Ledger> {
    t.alt_screen_on()?;
    // Probe AFTER raw mode (so replies arrive as raw bytes) and inside the alt screen (so query echoes, if
    // any, never touch the parent scrollback).
    let ledger = t.probe();
    // Armed before the push: a mode the terminal took is popped even if the write fails part way.
    t.arm(caps::sequences::exit(&ledger));
    t.push(&caps::sequences::enter(&ledger))?;
    Ok(ledger)
}

/// The real terminal behind [`Setup`].
struct LiveTerminal;

impl Setup for LiveTerminal {
    fn raw_mode_on(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()
    }

    fn arm(&mut self, exit: Vec<u8>) {
        arm(exit);
    }

    fn alt_screen_on(&mut self) -> io::Result<()> {
        queue!(io::stdout(), terminal::EnterAlternateScreen)?;
        io::stdout().flush()
    }

    fn probe(&mut self) -> caps::ledger::Ledger {
        let (ledger, typed) = detect_capabilities();
        // Keys typed while the probe read the terminal are read first, before any new input (#1013).
        for key in caps::typeahead::decode(&typed) {
            crate::breakcheck::push(crossterm::event::Event::Key(key));
        }
        ledger
    }

    fn push(&mut self, enter: &[u8]) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(enter)?;
        out.flush()
    }

    fn restore(&mut self) {
        restore(false);
    }
}

/// What restoring the terminal takes, armed by [`TermGuard::enter`] and taken by the first [`restore`], so
/// the terminal is restored exactly once: leaving the alternate screen a second time would restore the
/// cursor saved on entry and put the shell prompt over a panic message printed after the first restore.
struct Restore {
    exit: Vec<u8>,
    /// The thread that entered the terminal, whose panic ends the session.
    thread: std::thread::ThreadId,
}

static RESTORE: std::sync::Mutex<Option<Restore>> = std::sync::Mutex::new(None);

/// Arm (or re-arm) [`restore`] with the mode pops `exit`, for the calling thread.
fn arm(exit: Vec<u8>) {
    *RESTORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Restore {
        exit,
        thread: std::thread::current().id(),
    });
}

/// [`restore`] for the signal watchdog (#897), whose main thread is stuck and may hold the stdout lock:
/// the same bytes (the pushed modes popped, then the alternate screen left and the cursor shown) as one
/// raw `write(2)` to fd 1. Pops focus reports, mouse and bracketed paste too, so the parent shell is not
/// sent `CSI I` / `CSI O` or mouse reports after a fatal signal (#1220). Does nothing once restored.
#[cfg(unix)]
pub(crate) fn restore_raw() {
    let mut slot = match RESTORE.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    let Some(r) = slot.take() else {
        return;
    };
    drop(slot);
    let mut seq = r.exit;
    seq.extend_from_slice(b"\x1b[?1049l\x1b[?25h");
    // SAFETY: writes a valid buffer to fd 1; a failure (no terminal after a hang-up) is ignored.
    unsafe {
        libc::write(1, seq.as_ptr().cast(), seq.len());
    }
    let _ = terminal::disable_raw_mode();
}

/// Restore the terminal if it is still in the editor's modes: reset the pushed modes, leave the alternate
/// screen, show the cursor, leave raw mode. Called by the guard's drop and by the panic hook BEFORE the
/// panic message prints (#866), since a message printed on the alternate screen is wiped when it is left.
/// `only_from_owner`: do nothing unless called on the thread that entered the terminal (a panic on a worker
/// thread does not end the session, so the editor's screen must stay). Never blocks on a crash path.
pub(crate) fn restore(only_from_owner: bool) {
    let mut slot = match RESTORE.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if only_from_owner
        && slot
            .as_ref()
            .is_some_and(|r| r.thread != std::thread::current().id())
    {
        return;
    }
    let Some(r) = slot.take() else {
        return;
    };
    drop(slot);
    let mut out = io::stdout();
    let _ = out.write_all(&r.exit); // pop/reset before leaving
    let _ = queue!(out, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore(false);
    }
}

#[cfg(test)]
mod tests {
    //! The unwind of [`enter_with`] as a pure sequence (#786): each step can be made to fail, and the
    //! steps taken are recorded in order.

    use super::*;

    #[derive(Default)]
    struct Script {
        fail: Option<&'static str>,
        log: Vec<&'static str>,
        armed: bool,
    }

    impl Script {
        fn failing(step: &'static str) -> Script {
            Script {
                fail: Some(step),
                ..Script::default()
            }
        }

        fn step(&mut self, name: &'static str) -> io::Result<()> {
            self.log.push(name);
            if self.fail == Some(name) {
                Err(io::Error::other(name))
            } else {
                Ok(())
            }
        }
    }

    impl Setup for Script {
        fn raw_mode_on(&mut self) -> io::Result<()> {
            self.step("raw")
        }
        fn arm(&mut self, _: Vec<u8>) {
            self.log.push("arm");
            self.armed = true;
        }
        fn alt_screen_on(&mut self) -> io::Result<()> {
            self.step("alt")
        }
        fn probe(&mut self) -> caps::ledger::Ledger {
            self.log.push("probe");
            caps::ledger::Ledger::with_defaults()
        }
        fn push(&mut self, _: &[u8]) -> io::Result<()> {
            self.step("push")
        }
        fn restore(&mut self) {
            self.log.push("restore");
            self.armed = false;
        }
    }

    #[test]
    fn a_clean_enter_stays_armed_and_never_restores() {
        let mut t = Script::default();
        assert!(enter_with(&mut t).is_ok());
        assert_eq!(t.log, ["raw", "arm", "alt", "probe", "arm", "push"]);
        assert!(t.armed);
    }

    #[test]
    fn a_failed_raw_mode_leaves_nothing_to_undo() {
        let mut t = Script::failing("raw");
        assert!(enter_with(&mut t).is_err());
        assert_eq!(t.log, ["raw"]);
        assert!(!t.armed);
    }

    #[test]
    fn a_failed_alternate_screen_leaves_raw_mode() {
        let mut t = Script::failing("alt");
        assert!(enter_with(&mut t).is_err());
        assert_eq!(t.log, ["raw", "arm", "alt", "restore"]);
        assert!(!t.armed, "the restore is disarmed");
    }

    #[test]
    fn a_failed_mode_push_pops_the_modes_and_leaves_the_screen() {
        let mut t = Script::failing("push");
        let err = enter_with(&mut t).unwrap_err();
        assert_eq!(err.to_string(), "push");
        assert_eq!(
            t.log,
            ["raw", "arm", "alt", "probe", "arm", "push", "restore"]
        );
        assert!(!t.armed, "the restore is disarmed");
    }
}
