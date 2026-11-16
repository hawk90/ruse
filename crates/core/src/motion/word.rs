//! Vim small-word / WORD stepping (`w` `b` `e` `ge`) over Vim's line model, and Emacs word stepping.

use super::boundary::{at_bol, at_nul, Steps};
use super::line::content_end;

fn is_ws(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n'
}

/// The Vim character class of a byte for word motions. Non-ASCII bytes (`>= 0x80`) count as `Word` so a
/// multibyte identifier (e.g. Hangul) is one word; class changes only ever fall on char boundaries.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    Space,
    Word,
    Punct,
}

pub(super) fn class(c: u8) -> Class {
    if is_ws(c) {
        Class::Space
    } else if c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 {
        Class::Word
    } else {
        Class::Punct
    }
}

// Vim's word motions walk a line model in which every line ends in a NUL slot, which is the `\n` byte here
// (or the buffer end of an unterminated last line); the phantom slot after a final `\n` is no line. The
// helpers below mirror `inc()`/`dec()`/`cls()` of Vim's search.c over that model, so an empty line is a
// stop of its own (#746) and an operator's last word stops at its line end (#742).

/// Vim `cls()` of the char at byte `p`: 0 for a blank (a line break and the buffer end included), else
/// its Vim class ([`crate::charclass::vim_class`]: 1 punctuation, 2 word, 3 emoji, a per-script value), or 1
/// for any non-blank when `big`. A combining mark or the rest of a cluster is never asked: callers step by
/// grapheme cluster, so `p` is a cluster's first char. A byte that is not UTF-8 has the class of the
/// Latin-1 char of its value ([`crate::charclass::cell_class`], #1243).
pub(crate) fn char_cls(b: &[u8], p: usize, big: bool) -> u32 {
    use crate::charclass::{vim_class, WORD};
    let Some(&lead) = b.get(p) else {
        return 0;
    };
    let class = if lead < 0x80 {
        vim_class(char::from(lead))
    } else {
        crate::charclass::cell_class(b, p).unwrap_or(WORD)
    };
    match class {
        _ if lead == b'\n' => 0,
        0 => 0,
        _ if big => 1,
        k => k,
    }
}

// The word walks below step through one [`Steps`], so a cluster step reads only the text it crosses
// (#1224), and they tell a line's ends by its bytes (a `\n`, the buffer's ends) instead of finding the
// line, so a walk costs what it crosses (#1231).

/// Vim `cls()` at `p` on Vim's line model: a line's NUL slot is blank.
pub(super) fn vcls(s: &Steps, p: usize, big: bool) -> u32 {
    if at_nul(s.b, p) {
        return 0;
    }
    char_cls(s.b, p, big)
}

/// Whether `p` is on an empty line (Vim `col == 0 && LINEEMPTY`).
pub(super) fn on_empty_line(s: &Steps, p: usize) -> bool {
    at_bol(s.b, p) && at_nul(s.b, p)
}

/// Vim `inc()`: the next position and how it got there (0 within the line, 2 onto the line's NUL slot, 1 to
/// the next line's start), or `None` from the last line's NUL slot.
pub(super) fn vinc(s: &Steps, p: usize) -> Option<(usize, u8)> {
    if !at_nul(s.b, p) {
        // A cluster never crosses the `\n`, so the step lands inside the line or on its NUL slot.
        let q = s.next(p);
        Some((q, if at_nul(s.b, q) { 2 } else { 0 }))
    } else if p + 1 < s.b.len() {
        Some((p + 1, 1))
    } else {
        None
    }
}

/// Vim `dec()`: the previous position (0 within the line, 1 back onto the previous line's NUL slot), or
/// `None` at the buffer start.
pub(super) fn vdec(s: &Steps, p: usize) -> Option<(usize, u8)> {
    // `p > line_start(b, p)`: `line_start` clamps `p` to the buffer, so past its end that holds too.
    if p > 0 && (p > s.b.len() || s.b[p - 1] != b'\n') {
        Some((s.prev(p), 0))
    } else if p > 0 {
        Some((p - 1, 1))
    } else {
        None
    }
}

