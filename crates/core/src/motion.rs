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
    /// Vim `g_` — the LAST non-blank char of the line (`{count}g_` = `count-1` lines down). Inclusive
    /// under an operator (`dg_` deletes through the last non-blank).
    LineLastNonBlank,
    /// Vim `|` — go to the `count`-th (1-based) display column of the current line (`5|` = column 5).
    /// Exclusive charwise under an operator.
    Column,
    /// Vim `gM` — go to the char at `count` PERCENT of the line's CHARACTER length (bare `gM` = 50%, the
    /// middle char; `25gM` = a quarter along). The 0-based char index is `min(count * chars / 100, chars-1)`,
    /// counting every char on the line including leading/trailing whitespace. Char columns, like the rest of
    /// the motion model — nvim's `gM` actually measures DISPLAY width, so a line containing `<Tab>`s or
    /// double-width chars lands one-off from nvim (the same tab/wide-char follow-up the whole motion model
    /// carries; see `vmove`/`Column`). Exclusive charwise under an operator (`dgM`), like `|`.
    MidLine,
    /// Small-word motions (Vim `w`/`b`/`e`): three classes — whitespace, word (alnum + `_` + non-ASCII),
    /// punctuation — so `foo.bar` is three words.
    WordFwd,
    WordBack,
    WordEnd,
    /// Vim `ge` — backward to the end of the previous (small-)word. Inclusive under an operator (`dge`).
    WordEndBack,
    /// Emacs `forward-word` / `M-f` (and the span for `kill-word` / `M-d`). Emacs word motions use a TWO-class
    /// syntax split — word constituents (ASCII alphanumerics + non-ASCII) vs everything else (whitespace AND
    /// punctuation AND `_`, all non-word in fundamental-mode) — so it SKIPS a leading non-word run then moves
    /// over the word, landing AFTER the last word char (Emacs point is between-character, D-050). This differs
    /// from Vim's three-class `e`/`WordEnd` (where a punctuation run is its OWN word and `_` is a word char):
    /// `forward-word` on `...foo` reaches the end of `foo`, and on `foo_bar` stops after `foo`. Its operator
    /// span is `[cursor, emacs_word_fwd)`, so `Delete(EmacsWordFwd)` is Emacs `kill-word`.
    EmacsWordFwd,
    /// Emacs `backward-word` / `M-b` (and the span for `backward-kill-word` / `M-DEL`). The mirror of
    /// [`Motion::EmacsWordFwd`]: two-class syntax, so it skips a trailing non-word run backward then moves
    /// over the word to its start. Differs from Vim `b` (`WordBack`) exactly as the forward pair differs:
    /// `backward-word` on `foo.bar` from inside `bar` skips the `.` and lands at the start of `foo`.
    EmacsWordBack,
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
    /// Text object: a matching HTML/XML tag block (`it`/`at`). `around` includes the `<tag>`/`</tag>`
    /// delimiters (`at`), else just the content between them (`it`). Nesting-aware; may span lines.
    Tag {
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
    /// Vim `+` / `<CR>` — `count` lines DOWN, landing on the first non-blank char (`^` on the target
    /// line). Linewise under an operator (`d+` deletes this line and the next). Clamps at the last line.
    DownFirstNonBlank,
    /// Vim `-` — `count` lines UP, landing on the first non-blank char. Linewise under an operator
    /// (`d-` deletes this line and the one above). Clamps at the first line.
    UpFirstNonBlank,
    /// Vim `_` — `count-1` lines DOWN, landing on the first non-blank char (`_` alone == `^`, `2_` == one
    /// line down). Linewise under an operator (`d_` == `dd`, `2d_` deletes two lines). Clamps at the last line.
    LineUnderscore,
    /// Vim `[count]go` — go to the `count`-th BYTE of the buffer (1-based; bare `go` = byte 1), snapped to a
    /// char boundary. Exclusive charwise under an operator (`dgo`). `count` past the end clamps to the last byte.
    GotoByte,
    /// Jump to the matching bracket of `()`, `[]`, or `{}` (`%`). Nesting-aware; the match may be on another
    /// line. If the cursor is not on a bracket, the first bracket forward on the line is matched instead.
    MatchBracket,
    /// Vim `{count}%` — jump to `count` PERCENT of the file: line `(count * line_count + 99) / 100`
    /// (1-based), landing on its first non-blank. Linewise under an operator. `count` is the percentage
    /// (not a repeat); bare `%` with no count is [`Motion::MatchBracket`], resolved by the input engine.
    GotoPercent,
    /// Paragraph motion forward (`}`): to the start of the next blank line (or end of buffer). Exclusive
    /// charwise as a bare move; under an operator Vim's exclusive-linewise rule can make it linewise (`d}`).
    ParagraphFwd,
    /// Paragraph motion backward (`{`): to the start of the previous blank line (or start of buffer).
    ParagraphBack,
    /// Sentence motion forward (`)`): to the start of the next sentence (exclusive charwise).
    SentenceFwd,
    /// Sentence motion backward (`(`): to the start of the current/previous sentence (exclusive charwise).
    SentenceBack,
    /// Vim `]]` — forward to the start of the NEXT section: the next line whose first column is `{` (or a
    /// form-feed `\x0C`). Lands on that line's first non-blank (which, for a `{`/form-feed line, is column 0).
    /// With no further section it clamps to the last content line. Exclusive; under an operator Vim's
    /// exclusive-linewise rule makes `d]]`/`y]]` linewise (shared with `ParagraphFwd` in `editor::op_span`).
    SectionFwd,
    /// Vim `[[` — backward to the start of the PREVIOUS section (a `{`/form-feed line in column 0). Clamps to
    /// the first line. Exclusive; backward span is `[target, cursor)`.
    SectionBack,
    /// Vim `][` — forward to the NEXT section END: the next line whose first column is `}` (or a form-feed).
    /// Same wise handling as [`Motion::SectionFwd`].
    SectionEndFwd,
    /// Vim `[]` — backward to the PREVIOUS section end (a `}`/form-feed line in column 0). Clamps to the first
    /// line. Backward span is `[target, cursor)`.
    SectionEndBack,
    /// Vim `[(` — go to the `count`-th previous UNMATCHED `(` (the open paren enclosing the cursor; `count`
    /// steps out `count` nesting levels). Nesting-aware: balanced `()` pairs are skipped while scanning left,
    /// other bracket types are ignored. EXCLUSIVE charwise under an operator — backward span `[target, cursor)`,
    /// which then takes Vim's exclusive-linewise reduction (shared with `d}`/`d[[`). No-op when the cursor is
    /// not inside a `(`.
    UnmatchedParenBack,
    /// Vim `])` — go to the `count`-th next UNMATCHED `)` (the close paren enclosing the cursor). The forward
    /// mirror of [`Motion::UnmatchedParenBack`]: EXCLUSIVE charwise forward span `[cursor, target)`, also
    /// subject to the exclusive-linewise reduction. No-op when the cursor is not inside a `)`.
    UnmatchedParenFwd,
    /// Vim `[{` — go to the `count`-th previous unmatched `{`. As [`Motion::UnmatchedParenBack`] but for braces.
    UnmatchedBraceBack,
    /// Vim `]}` — go to the `count`-th next unmatched `}`. As [`Motion::UnmatchedParenFwd`] but for braces.
    UnmatchedBraceFwd,
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
    // A grapheme cluster never crosses a line break — UAX#29 always breaks around LF — so segmenting just
    // the CURRENT LINE is exactly equivalent to segmenting the whole buffer, but O(line) instead of O(buffer)
    // on the most frequently pressed motion. At/past the line end (window exhausted) the next boundary is the
    // newline byte itself, handled by the char-boundary fallback (LF is 1 byte, so char == grapheme there).
    let ls = line_start(b, pos);
    let le = line_end(b, pos);
    match std::str::from_utf8(&b[ls..le]) {
        Ok(s) => {
            let mut gc = GraphemeCursor::new(pos.saturating_sub(ls).min(s.len()), s.len(), true);
            match gc.next_boundary(s, 0).ok().flatten() {
                Some(rel) => ls + rel,
                None => next_boundary(b, pos), // window exhausted: cross the newline
            }
        }
        Err(_) => next_boundary(b, pos),
    }
}

