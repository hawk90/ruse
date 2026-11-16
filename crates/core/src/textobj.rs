//! Vim's character-wise text objects — `iw`/`aw`/`iW`/`aW`, `is`/`as`, the bracket blocks `i(`/`a(` (and
//! `{`/`[`/`<`), and the quotes `i"`/`a"` (and `'`/`` ` ``) — for both operator-pending and Visual mode.
//!
//! Each object is a port of the corresponding Neovim v0.12 routine (`current_word`, `current_sent`,
//! `current_block`, `current_quote` in `textobject.c`) onto ruse's flat byte buffer. The port keeps Vim's
//! cursor model rather than re-deriving the geometry, because the corner cases (#740, #751, #752, #753,
//! #703) all live in how Vim's position primitives step over a line break:
//!
//! - A position is a byte offset. A line's `NUL` slot (Vim's `col == STRLEN`) is the offset of its `\n`,
//!   or the buffer's content end for the last line. The terminating `\n` of a newline-ended buffer is the
//!   last line's terminator, not a phantom empty line (see [`motion::content_end`]).
//! - [`Buf::inc`] / [`Buf::dec`] / [`Buf::incl`] / [`Buf::decl`] are Vim's `inc`/`dec`/`incl`/`decl`,
//!   with the same return codes (`0` moved within the line, `2` landed on the `NUL`, `1` crossed a line,
//!   `-1` at a buffer edge).
//! - Character classes are Vim's (`cls()` over [`crate::charclass::vim_class`], #1136), the same ones the
//!   `w`/`b`/`e` motions use, and a step crosses one grapheme cluster, as nvim's `inc()`/`dec()` do.
//!
//! An operator result is Vim's `(oap->start, cursor, oap->inclusive)`; [`op_span`] turns it into a byte
//! range and applies the exclusive-linewise rule. A Visual result is the new `(VIsual, cursor)` pair.
//! Bracket matching ignores quotes, as Vim's text objects do (`cpo` is forced to `%` there), but skips a
//! bracket escaped by an odd number of backslashes. Quote objects honour the default `quoteescape` (`\`).

use crate::motion::{self, Motion};

mod block;
mod quote;
mod sentence;
mod word;

use block::current_block;
use quote::current_quote;
use sentence::current_sent;
pub(crate) use sentence::{sentence_motion, sentence_motion_fails};
use word::{current_word, current_word_at};
/// The buffer seen through Vim's position primitives. `last` is the `NUL` slot of the last line.
/// Steps go through one [`motion::Steps`], so a walk finds and validates each line once, not once per
/// cluster (#1224).
struct Buf<'a> {
    b: &'a [u8],
    last: usize,
    steps: motion::Steps<'a>,
}

impl<'a> Buf<'a> {
    fn new(b: &'a [u8]) -> Self {
        Buf {
            b,
            last: motion::content_end(b),
            steps: motion::Steps::new(b),
        }
    }

    /// Whether `p` is a line's `NUL` slot (its newline, or the end of the last line).
    fn is_nul(&self, p: usize) -> bool {
        p >= self.last || self.b[p] == b'\n'
    }

    /// The byte under `p`, `0` on a `NUL` slot. Non-ASCII chars report their lead byte (`>= 0x80`), which is
    /// never one of the ASCII delimiters or blanks the objects test for.
    fn gchar(&self, p: usize) -> u8 {
        if self.is_nul(p) {
            0
        } else {
            self.b[p]
        }
    }

    fn col0(&self, p: usize) -> bool {
        p == 0 || self.b[p - 1] == b'\n'
    }

    fn line_start(&self, p: usize) -> usize {
        self.steps.line_start(p)
    }

    /// `p` sits at column 0 of an empty line.
    fn line_empty(&self, p: usize) -> bool {
        self.col0(p) && self.is_nul(p)
    }

    /// Whether the line holding `p` is an empty line (Vim `LINEEMPTY(lnum)`).
    fn line_of_is_empty(&self, p: usize) -> bool {
        self.line_empty(self.line_start(p))
    }

    fn is_last_line(&self, p: usize) -> bool {
        self.steps.line_end(p) >= self.last
    }