/// Vim `fwd_word` (`w` / `W`), `count` times. `eol` is set under an operator: the LAST word moved over then
/// stops at its line's NUL slot instead of crossing to the next line ("the end of that word becomes the end
/// of the operated text", `:help word`), while earlier counts cross freely (`3dw` takes three words over a
/// line break, #742). Skipping white space stops on an empty line, which counts as a word (#746). A landing
/// on a NUL slot is the end of the motion; a bare move pulls it back onto the last char.
pub(super) fn word_fwd(s: &Steps, pos: usize, big: bool, count: u32, eol: bool) -> usize {
    let b = s.b;
    let mut p = pos.min(b.len());
    for left in (0..count.max(1)).rev() {
        let stop = |i: u8| i >= 1 && eol && left == 0;
        let sclass = vcls(s, p, big);
        let Some((q, i)) = vinc(s, p) else {
            return p;
        };
        // Whether `p` was on the last line (`line_end(b, p) + 1 >= b.len()`), asked only once `inc` left
        // the line's text, which tells where the line ends without a scan: onto the NUL slot (`i == 2`)
        // it is `q`; from the NUL slot (`i == 1`) it is `p`.
        let last_line = |le: usize| le + 1 >= b.len();
        let crossed = match i {
            2 => last_line(q),
            1 => last_line(p),
            _ => false,
        };
        p = q;
        if crossed || stop(i) {
            return p;
        }
        if sclass != 0 {
            while vcls(s, p, big) == sclass {
                let Some((q, i)) = vinc(s, p) else {
                    return p;
                };
                p = q;
                if stop(i) {
                    return p;
                }
            }
        }
        while vcls(s, p, big) == 0 {
            if on_empty_line(s, p) {
                break;
            }
            let Some((q, i)) = vinc(s, p) else {
                return p;
            };
            p = q;
            if stop(i) {
                return p;
            }
        }
    }
    p
}

/// Start of the next (small-)word / WORD — a bare Vim `w` / `W` step.
pub(super) fn next_word_start(s: &Steps, pos: usize, big: bool) -> usize {
    word_fwd(s, pos, big, 1, false)
}