/// The byte offset of the previous grapheme-cluster boundary before `pos` (the `h`/`MoveLeft` step).
pub(crate) fn prev_grapheme(b: &[u8], pos: usize) -> usize {
    // Windowed to the current line, for the same reason as [`next_grapheme`]. At the line start the previous
    // boundary is the preceding newline, handled by the char-boundary fallback.
    let ls = line_start(b, pos);
    let le = line_end(b, pos);
    match std::str::from_utf8(&b[ls..le]) {
        Ok(s) => {
            let mut gc = GraphemeCursor::new(pos.saturating_sub(ls).min(s.len()), s.len(), true);
            match gc.prev_boundary(s, 0).ok().flatten() {
                Some(rel) => ls + rel,
                None => prev_boundary(b, pos), // window exhausted: cross back over the newline
            }
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
pub(crate) fn word_end_excl(b: &[u8], pos: usize, big: bool) -> usize {
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
fn emacs_word_fwd(b: &[u8], pos: usize) -> usize {
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
fn emacs_word_back(b: &[u8], pos: usize) -> usize {
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
    let cw = class(b[pos]);
    if cw == Class::Space {
        return pos; // on blank: not the cw special case — caller uses the ordinary word span
    }
    let mut i = pos;
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

/// The keyword span under (or, failing that, forward on the current line from) the cursor — Vim `*`/`#`.
/// A keyword is a Word-class run (alnum + `_` + non-ASCII). Returns `None` when the current line has no
/// keyword at or after the cursor. Char-boundary safe (spans are built from equal-class runs).
pub(crate) fn word_under_cursor(b: &[u8], cur: usize) -> Option<(usize, usize)> {
    if b.is_empty() {
        return None;
    }
    let mut i = cur.min(b.len() - 1);
    if class(b[i]) != Class::Word {
        // Not on a keyword — scan forward on the current line for the next one (Vim `*`).
        let le = line_end(b, i);
        while i < le && class(b[i]) != Class::Word {
            i += 1;
        }
        if i >= le || class(b[i]) != Class::Word {
            return None;
        }
    }
    Some(inner_word_span(b, i, false))
}

/// The `<cword>` (`big == false`) or `<cWORD>` (`big == true`) span under the cursor — the buffer text
/// Vim's command-line `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A` splice at the cmdline caret
/// (`:help c_CTRL-R_CTRL-W`, `:help expand()` `<cword>`/`<cWORD>`). Distinct from [`word_under_cursor`]
/// (Vim `*`): that returns `None` when the line has no keyword, whereas `<cword>` falls back to the
/// non-blank (punctuation) run — so `C-r C-w` on `foo ...` at the first `.` yields `...`, where `*` bells.
///
/// `<cword>`: the keyword (alnum/`_`/non-ASCII run) under the cursor; if the cursor is not on a keyword,
/// the search scans FORWARD on the line for the next keyword char (crossing blanks and punctuation). When
/// the rest of the line has no keyword, it falls back to the non-blank run under/forward of the cursor.
///
/// `<cWORD>`: the whitespace-delimited WORD (maximal non-blank run) containing the cursor; if the cursor
/// is on whitespace, the next such run forward on the line.
///
/// Returns `None` when the rest of the line (from the cursor) is blank. Char-boundary safe (spans are
/// built from equal-class runs). Verified against nvim v0.12.4.
pub(crate) fn cword_under_cursor(b: &[u8], cur: usize, big: bool) -> Option<(usize, usize)> {
    if b.is_empty() {
        return None;
    }
    let i = cur.min(b.len() - 1);
    let le = line_end(b, i);
    // The next non-blank position at or forward of the cursor, within the current line (`None` if the
    // rest of the line is blank). Both variants fall back to this run when there is no keyword.
    let first_nonblank = |from: usize| {
        let mut k = from;
        while k < le && class(b[k]) == Class::Space {
            k += 1;
        }
        (k < le && class(b[k]) != Class::Space).then_some(k)
    };
    if big {
        // `<cWORD>`: skip leading blanks, then the maximal non-blank run (word+punct merge under `big`).
        return first_nonblank(i).map(|k| inner_word_span(b, k, true));
    }
    // `<cword>`: prefer the next keyword char forward on the line (crossing blanks and punctuation).
    let mut j = i;
    while j < le && class(b[j]) != Class::Word {
        j += 1;
    }
    if j < le && class(b[j]) == Class::Word {
        return Some(inner_word_span(b, j, false));
    }
    // No keyword on the rest of the line: fall back to the non-blank (punctuation) run.
    first_nonblank(i).map(|k| inner_word_span(b, k, false))
}

/// The keyword (Word-class run) ending exactly AT the caret — the "base" Vim's insert-mode keyword
/// completion (`i_CTRL-N` / `i_CTRL-P`) matches against. Returns `(start, base)`: `start` is the byte
/// offset where the run begins, `base` its text (empty, with `start == cur`, when the char before the
/// caret is not keyword-class or the caret is at a line/buffer start). Char-boundary safe: a multibyte
/// keyword char is all `Word`-class bytes, so walking back to the first non-word byte lands on a boundary.
pub(crate) fn keyword_before_cursor(b: &[u8], cur: usize) -> (usize, &str) {
    let mut s = cur.min(b.len());
    while s > 0 && class(b[s - 1]) == Class::Word {
        s -= 1;
    }
    (
        s,
        std::str::from_utf8(&b[s..cur.min(b.len())]).unwrap_or(""),
    )
}

/// The current-buffer keyword-completion candidates for `i_CTRL-N` / `i_CTRL-P` (F-003), in Vim's scan
/// order. `base` is [`keyword_before_cursor`]'s text; the result is every keyword in the buffer that
/// STARTS WITH `base`, ordered forward from the caret to end-of-buffer then wrapping from the buffer
/// start back to the caret, with the word under construction (the run at the caret) excluded and later
/// DUPLICATES removed (first occurrence in scan order wins). Matching is case-sensitive (nvim default
/// `noignorecase`; the 'ignorecase'/'infercase' interaction is out of scope for this slice). C-P walks
/// this same list backward — the caller cycles the index, so only one order is built. Verified against
/// nvim v0.12.4.
pub(crate) fn keyword_completions(b: &[u8], cur: usize, base: &str) -> Vec<String> {
    // The run under construction begins here; it is never offered as its own candidate. Only meaningful
    // when `base` is non-empty (an empty base sits after a non-word char, so no run starts at the caret).
    let base_start = cur.saturating_sub(base.len());
    // All keyword runs, as (start, text), in positional order.
    let mut runs: Vec<(usize, &str)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if class(b[i]) == Class::Word {
            let s = i;
            while i < b.len() && class(b[i]) == Class::Word {
                i += 1;
            }
            if let Ok(t) = std::str::from_utf8(&b[s..i]) {
                runs.push((s, t));
            }
        } else {
            i += 1;
        }
    }
    // Forward-from-caret first (runs starting at/after the caret), then wrap from the buffer start.
    let ordered = runs
        .iter()
        .filter(|(s, _)| *s >= cur)
        .chain(runs.iter().filter(|(s, _)| *s < cur));
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (s, t) in ordered {
        if !base.is_empty() && *s == base_start {
            continue; // the word being typed — never its own candidate
        }
        if !t.starts_with(base) {
            continue;
        }
        if seen.insert(t) {
            out.push((*t).to_string());
        }
    }
    out
}

/// The word plus its trailing whitespace (or leading, if there is no trailing) — Vim `aw` / `aW`.
/// `{count}iw`: the inner-word span extended forward over `count` alternating class runs (Vim). Each unit is
/// a maximal run of one class (a word-class run or a whitespace run), so `2iw` = word + following whitespace,
/// `3iw` = word + ws + word. `count <= 1` is the plain `iw` span.
fn inner_word_span_n(b: &[u8], cur: usize, big: bool, count: u32) -> (usize, usize) {
    // An `iw` unit is a maximal run of ONE class — a word-class run OR a whitespace run — so extension must
    // group whitespace too (unlike `same_group`, which is word-only). For `iW` a word-and-punct run is one.
    let same_unit = |c: Class, x: Class| {
        if c == Class::Space {
            x == Class::Space
        } else if big {
            x != Class::Space
        } else {
            x == c
        }
    };
    let (s, mut e) = inner_word_span(b, cur, big);
    for _ in 1..count.max(1) {
        if e >= b.len() {
            break;
        }
        let c = class(b[e]);
        while e < b.len() && same_unit(c, class(b[e])) {
            e += 1;
        }
    }
    (s, e)
}

/// `{count}aw`: `count` words, each including its trailing whitespace (Vim). `count <= 1` is the plain `aw`
/// span (which already carries the trailing — or, absent that, leading — whitespace).
fn a_word_span_n(b: &[u8], cur: usize, big: bool, count: u32) -> (usize, usize) {
    let (s, mut e) = a_word_span(b, cur, big);
    for _ in 1..count.max(1) {
        while e < b.len() && is_ws(b[e]) {
            e += 1;
        }
        if e >= b.len() {
            break;
        }
        let c = class(b[e]);
        while e < b.len() && same_group(c, class(b[e]), big) {
            e += 1; // the next word
        }
        while e < b.len() && is_ws(b[e]) {
            e += 1; // its trailing whitespace
        }
    }
    (s, e)
}

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

/// Whether the line starting at `ls` is a SECTION boundary line. A form-feed (`\x0C`) in column 0 is a
/// boundary for both the start (`]]`/`[[`) and end (`][`/`[]`) motions; otherwise the first byte must be the
/// brace for the direction — `{` for section starts, `}` for section ends. (Vim's default: `{`/`}` in the
/// first column plus form-feed; the nroff `sections` macros are a documented follow-up.)
fn is_section_line(b: &[u8], ls: usize, ends: bool) -> bool {
    match b.get(ls) {
        Some(&0x0C) => true,
        Some(&c) => c == if ends { b'}' } else { b'{' },
        None => false,
    }
}

/// The line start of the NEXT section boundary strictly after `ls`, or `None` when the run reaches the last
/// content line without finding one (the caller clamps to the last line, matching nvim's EOF landing).
fn scan_section_fwd(b: &[u8], ls: usize, ends: bool) -> Option<usize> {
    let mut cur = ls;
    loop {
        let le = line_end(b, cur);
        if le >= content_end(b) {
            return None; // `cur` is the last content line — no next line to test
        }
        cur = le + 1;
        if is_section_line(b, cur, ends) {
            return Some(cur);
        }
    }
}

/// The line start of the PREVIOUS section boundary strictly before `ls`, or `None` when none exists (the
/// caller clamps to the first line).
fn scan_section_back(b: &[u8], ls: usize, ends: bool) -> Option<usize> {
    let mut cur = ls;
    while cur > 0 {
        cur = line_start(b, cur - 1);
        if is_section_line(b, cur, ends) {
            return Some(cur);
        }
    }
    None
}

/// The `count`-th section boundary line-start forward (`]]`/`][`), clamping to the last content line when the
/// sections run out (nvim rests the cursor on the last line at that point).
fn next_section_line(b: &[u8], pos: usize, count: u32, ends: bool) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..count.max(1) {
        match scan_section_fwd(b, ls, ends) {
            Some(f) => ls = f,
            None => return last_line_start(b),
        }
    }
    ls
}

/// The `count`-th section boundary line-start backward (`[[`/`[]`), clamping to the first line.
fn prev_section_line(b: &[u8], pos: usize, count: u32, ends: bool) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..count.max(1) {
        match scan_section_back(b, ls, ends) {
            Some(f) => ls = f,
            None => return 0,
        }
    }
    ls
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

/// Vim `)` — the start of the NEXT sentence after `pos` (the byte after a terminator group + whitespace),
/// or the buffer end when none. Terminator-based like [`sentence_span`] (blank-line/paragraph sentence
/// boundaries are not modeled, matching the `is`/`as` object). Lands on a char boundary (starts follow
/// ASCII whitespace).
fn next_sentence_start(b: &[u8], pos: usize) -> usize {
    let n = b.len();
    let mut i = 0usize;
    while i < n {
        if let Some(j) = sentence_break_at(b, i) {
            let mut k = j;
            while k < n && is_ws(b[k]) {
                k += 1;
            }
            if k > pos {
                return k.min(n);
            }
            i = k.max(i + 1);
        } else {
            i += 1;
        }
    }
    n
}

/// Vim `(` — the start of the current sentence, or the PREVIOUS sentence's start when `pos` is already at
/// a start (the greatest sentence start strictly less than `pos`, else `0`).
fn prev_sentence_start(b: &[u8], pos: usize) -> usize {
    let n = b.len();
    let mut best = 0usize;
    let mut i = 0usize;
    while i < n {
        if let Some(j) = sentence_break_at(b, i) {
            let mut k = j;
            while k < n && is_ws(b[k]) {
                k += 1;
            }
            if k < pos && k < n {
                best = k;
            }
            i = k.max(i + 1);
        } else {
            i += 1;
        }
    }
    best
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

/// The leading tag NAME of a tag's interior bytes (the run after `<` or `</`): ASCII letters/digits and
/// `-`/`_`/`:`, stopping at whitespace, `/`, or end. `<div class=x>` → `div`; `</div>` interior → `div`.
fn tag_name(interior: &[u8]) -> &[u8] {
    let end = interior
        .iter()
        .position(|&c| !(c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':')))
        .unwrap_or(interior.len());
    &interior[..end]
}

/// The HTML/XML tag block enclosing `cur` (Vim `it`/`at`) — a nesting-aware byte scan (no syntax tree
/// needed, like bracket matching). `around` = `at` (include the `<tag>`/`</tag>` delimiters), else `it`
/// (the content between them). `count` picks how many nesting levels to climb from the innermost enclosing
/// pair: `1` (bare `it`/`at`) is the innermost, `2` (`2it`/`2at`) its enclosing tag, and so on (Vim's
/// count-expands-outward semantics). Returns `(cur, cur)` (a no-op) when the cursor is not inside any tag
/// pair, or when `count` exceeds the enclosing nesting depth (`3dit` on a 2-deep block is a no-op in Vim).
/// The returned offsets sit at ASCII `<`/`>` boundaries, so they are always char-boundary safe.
fn tag_span(b: &[u8], cur: usize, around: bool, count: u32) -> (usize, usize) {
    let n = b.len();
    // Open tags awaiting their close: (name, open_start, open_end-exclusive).
    let mut stack: Vec<(&[u8], usize, usize)> = Vec::new();
    // Every completed pair CONTAINING `cur`, as (open_start, open_end, close_start, close_end). Collected
    // as their closes are found; later sorted innermost-first so `count` can index outward.
    let mut enclosing: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i + 1;
        while j < n && b[j] != b'>' {
            j += 1;
        }
        if j >= n {
            break; // unterminated `<…`
        }
        let end = j + 1; // one past the `>`
        let interior = &b[start + 1..j];
        if interior.first() == Some(&b'/') {
            // Close tag `</name>`: pop to the matching open, forming a pair.
            let name = tag_name(&interior[1..]);
            while let Some((oname, os, oe)) = stack.pop() {
                if oname == name {
                    if os <= cur && cur < end {
                        enclosing.push((os, oe, start, end));
                    }
                    break;
                }
                // Mismatched close (malformed markup): keep popping to recover.
            }
        } else if interior.last() != Some(&b'/') {
            // Open tag `<name …>` (a `<name/>` self-close has no content and is skipped).
            let name = tag_name(interior);
            if !name.is_empty() {
                stack.push((name, start, end));
            }
        }
        i = end;
    }
    // Innermost first (smallest span). `count == 1` picks it; each extra count climbs one level out.
    enclosing.sort_by_key(|&(os, _, _, ce)| ce - os);
    match enclosing.get((count.max(1) - 1) as usize) {
        Some(&(os, _, _, ce)) if around => (os, ce),
        Some(&(_, oe, cs, _)) => (oe, cs),
        None => (cur, cur),
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

/// The byte offset just past the last line that holds content. A buffer ending in `\n` has that newline as
/// the last line's TERMINATOR, not the start of a new empty line (Vim's model), so the trailing (phantom)
/// slot at `b.len()` is excluded. Vertical / `G` / `+`-family motions never step past this — `G` on
/// `"abc\n"` lands on `abc`, not on a blank line below it. Equals `b.len()` when there is no trailing `\n`.
pub(crate) fn content_end(b: &[u8]) -> usize {
    if b.last() == Some(&b'\n') {
        b.len() - 1
    } else {
        b.len()
    }
}

fn down(b: &[u8], cur: usize) -> usize {
    let le = line_end(b, cur);
    // Stop at the last content line: `le >= content_end` covers both a no-newline last line (`le == len`)
    // and a trailing-newline last line (`le == len-1`), so `j` never lands on the phantom empty slot.
    if le >= content_end(b) {
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
            if le >= content_end(b) {
                break; // don't descend onto the phantom line after a trailing '\n'
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

/// The last non-blank char of the line containing `pos` (Vim `g_`), or the line start when the line is
/// blank/empty. Lands ON the char (like `$`), not past it.
fn last_non_blank(b: &[u8], pos: usize) -> usize {
    let ls = line_start(b, pos);
    let le = line_end(b, ls);
    let mut i = le;
    while i > ls {
        let p = prev_boundary(b, i);
        if b[p] != b' ' && b[p] != b'\t' {
            return p;
        }
        i = p;
    }
    ls
}

/// Byte start of the `n`-th line (1-based), clamped to the last line.
pub(crate) fn nth_line_start(b: &[u8], n: u32) -> usize {
    let target = n.max(1) - 1; // 0-based
    let mut seen = 0u32;
    let mut ls = 0usize;
    while seen < target {
        let le = line_end(b, ls);
        if le >= content_end(b) {
            break; // no more content lines; clamp to the last real one (never the trailing phantom)
        }
        ls = le + 1;
        seen += 1;
    }
    ls
}

/// Byte start of the last content line (the trailing phantom slot after a final `\n` is not a line).
pub(crate) fn last_line_start(b: &[u8]) -> usize {
    line_start(b, content_end(b))
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

/// The `count`-th UNMATCHED bracket of one type from the cursor — Vim `[(`/`])` (parens) and `[{`/`]}`
/// (braces). `forward` scans right from `cursor+1` for an unmatched CLOSE (`])`/`]}`); otherwise it scans
/// left from `cursor-1` for an unmatched OPEN (`[(`/`[{`). Nesting is same-type: a balanced pair is skipped
/// (a `)` seen while scanning left raises the depth so its matching `(` is not counted), and other bracket
/// types are ignored — exactly as Vim's `[(` skips `{}`/`[]`. Returns `cursor` (a no-op) when there is no
/// such unmatched bracket. Brackets are ASCII → the result is always a char boundary.
fn unmatched_bracket(
    b: &[u8],
    cursor: usize,
    open: u8,
    close: u8,
    forward: bool,
    count: u32,
) -> usize {
    let mut remaining = count.max(1);
    let mut depth = 0u32;
    if forward {
        let mut i = cursor.saturating_add(1); // strictly after the cursor (an on-cursor close does not count)
        while i < b.len() {
            if b[i] == open {
                depth += 1;
            } else if b[i] == close {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        return i;
                    }
                } else {
                    depth -= 1;
                }
            }
            i += 1;
        }
        cursor
    } else {
        let mut i = cursor.min(b.len()); // decremented before each read → starts strictly before the cursor
        while i > 0 {
            i -= 1;
            if b[i] == close {
                depth += 1;
            } else if b[i] == open {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        return i;
                    }
                } else {
                    depth -= 1;
                }
            }
        }
        cursor
    }
}

/// The (open, close, forward) triple for an unmatched-bracket motion, or `None` for any other motion.
fn unmatched_spec(m: Motion) -> Option<(u8, u8, bool)> {
    match m {
        Motion::UnmatchedParenBack => Some((b'(', b')', false)),
        Motion::UnmatchedParenFwd => Some((b'(', b')', true)),
        Motion::UnmatchedBraceBack => Some((b'{', b'}', false)),
        Motion::UnmatchedBraceFwd => Some((b'{', b'}', true)),
        _ => None,
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
    // `{count}go`: the count is an absolute 1-based BYTE offset (not a repeat); a count past EOF lands on the
    // LAST byte (Vim never rests the cursor past the final char). Snapped to a char boundary.
    if m == Motion::GotoByte {
        return snap(
            b,
            (n as usize)
                .saturating_sub(1)
                .min(b.len().saturating_sub(1)),
        );
    }
    // Bracket match jumps to a single computed position (count is not a repeat; `count%` is deferred).
    if m == Motion::MatchBracket {
        return match_bracket(b, cur0).unwrap_or(cur0);
    }
    // `{count}%`: the count is a PERCENTAGE — go to line `(count*line_count + 99)/100` (Vim), first
    // non-blank. `line_count` follows the terminator model (a trailing `\n` is not a new line).
    if m == Motion::GotoPercent {
        let nl = b.iter().filter(|&&c| c == b'\n').count();
        let line_count = if b.is_empty() {
            1
        } else if b.last() == Some(&b'\n') {
            nl
        } else {
            nl + 1
        };
        let line = ((n as usize) * line_count)
            .div_ceil(100)
            .clamp(1, line_count) as u32;
        return first_non_blank(b, nth_line_start(b, line));
    }
    // `{count}g_`: the count is `count-1` lines down, then the last non-blank char (not a repeat).
    if m == Motion::LineLastNonBlank {
        let mut ls = line_start(b, cur0);
        for _ in 1..n {
            let le = line_end(b, ls);
            if le >= b.len() {
                break;
            }
            ls = le + 1;
        }
        return last_non_blank(b, ls);
    }
    // `+`/`<CR>` (down), `-` (up), `_` (count-1 down): the count is a line delta (not a repeat), then the
    // first non-blank char of the target line. Clamped at the buffer edges.
    if matches!(
        m,
        Motion::DownFirstNonBlank | Motion::UpFirstNonBlank | Motion::LineUnderscore
    ) {
        let delta: isize = match m {
            Motion::DownFirstNonBlank => n as isize,
            Motion::LineUnderscore => (n - 1) as isize,
            _ => -(n as isize), // UpFirstNonBlank
        };
        let mut ls = line_start(b, cur0);
        if delta >= 0 {
            for _ in 0..delta {
                let le = line_end(b, ls);
                if le >= content_end(b) {
                    break; // clamp at the last content line, not the trailing phantom
                }
                ls = le + 1;
            }
        } else {
            for _ in 0..(-delta) {
                if ls == 0 {
                    break;
                }
                ls = line_start(b, ls - 1);
            }
        }
        return first_non_blank(b, ls);
    }
    // Section motions (`]]`/`[[`/`][`/`[]`): jump `count` section boundaries and rest on the target line's
    // first non-blank (column 0 for a brace/form-feed line). The count is a repeat, resolved by the scanner.
    if matches!(
        m,
        Motion::SectionFwd | Motion::SectionBack | Motion::SectionEndFwd | Motion::SectionEndBack
    ) {
        let ls = match m {
            Motion::SectionFwd => next_section_line(b, cur0, n, false),
            Motion::SectionEndFwd => next_section_line(b, cur0, n, true),
            Motion::SectionBack => prev_section_line(b, cur0, n, false),
            _ => prev_section_line(b, cur0, n, true), // SectionEndBack
        };
        return first_non_blank(b, ls);
    }
    // Unmatched-bracket motions (`[(`/`])`/`[{`/`]}`): the count is a repeat (levels to step out); the scanner
    // resolves it directly to a single target (or `cur0` when there is no enclosing unmatched bracket).
    if let Some((open, close, forward)) = unmatched_spec(m) {
        return unmatched_bracket(b, cur0, open, close, forward, n);
    }
    // `{count}|`: the count is the 1-based display column of the current line (not a repeat).
    if m == Motion::Column {
        return at_col(b, line_start(b, cur0), (n - 1) as usize);
    }
    // `gM` / `{count}gM`: the count is a PERCENTAGE of the line's character length (not a repeat); land on
    // char index `min(count*chars/100, chars-1)`. The frontend passes 50 for a bare `gM`.
    if m == Motion::MidLine {
        let ls = line_start(b, cur0);
        let chars = col_of(b, ls, line_end(b, cur0)); // char count of the line (excludes the newline)
        if chars == 0 {
            return ls; // empty line — nowhere to go
        }
        let idx = ((n as usize) * chars / 100).min(chars - 1);
        return at_col(b, ls, idx);
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
            Motion::SentenceFwd => next_sentence_start(b, c),
            Motion::SentenceBack => prev_sentence_start(b, c),
            Motion::WordFwd => next_word_start(b, c, false),
            Motion::WordBack => prev_word_start(b, c, false),
            Motion::WordEnd => prev_boundary(b, word_end_excl(b, c, false)), // land ON the last char
            Motion::WordEndBack => prev_boundary(b, prev_word_end_excl(b, c, false)), // Vim `ge`
            Motion::EmacsWordFwd => emacs_word_fwd(b, c), // Emacs forward-word: two-class, land AFTER the word
            Motion::EmacsWordBack => emacs_word_back(b, c), // Emacs backward-word: two-class mirror
            Motion::BigWordFwd => next_word_start(b, c, true),
            Motion::BigWordBack => prev_word_start(b, c, true),
            Motion::BigWordEnd => prev_boundary(b, word_end_excl(b, c, true)),
            Motion::BigWordEndBack => prev_boundary(b, prev_word_end_excl(b, c, true)), // Vim `gE`
            // FindChar / line-jumps are resolved by the early returns above; objects/linewise have no
            // bare-move target.
            Motion::FindChar { .. }
            | Motion::GotoLine
            | Motion::LastLine
            | Motion::DownFirstNonBlank
            | Motion::UpFirstNonBlank
            | Motion::LineUnderscore
            | Motion::GotoByte
            | Motion::MatchBracket
            | Motion::GotoPercent
            | Motion::LineLastNonBlank
            | Motion::Column
            | Motion::MidLine
            | Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::SectionFwd
            | Motion::SectionBack
            | Motion::SectionEndFwd
            | Motion::SectionEndBack
            | Motion::UnmatchedParenBack
            | Motion::UnmatchedParenFwd
            | Motion::UnmatchedBraceBack
            | Motion::UnmatchedBraceFwd
            | Motion::Pair { .. }
            | Motion::Quote { .. }
            | Motion::Tag { .. }
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
        Motion::Right => (cur, target(b, cur, m, n)),
        // `dw`/`dW`: Vim does NOT let an `w`/`W` operator cross the line's newline. When the motion would
        // land on a later line, the operated text ends at the END OF THIS LINE — through any trailing
        // whitespace, but before the newline — so `dw` on the last word EMPTIES the line instead of joining
        // it to the next. (Captured from nvim: `dw` on ["foo","bar"] → ["","bar"], on ["foo   ","bar"] →
        // ["","bar"] deleting "foo   ".) When the motion stays on the line, the target is unchanged.
        Motion::WordFwd | Motion::BigWordFwd => {
            let t = target(b, cur, m, n);
            let le = line_end(b, cur);
            (cur, if t > le { le } else { t })
        }
        // `d$` reaches the line end (deletes the last char too), unlike the bare `$` move which stops on it.
        Motion::LineEnd => (cur, line_end(b, cur)),
        // Paragraph motions as a charwise span (`c}` / a Visual `}` extend): forward → [cursor, target),
        // backward → [target, cursor). The operator's exclusive-linewise reshaping lives in `editor::op_span`.
        Motion::ParagraphFwd => (cur, target(b, cur, m, n)),
        Motion::ParagraphBack => (target(b, cur, m, n), cur),
        // Sentence motions are exclusive charwise: forward → [cursor, next), backward → [prev, cursor).
        Motion::SentenceFwd => (cur, target(b, cur, m, n)),
        Motion::SentenceBack => (target(b, cur, m, n), cur),
        // Section motions as a charwise span (`c]]` / a Visual `]]` extend): forward → [cursor, target),
        // backward → [target, cursor). The operator's exclusive-linewise reshaping lives in `editor::op_span`
        // (shared with the paragraph motions), so `d]]`/`d[[` come out linewise exactly like `d}`/`d{`.
        Motion::SectionFwd | Motion::SectionEndFwd => (cur, target(b, cur, m, n)),
        Motion::SectionBack | Motion::SectionEndBack => (target(b, cur, m, n), cur),
        // Unmatched-bracket motions are EXCLUSIVE charwise (Vim): forward (`])`/`]}`) → [cursor, target),
        // backward (`[(`/`[{`) → [target, cursor). The exclusive-linewise reduction that can make them
        // whole-line (a target at column 0) lives in `editor::op_span`, shared with the paragraph/section
        // motions. A no-op target (== cursor) collapses to an empty range there.
        Motion::UnmatchedParenFwd | Motion::UnmatchedBraceFwd => (cur, target(b, cur, m, n)),
        Motion::UnmatchedParenBack | Motion::UnmatchedBraceBack => (target(b, cur, m, n), cur),
        // inclusive end-of-word (Vim `de` / `dE`).
        Motion::WordEnd | Motion::BigWordEnd => {
            let big = m == Motion::BigWordEnd;
            let mut e = cur;
            for _ in 0..n {
                e = word_end_excl(b, e, big);
            }
            (cur, e)
        }
        // Emacs `kill-word` (`M-d`): `[cursor, emacs_word_fwd)` — the two-class forward-word span, which
        // (unlike Vim `de`) skips a leading non-word run and stops at `_`/punctuation boundaries.
        Motion::EmacsWordFwd => {
            let mut e = cur;
            for _ in 0..n {
                e = emacs_word_fwd(b, e);
            }
            (cur, e)
        }
        // backward / leftward → [target, cursor)
        Motion::Left
        | Motion::WordBack
        | Motion::EmacsWordBack
        | Motion::BigWordBack
        | Motion::LineStart => (target(b, cur, m, n), cur),
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
        // `^` / `|` / `go` / `gM` are exclusive and can point either way (indent / column / byte / line-%,
        // left or right): ordered pair from the cursor to the absolute target.
        Motion::LineFirstNonBlank | Motion::Column | Motion::GotoByte | Motion::MidLine => {
            let t = target(b, cur, m, n);
            (cur.min(t), cur.max(t))
        }
        // `g_` is INCLUSIVE (Vim `dg_` deletes through the last non-blank char).
        Motion::LineLastNonBlank => {
            let t = target(b, cur, m, n);
            (cur.min(t), next_boundary(b, cur.max(t)))
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
        Motion::InnerWord => inner_word_span_n(b, cur, false, n),
        Motion::AWord => a_word_span_n(b, cur, false, n),
        Motion::InnerBigWord => inner_word_span_n(b, cur, true, n),
        Motion::ABigWord => a_word_span_n(b, cur, true, n),
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
        Motion::Tag { around } => tag_span(b, cur, around, n),
        // vertical / linewise motions are not charwise — callers handle Line / line-jumps specially
        Motion::Up
        | Motion::Down
        | Motion::Line
        | Motion::GotoLine
        | Motion::LastLine
        | Motion::DownFirstNonBlank
        | Motion::UpFirstNonBlank
        | Motion::LineUnderscore
        | Motion::GotoPercent => (cur, cur),
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

#[cfg(test)]
mod gunderscore_column_tests {
    //! Vim `g_` (last non-blank) and `|` (go to column).
    use super::{char_span, target, Motion};

    #[test]
    fn g_underscore_lands_on_last_non_blank() {
        let b = b"  foo bar  \nx\n"; // line 0: 'r' is the last non-blank at index 8
        assert_eq!(target(b, 0, Motion::LineLastNonBlank, 1), 8);
        assert_eq!(
            target(b, 8, Motion::LineLastNonBlank, 1),
            8,
            "idempotent on the char"
        );
        // `dg_` is inclusive: from 'f' (index 2) through 'r' → "foo bar".
        assert_eq!(char_span(b, 2, Motion::LineLastNonBlank, 1), (2, 9));
    }

    #[test]
    fn pipe_goes_to_the_count_th_column() {
        let b = b"abcdef\n";
        assert_eq!(target(b, 0, Motion::Column, 1), 0, "1| = column 1");
        assert_eq!(target(b, 0, Motion::Column, 5), 4, "5| = column 5");
        // Past the line end clamps to the last column.
        assert_eq!(target(b, 0, Motion::Column, 99), 6);
        // `d5|` from column 1 spans the ordered pair [0, 4).
        assert_eq!(char_span(b, 0, Motion::Column, 5), (0, 4));
    }

    #[test]
    fn gm_lands_at_the_percent_char_of_the_line() {
        // Landings VERIFIED against nvim v0.12.4 (`gM` = 50% by char count; 0-based index = 50*L/100).
        // "abcdefghij" (L=10): bare gM (count 50) → index 5 ('f').
        let b = b"abcdefghij\n";
        assert_eq!(target(b, 0, Motion::MidLine, 50), 5, "gM = middle char");
        assert_eq!(target(b, 9, Motion::MidLine, 50), 5, "gM ignores start col");
        // `{count}gM` as a percentage (nvim: 25gM→col3/idx2, 100gM→last/idx9, 10gM→idx1, 1gM→idx0).
        assert_eq!(target(b, 0, Motion::MidLine, 25), 2, "25gM");
        assert_eq!(
            target(b, 0, Motion::MidLine, 100),
            9,
            "100gM clamps to last char"
        );
        assert_eq!(target(b, 0, Motion::MidLine, 10), 1, "10gM");
        assert_eq!(target(b, 0, Motion::MidLine, 1), 0, "1gM");
        // Odd length floors: "abcde" (L=5) → 50*5/100 = 2 (nvim col 3).
        let odd = b"abcde\n";
        assert_eq!(
            target(odd, 0, Motion::MidLine, 50),
            2,
            "gM on a 5-char line"
        );
        // Single char / empty line: stay put at the line start.
        assert_eq!(target(b"a\n", 0, Motion::MidLine, 50), 0);
        assert_eq!(target(b"\n", 0, Motion::MidLine, 50), 0, "empty line");
        // `dgM` is exclusive charwise (nvim: `0dgM` on "abcdefghij" → "fghij" = span [0,5); `$dgM` → [5,9)).
        assert_eq!(char_span(b, 0, Motion::MidLine, 50), (0, 5));
        assert_eq!(char_span(b, 9, Motion::MidLine, 50), (5, 9));
        // gM counts leading whitespace too: "    abcdef" (L=10) → idx 5 (nvim col 6).
        assert_eq!(target(b"    abcdef\n", 0, Motion::MidLine, 50), 5);
    }
}

#[cfg(test)]
mod line_first_non_blank_motion_tests {
    //! Vim `+` / `<CR>` (down), `-` (up), `_` (count-1 down) — line motions landing on the first
    //! non-blank of the target line. (Operator linewise-ness is covered in `editor::tests`.)
    use super::{target, Motion};

    // "a\n  bc\n   d\nef\n": line starts 0,2,7,12; first-non-blanks 0,4,10,12.
    const B: &[u8] = b"a\n  bc\n   d\nef\n";

    #[test]
    fn plus_goes_count_lines_down_to_first_non_blank() {
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 1),
            4,
            "+ -> line 1 'b'"
        );
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 2),
            10,
            "2+ -> line 2 'd'"
        );
        // Past the last line clamps to the last CONTENT line ('ef' at byte 12). The trailing '\n' is a
        // terminator, not a navigable empty line (Vim), so `+` never lands on the phantom slot at byte 15.
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 99),
            12,
            "clamp at last content line"
        );
    }

    #[test]
    fn minus_goes_count_lines_up_to_first_non_blank() {
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 1),
            10,
            "- -> line 2 'd'"
        );
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 3),
            0,
            "3- -> line 0 'a'"
        );
        // Past the first line clamps to line 0.
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 99),
            0,
            "clamp at first line"
        );
    }

    #[test]
    fn underscore_is_count_minus_one_lines_down() {
        // `_` alone == `^` (first non-blank of the current line), no vertical move.
        assert_eq!(
            target(B, 5, Motion::LineUnderscore, 1),
            target(B, 5, Motion::LineFirstNonBlank, 1),
            "_ == ^"
        );
        // `2_` moves one line down; `3_` two lines down.
        assert_eq!(
            target(B, 4, Motion::LineUnderscore, 2),
            10,
            "2_ -> one line down"
        );
        assert_eq!(
            target(B, 0, Motion::LineUnderscore, 3),
            10,
            "3_ -> two lines down"
        );
    }
}

