//! UTF-8 and grapheme-cluster boundary stepping, Vim's line-model slot tests and column math: the
//! shared helpers the whole crate reaches as `crate::motion::*`.

use crate::cells::Tabs;
use crate::pos::raw::{line_end, line_start};
use std::cell::Cell;

use unicode_segmentation::GraphemeCursor;

/// The length of the UTF-8 char that starts at `i`, or `None` when the bytes there are not one: an invalid
/// lead, a stray continuation byte, a cut, overlong or surrogate sequence (#1232).
pub(crate) fn char_len_at(b: &[u8], i: usize) -> Option<usize> {
    let lead = *b.get(i)?;
    // The lead's length and the range its second byte must fall in (no overlong form, no surrogate, nothing
    // past U+10FFFF), as `std::str::from_utf8` checks.
    let (w, lo, hi) = match lead {
        0x00..=0x7F => return Some(1),
        0xC2..=0xDF => (2, 0x80, 0xBF),
        0xE0 => (3, 0xA0, 0xBF),
        0xED => (3, 0x80, 0x9F),
        0xE1..=0xEF => (3, 0x80, 0xBF),
        0xF0 => (4, 0x90, 0xBF),
        0xF4 => (4, 0x80, 0x8F),
        0xF1..=0xF3 => (4, 0x80, 0xBF),
        _ => return None,
    };
    let rest = b.get(i + 1..i + w)?;
    let ok = (lo..=hi).contains(&rest[0]) && rest[1..].iter().all(|&c| c & 0xC0 == 0x80);
    ok.then_some(w)
}

/// Whether `i` is on a continuation byte of a valid char (strictly inside it). A continuation byte that no
/// valid char owns is a cell of its own, as in nvim (#1232). Looks at most 3 bytes back.
fn inside_char(b: &[u8], i: usize) -> bool {
    if i == 0 || i >= b.len() || b[i] & 0xC0 != 0x80 {
        return false;
    }
    (i.saturating_sub(3)..i)
        .rev()
        .find(|&j| b[j] & 0xC0 != 0x80)
        .is_some_and(|j| char_len_at(b, j).is_some_and(|w| j + w > i))
}

/// Whether `i` is a char boundary: not strictly inside a valid UTF-8 char. A byte that belongs to no valid
/// char is a char of its own (#1232), as nvim counts it; on valid text this is the lead-byte test.
pub(crate) fn is_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || i == b.len() || !inside_char(b, i)
}

pub(crate) fn prev_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i > 0 {
        i -= 1;
        if is_boundary(b, i) {
            return i;
        }
    }
    0
}

pub(crate) fn next_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = (pos + 1).min(b.len());
    while i < b.len() && !is_boundary(b, i) {
        i += 1;
    }
    i
}

/// The byte offset of the next **grapheme cluster** boundary after `pos` (F-002): one `l`/`MoveRight`
/// step crosses a whole user-perceived character — an emoji ZWJ sequence, a base+combining mark, a
/// wide CJK glyph — not a single `char`, so the cursor never desyncs from the logical text (UAX#29).
/// A byte that is not UTF-8 is a cell of its own, and the valid text around it keeps its clusters (#1232).
///
/// A call costs the cluster it crosses ([`Steps`]).
pub(crate) fn next_grapheme(b: &[u8], pos: usize) -> usize {
    Steps::new(b).next(pos)
}

/// The byte offset of the previous grapheme-cluster boundary before `pos` (the `h`/`MoveLeft` step).
pub(crate) fn prev_grapheme(b: &[u8], pos: usize) -> usize {
    Steps::new(b).prev(pos)
}

/// How many bytes either side of a position a cluster step reads first; doubled while the segmenter asks
/// for more (a long ZWJ sequence, a run of flags, many combining marks).
const STEP_WINDOW: usize = 16;

