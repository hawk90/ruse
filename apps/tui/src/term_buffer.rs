//! Session-side terminal state (F-011 slice 1). A [`Terminal`] owns its PTY and a **sanitized, line-mode**
//! scrollback: incoming bytes are stripped of escape sequences ([`AnsiStrip`]) and appended to a bounded
//! byte buffer that the renderer paints through the ordinary text path. This is the honest line-mode stand-in
//! for a full VT grid (slice 2): colors, cursor addressing, and full-screen TUIs are out of scope here.
//!
//! **Determinism boundary (F-022):** terminal input/output is external and non-deterministic — it is not
//! recorded as `Command`s, never mutates a `Document`, and `--replay` ignores it.

#![cfg(unix)]

use std::sync::mpsc::{Receiver, TryRecvError};

use crate::pty::{Pty, UnixPty};

/// Keep at most this many bytes of scrollback; older whole lines are dropped past it (slice 1 has no
/// scrollback paging). Trimming happens at a newline so the buffer stays line- and UTF-8-boundary aligned.
const SCROLLBACK_CAP: usize = 256 * 1024;

/// A live terminal buffer: its PTY, the output channel, the sanitizer, and the rendered scrollback.
pub struct Terminal {
    pty: UnixPty,
    rx: Receiver<Vec<u8>>,
    sanitizer: AnsiStrip,
    scrollback: Vec<u8>,
    /// Set once the channel disconnects (child exited). Guards `send` (no writes to a dead child) and makes
    /// the `[process exited]` marker append exactly once; the buffer stays as a readable transcript until closed.
    exited: bool,
}

impl Terminal {
    /// Spawn a shell on a PTY sized `rows × cols`.
    pub fn spawn(rows: u16, cols: u16) -> std::io::Result<Terminal> {
        let (pty, rx) = UnixPty::spawn(rows, cols)?;
        Ok(Terminal {
            pty,
            rx,
            sanitizer: AnsiStrip::default(),
            scrollback: Vec::new(),
            exited: false,
        })
    }

    /// Forward keystroke bytes to the child (Terminal mode). A dead child silently drops them.
    pub fn send(&mut self, bytes: &[u8]) {
        if !self.exited {
            let _ = self.pty.write(bytes);
        }
    }

    /// Drain all pending output into the scrollback. Returns `true` if anything changed (new output or the
    /// child just exited) so the caller knows to re-render.
    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.sanitizer.feed(&chunk, &mut self.scrollback);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.exited {
                        self.exited = true;
                        self.scrollback.extend_from_slice(b"\n[process exited]\n");
                        changed = true;
                    }
                    break;
                }
            }
        }
        if changed {
            self.trim();
        }
        changed
    }

    /// The rendered scrollback bytes (sanitized plain text).
    pub fn scrollback(&self) -> &[u8] {
        &self.scrollback
    }

    /// Drop whole leading lines once the cap is exceeded, keeping line + UTF-8 boundaries intact.
    fn trim(&mut self) {
        trim_scrollback(&mut self.scrollback, SCROLLBACK_CAP);
    }
}

/// Drop whole leading lines from `buf` until it fits `cap`, cutting at a newline so a line or UTF-8 sequence
/// is never split. A run with no newline past the cap is left intact (a single very long line is not split).
fn trim_scrollback(buf: &mut Vec<u8>, cap: usize) {
    if buf.len() <= cap {
        return;
    }
    let overflow = buf.len() - cap;
    if let Some(nl) = buf[overflow..].iter().position(|&b| b == b'\n') {
        buf.drain(..overflow + nl + 1);
    }
}

/// A minimal escape-sequence stripper: the line-mode substitute for a VT parser (slice 2 replaces it). It
/// drops CSI (`ESC [ … final`) and OSC (`ESC ] … BEL/ST`) sequences and C0 control bytes except `\n`/`\t`,
/// and treats a bare carriage return (`\r` not part of `\r\n`) as "rewrite the current line". UTF-8 text and
/// the bytes `\n`/`\t` pass through untouched.
#[derive(Default)]
pub struct AnsiStrip {
    state: State,
}

#[derive(Default, PartialEq)]
enum State {
    #[default]
    Ground,
    /// Saw a bare `\r`; the next byte decides — `\n` makes it a CRLF line ending, anything else makes it a
    /// carriage-return line rewrite (then the byte is reprocessed in Ground).
    Cr,
    /// Saw `ESC`, awaiting the sequence introducer.
    Esc,
    /// Inside a CSI sequence (`ESC [`), consuming until a final byte `0x40..=0x7e`.
    Csi,
    /// Inside an OSC sequence (`ESC ]`), consuming until `BEL` or the `ESC` of an `ST`.
    Osc,
}