#[cfg(test)]
mod tag_object_tests {
    //! Vim `it`/`at` — HTML/XML tag text objects (nesting-aware byte scan).
    use super::{char_span, Motion};

    fn tag(around: bool) -> Motion {
        Motion::Tag { around }
    }

    #[test]
    fn inner_and_around_tag() {
        let b = b"<div>hello</div>"; // content "hello" at [5,10); whole at [0,16)
        assert_eq!(char_span(b, 6, tag(false), 1), (5, 10), "it = content");
        assert_eq!(char_span(b, 6, tag(true), 1), (0, 16), "at = incl. tags");
        // Cursor ON a tag still resolves the block.
        assert_eq!(char_span(b, 1, tag(false), 1), (5, 10));
    }

    #[test]
    fn nesting_picks_the_innermost() {
        let b = b"<a><b>x</b></a>";
        assert_eq!(
            char_span(b, 6, tag(false), 1),
            (6, 7),
            "it = innermost content 'x'"
        );
        assert_eq!(
            char_span(b, 6, tag(true), 1),
            (3, 11),
            "at = innermost '<b>x</b>'"
        );
    }

    #[test]
    fn no_enclosing_tag_is_a_noop() {
        assert_eq!(char_span(b"plain text", 3, tag(false), 1), (3, 3));
        // A self-closing tag has no content to enclose the cursor.
        assert_eq!(char_span(b"<br/>x", 5, tag(true), 1), (5, 5));
    }