    /// Vim `inc`: one char forward; `2` when that lands on the `NUL`, `1` when it crosses into the next
    /// line, `-1` (no move) on the last line's `NUL`.
    fn inc(&self, p: &mut usize) -> i32 {
        if !self.is_nul(*p) {
            let q = self.steps.next(*p);
            *p = q;
            return if self.is_nul(q) { 2 } else { 0 };
        }
        if *p >= self.last {
            return -1;
        }
        *p += 1;
        1
    }

    /// Vim `dec`: one char back; at column 0 it moves to the previous line's `NUL` (`1`), `-1` at offset 0.
    fn dec(&self, p: &mut usize) -> i32 {
        if !self.col0(*p) {
            *p = self.steps.prev(*p);
            return 0;
        }
        if *p == 0 {
            return -1;
        }
        *p -= 1;
        1
    }

    /// Vim `incl`: [`Buf::inc`], skipping over a `NUL` it lands on.
    fn incl(&self, p: &mut usize) -> i32 {
        let r = self.inc(p);
        if r >= 1 && !self.col0(*p) {
            return self.inc(p);
        }
        r
    }

    /// Vim `decl`: [`Buf::dec`], skipping over the `NUL` of the line it crosses back into.
    fn decl(&self, p: &mut usize) -> i32 {
        let r = self.dec(p);
        if r == 1 && !self.col0(*p) {
            return self.dec(p);
        }
        r
    }

    /// Vim `oneleft`: one char left within the line; `false` at column 0.
    fn oneleft(&self, p: &mut usize) -> bool {
        if self.col0(*p) {
            return false;
        }
        *p = self.steps.prev(*p);
        true
    }

    /// Vim `inindent(extra)`: the line's leading blanks reach at least `col(p) + extra`.
    fn inindent(&self, p: usize, extra: usize) -> bool {
        let ls = self.line_start(p);
        let mut i = ls;
        while i < self.b.len() && is_white(self.b[i]) {
            i += 1;
        }
        i - ls >= p - ls + extra
    }

    /// Vim `cls()`: `0` for a blank or `NUL`, else the char's Vim class; for WORD objects (`big`) every
    /// non-blank is class `1`.
    fn cls(&self, p: usize, big: bool) -> u32 {
        if self.is_nul(p) {
            0
        } else {
            motion::char_cls(self.b, p, big)
        }
    }
}

fn is_white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

// --- entry points --------------------------------------------------------------------------------------

fn delim(c: char) -> bool {
    c.is_ascii_graphic()
}

/// Dispatch a text-object motion to its Vim routine. `None` for a motion this module does not own
/// (paragraphs and tags live in `motion`) or an object that fails (Vim beeps and does nothing).
fn object(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    m: Motion,
    count: u32,
) -> Option<(usize, usize, bool)> {
    match m {
        Motion::InnerWord => current_word(buf, cursor, visual, count, false, false),
        Motion::AWord => current_word(buf, cursor, visual, count, true, false),
        Motion::InnerBigWord => current_word(buf, cursor, visual, count, false, true),
        Motion::ABigWord => current_word(buf, cursor, visual, count, true, true),
        Motion::InnerSentence => current_sent(buf, cursor, visual, count, false),
        Motion::ASentence => current_sent(buf, cursor, visual, count, true),
        // Delimiters are printable ASCII (the input engine only produces those); a non-ASCII, NUL or
        // newline delimiter is no object — it would alias the `NUL` slot or a line break.
        Motion::Pair {
            open,
            close,
            around,
        } if delim(open) && delim(close) => {
            current_block(buf, cursor, visual, count, around, open as u8, close as u8)
        }
        Motion::Quote { ch, around } if delim(ch) => {
            current_quote(buf, cursor, visual, count, around, ch as u8)
        }
        _ => None,
    }
}