/// A grapheme-cluster stepper over `b`, with the bounds of the line it last asked about.
///
/// A step reads only the text around `pos` (#1224, #1232). The cells of a line are its valid UTF-8 runs,
/// each segmented into extended grapheme clusters (UAX#29) as if it stood alone, and the bytes that belong
/// to no valid char, one cell each (nvim's `utfc_ptr2len` gives such a byte length 1, and a combining mark
/// after it does not join it). A run ends at a `\n` (a cluster never crosses a line break), at a byte that
/// is not UTF-8, or at the buffer's ends; the segmenter is told so, and is given a window of the run that
/// grows only while it asks for more context ([`GraphemeCursor`]'s chunk protocol). At a line's end (its
/// `\n`) or start a step crosses the newline by the char-boundary fallback, and a position strictly inside
/// a valid char steps to the next or previous char boundary.
///
/// The line-bounds cache is in a `Cell` so a shared `&Steps` (the text objects' buffer view) can use it.
pub(crate) struct Steps<'a> {
    pub(super) b: &'a [u8],
    /// The cached line `[ls, le]`, `le` being its `\n` or the buffer end; empty (`ls > le`) before first use.
    span: Cell<(usize, usize)>,
}

impl<'a> Steps<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Steps {
            b,
            span: Cell::new((1, 0)),
        }
    }

    /// The line holding `pos` (clamped to the buffer) as `(start, end)`, from the cache when `pos` is on
    /// the cached line: [`line_start`] and [`line_end`] give the same line for every offset in `[ls, le]`.
    fn line(&self, pos: usize) -> (usize, usize) {
        let p = pos.min(self.b.len());
        let (ls, le) = self.span.get();
        if ls <= p && p <= le {
            return (ls, le);
        }
        let span = (line_start(self.b, p), line_end(self.b, p));
        self.span.set(span);
        span
    }

    /// [`line_start`] of `pos`.
    pub(crate) fn line_start(&self, pos: usize) -> usize {
        self.line(pos).0
    }

    /// [`line_end`] of `pos`.
    pub(crate) fn line_end(&self, pos: usize) -> usize {
        self.line(pos).1
    }

    /// [`next_grapheme`] of `pos`.
    pub(crate) fn next(&self, pos: usize) -> usize {
        // Two ASCII bytes always have a cluster boundary between them inside a line (no ASCII char extends,
        // prepends or joins; the CR LF pair spans the line end, which is a boundary anyway).
        let b = self.b;
        if pos < b.len() && b[pos] < 0x80 && b.get(pos + 1).is_none_or(|&c| c < 0x80) {
            return pos + 1;
        }
        // The buffer's end, a line's NUL slot, or inside a char: the char step.
        if pos >= b.len() || b[pos] == b'\n' || inside_char(b, pos) {
            return next_boundary(b, pos);
        }
        // A byte that is not UTF-8 is a cell of its own.
        let Some(w) = char_len_at(b, pos) else {
            return pos + 1;
        };
        // A char followed by ASCII (or the line's end): no ASCII char extends, joins or is joined, so the
        // pair alone decides; only a Prepend char would take the ASCII char, which the full step handles.
        let after = pos + w;
        // The run ends after it (the line's end, a byte that is not UTF-8): a cluster ends there.
        if at_nul(b, after) || char_len_at(b, after).is_none() {
            return after;
        }
        // An ASCII char followed by a non-ASCII one: an ASCII char takes part in no rule that looks further
        // back (it is no Prepend, ZWJ, regional indicator or InCB linker), so the pair alone decides whether
        // the next char joins it; a join takes the full step.
        if w == 1 {
            if let Some(d) = char_len_at(b, after) {
                if let Ok(s) = std::str::from_utf8(&b[pos..after + d]) {
                    if let Ok(Some(1)) = GraphemeCursor::new(0, s.len(), true).next_boundary(s, 0) {
                        return after;
                    }
                }
            }
        }
        if b[after] < 0x80 {
            if let Ok(s) = std::str::from_utf8(&b[pos..=after]) {
                if let Ok(Some(r)) = GraphemeCursor::new(0, s.len(), true).next_boundary(s, 0) {
                    if r == w {
                        return after;
                    }
                }
            }
        }
        cluster_step(b, pos, true)
    }

    /// [`prev_grapheme`] of `pos`.
    pub(crate) fn prev(&self, pos: usize) -> usize {
        // The mirror of the ASCII step in [`Steps::next`]: an ASCII byte before `pos` whose own predecessor
        // is ASCII, a newline or the buffer start begins a cluster of its own (a non-ASCII predecessor could
        // be a Prepend char, so that takes the full path).
        let b = self.b;
        if pos > 0
            && pos <= b.len()
            && b[pos - 1] < 0x80
            && (pos == 1 || b[pos - 2] < 0x80 || b[pos - 1] == b'\n')
        {
            return pos - 1;
        }
        let pos = pos.min(b.len());
        // The buffer's start, a line's start (the step crosses back over its `\n`), or inside a char.
        if pos == 0 || b[pos - 1] == b'\n' || inside_char(b, pos) {
            return prev_boundary(b, pos);
        }
        let q = prev_boundary(b, pos);
        // The cell before is a byte that is not UTF-8.
        if char_len_at(b, q).is_none() {
            return q;
        }
        // An ASCII char before `pos` after a non-ASCII one: it extends nothing, so it starts a cluster unless
        // the char before it is a Prepend char, which the pair alone tells.
        if q + 1 == pos && !at_bol(b, q) {
            let p = prev_boundary(b, q);
            if let Some(s) = char_len_at(b, p).and_then(|_| std::str::from_utf8(&b[p..pos]).ok()) {
                let mut gc = GraphemeCursor::new(s.len(), s.len(), true);
                if let Ok(Some(r)) = gc.prev_boundary(s, 0) {
                    if p + r == q {
                        return q;
                    }
                }
            }
        }
        // The char before `pos` starts its run (a line's start, after a byte that is not UTF-8), or has
        // ASCII before it that nothing before can join (its own predecessor is ASCII or the line's start):
        // the text from there to `pos` alone decides.
        let start = if at_bol(b, q) || char_len_at(b, prev_boundary(b, q)).is_none() {
            Some(q)
        } else if b[q - 1] < 0x80 && (at_bol(b, q - 1) || b[q - 2] < 0x80) {
            Some(q - 1)
        } else {
            None
        };
        if let Some(start) = start {
            if let Ok(s) = std::str::from_utf8(&b[start..pos]) {
                let mut gc = GraphemeCursor::new(s.len(), s.len(), true);
                if let Ok(Some(r)) = gc.prev_boundary(s, 0) {
                    return start + r;
                }
            }
        }
        cluster_step(b, pos, false)
    }
}