    #[test]
    fn count_climbs_outward_one_level_per_count() {
        let b = b"<a><b>x</b></a>"; // <a>[0,15), <b>[3,11), content 'x' at [6,7)
        assert_eq!(char_span(b, 6, tag(false), 1), (6, 7), "1it = innermost");
        assert_eq!(char_span(b, 6, tag(false), 2), (3, 11), "2it = <a>'s inner");
        assert_eq!(char_span(b, 6, tag(true), 2), (0, 15), "2at = whole <a>");
        // Count beyond the nesting depth is a no-op (Vim).
        assert_eq!(char_span(b, 6, tag(false), 3), (6, 6));
    }
}

#[cfg(test)]
mod sentence_motion_tests {
    //! Vim `)` / `(` — forward/backward to a sentence start.
    use super::{char_span, target, Motion};

    #[test]
    fn sentence_forward_and_back_find_starts() {
        let b = b"Hello. World! Foo?"; // starts at 0 (Hello), 7 (World), 14 (Foo)
        assert_eq!(target(b, 0, Motion::SentenceFwd, 1), 7);
        assert_eq!(target(b, 7, Motion::SentenceFwd, 1), 14);
        assert_eq!(
            target(b, 14, Motion::SentenceFwd, 1),
            18,
            "past the last → buffer end"
        );
        // `(` from mid-sentence → that sentence's start; from a start → the previous start.
        assert_eq!(target(b, 10, Motion::SentenceBack, 1), 7);
        assert_eq!(target(b, 7, Motion::SentenceBack, 1), 0);
        // Counted: two sentences forward.
        assert_eq!(target(b, 0, Motion::SentenceFwd, 2), 14);
    }

    #[test]
    fn d_close_paren_spans_to_next_sentence() {
        // `d)` from the start deletes up to (exclusive) the next sentence: "Hello. " → "World! Foo?".
        assert_eq!(
            char_span(b"Hello. World! Foo?", 0, Motion::SentenceFwd, 1),
            (0, 7)
        );
    }
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