/// Where a failed word object (`aw`/`iw`/`aW`/`iW` under an operator, or in Visual with the anchor
/// `visual`) leaves the cursor, or `None` when it does not fail (#1089). In Visual the selection then
/// reaches that spot, as Vim keeps the moved cursor: `vaw` on a blank line selects to its end (#1111).
pub(crate) fn word_object_fails(
    b: &[u8],
    cursor: usize,
    visual: Option<usize>,
    m: Motion,
    count: u32,
) -> Option<usize> {
    let (include, big) = match m {
        Motion::InnerWord => (false, false),
        Motion::AWord => (true, false),
        Motion::InnerBigWord => (false, true),
        Motion::ABigWord => (true, true),
        _ => return None,
    };
    let buf = Buf::new(b);
    let cur = cursor.min(b.len()).min(buf.last);
    let visual = visual.map(|v| v.min(buf.last));
    current_word_at(&buf, cur, visual, count, include, big).err()
}

/// Whether a block, quote or sentence object (`i(`, `a"`, `is`) finds nothing to select from `cursor` under
/// an operator, where Vim beeps and cancels the operator (#1179). Word objects fail through
/// [`word_object_fails`], which also places the cursor.
pub(crate) fn object_fails(b: &[u8], cursor: usize, m: Motion, count: u32) -> bool {
    if !matches!(
        m,
        Motion::Pair { .. } | Motion::Quote { .. } | Motion::InnerSentence | Motion::ASentence
    ) {
        return false;
    }
    let buf = Buf::new(b);
    let cur = cursor.min(b.len()).min(buf.last);
    object(&buf, cur, None, m, count).is_none()
}

/// Whether `m` is one of the objects this module computes.
pub(crate) fn owns(m: Motion) -> bool {
    matches!(
        m,
        Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::Pair { .. }
            | Motion::Quote { .. }
    )
}

/// The operator-pending object as Vim leaves it: a byte range `[s, e)` and whether it is EXCLUSIVE with
/// its end at a line start (the case Vim's exclusive-linewise rule reshapes). A failed object is the empty
/// range at the cursor; an object that selects nothing (`di(` on `()`) is the empty range at its start.
pub(crate) fn op_range(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    let cur = cursor.min(b.len());
    let buf = Buf::new(b);
    let cur = cur.min(buf.last);
    let Some((start, end, inclusive)) = object(&buf, cur, None, m, count) else {
        return (cur, cur, false);
    };
    let e = if inclusive && !buf.is_nul(end) {
        motion::next_grapheme(b, end)
    } else {
        end
    };
    if e <= start {
        return (start, start, false);
    }
    (start, e, !inclusive && buf.col0(e))
}

/// The operator span of an object: [`op_range`] reduced by the exclusive-linewise rule (an exclusive span
/// ending at a line start drops that line break, and goes linewise when it starts in the indent). Returns
/// `(s, e, linewise)`. The editor's operator path computes the same through `editor::op_span_full`.
#[cfg(test)]
pub(crate) fn op_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    let (s, e, exclusive_at_col0) = op_range(b, cursor, m, count);
    if exclusive_at_col0 {
        crate::editor::exclusive_linewise(b, s, e)
    } else {
        (s, e, false)
    }
}

/// Vi's `d` quirk (nvim `op_delete`): a charwise delete `[s, e)` spanning several lines, that starts in its
/// line's indent and leaves only blanks after its end, deletes whole lines instead. Returns `(s, e,
/// linewise)`; not applied to Visual deletes, `c` or `y`.
pub(crate) fn delete_reshape(b: &[u8], s: usize, e: usize) -> (usize, usize, bool) {
    let buf = Buf::new(b);
    if s >= e || e > buf.last || buf.line_start(e) == buf.line_start(s) {
        return (s, e, false);
    }
    let mut i = e;
    while !buf.is_nul(i) && is_white(b[i]) {
        i += 1;
    }
    if !buf.is_nul(i) || !buf.inindent(s, 0) {
        return (s, e, false);
    }
    let le = motion::line_end(b, e);
    (
        buf.line_start(s),
        if le < b.len() { le + 1 } else { le },
        true,
    )
}