/// The valid UTF-8 run holding `pos`, as far as `limit` bytes either side: `(start, start_known, text,
/// end_known)`. A side is known when it is the run's real edge (a buffer end, a `\n`, a byte that is not
/// UTF-8) rather than where the window stopped. `pos` is a char boundary inside the run or at its end.
fn utf8_run(b: &[u8], pos: usize, limit: usize) -> (usize, bool, &str, bool) {
    let lo = pos.saturating_sub(limit);
    let hi = (pos + limit).min(b.len());
    let (mut c, mut start_known) = match b[lo..pos].iter().rposition(|&x| x == b'\n') {
        Some(i) => (lo + i + 1, true),
        None => (lo, lo == 0),
    };
    let (e, mut end_known) = match b[pos..hi].iter().position(|&x| x == b'\n') {
        Some(i) => (pos + i, true),
        None => (hi, hi == b.len()),
    };
    if !start_known {
        // The window may cut a char: start at the next char's first byte.
        while c < pos && b[c] & 0xC0 == 0x80 {
            c += 1;
        }
    }
    loop {
        match std::str::from_utf8(&b[c..e]) {
            Ok(s) => return (c, start_known, s, end_known),
            Err(err) => {
                let bad = c + err.valid_up_to();
                let cut = err.error_len().is_none() && !end_known;
                if bad >= pos {
                    // The first fault is at or after `pos`: the run ends there, at a byte that is not UTF-8,
                    // or at a char the window's end cut (the run goes on past the window).
                    end_known = !cut;
                    let s = std::str::from_utf8(&b[c..bad]).unwrap_or_default();
                    return (c, start_known, s, end_known);
                }
                // A byte before `pos` that is not UTF-8: the run starts after it.
                c = bad + err.error_len().unwrap_or(e - bad);
                start_known = true;
            }
        }
    }
}

