//! Session-side terminal state (F-011). A [`Terminal`] owns its PTY and a VT [`Grid`]: incoming bytes are
//! fed to the `vte` parser (slice 2), which drives the grid the renderer paints. Slice 1's line-mode
//! `AnsiStrip` is gone — the grid handles colors, cursor addressing, erases, and the alternate screen.
//!
//! **Determinism boundary (F-022):** terminal input/output is external and non-deterministic — it is not
//! recorded as `Command`s, never mutates a `Document`, and `--replay` ignores it.

#![cfg(unix)]

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::pty::{Pty, UnixPty};
use crate::term_grid::{ChunkParser, Grid};

/// How much output one [`Terminal::drain`] may feed the VT parser (#770). A child that floods output (`yes`)
/// no longer keeps the event loop in `drain`: the rest waits for the next tick, so keys (CTRL-C, `:q!`) still
/// get read. The backlog stays bounded: the PTY channel holds `pty::CHANNEL_CHUNKS` chunks and [`Output`]
/// at most one partly fed chunk, and past that the reader stops and the child blocks on a full PTY.
#[derive(Clone, Copy, Debug)]
pub struct DrainBudget {
    /// Bytes fed per tick at most.
    pub bytes: usize,
    /// Wall time per tick: checked between slices, so one tick overshoots by at most one slice.
    pub time: Duration,
}

/// The per-tick budget the session uses. Parsing is the slow side: each scrolled line copies the whole
/// grid, so a flood of `y\n` on a 50×200 grid parses at ~100 µs a line (64 KiB in ~2 s, measured in a
/// release build). The time cap is therefore what normally binds; the byte cap bounds a tick when parsing
/// is cheap.
pub const DRAIN_BUDGET: DrainBudget = DrainBudget {
    bytes: 64 * 1024,
    time: Duration::from_millis(8),
};

/// The parser is fed in slices of at most this many bytes, so the time budget can stop a tick inside a
/// 4 KiB chunk (a slice of `y\n` on a 50×200 grid is ~9 ms in a release build).
const DRAIN_SLICE: usize = 256;

/// The PTY output side of a terminal: the channel, the chunk being fed (kept across ticks when a budget
/// stops mid-chunk), and whether the child has exited.
struct Output {
    rx: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    /// Bytes of `chunk` already fed.
    at: usize,
    /// Set once the channel disconnects (child exited) and the backlog is fed. Guards `send` (no writes to
    /// a dead child) and makes the `[process exited]` line appear exactly once; the buffer stays as a
    /// readable screen until closed.
    exited: bool,
}

impl Output {
    fn new(rx: Receiver<Vec<u8>>) -> Output {
        Output {
            rx,
            chunk: Vec::new(),
            at: 0,
            exited: false,
        }
    }

    /// One drain tick (#770): feed queued output to the parser in slices until it runs out or `budget` is
    /// spent; a tick always feeds at least one slice when output is pending, so a flood still progresses.
    /// A disconnect appends `[process exited]` once and sets `exited`; it is only seen after the backlog,
    /// so the last output lands first. `true` if anything changed.
    fn drain_into(
        &mut self,
        parser: &mut ChunkParser,
        grid: &mut Grid,
        budget: DrainBudget,
    ) -> bool {
        let start = Instant::now();
        let mut fed = 0usize;
        let mut changed = false;
        while fed < budget.bytes && (fed == 0 || start.elapsed() < budget.time) {
            if self.at == self.chunk.len() {
                match self.rx.try_recv() {
                    Ok(chunk) => {
                        self.chunk = chunk;
                        self.at = 0;
                        continue;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if !self.exited {
                            self.exited = true;
                            parser.advance(grid, b"\r\n[process exited]\r\n");
                            changed = true;
                        }
                        break;
                    }
                }
            }
            let end = self
                .chunk
                .len()
                .min(self.at + DRAIN_SLICE)
                .min(self.at + (budget.bytes - fed));
            parser.advance(grid, &self.chunk[self.at..end]);
            fed += end - self.at;
            self.at = end;
            changed = true;
        }
        changed
    }
}

/// A live terminal buffer: its PTY, the output side, the VT parser, and the screen grid.
pub struct Terminal {
    pty: UnixPty,
    out: Output,
    parser: ChunkParser,
    grid: Grid,
}

impl Terminal {
    /// Spawn a shell on a PTY sized `rows × cols`, with a matching grid.
    pub fn spawn(rows: u16, cols: u16) -> std::io::Result<Terminal> {
        let (pty, rx) = UnixPty::spawn(rows, cols)?;
        Ok(Terminal {
            pty,
            out: Output::new(rx),
            parser: ChunkParser::new(),
            grid: Grid::new(rows, cols),
        })
    }

    /// Whether the child still runs (Emacs's `confirm-kill-processes` asks about it, #619). The child's own
    /// state, not the output's (#1738): an exited child stops counting at once, while its queued output
    /// and the `[process exited]` notice after it are still drained tick by tick.
    pub fn is_running(&self) -> bool {
        !self.out.exited && !self.pty.has_exited()
    }