/// The object applied to an active Visual selection (`anchor`..`cursor`): the new `(anchor, cursor)`, the
/// cursor on the last selected char. A repeated object extends the area (`viwiw`, `va(a(`, `visis`).
/// `None` when the object fails (the selection is left alone).
pub(crate) fn visual(
    b: &[u8],
    anchor: usize,
    cursor: usize,
    m: Motion,
    count: u32,
) -> Option<(usize, usize)> {
    let buf = Buf::new(b);
    let anchor = anchor.min(buf.last);
    let cursor = cursor.min(buf.last);
    let (a, c, _) = object(&buf, cursor, Some(anchor), m, count)?;
    Some((a, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    fn quote(ch: char, around: bool) -> Motion {
        Motion::Quote { ch, around }
    }

    /// The text an operator over `m` would take (`d`/`y` register content), after the
    /// exclusive-linewise rule, plus whether it is linewise.
    fn take(b: &str, cur: usize, m: Motion, count: u32) -> (String, bool) {
        let (s, e, line) = op_span(b.as_bytes(), cur, m, count);
        (b[s..e].to_string(), line)
    }

    fn sel(b: &str, anchor: usize, cur: usize, m: Motion, count: u32) -> Option<String> {
        let (a, c) = visual(b.as_bytes(), anchor, cur, m, count)?;
        let (lo, hi) = (a.min(c), a.max(c));
        let hi = if hi < b.len() {
            motion::next_boundary(b.as_bytes(), hi)
        } else {
            hi
        };
        Some(b[lo..hi].to_string())
    }

    #[test]
    fn words_stop_at_the_line_break() {
        let iw = Motion::InnerWord;
        let aw = Motion::AWord;
        // #740: trailing blanks, empty line, last word on a line, only word on a line.
        assert_eq!(take("foo  \n  bar", 4, iw, 1), ("  ".into(), false));
        assert_eq!(take("a\n\nb", 2, iw, 1), ("".into(), false));
        assert_eq!(take("foo bar\nbaz", 4, aw, 1), (" bar".into(), false));
        assert_eq!(take("foo\nbar", 0, aw, 1), ("foo".into(), false));
        assert_eq!(take("  foo\nbar", 2, aw, 1), ("foo".into(), false));
        assert_eq!(take("foo  \nbar", 4, aw, 1), ("  \nbar".into(), false));
        // Counts do cross lines, but never take the next line's first char by accident.
        assert_eq!(
            take("foo bar\nbaz qux", 4, iw, 2),
            ("bar\nbaz".into(), false)
        );
        assert_eq!(
            take("foo bar\nbaz qux", 4, iw, 3),
            ("bar\nbaz ".into(), false)
        );
        assert_eq!(
            take("foo bar\nbaz qux", 4, aw, 2),
            ("bar\nbaz ".into(), false)
        );
        assert_eq!(
            take("foo\n\nbar baz", 0, iw, 4),
            ("foo\n\nbar ".into(), false)
        );
        // Multibyte words; the empty buffer.
        assert_eq!(take("한글 abc\ndef", 0, aw, 1), ("한글 ".into(), false));
        assert_eq!(take("abc 한글\ndef", 4, aw, 1), (" 한글".into(), false));
        // Vim classes split a Latin word from a Hangul particle; a cluster keeps its marks (#1136).
        assert_eq!(take("API를 x", 0, iw, 1), ("API".into(), false));
        assert_eq!(take("API를 x", 3, iw, 1), ("를".into(), false));
        assert_eq!(take("x API를 y", 2, aw, 1), (" API".into(), false));
        assert_eq!(take("x 가\u{301}b", 7, iw, 1), ("b".into(), false));
        assert_eq!(take("x 가\u{301}b", 2, iw, 1), ("가\u{301}".into(), false));
        assert_eq!(take("", 0, iw, 1), ("".into(), false));
        assert_eq!(take("", 0, aw, 1), ("".into(), false));
        // A newline-terminated buffer: the final `\n` is the last line's terminator.
        assert_eq!(take("foo bar\n", 6, aw, 1), (" bar".into(), false));
    }

    #[test]
    fn repeated_word_objects_extend_a_visual_area() {
        let b = "foo bar baz";
        // `viw` then `iw`: the anchor stays, the cursor grows by one word / blank run each time.
        assert_eq!(sel(b, 0, 2, Motion::InnerWord, 1).as_deref(), Some("foo "));
        assert_eq!(
            sel(b, 0, 3, Motion::InnerWord, 1).as_deref(),
            Some("foo bar")
        );
        assert_eq!(
            sel(b, 0, 0, Motion::InnerWord, 3).as_deref(),
            Some("foo bar")
        );
        assert_eq!(sel(b, 0, 1, Motion::InnerWord, 1).as_deref(), Some("foo"));
        // Cursor before the anchor: extend backward.
        assert_eq!(sel(b, 10, 9, Motion::InnerWord, 1).as_deref(), Some("baz"));
        assert_eq!(sel(b, 10, 8, Motion::InnerWord, 1).as_deref(), Some(" baz"));
    }

    #[test]
    fn quotes_honour_backslash_escapes() {
        let i = quote('"', false);
        assert_eq!(take(r#"a"b\"c"d"#, 0, i, 1).0, r#"b\"c"#);
        assert_eq!(take(r#"a"b\"c"d"#, 4, i, 1).0, r#"b\"c"#);
        assert_eq!(take(r#""a\"b" "c""#, 9, i, 1).0, "c");
        assert_eq!(
            take(r#"x "a\"b" y"#, 0, quote('"', true), 1).0,
            r#""a\"b" "#
        );
        assert_eq!(take(r#""a\\" b "c""#, 0, i, 1).0, r"a\\");
        assert_eq!(take(r#"x "한\"글" y"#, 0, i, 1).0, r#"한\"글"#);
        assert_eq!(take(r#""a" x "b""#, 4, i, 1).0, " x ");
        assert_eq!(take(r#"x "ab" y"#, 0, i, 2).0, r#""ab""#);
        assert_eq!(take("no quotes", 3, i, 1), ("".into(), false));
        // `vi"` then `i"` takes the quotes.
        assert_eq!(sel(r#"x "ab" y"#, 3, 4, i, 1).as_deref(), Some(r#""ab""#));
    }

    #[test]
    fn sentences_pick_the_sentence_or_gap_under_the_cursor() {
        let is = Motion::InnerSentence;
        let as_ = Motion::ASentence;
        assert_eq!(take("Hi. Bye.", 3, is, 1).0, " ");
        assert_eq!(take("Hi. Bye.", 3, as_, 1).0, " Bye.");
        assert_eq!(take("Hi there. Bye.", 13, as_, 1).0, " Bye.");
        assert_eq!(take("Hi there. Bye.", 0, as_, 1).0, "Hi there. ");
        assert_eq!(take("A b. C d. E f.", 0, is, 2).0, "A b. ");
        assert_eq!(take("A b. C d. E f.", 0, as_, 2).0, "A b. C d. ");
        assert_eq!(take("가나. 다라. 마바.", 8, as_, 1).0, "다라. ");
        // A sentence ending its line is linewise under an operator (exclusive end at the next line start).
        assert_eq!(take("Hi.\nBye.", 0, is, 1), ("Hi.\n".into(), true));
        assert_eq!(take("", 0, is, 1), ("".into(), false));
    }

    #[test]
    fn block_counts_climb_outward_and_search_forward() {
        let i = pair('(', ')', false);
        assert_eq!(take("((a))", 2, i, 2).0, "(a)");
        assert_eq!(take("((a))", 0, i, 2).0, "");
        assert_eq!(take("((a))", 2, i, 3).0, "");
        assert_eq!(take("x((a)b)y", 3, pair('(', ')', true), 2).0, "((a)b)");
        assert_eq!(take("foo\n(bar)", 0, i, 1).0, "bar");
        assert_eq!(take("(a(b)c)", 4, i, 1).0, "b");
        assert_eq!(take("(한(글)x)", 7, i, 2).0, "한(글)x");
        assert_eq!(take(r"(a \) b)", 1, i, 1).0, r"a \) b");
        // Visual: a repeated `a(` grows to the enclosing block; `i(` too once the area is the whole inner.
        assert_eq!(
            sel("x((a) b)y", 3, 3, pair('(', ')', true), 1).as_deref(),
            Some("(a)")
        );
        assert_eq!(
            sel("x((a) b)y", 2, 4, pair('(', ')', true), 1).as_deref(),
            Some("((a) b)")
        );
        assert_eq!(sel("((ab))", 2, 3, i, 1).as_deref(), Some("(ab)"));
        // No enclosing block left to grow into: the object fails and the selection stays. (nvim then beeps
        // and flushes typeahead, which is why this is a unit test rather than an oracle fixture.)
        assert_eq!(sel("(x(ab)y)", 1, 6, i, 1), None);
    }

    fn objects() -> Vec<Motion> {
        let mut v = vec![
            Motion::InnerWord,
            Motion::AWord,
            Motion::InnerBigWord,
            Motion::ABigWord,
            Motion::InnerSentence,
            Motion::ASentence,
        ];
        for (o, c) in [('(', ')'), ('{', '}'), ('[', ']'), ('<', '>')] {
            v.push(pair(o, c, false));
            v.push(pair(o, c, true));
        }
        for q in ['"', '\'', '`'] {
            v.push(quote(q, false));
            v.push(quote(q, true));
        }
        v
    }

    #[test]
    fn hostile_counts_terminate() {
        let b = "Hi. Bye.\n\n(a (b)) \"q\" w";
        for m in objects() {
            for cur in [0, 4, 11, b.len()] {
                let _ = op_span(b.as_bytes(), cur, m, u32::MAX);
                let _ = visual(b.as_bytes(), 0, cur, m, u32::MAX);
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// Every object, from every cursor, with any count, in operator-pending and Visual mode, yields
        /// in-bounds ranges on char boundaries (the buffer mixes blanks, line breaks, sentence and bracket
        /// punctuation, escapes and multibyte text) — and never spins.
        #[test]
        fn object_ranges_stay_in_bounds_on_char_boundaries(
            text in "[a .!?\"'`()\\[\\]{}<>\\\\\t\n한]{0,24}",
            count in 1u32..4,
        ) {
            let b = text.as_bytes();
            let ok = |p: usize| p <= b.len() && motion::is_boundary(b, p);
            for m in objects() {
                for cur in (0..=b.len()).filter(|&p| ok(p)) {
                    let (s, e, _) = op_span(b, cur, m, count);
                    proptest::prop_assert!(s <= e && ok(s) && ok(e), "{m:?} at {cur}: {s}..{e}");
                    let (s, e) = motion::char_span(b, cur, m, count);
                    proptest::prop_assert!(s <= e && ok(s) && ok(e), "{m:?} at {cur}: {s}..{e}");
                    for anchor in [cur, 0, b.len()] {
                        if let Some((a, c)) = visual(b, anchor, cur, m, count) {
                            proptest::prop_assert!(ok(a) && ok(c), "{m:?} visual {anchor}..{cur}: {a}..{c}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn indented_closer_keeps_its_line() {
        let i = pair('{', '}', false);
        // #703: a closer preceded only by indent leaves that indent (and its line) alone.
        let b = "    if x {\n        foo\n    }";
        assert_eq!(take(b, 12, i, 1), ("        foo\n".into(), true));
        assert_eq!(take("{\n\tfoo\n\t}", 2, i, 1), ("\tfoo\n".into(), true));
        assert_eq!(
            take("{ foo\n  bar\n  }", 7, i, 1),
            (" foo\n  bar".into(), false)
        );
        assert_eq!(take("{  \nfoo\n}", 4, i, 1), ("  \nfoo".into(), false));
        assert_eq!(take("{\n  foo }", 2, i, 1), ("  foo ".into(), false));
        assert_eq!(
            take("(\n)", 0, pair('(', ')', false), 1),
            ("".into(), false)
        );
        assert_eq!(
            op_span(b"(\n)", 0, pair('(', ')', false), 1).0,
            2,
            "empty inner sits after the break"
        );
        // Visual `i{` is charwise and takes the last inner line break.
        assert_eq!(sel(b, 12, 12, i, 1).as_deref(), Some("        foo\n"));
    }
}