/// The cluster boundary after (`forward`) or before `pos`, a char boundary inside a valid run (after a
/// valid char, going back). The segmenter sees a window of the run: where the window's edge is not the
/// run's, the text is said to go on (an offset far from 0, a length far past the window), so it asks for
/// more context instead of breaking there, and the window doubles.
fn cluster_step(b: &[u8], pos: usize, forward: bool) -> usize {
    const FAR: usize = 1 << 40;
    let char_step = || {
        if forward {
            next_boundary(b, pos)
        } else {
            prev_boundary(b, pos)
        }
    };
    let mut limit = STEP_WINDOW;
    loop {
        let (ws, start_known, chunk, end_known) = utf8_run(b, pos, limit);
        let base = if start_known { 0 } else { FAR };
        let len = base + chunk.len() + if end_known { 0 } else { FAR };
        let mut gc = GraphemeCursor::new(base + (pos - ws), len, true);
        let step = if forward {
            gc.next_boundary(chunk, base)
        } else {
            gc.prev_boundary(chunk, base)
        };
        match step {
            Ok(Some(r)) => return ws + (r - base),
            // Unreachable: a valid char lies on the stepped side of `pos`. The char step is the safe answer.
            Ok(None) => return char_step(),
            Err(_) if start_known && end_known => return char_step(),
            Err(_) => limit *= 2,
        }
    }
}

pub(crate) fn snap(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    if is_boundary(b, p) {
        p
    } else {
        prev_boundary(b, p)
    }
}

/// The char column of `pos` on the line starting at `start`: how many chars lie before it, a byte that
/// belongs to no valid UTF-8 char counting as one (#1243), as [`at_col`] steps them.
pub(crate) fn col_of(b: &[u8], start: usize, pos: usize) -> usize {
    b[start..pos]
        .utf8_chunks()
        .map(|c| c.valid().chars().count() + c.invalid().len())
        .sum()
}

pub(crate) fn at_col(b: &[u8], start: usize, col: usize) -> usize {
    let end = line_end(b, start);
    let mut i = start;
    for _ in 0..col {
        if i >= end {
            break;
        }
        i = next_boundary(b, i);
    }
    i.min(end)
}

/// The VIRTUAL (display) column of `pos` on its line: the cells before it, a `<Tab>` advancing to the next
/// multiple of `tabstop` and every other cluster taking what the renderer paints it in (a wide char 2, an
/// invalid byte's `<ff>` 4, #1251). `start` is the line start. Used by Virtual Replace (`gR`) to size a
/// tab's on-screen width and by `g CTRL-G`; distinct from [`col_of`], which counts characters.
pub(crate) fn vcol_of(b: &[u8], start: usize, pos: usize, tabstop: Tabs) -> usize {
    let end = pos.min(b.len());
    let le = line_end(b, start);
    let mut v = 0;
    let mut i = start;
    while i < end {
        let (j, w) = crate::editor::cluster_cells(b, i, le.max(end), v, tabstop);
        v += w;
        i = j;
    }
    v
}

/// Whether `p` is a line's NUL slot on Vim's line model: its `\n`, or the end of the buffer. The same as
/// `p >= line_end(b, p)`, without the scan.
pub(crate) fn at_nul(b: &[u8], p: usize) -> bool {
    p >= b.len() || b[p] == b'\n'
}

/// Whether `p` starts its line: the same as `p == line_start(b, p)` for `p <= b.len()`, without the scan.
pub(crate) fn at_bol(b: &[u8], p: usize) -> bool {
    p == 0 || (p <= b.len() && b[p - 1] == b'\n')
}

#[cfg(test)]
mod windowed_grapheme_tests {
    //! The line-windowed `next_grapheme`/`prev_grapheme` must be byte-for-byte equivalent to segmenting the
    //! WHOLE buffer (the previous implementation), including across line breaks and over multi-byte clusters.
    use super::{next_boundary, next_grapheme, prev_boundary, prev_grapheme};
    use unicode_segmentation::GraphemeCursor;