    /// Forward keystroke bytes to the child (Terminal mode). A dead child silently drops them.
    pub fn send(&mut self, bytes: &[u8]) {
        if !self.out.exited {
            let _ = self.pty.write(bytes);
        }
    }

    /// Forward a bracketed paste (`\n` line endings) to the child: see [`paste_bytes`].
    pub fn paste(&mut self, text: &str) {
        let bytes = paste_bytes(text, self.grid.bracketed_paste());
        self.send(&bytes);
    }

    /// Drain pending output into the grid via the VT parser, within [`DRAIN_BUDGET`] per call. Returns
    /// `true` if anything changed (new output or the child just exited) so the caller knows to re-render.
    pub fn drain(&mut self) -> bool {
        self.out
            .drain_into(&mut self.parser, &mut self.grid, DRAIN_BUDGET)
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
        let mut parser = ChunkParser::new();
        parser.advance(&mut grid, b"\x1b[?1049;2004h");
        assert!(grid.bracketed_paste());
        parser.advance(&mut grid, b"\x1b[?2004l");
        assert!(!grid.bracketed_paste());
        parser.advance(&mut grid, b"\x1b[?2004h\x1bc");
        assert!(!grid.bracketed_paste(), "RIS resets the mode");
    }

    /// `n` full chunks queued behind a disconnected sender (the child "exited" behind the backlog).
    fn backlog(n: usize) -> Output {
        let (tx, rx) = std::sync::mpsc::sync_channel(n);
        for _ in 0..n {
            tx.send(vec![b'y'; crate::pty::READ_CHUNK]).unwrap();
        }
        Output::new(rx)
    }

    /// #770: one drain tick feeds the parser at most the byte budget however much is queued; the rest waits
    /// for later ticks, and the exit notice comes only after the backlog.
    #[test]
    fn a_drain_tick_is_bounded_by_the_byte_budget() {
        let budget = DrainBudget {
            bytes: DRAIN_BUDGET.bytes,
            time: Duration::from_secs(3600),
        };
        let per_tick = budget.bytes / crate::pty::READ_CHUNK;
        let (mut parser, mut grid) = (ChunkParser::new(), Grid::new(4, 10));

        // One tick takes exactly the budget's worth and leaves the rest queued.
        let mut out = backlog(3 * per_tick);
        assert!(out.drain_into(&mut parser, &mut grid, budget));
        assert!(!out.exited);
        assert_eq!(out.rx.try_iter().count(), 2 * per_tick);

        // Draining the whole backlog takes three ticks; the fourth sees the disconnect.
        let mut out = backlog(3 * per_tick);
        for _ in 0..3 {
            assert!(out.drain_into(&mut parser, &mut grid, budget));
            assert!(!out.exited, "exit reported before the backlog drained");
        }
        assert!(out.drain_into(&mut parser, &mut grid, budget));
        assert!(out.exited);
        assert!(!out.drain_into(&mut parser, &mut grid, budget));
    }

    /// #770: a spent time budget stops a tick after one slice, inside the chunk; the next tick resumes there.
    #[test]
    fn a_drain_tick_is_bounded_by_the_time_budget() {
        let budget = DrainBudget {
            bytes: DRAIN_BUDGET.bytes,
            time: Duration::ZERO,
        };
        let (mut parser, mut grid) = (ChunkParser::new(), Grid::new(4, 10));
        let mut out = backlog(2);
        assert!(out.drain_into(&mut parser, &mut grid, budget));
        assert_eq!(out.at, DRAIN_SLICE, "one slice per tick");
        assert!(out.drain_into(&mut parser, &mut grid, budget));
        assert_eq!(out.at, 2 * DRAIN_SLICE, "the next tick resumes mid-chunk");
        assert_eq!(out.rx.try_iter().count(), 1);
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

    /// #1738: a child that exits with output still queued stops counting as running at once; the backlog
    /// and then the exit notice are drawn afterwards.
    #[test]
    fn an_exited_child_with_a_backlog_is_not_running() {
        // One write, small enough to fit in the channel however the PTY splits it into reads, so the child
        // exits with all of it still queued (nothing drains until the loop below).
        let argv = [
            "/bin/sh",
            "-c",
            "head -c 3000 /dev/zero | tr '\\0' y; printf LAST",
        ];
        let Ok((pty, rx)) = UnixPty::spawn_argv(&argv, 24, 80) else {
            return; // no PTY available (sandbox)
        };
        let mut term = Terminal {
            pty,
            out: Output::new(rx),
            parser: ChunkParser::new(),
            grid: Grid::new(24, 80),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while term.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!term.is_running(), "the child never exited");
        assert!(
            !term.out.exited,
            "nothing was drained, so the output is queued and no notice drawn"
        );
        while !term.out.exited && Instant::now() < deadline {
            term.drain();
        }
        assert!(term.out.exited, "the backlog never drained");
        assert!(
            grid_contains(term.grid(), "LAST"),
            "the last output is drawn"
        );
        assert!(
            grid_contains(term.grid(), "[process exited]"),
            "the notice follows the output"
        );
    }
}
