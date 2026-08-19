//! Motions — the geometry half of the editing grammar (C-EDITLANG). A motion resolves, from the cursor, to
//! a target position (a bare move like `w`) or to a byte range (an operator like `dw`). It also owns the
//! shared char-boundary / line helpers used across the editor.
//!
//! Word motions come in two flavors: small-word (`w`/`b`/`e`) split on Vim's three classes
//! (whitespace / word = alnum+`_`+non-ASCII / punctuation), and WORD (`W`/`B`/`E`) split on whitespace only.
//! All positions land on char boundaries.

use unicode_segmentation::GraphemeCursor;

/// A motion in the editing grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    /// Vim `^` / Emacs `back-to-indentation` (M-m): the first non-blank char of the current line (the line
    /// end when the line is all blank). Shared by both profiles — same target, so not a distinct command.
    LineFirstNonBlank,
    /// Small-word motions (Vim `w`/`b`/`e`): three classes — whitespace, word (alnum + `_` + non-ASCII),
    /// punctuation — so `foo.bar` is three words.
    WordFwd,
    WordBack,
    WordEnd,
    /// Vim `ge` — backward to the end of the previous (small-)word. Inclusive under an operator (`dge`).
    WordEndBack,
    /// Emacs `forward-word` / `M-f` (and the span for `kill-word` / `M-d`). Like `WordEnd` it stops at the
    /// end of the word, but as a CURSOR move it rests point AFTER the last word char (Emacs point is
    /// between-character, D-050), where Vim `e` (`WordEnd`) rests ON it. Its operator span is the same
    /// `[cursor, word_end_excl)` as `WordEnd`, so `Delete(EmacsWordFwd)` is Emacs `kill-word`.
    EmacsWordFwd,
    /// WORD motions (Vim `W`/`B`/`E`): whitespace-delimited only, so `foo.bar` is one WORD.
    BigWordFwd,
    BigWordBack,
    BigWordEnd,
    /// Vim `gE` — backward to the end of the previous WORD (whitespace-delimited). Inclusive under `dgE`.
    BigWordEndBack,
    /// Text object: the word under the cursor (`iw`) — small-word classes (word / punct / space).
    /// Only meaningful under an operator or in a selection.
    InnerWord,
    /// Text object: the word plus its adjacent whitespace (`aw`). Only meaningful under an operator/selection.
    AWord,
    /// Text object: the WORD under the cursor (`iW`) — whitespace-delimited (`foo.bar` is one WORD).
    InnerBigWord,
    /// Text object: the WORD plus its adjacent whitespace (`aW`).
    ABigWord,
    /// Text object: the paragraph under the cursor (`ip`) — the run of non-blank (or blank) lines. Linewise.
    InnerParagraph,
    /// Text object: the paragraph plus its trailing blank lines, else leading (`ap`). Linewise.
    AParagraph,
    /// Text object: the sentence under the cursor (`is`), excluding trailing whitespace.
    InnerSentence,
    /// Text object: the sentence plus its trailing whitespace (`as`).
    ASentence,
    /// Text object: a matching delimiter pair. `around` includes the delimiters (`a(`), else the inside
    /// (`i(`). Nesting-aware; the pair may span lines. Covers `()`, `{}`, `[]`, `<>`.
    Pair {
        open: char,
        close: char,
        around: bool,
    },
    /// Text object: a quote pair on the current line. `around` includes the quotes and adjacent whitespace
    /// (`a"`), else just the inside (`i"`). Single-line (Vim). Covers `"`, `'`, `` ` ``.
    Quote {
        ch: char,
        around: bool,
    },
    /// Linewise (only meaningful under an operator: `dd` / `cc`).
    Line,
    /// Char-search within the current line: `f`/`F` land on the `count`-th `ch`; `t`/`T` stop one char
    /// short of it. `forward` picks the direction, `till` picks find-vs-till. Repeated by `;`/`,`.
    FindChar {
        ch: char,
        forward: bool,
        till: bool,
    },
    /// Go to the `count`-th line (1-based), landing on its first non-blank char (`gg`, `{count}G`). Linewise
    /// under an operator.
    GotoLine,
    /// Go to the last line's first non-blank char (bare `G`). Linewise under an operator.
    LastLine,
    /// Jump to the matching bracket of `()`, `[]`, or `{}` (`%`). Nesting-aware; the match may be on another
    /// line. If the cursor is not on a bracket, the first bracket forward on the line is matched instead.
    MatchBracket,
    /// Paragraph motion forward (`}`): to the start of the next blank line (or end of buffer). Exclusive
    /// charwise as a bare move; under an operator Vim's exclusive-linewise rule can make it linewise (`d}`).
    ParagraphFwd,
    /// Paragraph motion backward (`{`): to the start of the previous blank line (or start of buffer).
    ParagraphBack,
}