    // The pre-refactor whole-buffer implementations, kept here as the oracle.
    fn next_whole(b: &[u8], pos: usize) -> usize {
        match std::str::from_utf8(b) {
            Ok(s) => {
                let mut gc = GraphemeCursor::new(pos.min(s.len()), s.len(), true);
                gc.next_boundary(s, 0)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| next_boundary(b, pos))
            }
            Err(_) => next_boundary(b, pos),
        }
    }
    fn prev_whole(b: &[u8], pos: usize) -> usize {
        match std::str::from_utf8(b) {
            Ok(s) => {
                let mut gc = GraphemeCursor::new(pos.min(s.len()), s.len(), true);
                gc.prev_boundary(s, 0)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| prev_boundary(b, pos))
            }
            Err(_) => prev_boundary(b, pos),
        }
    }

    fn assert_equivalent(text: &str) {
        let b = text.as_bytes();
        // Real callers only ever pass a char boundary (the cursor is always snapped), which is what
        // `GraphemeCursor` requires — so sweep those positions plus the end.
        let boundaries = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(b.len()));
        for pos in boundaries {
            assert_eq!(
                next_grapheme(b, pos),
                next_whole(b, pos),
                "next_grapheme mismatch at pos {pos} in {text:?}"
            );
            assert_eq!(
                prev_grapheme(b, pos),
                prev_whole(b, pos),
                "prev_grapheme mismatch at pos {pos} in {text:?}"
            );
        }
    }

    #[test]
    fn windowed_matches_whole_buffer_across_lines_and_clusters() {
        assert_equivalent("abc\ndef\nghi"); // plain ASCII, multiple lines
        assert_equivalent("a👨‍👩‍👧b\ncd"); // ZWJ emoji family mid-line, then a line break
        assert_equivalent("é\ncafé\n"); // combining/precomposed + trailing newline
        assert_equivalent("한\n글자\nx"); // multi-byte CJK on several lines
        assert_equivalent(""); // empty buffer
        assert_equivalent("\n\n"); // blank lines only
        assert_equivalent("e\u{0301}x\ny"); // base + combining acute (a real 2-codepoint cluster)
    }

    #[test]
    fn crossing_a_newline_still_steps_one_cluster() {
        // `l` at the last char of a line lands on the newline; a further `l` crosses to the next line start.
        let b = b"ab\ncd";
        assert_eq!(next_grapheme(b, 1), 2, "b → the newline position");
        assert_eq!(next_grapheme(b, 2), 3, "newline → next line start");
        // `h` at a line start crosses back over the newline.
        assert_eq!(prev_grapheme(b, 3), 2, "line start → the newline");
        assert_eq!(prev_grapheme(b, 2), 1, "newline → prev line's last char");
    }
}

#[cfg(test)]
mod steps_tests {
    //! #1224, #1232: [`Steps`] reads a bounded window around a position; its answers must equal segmenting
    //! the whole line into its cells, for any order of queries over any bytes (bytes that are not UTF-8,
    //! positions inside a char, CRLF, combining marks, ZWJ and flag sequences longer than the first window),
    //! and a word walk over a long line must cost the text it crosses.
    use super::{
        char_len_at, inside_char, line_end, line_start, next_boundary, prev_boundary, Steps,
    };
    use crate::motion::{target, Motion};
    use proptest::prelude::*;
    use unicode_segmentation::UnicodeSegmentation;

    /// The reference cells of the line `[ls, le)`: each byte that belongs to no valid char alone, and each
    /// maximal run of valid chars cut into extended grapheme clusters as a string of its own. Returns the
    /// cell starts.
    fn cell_starts(b: &[u8], ls: usize, le: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = ls;
        while i < le {
            if char_len_at(b, i).is_none() {
                out.push(i);
                i += 1;
                continue;
            }
            let mut j = i;
            while j < le {
                match char_len_at(b, j) {
                    Some(w) => j += w,
                    None => break,
                }
            }
            let run = std::str::from_utf8(&b[i..j]).expect("a run of valid chars");
            out.extend(run.grapheme_indices(true).map(|(k, _)| i + k));
            i = j;
        }
        out
    }

    /// The reference `next_grapheme`: the first cell start after `pos` on its line, else the line end;
    /// the char step at a line's end, past the buffer, or inside a char.
    fn next_ref(b: &[u8], pos: usize) -> usize {
        if pos >= b.len() || b[pos] == b'\n' || inside_char(b, pos) {
            return next_boundary(b, pos);
        }
        let (ls, le) = (line_start(b, pos), line_end(b, pos));
        cell_starts(b, ls, le)
            .into_iter()
            .find(|&s| s > pos)
            .unwrap_or(le)
    }

