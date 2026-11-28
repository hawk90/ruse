//! The chunk-safe `vte` driver for the terminal [`Grid`] (#1765, #1750, #1779), split from `term_grid.rs`.

use super::Grid;

/// OSC body bytes handed to vte per OSC; the rest of a longer one is dropped (#1779). vte's `std` build keeps
/// the OSC in an unbounded `Vec`, so a child that opens an OSC and never ends it would grow it without limit.
/// tmux bounds OSC/DCS/APC strings at 1 MiB (`INPUT_BUF_DEFAULT_SIZE`), which leaves room for an OSC 52
/// clipboard payload; this matches it.
const OSC_MAX: usize = 1 << 20;

/// The `vte` parser fed so that a PTY read boundary cannot change the grid (#1765). Feeding a stream in
/// pieces must give the grid one feed gives, since a read can end anywhere.
///
/// vte 0.15 breaks that when a piece ends inside a UTF-8 sequence. It carries the partial sequence into the
/// next `advance` and completes it from a 4-byte window. Two things go wrong there. When the window does
/// not end on a char boundary, every byte in it after the char is skipped (`é` cut after `C3`, then
/// `A9 0A E4 B8 AD`, loses the LF before `中`). A completed U+0080..=U+009F is also printed, where one feed
/// executes it as C1.
///
/// So this parser never ends an `advance` call inside a UTF-8 sequence. It keeps an incomplete trailing
/// sequence (a lead byte and fewer continuation bytes than it needs, at most 3 bytes) and puts it in front
/// of the next piece. vte's partial path is then never taken. Holding bytes back is safe in any parser
/// state: vte sees the same bytes in the same order. A stream that ends inside a sequence prints nothing
/// for it either way.
///
/// It also splits each piece around a lone 0x80..=0x9F byte and marks the grid while vte takes that byte
/// alone, so the grid can tell it from an encoded U+0080..=U+009F and print U+FFFD for it (#1750). vte
/// executes either as a C1 control only in its ground state; in any other state the byte reaches no
/// `execute`, so the mark is inert there. Splitting at a UTF-8 chunk boundary does not change what vte does.
///
/// Last, it bounds an OSC (#1779): it follows vte's state as far as OSC entry and exit ([`OscTrack`]) and
/// gives vte at most [`OSC_MAX`] body bytes of one OSC, dropping the rest up to the terminator, which still
/// reaches vte. vte then dispatches the OSC cut at the cap; the grid ignores every OSC, so this only bounds
/// memory. A grid that comes to act on an OSC must not act on one cut at the cap.
pub struct ChunkParser {
    parser: vte::Parser,
    carry: [u8; 3],
    carry_len: usize,
    osc: OscTrack,
    pub(super) osc_max: usize,
}

/// vte's parser state, as far as it decides where an OSC starts and ends. Every vte state goes to Escape on
/// ESC (0x1B); from Escape, `]` starts an OSC, C0 controls (but CAN and SUB), DEL, ESC and 0x80..=0xFF keep
/// Escape, and any other byte leaves it. An OSC ends at BEL, CAN, SUB or ESC (vte-0.15.0 `advance_esc`,
/// `advance_osc_string`, `anywhere`). Other states never enter an OSC except through Escape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum OscTrack {
    Other,
    Escape,
    /// Inside an OSC, with this many body bytes given to vte.
    Osc(usize),
}

impl OscTrack {
    /// The state after `byte`, and whether vte gets `byte`: `false` for an OSC body byte past `max`.
    pub(super) fn step(self, byte: u8, max: usize) -> (OscTrack, bool) {
        use OscTrack::*;
        match (self, byte) {
            (_, 0x1b) => (Escape, true),
            (Osc(_), 0x07 | 0x18 | 0x1a) => (Other, true),
            (Osc(n), _) if n >= max => (Osc(n), false),
            (Osc(n), _) => (Osc(n + 1), true),
            (Escape, b']') => (Osc(0), true),
            (Escape, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff) => (Escape, true),
            _ => (Other, true),
        }
    }
}

impl Default for ChunkParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkParser {
    pub fn new() -> ChunkParser {
        ChunkParser {
            parser: vte::Parser::new(),
            carry: [0; 3],
            carry_len: 0,
            osc: OscTrack::Other,
            osc_max: OSC_MAX,
        }
    }

    /// Hand `bytes` to vte, without the OSC body bytes past the cap.
    fn feed(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let mut start = 0;
        for (i, &b) in bytes.iter().enumerate() {
            let (next, keep) = self.osc.step(b, self.osc_max);
            self.osc = next;
            if !keep {
                if start < i {
                    self.parser.advance(grid, &bytes[start..i]);
                }
                start = i + 1;
            }
        }
        if start < bytes.len() {
            self.parser.advance(grid, &bytes[start..]);
        }
    }

    /// Feed one piece of child output to `grid`.
    pub fn advance(&mut self, grid: &mut Grid, bytes: &[u8]) {
        if self.carry_len == 0 {
            self.advance_whole_chars(grid, bytes);
            return;
        }
        let mut joined = Vec::with_capacity(self.carry_len + bytes.len());
        joined.extend_from_slice(&self.carry[..self.carry_len]);
        joined.extend_from_slice(bytes);
        self.carry_len = 0;
        self.advance_whole_chars(grid, &joined);
    }

    /// Feed `bytes` without an incomplete trailing UTF-8 sequence, which is kept for the next call.
    fn advance_whole_chars(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let keep = incomplete_utf8_tail(bytes);
        let (now, later) = bytes.split_at(bytes.len() - keep);
        self.advance_marking_lone_c1(grid, now);
        self.carry[..keep].copy_from_slice(later);
        self.carry_len = keep;
    }

    /// Feed `bytes`, giving each lone 0x80..=0x9F byte to vte in an `advance` of its own with
    /// [`Grid::lone_c1`] set.
    fn advance_marking_lone_c1(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let mut start = 0;
        let mut pos = 0;
        for chunk in bytes.utf8_chunks() {
            pos += chunk.valid().len();
            let bad = chunk.invalid();
            if let [b @ 0x80..=0x9F] = bad {
                if start < pos {
                    self.feed(grid, &bytes[start..pos]);
                }
                grid.lone_c1 = true;
                self.feed(grid, std::slice::from_ref(b));
                grid.lone_c1 = false;
                start = pos + 1;
            }
            pos += bad.len();
        }
        if start < bytes.len() {
            self.feed(grid, &bytes[start..]);
        }
    }
}

/// How many trailing bytes of `bytes` are an incomplete UTF-8 sequence: a lead byte followed only by
/// continuation bytes, still a valid prefix and short of its length. 0 when the tail is complete or invalid.
pub(super) fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for k in 1..=bytes.len().min(3) {
        let tail = &bytes[bytes.len() - k..];
        match tail[0] {
            0x80..=0xBF => continue, // a continuation byte: keep looking for the lead
            0xC0..=0xFF => {
                return match std::str::from_utf8(tail) {
                    Err(e) if e.valid_up_to() == 0 && e.error_len().is_none() => k,
                    _ => 0,
                };
            }
            _ => return 0,
        }
    }
    0
}