impl AnsiStrip {
    /// Feed a raw chunk, appending the sanitized result to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            // A deferred `\r`: CRLF keeps the newline; a bare CR rewrites the line, then `b` falls through to
            // Ground handling below (state is reset so the `match` treats it fresh).
            if self.state == State::Cr {
                if b == b'\n' {
                    out.push(b'\n');
                    self.state = State::Ground;
                    continue;
                }
                rewrite_current_line(out);
                self.state = State::Ground;
            }
            match self.state {
                State::Cr => unreachable!("handled above"),
                State::Ground => match b {
                    0x1b => self.state = State::Esc,
                    b'\r' => self.state = State::Cr,
                    b'\n' | b'\t' => out.push(b),
                    0x00..=0x1f => {} // other C0 controls: drop
                    _ => out.push(b), // printable ASCII or any UTF-8 continuation/lead byte
                },
                State::Esc => match b {
                    b'[' => self.state = State::Csi,
                    b']' => self.state = State::Osc,
                    _ => self.state = State::Ground, // 2-byte / unsupported escape: drop both bytes
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = State::Ground; // final byte reached
                    }
                }
                State::Osc => match b {
                    0x07 => self.state = State::Ground, // BEL terminator
                    0x1b => self.state = State::Esc, // start of an ST (`ESC \`) — treat as terminator
                    _ => {}                          // OSC payload: drop
                },
            }
        }
    }
}

/// Carriage return (`\r`): truncate back to the start of the current line so the next write overwrites it —
/// approximates the "progress bar redraws in place" behaviour of CR without a full grid.
fn rewrite_current_line(out: &mut Vec<u8>) {
    let line_start = out.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    out.truncate(line_start);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(input: &[u8]) -> Vec<u8> {
        let mut s = AnsiStrip::default();
        let mut out = Vec::new();
        s.feed(input, &mut out);
        out
    }

    #[test]
    fn strips_csi_color_and_erase() {
        assert_eq!(strip(b"\x1b[31mred\x1b[0m\n"), b"red\n");
        assert_eq!(strip(b"\x1b[2J\x1b[Hclear"), b"clear");
    }

    #[test]
    fn strips_osc_title() {
        assert_eq!(strip(b"\x1b]0;my title\x07done\n"), b"done\n");
        // OSC terminated by ST (ESC \) instead of BEL.
        assert_eq!(strip(b"\x1b]0;t\x1b\\ok"), b"ok");
    }

    #[test]
    fn keeps_utf8_and_tabs_drops_other_controls() {
        assert_eq!(
            strip("héllo\tworld\x00\x07\n".as_bytes()),
            "héllo\tworld\n".as_bytes()
        );
    }

    #[test]
    fn carriage_return_rewrites_the_current_line() {
        // A progress bar overwriting the same line: only the last write survives.
        assert_eq!(strip(b"10%\r50%\r100%\n"), b"100%\n");
        // `\r\n` keeps the newline (the `\r` clears an empty current line, the `\n` ends it).
        assert_eq!(strip(b"line1\r\nline2"), b"line1\nline2");
    }

    #[test]
    fn split_escape_across_chunks_is_still_stripped() {
        let mut s = AnsiStrip::default();
        let mut out = Vec::new();
        s.feed(b"a\x1b[3", &mut out); // CSI split mid-sequence
        s.feed(b"1mb\n", &mut out);
        assert_eq!(out, b"ab\n");
    }

    #[test]
    fn trim_drops_whole_leading_lines_on_a_boundary() {
        let mut buf = b"one\ntwo\nthree\nfour\n".to_vec();
        trim_scrollback(&mut buf, 10); // len 18 > 10 → drop until it fits, at a newline
                                       // overflow = 8; first newline at/after index 8 ends "three" → drop through it, keep "four\n".
        assert_eq!(buf, b"four\n");
        // A UTF-8 line is never split: cutting lands on the ASCII newline, so the survivor stays valid.
        assert!(std::str::from_utf8(&buf).is_ok());
    }

    #[test]
    fn trim_leaves_a_single_long_unbroken_line_intact() {
        let mut buf = vec![b'x'; 100];
        trim_scrollback(&mut buf, 10); // no newline → nothing to cut on, keep as-is
        assert_eq!(buf.len(), 100);
    }

    // A real PTY round-trip (unix). Runs on the Linux CI; returns early if the environment denies a PTY.
    #[test]
    fn pty_round_trips_a_command() {
        let Ok(mut term) = Terminal::spawn(24, 80) else {
            return; // no PTY available (sandbox) — nothing to assert
        };
        term.send(b"echo ruse_pty_ok\n");
        term.send(b"exit\n");
        let mut waited = 0;
        while waited < 4000 {
            term.drain();
            if String::from_utf8_lossy(term.scrollback()).contains("ruse_pty_ok") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 50;
        }
        let out = String::from_utf8_lossy(term.scrollback()).into_owned();
        assert!(
            out.contains("ruse_pty_ok"),
            "scrollback missing echo output: {out:?}"
        );
    }
}
