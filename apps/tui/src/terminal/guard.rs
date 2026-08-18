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

    pub(crate) fn enter() -> io::Result<TermGuard> {
        terminal::enable_raw_mode()?;
        queue!(io::stdout(), terminal::EnterAlternateScreen)?;
        io::stdout().flush()?;
        // Probe AFTER raw mode (so replies arrive as raw bytes) and inside the alt screen (so query
        // echoes, if any, never touch the parent scrollback).
        let ledger = detect_capabilities();
        let mut out = io::stdout();
        out.write_all(&caps::sequences::enter(&ledger))?;
        out.flush()?;
        Ok(TermGuard { ledger })
    }
}
impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(&caps::sequences::exit(&self.ledger)); // pop/reset before leaving
        let _ = queue!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}