// --- shared byte / char-boundary / line helpers (one home; editor.rs reuses these) ---

pub(crate) fn is_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || i == b.len() || (b[i] & 0xC0) != 0x80
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
/// Falls back to the char boundary if the buffer is not valid UTF-8 (a binary file has no graphemes).
pub(crate) fn next_grapheme(b: &[u8], pos: usize) -> usize {
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

/// The byte offset of the previous grapheme-cluster boundary before `pos` (the `h`/`MoveLeft` step).
pub(crate) fn prev_grapheme(b: &[u8], pos: usize) -> usize {
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

pub(crate) fn snap(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    if is_boundary(b, p) {
        p
    } else {
        prev_boundary(b, p)
    }
}

// Line boundaries are the one implementation in `crate::pos`; re-exported here so motion call sites
// (`line_start`/`line_end`) read unchanged.
pub(crate) use crate::pos::{line_end, line_start};

pub(crate) fn col_of(b: &[u8], start: usize, pos: usize) -> usize {
    std::str::from_utf8(&b[start..pos])
        .map(|s| s.chars().count())
        .unwrap_or(0)
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

/// The VIRTUAL (display) column of `pos` on its line, where a `<Tab>` advances to the next multiple of
/// `tabstop` and every other char is one column. `start` is the line start. Used by Virtual Replace (`gR`)
/// to size a tab's on-screen width; distinct from [`col_of`], which counts characters. `tabstop` is clamped
/// to at least 1.
pub(crate) fn vcol_of(b: &[u8], start: usize, pos: usize, tabstop: usize) -> usize {
    let ts = tabstop.max(1);
    let mut v = 0;
    let mut i = start;
    while i < pos.min(b.len()) {
        if b[i] == b'\t' {
            v += ts - (v % ts);
            i += 1;
        } else {
            v += 1;
            i = next_boundary(b, i);
        }
    }
    v
}

fn is_ws(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n'
}

/// The Vim character class of a byte for word motions. Non-ASCII bytes (`>= 0x80`) count as `Word` so a
/// multibyte identifier (e.g. Hangul) is one word; class changes only ever fall on char boundaries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: u8) -> Class {
    if is_ws(c) {
        Class::Space
    } else if c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Whether two bytes belong to the same word group. For WORD motions (`big`) any two non-space bytes share a
/// group; for small-word motions the classes must also match (so a word↔punct transition is a boundary).
fn same_group(a: Class, b: Class, big: bool) -> bool {
    a != Class::Space && b != Class::Space && (big || a == b)
}

/// Start of the next (small-)word / WORD — Vim `w` / `W`.
fn next_word_start(b: &[u8], pos: usize, big: bool) -> usize {
    let mut i = pos.min(b.len());
    if i < b.len() {
        let c0 = class(b[i]);
        if c0 != Class::Space {
            while i < b.len() && same_group(c0, class(b[i]), big) {
                i += 1;
            }
        }
    }
    while i < b.len() && class(b[i]) == Class::Space {
        i += 1;
    }
    i
}

/// Start of the current/previous word / WORD — Vim `b` / `B`.
fn prev_word_start(b: &[u8], pos: usize, big: bool) -> usize {
    let mut i = pos.min(b.len());
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && class(b[i]) == Class::Space {
        i -= 1;
    }
    if class(b[i]) == Class::Space {
        return i;
    }
    let cw = class(b[i]);
    while i > 0 && same_group(cw, class(b[i - 1]), big) {
        i -= 1;
    }
    i
}

/// One past the last byte of the current/next word / WORD (the exclusive end for `de` / `dE`).
fn word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
    let mut i = (pos + 1).min(b.len());
    while i < b.len() && class(b[i]) == Class::Space {
        i += 1;
    }
    if i >= b.len() {
        return b.len();
    }
    let cw = class(b[i]);
    while i < b.len() && same_group(cw, class(b[i]), big) {
        i += 1;
    }
    i
}

/// One past the last byte of the PREVIOUS word / WORD before `pos` — the exclusive end for Vim `ge` / `gE`.
/// Scans left for the first word-end byte (a non-space whose right neighbour is a different group or EOF),
/// which naturally skips the rest of the word the cursor sits in. `0` when there is no earlier word. The
/// caller snaps with [`prev_boundary`] so the cursor lands on the last CHAR (multibyte-safe), like `WordEnd`.
fn prev_word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
    let pos = pos.min(b.len());
    if pos == 0 {
        return 0;
    }
    let mut e = pos - 1;
    loop {
        let is_end = class(b[e]) != Class::Space
            && (e + 1 >= b.len() || !same_group(class(b[e]), class(b[e + 1]), big));
        if is_end {
            return e + 1;
        }
        if e == 0 {
            return 0;
        }
        e -= 1;
    }
}

/// The word-object class of a byte: for `iW`/`aW` (`big`) only whitespace-vs-not matters, so punctuation
/// joins the word; for `iw`/`aw` the three small-word classes stay distinct. Class changes only fall on char
/// boundaries (non-ASCII bytes are all `Word`), so a span built from runs of equal `obj_class` is boundary-safe.
fn obj_class(c: u8, big: bool) -> u8 {
    match class(c) {
        Class::Space => 0,
        Class::Word => 1,
        Class::Punct => {
            if big {
                1
            } else {
                2
            }
        }
    }
}

/// The run of same-object-class bytes containing the cursor — Vim `iw` (small) / `iW` (big).
fn inner_word_span(b: &[u8], cur: usize, big: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let c = cur.min(b.len() - 1);
    let cls = obj_class(b[c], big);
    let mut s = c;
    while s > 0 && obj_class(b[s - 1], big) == cls {
        s -= 1;
    }
    let mut e = c;
    while e < b.len() && obj_class(b[e], big) == cls {
        e += 1;
    }
    (s, e)
}

/// The word plus its trailing whitespace (or leading, if there is no trailing) — Vim `aw` / `aW`.
fn a_word_span(b: &[u8], cur: usize, big: bool) -> (usize, usize) {
    let (s, e) = inner_word_span(b, cur, big);
    let mut e2 = e;
    while e2 < b.len() && is_ws(b[e2]) {
        e2 += 1;
    }
    if e2 > e {
        (s, e2)
    } else {
        let mut s2 = s;
        while s2 > 0 && is_ws(b[s2 - 1]) {
            s2 -= 1;
        }
        (s2, e)
    }
}

/// Whether the line starting at `ls` is blank — empty or (a start pointing at) a newline / end of buffer.
/// Paragraph boundaries in Vim are truly-empty lines (a whitespace-only line is *not* a boundary).
fn is_blank_line(b: &[u8], ls: usize) -> bool {
    ls >= b.len() || b[ls] == b'\n'
}

/// The paragraph object (`ip` / `ap`), returned as a linewise byte range `[s, e)` covering whole lines. The
/// paragraph is the maximal run of lines with the same blank-ness as the cursor's line; `around` extends it
/// over the following blank lines (Vim's `ap`), or the preceding ones when there are none trailing.
fn paragraph_span(b: &[u8], cur: usize, around: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let start_ls = line_start(b, cur);
    let blank = is_blank_line(b, start_ls);
    // Extend up over same-blankness lines.
    let mut s = start_ls;
    while s > 0 {
        let prev_ls = line_start(b, s - 1);
        if is_blank_line(b, prev_ls) != blank {
            break;
        }
        s = prev_ls;
    }
    // Walk down to the last line of the block, then take the exclusive end (past its newline).
    let mut last_ls = start_ls;
    loop {
        let le = line_end(b, last_ls);
        if le >= b.len() {
            break; // last line of the buffer (no trailing newline)
        }
        let next_ls = le + 1;
        if next_ls > b.len() || is_blank_line(b, next_ls) != blank {
            break;
        }
        last_ls = next_ls;
    }
    let block_end = {
        let le = line_end(b, last_ls);
        if le < b.len() {
            le + 1
        } else {
            le
        }
    };
    if !around {
        return (s, block_end);
    }
    // `ap`: include trailing lines of the opposite blank-ness, else leading ones.
    let mut e2 = block_end;
    while e2 < b.len() && is_blank_line(b, e2) != blank {
        let le = line_end(b, e2);
        e2 = if le < b.len() { le + 1 } else { le };
        if le >= b.len() {
            break;
        }
    }
    if e2 > block_end {
        return (s, e2);
    }
    let mut s2 = s;
    while s2 > 0 {
        let prev_ls = line_start(b, s2 - 1);
        if is_blank_line(b, prev_ls) != blank {
            s2 = prev_ls;
        } else {
            break;
        }
    }
    (s2, block_end)
}

/// The `}` target: the start of the next blank line strictly after the cursor's line, or the end of the
/// buffer if there is none. Vim's paragraph-forward motion. Lands on a char boundary (a line start).
fn next_para_boundary(b: &[u8], pos: usize) -> usize {
    let mut ls = line_start(b, pos);
    loop {
        let le = line_end(b, ls);
        if le >= b.len() {
            return b.len();
        }
        ls = le + 1;
        if is_blank_line(b, ls) {
            return ls;
        }
    }
}

/// The `{` target: the start of the previous blank line strictly before the cursor's line, or the start of
/// the buffer if there is none. Vim's paragraph-backward motion. Lands on a char boundary (a line start).
fn prev_para_boundary(b: &[u8], pos: usize) -> usize {
    let mut ls = line_start(b, pos);
    while ls > 0 {
        let prev_ls = line_start(b, ls - 1);
        if is_blank_line(b, prev_ls) {
            return prev_ls;
        }
        ls = prev_ls;
    }
    0
}

fn is_space_or_tab(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// If a sentence terminator (`.`/`!`/`?`), optionally followed by closing `)`/`]`/`"`/`'`, begins at `i`
/// and is itself followed by whitespace or end-of-buffer, return the byte just past the terminator group
/// (the exclusive inner-sentence end). Otherwise `None`. All the scanned bytes are ASCII → boundary-safe.
fn sentence_break_at(b: &[u8], i: usize) -> Option<usize> {
    if i >= b.len() || !matches!(b[i], b'.' | b'!' | b'?') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && matches!(b[j], b')' | b']' | b'"' | b'\'') {
        j += 1;
    }
    if j >= b.len() || is_ws(b[j]) {
        Some(j)
    } else {
        None
    }
}

/// The sentence object (`is` / `as`). A sentence ends at `.`/`!`/`?` (plus any closing quotes/brackets)
/// followed by whitespace; the next sentence starts after that whitespace. `around` includes the trailing
/// spaces/tabs. v0 keeps this within the buffer's flat text (blank-line paragraph splitting is not modeled).
fn sentence_span(b: &[u8], cur: usize, around: bool) -> (usize, usize) {
    let n = b.len();
    if n == 0 {
        return (0, 0);
    }
    let cur = cur.min(n - 1);
    // The start of the sentence containing the cursor: the last sentence start at or before `cur`.
    let mut s = 0usize;
    let mut i = 0usize;
    while i < n {
        if let Some(j) = sentence_break_at(b, i) {
            let mut k = j;
            while k < n && is_ws(b[k]) {
                k += 1;
            }
            if k > cur {
                break; // the next sentence starts past the cursor — keep the current `s`
            }
            if k < n {
                s = k;
            }
            i = k;
        } else {
            i += 1;
        }
    }
    // The inner end: the terminator group that closes this sentence (or end of buffer).
    let mut inner_end = n;
    let mut t = s;
    while t < n {
        if let Some(j) = sentence_break_at(b, t) {
            inner_end = j;
            break;
        }
        t += 1;
    }
    if !around {
        return (s, inner_end);
    }
    let mut e = inner_end;
    while e < n && is_space_or_tab(b[e]) {
        e += 1;
    }
    (s, e)
}

/// The delimiter-pair object (`i(`/`a(` etc.). Finds the pair enclosing the cursor (nesting-aware; may span
/// lines). `around` includes the delimiters, else only the interior. A no-op range `(cur, cur)` if the cursor
/// is not inside a pair. Delimiters are ASCII bytes, so all returned positions are char boundaries.
fn pair_span(b: &[u8], cur: usize, open: u8, close: u8, around: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let cur = cur.min(b.len() - 1);
    // The enclosing opener: the cursor's own byte if it is the opener, else scan left counting nesting.
    let open_pos = if b[cur] == open {
        Some(cur)
    } else {
        let mut depth = 0i32;
        let mut i = cur;
        let mut found = None;
        while i > 0 {
            i -= 1;
            if b[i] == close {
                depth += 1;
            } else if b[i] == open {
                if depth == 0 {
                    found = Some(i);
                    break;
                }
                depth -= 1;
            }
        }
        found
    };
    // Vim: when the cursor is not inside a pair, i(/a( uses the next pair FORWARD on the current
    // line (e.g. `di(` at col 0 of `foo (bar) baz` deletes `bar`). Oracle finding, PR #53.
    let open_pos = open_pos.or_else(|| {
        let mut i = cur;
        while i < b.len() && b[i] != b'\n' {
            if b[i] == open {
                return Some(i);
            }
            i += 1;
        }
        None
    });
    let Some(o) = open_pos else {
        return (cur, cur);
    };
    // The matching closer: scan right from the opener, counting nesting.
    let mut depth = 0i32;
    let mut close_pos = None;
    let mut i = o;
    while i < b.len() {
        if b[i] == open {
            depth += 1;
        } else if b[i] == close {
            depth -= 1;
            if depth == 0 {
                close_pos = Some(i);
                break;
            }
        }
        i += 1;
    }
    let Some(c) = close_pos else {
        return (cur, cur);
    };
    if around {
        (o, c + 1) // both delimiters are single ASCII bytes
    } else {
        (o + 1, c) // interior; empty pair `()` yields an empty (no-op) range
    }
}

/// The quote object (`i"`/`a"` etc.), confined to the current line (Vim). Quotes pair up left-to-right; the
/// target pair is the first whose closing quote is at or after the cursor. `around` includes the quotes plus
/// trailing whitespace (else leading). No matching pair → a no-op range. Quotes are ASCII → boundary-safe.
fn quote_span(b: &[u8], cur: usize, q: u8, around: bool) -> (usize, usize) {
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let cur = cur.clamp(ls, le);
    let positions: Vec<usize> = (ls..le).filter(|&i| b[i] == q).collect();
    let mut pair = None;
    let mut k = 0;
    while k + 1 < positions.len() {
        let (o, c) = (positions[k], positions[k + 1]);
        if c >= cur {
            pair = Some((o, c));
            break;
        }
        k += 2;
    }
    let Some((o, c)) = pair else {
        return (cur, cur);
    };
    if !around {
        return (o + 1, c);
    }
    // `a"`: prefer trailing whitespace on the line, else leading.
    let mut e = c + 1;
    while e < le && is_space_or_tab(b[e]) {
        e += 1;
    }
    if e > c + 1 {
        (o, e)
    } else {
        let mut s = o;
        while s > ls && is_space_or_tab(b[s - 1]) {
            s -= 1;
        }
        (s, c + 1)
    }
}

fn up(b: &[u8], cur: usize) -> usize {
    let ls = line_start(b, cur);
    if ls == 0 {
        cur
    } else {
        at_col(b, line_start(b, ls - 1), col_of(b, ls, cur))
    }
}

fn down(b: &[u8], cur: usize) -> usize {
    let le = line_end(b, cur);
    if le >= b.len() {
        cur
    } else {
        at_col(b, le + 1, col_of(b, line_start(b, cur), cur))
    }
}

/// Vertical motion (`j`/`k`) that honours a STICKY desired column (`want`, Vim `curswant`) instead of the
/// cursor's current column, so moving through a shorter interior line does not collapse the column: `want`
/// is clamped to each line's length by [`at_col`] but never forgotten. `want == usize::MAX` (set by `$`)
/// rides each line's end. `down` picks the direction; `count` repeats. Char columns, like the rest of the
/// motion model (tab/wide-char virtual columns are a documented follow-up).
pub(crate) fn vmove(b: &[u8], cursor: usize, count: u32, down: bool, want: usize) -> usize {
    let mut c = cursor.min(b.len());
    for _ in 0..count.max(1) {
        if down {
            let le = line_end(b, c);
            if le >= b.len() {
                break;
            }
            c = at_col(b, le + 1, want);
        } else {
            let ls = line_start(b, c);
            if ls == 0 {
                break;
            }
            c = at_col(b, line_start(b, ls - 1), want);
        }
    }
    c
}

/// The bare-move landing for a char-search (`f`/`F`/`t`/`T`), or `None` if the `count`-th match does not
/// exist on the current line. `f`/`F` land on the char; `t`/`T` land one boundary short of it. Search is
/// confined to the current line (Vim never crosses a newline for these).
fn find_char_target(
    b: &[u8],
    cursor: usize,
    ch: char,
    forward: bool,
    till: bool,
    count: u32,
) -> Option<usize> {
    let n = count.max(1) as usize;
    let ls = line_start(b, cursor);
    let le = line_end(b, cursor); // newline / end position for this line
    let line = std::str::from_utf8(b.get(ls..le)?).ok()?;
    if forward {
        // Matches strictly after the cursor, in order; take the n-th.
        let hit = line
            .char_indices()
            .map(|(i, c)| (ls + i, c))
            .filter(|&(pos, c)| pos > cursor && c == ch)
            .nth(n - 1)?
            .0;
        Some(if till { prev_boundary(b, hit) } else { hit })
    } else {
        // Matches strictly before the cursor; take the n-th counting backward.
        let hit = line
            .char_indices()
            .map(|(i, c)| (ls + i, c))
            .filter(|&(pos, c)| pos < cursor && c == ch)
            .rev()
            .nth(n - 1)?
            .0;
        Some(if till { next_boundary(b, hit) } else { hit })
    }
}

/// First non-blank (non-space, non-tab) byte position on the line containing `line_start_pos`, or the line
/// start itself if the whole line is blank. Where `gg`/`G` land (Vim).
pub(crate) fn first_non_blank(b: &[u8], line_start_pos: usize) -> usize {
    let ls = line_start(b, line_start_pos);
    let le = line_end(b, ls);
    let mut i = ls;
    while i < le && (b[i] == b' ' || b[i] == b'\t') {
        i = next_boundary(b, i);
    }
    i
}

/// Byte start of the `n`-th line (1-based), clamped to the last line.
pub(crate) fn nth_line_start(b: &[u8], n: u32) -> usize {
    let target = n.max(1) - 1; // 0-based
    let mut seen = 0u32;
    let mut ls = 0usize;
    while seen < target {
        let le = line_end(b, ls);
        if le >= b.len() {
            break; // no more lines; clamp to the last one
        }
        ls = le + 1;
        seen += 1;
    }
    ls
}

/// Byte start of the last line.
pub(crate) fn last_line_start(b: &[u8]) -> usize {
    line_start(b, b.len())
}

/// The bracket pairs `%` matches (v0: the three ASCII pairs; `matchpairs` config is deferred).
const BRACKET_PAIRS: [(u8, u8); 3] = [(b'(', b')'), (b'[', b']'), (b'{', b'}')];

/// The matching-bracket position for `%`, or `None` if there is nothing to match. If the byte at `cursor`
/// is a bracket, its pair is found (nesting-aware, possibly on another line); otherwise the first bracket
/// forward on the current line is matched (Vim). Brackets are ASCII, so byte scanning stays on boundaries.
fn match_bracket(b: &[u8], cursor: usize) -> Option<usize> {
    // Pick the bracket to match: the one under the cursor, else the first forward on this line.
    let mut bp = cursor.min(b.len());
    if bp >= b.len() || !BRACKET_PAIRS.iter().any(|&(o, c)| b[bp] == o || b[bp] == c) {
        let le = line_end(b, cursor);
        bp = cursor.min(b.len());
        while bp < le && !BRACKET_PAIRS.iter().any(|&(o, c)| b[bp] == o || b[bp] == c) {
            bp += 1;
        }
        if bp >= le {
            return None;
        }
    }
    let ch = b[bp];
    // Opener → scan forward for the matching closer; closer → scan backward for the opener. Depth counts
    // only the same pair type, so `([)]` matches `(`↔`)` exactly as Vim does.
    if let Some(&(open, close)) = BRACKET_PAIRS.iter().find(|&&(o, _)| o == ch) {
        let mut depth = 0i32;
        for (i, &byte) in b.iter().enumerate().skip(bp) {
            if byte == open {
                depth += 1;
            } else if byte == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    } else if let Some(&(open, close)) = BRACKET_PAIRS.iter().find(|&&(_, c)| c == ch) {
        let mut depth = 0i32;
        for i in (0..=bp).rev() {
            if b[i] == close {
                depth += 1;
            } else if b[i] == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    } else {
        None
    }
}

/// The cursor target for a bare move (`w`, `3l`, `k`, …), applying `count`.
#[must_use]
pub fn target(b: &[u8], cursor: usize, m: Motion, count: u32) -> usize {
    let n = count.max(1);
    let cur0 = cursor.min(b.len());
    // Char-search resolves the count-th match directly (repeating a single `t` step would stick in place).
    if let Motion::FindChar { ch, forward, till } = m {
        return find_char_target(b, cur0, ch, forward, till, count).unwrap_or(cur0);
    }
    // Line jumps use the count as an absolute line number (or the last line), not a repeat count.
    if m == Motion::GotoLine {
        return first_non_blank(b, nth_line_start(b, count));
    }
    if m == Motion::LastLine {
        return first_non_blank(b, last_line_start(b));
    }
    // Bracket match jumps to a single computed position (count is not a repeat; `count%` is deferred).
    if m == Motion::MatchBracket {
        return match_bracket(b, cur0).unwrap_or(cur0);
    }
    let mut c = cur0;
    for _ in 0..n {
        c = match m {
            Motion::Left => prev_boundary(b, c),
            Motion::Right => next_boundary(b, c),
            Motion::Up => up(b, c),
            Motion::Down => down(b, c),
            Motion::LineStart => line_start(b, c),
            // Vim `^` / Emacs `back-to-indentation` (M-m): the first non-blank of the line (or the line end
            // when the line is all blank). Count-agnostic — repeating stays on the same line.
            Motion::LineFirstNonBlank => first_non_blank(b, line_start(b, c)),
            // A bare `$` lands ON the last char of the line (Vim never rests the cursor past it in Normal),
            // unlike the `d$` operator span which reaches the line end — see `char_span`.
            Motion::LineEnd => {
                let ls = line_start(b, c);
                let le = line_end(b, c);
                if le > ls {
                    prev_boundary(b, le)
                } else {
                    le
                }
            }
            Motion::ParagraphFwd => next_para_boundary(b, c),
            Motion::ParagraphBack => prev_para_boundary(b, c),
            Motion::WordFwd => next_word_start(b, c, false),
            Motion::WordBack => prev_word_start(b, c, false),
            Motion::WordEnd => prev_boundary(b, word_end_excl(b, c, false)), // land ON the last char
            Motion::WordEndBack => prev_boundary(b, prev_word_end_excl(b, c, false)), // Vim `ge`
            Motion::EmacsWordFwd => word_end_excl(b, c, false), // land AFTER the last char (Emacs point)
            Motion::BigWordFwd => next_word_start(b, c, true),
            Motion::BigWordBack => prev_word_start(b, c, true),
            Motion::BigWordEnd => prev_boundary(b, word_end_excl(b, c, true)),
            Motion::BigWordEndBack => prev_boundary(b, prev_word_end_excl(b, c, true)), // Vim `gE`
            // FindChar / line-jumps are resolved by the early returns above; objects/linewise have no
            // bare-move target.
            Motion::FindChar { .. }
            | Motion::GotoLine
            | Motion::LastLine
            | Motion::MatchBracket
            | Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::Pair { .. }
            | Motion::Quote { .. }
            | Motion::Line => c,
        };
    }
    c
}

/// The charwise byte range `[start, end)` an operator (`d`/`c`) covers for a motion + count. Backward
/// motions produce a range ending at the cursor; `WordEnd` is inclusive (Vim `de`).
#[must_use]
pub fn char_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
    let cur = cursor.min(b.len());
    let n = count.max(1);
    match m {
        // forward / rightward → [cursor, target)
        Motion::Right | Motion::WordFwd | Motion::BigWordFwd => (cur, target(b, cur, m, n)),
        // `d$` reaches the line end (deletes the last char too), unlike the bare `$` move which stops on it.
        Motion::LineEnd => (cur, line_end(b, cur)),
        // Paragraph motions as a charwise span (`c}` / a Visual `}` extend): forward → [cursor, target),
        // backward → [target, cursor). The operator's exclusive-linewise reshaping lives in `editor::op_span`.
        Motion::ParagraphFwd => (cur, target(b, cur, m, n)),
        Motion::ParagraphBack => (target(b, cur, m, n), cur),
        // inclusive end-of-word (Vim `de` / `dE`); `EmacsWordFwd` shares the same `[cursor, word_end_excl)`
        // span, which is exactly Emacs `kill-word` (`M-d`).
        Motion::WordEnd | Motion::BigWordEnd | Motion::EmacsWordFwd => {
            let big = m == Motion::BigWordEnd;
            let mut e = cur;
            for _ in 0..n {
                e = word_end_excl(b, e, big);
            }
            (cur, e)
        }
        // backward / leftward → [target, cursor)
        Motion::Left | Motion::WordBack | Motion::BigWordBack | Motion::LineStart => {
            (target(b, cur, m, n), cur)
        }
        // `ge`/`gE` are INCLUSIVE backward: span from the previous word-end (landed-on char included) up to
        // and including the cursor char (Vim `dge` on `r` of "foo bar" → "fo").
        Motion::WordEndBack | Motion::BigWordEndBack => {
            let big = m == Motion::BigWordEndBack;
            let mut e = cur;
            for _ in 0..n {
                e = prev_boundary(b, prev_word_end_excl(b, e, big));
            }
            (e, next_boundary(b, cur))
        }
        // `^` is exclusive and can point either way (cursor in the indent → forward): span the ordered pair.
        Motion::LineFirstNonBlank => {
            let t = target(b, cur, m, n);
            (cur.min(t), cur.max(t))
        }
        // char-search: forward includes through the landing char (`dfx` incl. x, `dtx` up to x); backward
        // spans from the landing to the cursor. A missing match is a no-op range.
        Motion::FindChar { ch, forward, till } => {
            match find_char_target(b, cur, ch, forward, till, n) {
                None => (cur, cur),
                Some(t) if forward => (cur, next_boundary(b, t)),
                Some(t) => (t, cur),
            }
        }
        // bracket match: inclusive of both brackets and everything between (Vim `d%`). No match → no-op.
        Motion::MatchBracket => match match_bracket(b, cur) {
            Some(m) if m != cur => {
                let (lo, hi) = (cur.min(m), cur.max(m));
                (lo, next_boundary(b, hi))
            }
            _ => (cur, cur),
        },
        // text objects: a range around the cursor (count ignored in v0)
        Motion::InnerWord => inner_word_span(b, cur, false),
        Motion::AWord => a_word_span(b, cur, false),
        Motion::InnerBigWord => inner_word_span(b, cur, true),
        Motion::ABigWord => a_word_span(b, cur, true),
        Motion::InnerParagraph => paragraph_span(b, cur, false),
        Motion::AParagraph => paragraph_span(b, cur, true),
        Motion::InnerSentence => sentence_span(b, cur, false),
        Motion::ASentence => sentence_span(b, cur, true),
        // Delimiters/quotes are ASCII (the input engine only ever produces ASCII ones), so scanning by byte
        // stays on char boundaries. A non-ASCII delimiter is not a real object → no-op, and guarding here
        // keeps `as u8` from truncating a multibyte char into a byte that could land mid-codepoint.
        Motion::Pair {
            open,
            close,
            around,
        } => {
            if open.is_ascii() && close.is_ascii() {
                pair_span(b, cur, open as u8, close as u8, around)
            } else {
                (cur, cur)
            }
        }
        Motion::Quote { ch, around } => {
            if ch.is_ascii() {
                quote_span(b, cur, ch as u8, around)
            } else {
                (cur, cur)
            }
        }
        // vertical / linewise motions are not charwise — callers handle Line / line-jumps specially
        Motion::Up | Motion::Down | Motion::Line | Motion::GotoLine | Motion::LastLine => {
            (cur, cur)
        }
    }
}

#[cfg(test)]
mod emacs_word_tests {
    //! Emacs `forward-word` / `kill-word` (`M-f` / `M-d`) — the between-character word-forward motion.
    use super::{char_span, target, Motion};

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
}

#[cfg(test)]
mod backward_word_end_tests {
    //! Vim `ge` / `gE` — backward to the end of the previous word / WORD.
    use super::{char_span, target, Motion};

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