/// Start of the current/previous word / WORD — Vim `b` / `B` (`bck_word`). An empty line is a stop (#746).
pub(super) fn prev_word_start(s: &Steps, pos: usize, big: bool) -> usize {
    let Some((mut p, _)) = vdec(s, pos.min(s.b.len())) else {
        return 0;
    };
    // Skip the white space before the word; an empty line is a word of its own.
    while vcls(s, p, big) == 0 {
        if on_empty_line(s, p) {
            return p;
        }
        match vdec(s, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    // Back to the start of this word, then one forward over the overshoot.
    let c = vcls(s, p, big);
    while vcls(s, p, big) == c {
        match vdec(s, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    vinc(s, p).map_or(p, |(q, _)| q)
}

/// The landing of Vim `ge` / `gE` (`bckend_word`): the last char of the previous word, or an empty line
/// on the way (#746). Stays at the buffer start when there is no earlier word.
pub(super) fn prev_word_end(s: &Steps, pos: usize, big: bool) -> usize {
    let mut p = pos.min(s.b.len());
    let sclass = vcls(s, p, big);
    let Some((q, _)) = vdec(s, p) else {
        return p;
    };
    p = q;
    if sclass != 0 {
        while vcls(s, p, big) == sclass {
            match vdec(s, p) {
                Some((q, _)) => p = q,
                None => return p,
            }
        }
    }
    while vcls(s, p, big) == 0 {
        if on_empty_line(s, p) {
            break;
        }
        match vdec(s, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    p
}

/// One past the last byte of the current/next word / WORD (the exclusive end for `de` / `dE`). The scan
/// starts at the NEXT character, not the next byte: on a multibyte char the next byte is a continuation byte
/// of the same char, which made `e` re-find the word it sits at the end of (#741); and on a base char with
/// combining marks the next char is a mark of the same cell, which did the same (#909).
pub(crate) fn word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
    word_end_in(&Steps::new(b), pos, big)
}

/// [`word_end_excl`] through a caller's [`Steps`].
pub(super) fn word_end_in(s: &Steps, pos: usize, big: bool) -> usize {
    let b = s.b;
    let mut i = s.next(pos.min(b.len()));
    while i < b.len() && char_cls(b, i, big) == 0 {
        i = s.next(i);
    }
    // No word ahead: the end is the last line's end, before its newline (`3e` past the last word stays on
    // its last char, and `c3w` keeps the final newline, RFC-0024).
    if i >= b.len() {
        return content_end(b);
    }
    run_end(s, i, big)
}

/// One past the run of clusters from `i` that share the class of the one at `i`.
fn run_end(s: &Steps, mut i: usize, big: bool) -> usize {
    let b = s.b;
    let cw = char_cls(b, i, big);
    while i < b.len() && char_cls(b, i, big) == cw {
        i = s.next(i);
    }
    i
}

/// Whether a byte is an Emacs word constituent in fundamental-mode: ASCII alphanumerics, plus any non-ASCII
/// byte (a multibyte letter is a word char in Emacs too). Crucially `_`, punctuation, and whitespace are all
/// NON-word — this is the two-class split that distinguishes Emacs `forward-word`/`backward-word` from Vim's
/// three-class small-word motions (where punctuation is its own class and `_` joins the word).
fn is_emacs_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c >= 0x80
}

/// Emacs `forward-word` from `pos`: skip a leading run of non-word bytes, then consume the run of word bytes,
/// landing one past the last (Emacs point is between-character). This is the span end for `kill-word` too.
/// Crosses newlines freely — a newline is just another non-word byte, exactly as Emacs `forward-word` does.
pub(super) fn emacs_word_fwd(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i < b.len() && !is_emacs_word(b[i]) {
        i += 1;
    }
    while i < b.len() && is_emacs_word(b[i]) {
        i += 1;
    }
    i
}

/// Emacs `backward-word` from `pos`: the mirror of [`emacs_word_fwd`] — skip a trailing run of non-word bytes
/// going left, then consume the run of word bytes, landing on the word's first byte.
pub(super) fn emacs_word_back(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i > 0 && !is_emacs_word(b[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_emacs_word(b[i - 1]) {
        i -= 1;
    }
    i
}

/// The exclusive end of the word the cursor is ON (Vim `cw`'s target when the cursor is on a non-blank).
/// Unlike [`word_end_excl`] / the `e` motion — which, from a word's LAST char, skips forward to the NEXT
/// word's end — this consumes only the current run of same-class chars from `pos`, so `cw` on the last char
/// of a word changes just that char (Vim: "cw changes up to the end of the word"). Returns `pos` unchanged
/// (caller treats it as no-op / falls back) when `pos` is on whitespace or past the buffer.
pub(crate) fn current_word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
    if pos >= b.len() {
        return b.len();
    }
    if char_cls(b, pos, big) == 0 {
        return pos; // on blank: not the cw special case — caller uses the ordinary word span
    }
    run_end(&Steps::new(b), pos, big)
}

/// The Vim class of the grapheme cluster at `p` (its first char's, a byte that is not UTF-8 by its
/// Latin-1 char, #1243), 0 for a line break or the buffer end.
pub(super) fn cluster_class(b: &[u8], p: usize) -> u32 {
    if at_nul(b, p) {
        return 0;
    }
    crate::charclass::cell_class(b, p).unwrap_or(0)
}

#[cfg(test)]
mod word_class_tests {
    //! Word motions split where Vim's character class changes and step by grapheme cluster (#1136); the
    //! byte offsets are nvim 0.12.6's (`wcls_*` oracle fixtures).
    use crate::motion::{target, Motion};

    #[test]
    fn small_words_split_on_class_changes() {
        let b = "x API를 y".as_bytes();
        assert_eq!(target(b, 2, Motion::WordFwd, 1), 5); // "API" | "를"
        assert_eq!(target(b, 2, Motion::WordEnd, 1), 4);
        assert_eq!(target(b, 9, Motion::WordBack, 1), 5);
        assert_eq!(target(b, 9, Motion::WordEndBack, 1), 5);
        assert_eq!(target(b, 2, Motion::BigWordFwd, 1), 9);
        let s = "abc한글か漢字カナ x".as_bytes();
        assert_eq!(target(s, 0, Motion::WordFwd, 4), 18); // past abc, 한글, か, 漢字
        assert_eq!(target("😀😀a b".as_bytes(), 0, Motion::WordFwd, 1), 8);
        assert_eq!(target("a\u{a0}b c".as_bytes(), 0, Motion::WordFwd, 1), 3); // NBSP is blank
        assert_eq!(target("a×b c".as_bytes(), 0, Motion::WordFwd, 1), 5); // Latin-1 × is a word char
    }

    #[test]
    fn a_cluster_takes_its_first_chars_class() {
        // 가 + U+0301 is one cluster of the Hangul class, so `w` does not stop on the mark.
        assert_eq!(target("가\u{301}b c".as_bytes(), 0, Motion::WordFwd, 1), 5);
        assert_eq!(target("x 가\u{301}b".as_bytes(), 7, Motion::WordBack, 1), 2);
        // `.` + U+0301 is one punctuation cluster.
        assert_eq!(target("a.\u{301}b c".as_bytes(), 0, Motion::WordFwd, 2), 4);
    }
}

#[cfg(test)]
mod emacs_word_tests {
    //! Emacs `forward-word` / `kill-word` (`M-f` / `M-d`) — the between-character word-forward motion.
    use crate::motion::{char_span, target, Motion};

    #[test]
    fn emacs_word_fwd_lands_after_the_word() {
        let b = b"foo bar baz";
        // forward-word from the start rests point AFTER "foo" (index 3), where Vim `e` rests ON `o` (index 2).
        assert_eq!(target(b, 0, Motion::EmacsWordFwd, 1), 3);
        assert_eq!(target(b, 0, Motion::WordEnd, 1), 2);
        // From a space it skips to the end of the next word.
        assert_eq!(target(b, 3, Motion::EmacsWordFwd, 1), 7);
        // Mid-word it stops at the end of the current word.
        assert_eq!(target(b, 1, Motion::EmacsWordFwd, 1), 3);
        // Counted: two words forward.
        assert_eq!(target(b, 0, Motion::EmacsWordFwd, 2), 7);
    }

    #[test]
    fn emacs_kill_word_span_is_the_word_only() {
        // `kill-word` deletes `[cursor, word_end_excl)` — "foo", not Vim `dw`'s "foo " (which eats the space).
        assert_eq!(
            char_span(b"foo bar baz", 0, Motion::EmacsWordFwd, 1),
            (0, 3)
        );
        // From mid-buffer it takes the rest of the current word: "foobar baz" at index 3 kills "bar".
        assert_eq!(char_span(b"foobar baz", 3, Motion::EmacsWordFwd, 1), (3, 6));
    }

    #[test]
    fn emacs_word_fwd_is_two_class_over_punct_and_underscore() {
        // Oracle-pinned (GNU Emacs 30.2): Emacs `forward-word` uses a TWO-class syntax split. A leading
        // punctuation run is SKIPPED (not its own word as in Vim `e`): "...foo" reaches the end of "foo".
        assert_eq!(target(b"...foo", 0, Motion::EmacsWordFwd, 1), 6);
        // `_` is a NON-word char (symbol syntax) in fundamental-mode, so `foo_bar` is two words and
        // forward-word / kill-word stop after "foo" — where Vim would treat `_` as a word char and take all.
        assert_eq!(target(b"foo_bar", 0, Motion::EmacsWordFwd, 1), 3);
        assert_eq!(char_span(b"foo_bar", 0, Motion::EmacsWordFwd, 1), (0, 3));
        // A leading whitespace run is skipped too (kill-word from a space eats through the next word).
        assert_eq!(char_span(b"  foo", 0, Motion::EmacsWordFwd, 1), (0, 5));
    }

    #[test]
    fn emacs_word_back_is_the_two_class_mirror() {
        // Oracle-pinned: `backward-word` skips a trailing non-word run then moves over the word to its start.
        // From inside "bar" of "foo.bar" it skips the "." and lands at the start of "foo" (index 0) — where
        // Vim `b` (WordBack) stops at the "." (index 3, punctuation being its own word).
        assert_eq!(target(b"foo.bar", 4, Motion::EmacsWordBack, 1), 0);
        assert_eq!(target(b"foo.bar", 4, Motion::WordBack, 1), 3);
        // From the end of "foo.bar" it lands at the start of "bar" (skips nothing, over "bar").
        assert_eq!(target(b"foo.bar", 7, Motion::EmacsWordBack, 1), 4);
        // `_` is non-word: from the end of "foo_bar" backward-word stops at the start of "bar" (index 4).
        assert_eq!(target(b"foo_bar", 7, Motion::EmacsWordBack, 1), 4);
        // Backward-kill-word span crossing the "." run: [emacs_word_back, cursor) = [0, 4) = "foo.".
        assert_eq!(char_span(b"foo.bar", 4, Motion::EmacsWordBack, 1), (0, 4));
        // At buffer start it is inert.
        assert_eq!(target(b"foo bar", 0, Motion::EmacsWordBack, 1), 0);
    }
}

#[cfg(test)]
mod backward_word_end_tests {
    //! Vim `ge` / `gE` — backward to the end of the previous word / WORD.
    use crate::motion::{char_span, target, Motion};

    #[test]
    fn ge_lands_on_previous_word_end() {
        let b = b"foo bar"; // f0 o1 o2 sp3 b4 a5 r6
                            // From the last char, `ge` lands ON the previous word's last char ('o' of foo, index 2).
        assert_eq!(target(b, 6, Motion::WordEndBack, 1), 2);
        // From a word start ('b', index 4) → still the previous word's end.
        assert_eq!(target(b, 4, Motion::WordEndBack, 1), 2);
        // From within/at the first word with no earlier word → clamps to 0.
        assert_eq!(target(b, 2, Motion::WordEndBack, 1), 0);
        // Counted: two word-ends back from a later buffer.
        assert_eq!(target(b"aa bb cc", 7, Motion::WordEndBack, 2), 1); // cc→bb-end→aa-end(idx1)
    }

    #[test]
    fn big_ge_treats_punctuation_as_part_of_the_word() {
        let b = b"foo.bar baz"; // small `ge` stops at the punct boundary; big `gE` does not
                                // `ge` from 'baz' start (index 8) → end of "bar" (index 6) since punct/word split into groups.
        assert_eq!(target(b, 8, Motion::WordEndBack, 1), 6);
        // `gE` from index 8 → end of the WORD "foo.bar" which is also index 6 (its last char 'r').
        assert_eq!(target(b, 8, Motion::BigWordEndBack, 1), 6);
        // But inside "foo.bar": `ge` from 'b'(index4) → end of "foo" segment... the '.' at 3 is its own
        // small-word (punct), so `ge` lands on it (index 3); `gE` skips to "foo" end? No earlier WORD, → 0.
        assert_eq!(target(b, 4, Motion::WordEndBack, 1), 3);
        assert_eq!(target(b, 4, Motion::BigWordEndBack, 1), 0);
    }

    #[test]
    fn dge_is_inclusive_of_both_ends() {
        // `dge` on 'r' of "foo bar" deletes back through the previous word-end, inclusive: [2, 7) → "fo".
        assert_eq!(char_span(b"foo bar", 6, Motion::WordEndBack, 1), (2, 7));
    }
}
