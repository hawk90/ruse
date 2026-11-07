//! Session-side terminal state (F-011). A [`Terminal`] owns its PTY and a VT [`Grid`]: incoming bytes are
//! fed to the `vte` parser (slice 2), which drives the grid the renderer paints. Slice 1's line-mode
//! `AnsiStrip` is gone — the grid handles colors, cursor addressing, erases, and the alternate screen.
//!
//! **Determinism boundary (F-022):** terminal input/output is external and non-deterministic — it is not
//! recorded as `Command`s, never mutates a `Document`, and `--replay` ignores it.

#![cfg(unix)]

use std::sync::mpsc::{Receiver, TryRecvError};

use vte::Parser;

use crate::pty::{Pty, UnixPty};
use crate::term_grid::Grid;

/// A live terminal buffer: its PTY, the output channel, the VT parser, and the screen grid.
pub struct Terminal {
    pty: UnixPty,
    rx: Receiver<Vec<u8>>,
    parser: Parser,
    grid: Grid,
    /// Set once the channel disconnects (child exited). Guards `send` (no writes to a dead child) and makes
    /// the `[process exited]` line appear exactly once; the buffer stays as a readable screen until closed.
    exited: bool,
}

impl Terminal {
    /// Spawn a shell on a PTY sized `rows × cols`, with a matching grid.
    pub fn spawn(rows: u16, cols: u16) -> std::io::Result<Terminal> {
        let (pty, rx) = UnixPty::spawn(rows, cols)?;
        Ok(Terminal {
            pty,
            rx,
            parser: Parser::new(),
            grid: Grid::new(rows, cols),
            exited: false,
        })
    }

    /// Whether the child still runs (Emacs's `confirm-kill-processes` asks about it, #619).
    pub fn is_running(&self) -> bool {
        !self.exited
    }

    /// Forward keystroke bytes to the child (Terminal mode). A dead child silently drops them.
    pub fn send(&mut self, bytes: &[u8]) {
        if !self.exited {
            let _ = self.pty.write(bytes);
        }
    }

    /// Forward a bracketed paste (`\n` line endings) to the child: see [`paste_bytes`].
    pub fn paste(&mut self, text: &str) {
        let bytes = paste_bytes(text, self.grid.bracketed_paste());
        self.send(&bytes);
    }

    /// Drain all pending output into the grid via the VT parser. Returns `true` if anything changed (new
    /// output or the child just exited) so the caller knows to re-render.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.parser.advance(&mut self.grid, &chunk);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.exited {
                        self.exited = true;
                        self.parser
                            .advance(&mut self.grid, b"\r\n[process exited]\r\n");
                        changed = true;
                    }
                    break;
                }
            }
        }
        changed
    }

    /// The VT screen grid (for the renderer).
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Resize the grid AND the PTY (`TIOCSWINSZ`) so the child reflows — called when the window rect changes.
    /// A no-op when the size is unchanged, so the per-frame call does not spam `ioctl`.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.grid.size() == (rows.max(1), cols.max(1)) {
            return;
        }
        self.grid.resize(rows, cols);
        let _ = self.pty.resize(rows, cols);
    }
}

/// The bytes a paste of `text` sends to a terminal child, as nvim's `terminal_paste` does for ESC: ESC and
/// the C1 controls are dropped, so the payload can neither end the child's paste bracket early nor inject a
/// control sequence, and lines are joined with `\n`. Wrapped in `CSI 200 ~` / `CSI 201 ~` when the child
/// enabled bracketed paste (`bracketed`).
#[must_use]
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    if bracketed {
        out.extend_from_slice(b"\x1b[200~");
    }
    let mut buf = [0u8; 4];
    for c in text
        .chars()
        .filter(|&c| !crate::input::opens_control_sequence(c))
    {
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    if bracketed {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paste_is_bracketed_only_when_the_child_asked_and_carries_no_escape() {
        assert_eq!(paste_bytes("ls\nx", false), b"ls\nx");
        assert_eq!(
            paste_bytes("a\u{1b}[201~rm\u{9b}b", true),
            b"\x1b[200~a[201~rmb\x1b[201~"
        );
        let mut grid = Grid::new(4, 10);
        let mut parser = Parser::new();
        parser.advance(&mut grid, b"\x1b[?1049;2004h");
        assert!(grid.bracketed_paste());
        parser.advance(&mut grid, b"\x1b[?2004l");
        assert!(!grid.bracketed_paste());
        parser.advance(&mut grid, b"\x1b[?2004h\x1bc");
        assert!(!grid.bracketed_paste(), "RIS resets the mode");
    }

    fn grid_contains(grid: &Grid, needle: &str) -> bool {
        let (rows, cols) = grid.size();
        for r in 0..rows {
            let mut line = String::new();
            for c in 0..cols {
                if let Some((t, _)) = grid.cell(r, c) {
                    line.push_str(if t.is_empty() { " " } else { t });
                }
            }
            if line.contains(needle) {
                return true;
            }
        }
        false
    }

    // A real PTY round-trip (unix) through the VT grid. Runs on the Linux CI; returns early if the
    // environment denies a PTY.
    #[test]
    fn pty_output_lands_in_the_grid() {
        let Ok(mut term) = Terminal::spawn(24, 80) else {
            return; // no PTY available (sandbox) — nothing to assert
        };
        term.send(b"echo ruse_pty_ok\n");
        term.send(b"exit\n");
        let mut waited = 0;
        while waited < 4000 {
            term.drain();
            if grid_contains(term.grid(), "ruse_pty_ok") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 50;
        }
        assert!(
            grid_contains(term.grid(), "ruse_pty_ok"),
            "grid missing echo output"
        );
    }
}