    /// The reference `prev_grapheme`: the last cell start before `pos` on its line; the char step at a
    /// line's start or inside a char.
    fn prev_ref(b: &[u8], pos: usize) -> usize {
        let pos = pos.min(b.len());
        if pos == 0 || b[pos - 1] == b'\n' || inside_char(b, pos) {
            return prev_boundary(b, pos);
        }
        let ls = line_start(b, pos);
        cell_starts(b, ls, pos)
            .into_iter()
            .next_back()
            .expect("a cell before pos")
    }

    /// Text pieces that exercise the segmenter and the fallbacks.
    const PIECES: &[&[u8]] = &[
        b"a",
        b"_",
        b" ",
        b".",
        b"\t",
        b"\n",
        b"\r",
        b"\r\n",
        "e\u{301}".as_bytes(),
        "\u{301}".as_bytes(),
        "가".as_bytes(),
        "👨\u{200d}👩\u{200d}👧".as_bytes(),
        "\u{200d}".as_bytes(),
        "🇰🇷".as_bytes(),
        "🇯".as_bytes(),
        "👍🏽".as_bytes(),
        "\u{915}\u{94d}\u{937}".as_bytes(), // Devanagari conjunct (InCB)
        "\u{600}".as_bytes(),               // a Prepend char: joins the ASCII char after it
        b"\xff",
        b"\x80",
        b"\xe0\x80",
        b"\xea\xb0", // a cut 3-byte char
        b"\xed\xa0\x80", // a surrogate: three bytes that are not UTF-8
        b"\xc0\xaf",     // an overlong `/`
        // Clusters longer than the first window: the segmenter asks for more context.
        "👨\u{200d}👩\u{200d}👧\u{200d}👦👨\u{200d}👩\u{200d}👧\u{200d}👦👨\u{200d}👩\u{200d}👧\u{200d}👦".as_bytes(),
        "a\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}".as_bytes(),
        "🇰🇷🇯🇵🇺🇸🇰🇷🇯🇵🇺🇸🇰🇷🇯🇵🇺🇸".as_bytes(), // nine flags: a prev step counts back over them
    ];

    fn bytes(picks: &[usize]) -> Vec<u8> {
        picks
            .iter()
            .flat_map(|&i| PIECES[i % PIECES.len()].to_vec())
            .collect()
    }

    proptest! {
        #![proptest_config(crate::test_cases(512))]

        /// One `Steps` answers like the whole-line cells at every offset (char boundary or not, and one past the
        /// end), queried in an arbitrary order that jumps between lines and directions.
        #[test]
        fn steps_equal_the_whole_line_cells(
            picks in proptest::collection::vec(0usize..64, 0..40),
            order in proptest::collection::vec((0usize..400, any::<bool>()), 0..80),
        ) {
            let b = bytes(&picks);
            let steps = Steps::new(&b);
            let all = (0..=b.len() + 1).map(|p| (p, true)).chain((0..=b.len() + 1).map(|p| (p, false)));
            let jumps = order.iter().map(|&(p, f)| (p % (b.len() + 2), f));
            for (pos, fwd) in all.chain(jumps) {
                if fwd {
                    prop_assert_eq!(steps.next(pos), next_ref(&b, pos), "next at {} in {:?}", pos, b);
                } else {
                    prop_assert_eq!(steps.prev(pos), prev_ref(&b, pos), "prev at {} in {:?}", pos, b);
                }
                let p = pos.min(b.len());
                prop_assert_eq!(steps.line_start(pos), line_start(&b, p));
                prop_assert_eq!(steps.line_end(pos), line_end(&b, p));
            }
            // A forward and a backward walk, each step from the last landing, as the word loops do.
            let mut p = 0;
            while p < b.len() {
                let q = steps.next(p);
                prop_assert_eq!(q, next_ref(&b, p));
                p = q;
            }
            while p > 0 {
                let q = steps.prev(p);
                prop_assert_eq!(q, prev_ref(&b, p));
                p = q;
            }
        }
    }

    proptest! {
        #![proptest_config(crate::test_cases(2048))]

        /// The hand-rolled UTF-8 char check equals the standard library's on any bytes.
        #[test]
        fn char_len_at_equals_std(b in proptest::collection::vec(any::<u8>(), 1..6)) {
            let head = &b[..b.len().min(4)];
            let std_len = match std::str::from_utf8(head) {
                Ok(s) => s.chars().next().map(char::len_utf8),
                Err(e) if e.valid_up_to() > 0 => {
                    std::str::from_utf8(&head[..e.valid_up_to()]).ok().and_then(|s| s.chars().next()).map(char::len_utf8)
                }
                Err(_) => None,
            };
            prop_assert_eq!(char_len_at(&b, 0), std_len, "{:x?}", b);
        }
    }

    /// #1232, the cells nvim v0.12.6 steps (`l` from the line start, then `h` back from its end; captured
    /// headless, `nvim_win_get_cursor` byte columns): a byte that is not UTF-8 is one cell, and a
    /// combining mark or ZWJ sequence next to it still groups with its own valid text.
    #[test]
    fn invalid_bytes_are_cells_of_their_own() {
        let walk = |b: &[u8]| {
            let s = Steps::new(b);
            let mut fwd = vec![0];
            while *fwd.last().unwrap() < b.len() {
                fwd.push(s.next(*fwd.last().unwrap()));
            }
            let mut back = vec![b.len()];
            while *back.last().unwrap() > 0 {
                back.push(s.prev(*back.last().unwrap()));
            }
            (fwd, back)
        };
        // `é ÿ é` with a decomposed é and a lone 0xFF: l lands on 3, 4, 5, 6.
        let (fwd, back) = walk(b"e\xcc\x81 \xff e\xcc\x81");
        assert_eq!(fwd, [0, 3, 4, 5, 6, 9]);
        assert_eq!(back, [9, 6, 5, 4, 3, 0]);
        // A combining mark after the invalid byte does not join it: `\xff\u{301}a` is three cells.
        assert_eq!(walk(b"\xff\xcc\x81a").0, [0, 1, 3, 4]);
        // A cut char is one cell per byte, and a stray continuation byte is a cell (and a char boundary).
        assert_eq!(walk(b"\xea\xb0a\xcc\x81").0, [0, 1, 2, 5]);
        assert_eq!(walk(b"a\x80\xcc\x81b").0, [0, 1, 2, 4, 5]);
        assert!(super::is_boundary(b"a\x80", 1));
        assert!(!super::is_boundary("é".as_bytes(), 1));
        // A ZWJ family beside an invalid byte stays one cell.
        let fam = "👨\u{200d}👩".as_bytes();
        let b = [fam, b" \xff ", fam].concat();
        assert_eq!(walk(&b).0, [0, 11, 12, 13, 14, 25]);
    }

    /// Word motions over one 512 KiB line cross it in one pass: before #1224 every cluster re-validated the
    /// whole line, so these counts ran for hours; now they finish in well under a second even unoptimized.
    /// The landings are checked against the word count of one repeated unit.
    #[test]
    fn word_walks_over_a_long_line_are_linear() {
        const UNIT: &str = "ab_c, d.e(\"가나\") e\u{301}x ";
        let reps = (512 << 10) / UNIT.len();
        let b = UNIT.repeat(reps).into_bytes();
        let unit = UNIT.len();
        // Words per unit, from a short buffer: the `w` count that lands on the second unit's start.
        let short = UNIT.repeat(3).into_bytes();
        let per = (1..100)
            .find(|&k| target(&short, 0, Motion::WordFwd, k) == unit)
            .expect("the unit is a whole number of words");
        let k = (reps - 2) as u32;
        assert_eq!(target(&b, 0, Motion::WordFwd, per * k), k as usize * unit);
        let back = target(&b, (reps - 1) * unit, Motion::WordBack, per * k);
        assert_eq!(back, unit, "`b` walks back as many units");
        let e = target(&b, 0, Motion::WordEnd, per * k);
        assert!(
            e > (k as usize - 1) * unit && e < k as usize * unit,
            "`e` ends in unit {k}"
        );
        let ge = target(&b, (reps - 1) * unit, Motion::WordEndBack, per * k);
        assert!(ge > unit && ge < 2 * unit, "`ge` ends in the second unit");
        // A hostile `l` count walks the line once and stops at its edge (the caller clamps it onto a char).
        assert_eq!(target(&b, 0, Motion::Right, u32::MAX), b.len());
    }
}
