//! Motions — the geometry half of the editing grammar (C-EDITLANG). A motion resolves, from the cursor, to
//! a target position (a bare move like `w`) or to a byte range (an operator like `dw`). It also owns the
//! shared char-boundary / line helpers used across the editor.
//!
//! Word motions come in two flavors: small-word (`w`/`b`/`e`) split where Vim's character class changes
//! (blank / punctuation / word / emoji / one class per script, [`crate::charclass::vim_class`], #1136), and
//! WORD (`W`/`B`/`E`) split on blanks only. They step one grapheme cluster at a time, as nvim's `inc()` /
//! `dec()` do, and a cluster takes the class of its first char. All positions land on char boundaries.

use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// A motion in the editing grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    /// Vim `h` / `<Left>`: one char left, never past the line's first char (a count stops there).
    Left,
    /// Vim `l` / `<Right>`: one char right, never past the line end. A bare Normal-mode move is pulled back
    /// onto the last char by the planner; Insert may rest after it, and an operator span ends at the newline.
    Right,
    /// Vim `<BS>` with nvim's default `'whichwrap'` `b`: like `h`, but from the first char of a line it
    /// continues on the previous line's last char. Under an operator the newline itself is one step, so
    /// `d<BS>` at column 0 joins the line onto the one above.
    WrapLeft,
    /// Vim `<Space>` with nvim's default `'whichwrap'` `s`: like `l`, but from the last char of a line it
    /// continues on the next line's first char. Under an operator it stays on the line, exactly like `l`.
    WrapRight,
    /// Emacs `backward-char` with a count (`C-u 3 C-b`): one char back per step, the newline included, so
    /// it crosses lines (Emacs point is between chars, D-050).
    EmacsCharBack,
    /// Emacs `forward-char` with a count (`C-u 3 C-f`): one char forward per step, crossing lines.
    EmacsCharFwd,
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
    /// short of it. `forward` picks the direction, `till` picks find-vs-till. Repeated by `;`/`,`, which set
    /// `repeat`: a repeated `t`/`T` with count 1 skips a match right next to the cursor, so `;` advances
    /// past the char the last `t` stopped before (Vim without `cpo-;`, #802).
    FindChar {
        ch: char,
        forward: bool,
        till: bool,
        repeat: bool,
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
    /// Vim `]m` — go to the `count`-th next START of a method (brace block). Pure brace navigation
    /// (ruse has no language model): a faithful port of Vim's `nv_bracket_block` two-phase algorithm.
    /// EXCLUSIVE charwise under an operator (`d]m`), sharing the exclusive-linewise reduction with the
    /// section / unmatched-brace motions. See [`method_target`] for the exact algorithm and the
    /// string/comment divergence.
    MethodStartFwd,
    /// Vim `[m` — go to the `count`-th previous method start. The backward mirror of [`Motion::MethodStartFwd`].
    MethodStartBack,
    /// Vim `]M` — go to the `count`-th next method END (the matching `}`). As [`Motion::MethodStartFwd`].
    MethodEndFwd,
    /// Vim `[M` — go to the `count`-th previous method end. The backward mirror of [`Motion::MethodEndFwd`].
    MethodEndBack,
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
        Ok(s) if s.is_char_boundary(pos.saturating_sub(ls).min(s.len())) => {
            let mut gc = GraphemeCursor::new(pos.saturating_sub(ls).min(s.len()), s.len(), true);
            match gc.next_boundary(s, 0).ok().flatten() {
                Some(rel) => ls + rel,
                None => next_boundary(b, pos), // window exhausted: cross the newline
            }
        }
        // Not text, or `pos` inside a char: the char step is the safe answer.
        _ => next_boundary(b, pos),
    }
}

/// The byte offset of the previous grapheme-cluster boundary before `pos` (the `h`/`MoveLeft` step).
pub(crate) fn prev_grapheme(b: &[u8], pos: usize) -> usize {
    // Windowed to the current line, for the same reason as [`next_grapheme`]. At the line start the previous
    // boundary is the preceding newline, handled by the char-boundary fallback.
    let ls = line_start(b, pos);
    let le = line_end(b, pos);
    match std::str::from_utf8(&b[ls..le]) {
        Ok(s) if s.is_char_boundary(pos.saturating_sub(ls).min(s.len())) => {
            let mut gc = GraphemeCursor::new(pos.saturating_sub(ls).min(s.len()), s.len(), true);
            match gc.prev_boundary(s, 0).ok().flatten() {
                Some(rel) => ls + rel,
                None => prev_boundary(b, pos), // window exhausted: cross back over the newline
            }
        }
        _ => prev_boundary(b, pos),
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

// Vim's word motions walk a line model in which every line ends in a NUL slot, which is the `\n` byte here
// (or the buffer end of an unterminated last line); the phantom slot after a final `\n` is no line. The
// helpers below mirror `inc()`/`dec()`/`cls()` of Vim's search.c over that model, so an empty line is a
// stop of its own (#746) and an operator's last word stops at its line end (#742).

/// Vim `cls()` of the char at byte `p`: 0 for a blank (a line break and the buffer end included), else
/// its Vim class ([`crate::charclass::vim_class`]: 1 punctuation, 2 word, 3 emoji, a per-script value), or 1
/// for any non-blank when `big`. A combining mark or the rest of a cluster is never asked: callers step by
/// grapheme cluster, so `p` is a cluster's first char. A byte that is not UTF-8 counts as a word char.
pub(crate) fn char_cls(b: &[u8], p: usize, big: bool) -> u32 {
    use crate::charclass::{vim_class, WORD};
    let Some(&lead) = b.get(p) else {
        return 0;
    };
    let class = if lead < 0x80 {
        vim_class(char::from(lead))
    } else {
        let end = next_boundary(b, p);
        std::str::from_utf8(&b[p..end])
            .ok()
            .and_then(|s| s.chars().next())
            .map_or(WORD, vim_class)
    };
    match class {
        _ if lead == b'\n' => 0,
        0 => 0,
        _ if big => 1,
        k => k,
    }
}

/// Vim `cls()` at `p` on Vim's line model: a line's NUL slot is blank.
fn vcls(b: &[u8], p: usize, big: bool) -> u32 {
    if p >= line_end(b, p) {
        return 0;
    }
    char_cls(b, p, big)
}

/// Whether `p` is on an empty line (Vim `col == 0 && LINEEMPTY`).
fn on_empty_line(b: &[u8], p: usize) -> bool {
    let ls = line_start(b, p);
    p == ls && line_end(b, ls) == ls
}

/// Vim `inc()`: the next position and how it got there (0 within the line, 2 onto the line's NUL slot, 1 to
/// the next line's start), or `None` from the last line's NUL slot.
fn vinc(b: &[u8], p: usize) -> Option<(usize, u8)> {
    let le = line_end(b, p);
    if p < le {
        let q = next_grapheme(b, p);
        Some((q, if q < le { 0 } else { 2 }))
    } else if le + 1 < b.len() {
        Some((le + 1, 1))
    } else {
        None
    }
}

/// Vim `dec()`: the previous position (0 within the line, 1 back onto the previous line's NUL slot), or
/// `None` at the buffer start.
fn vdec(b: &[u8], p: usize) -> Option<(usize, u8)> {
    if p > line_start(b, p) {
        Some((prev_grapheme(b, p), 0))
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
fn word_fwd(b: &[u8], pos: usize, big: bool, count: u32, eol: bool) -> usize {
    let mut p = pos.min(b.len());
    for left in (0..count.max(1)).rev() {
        let stop = |i: u8| i >= 1 && eol && left == 0;
        let sclass = vcls(b, p, big);
        let last_line = line_end(b, p) + 1 >= b.len();
        let Some((q, i)) = vinc(b, p) else {
            return p;
        };
        p = q;
        if (i >= 1 && last_line) || stop(i) {
            return p;
        }
        if sclass != 0 {
            while vcls(b, p, big) == sclass {
                let Some((q, i)) = vinc(b, p) else {
                    return p;
                };
                p = q;
                if stop(i) {
                    return p;
                }
            }
        }
        while vcls(b, p, big) == 0 {
            if on_empty_line(b, p) {
                break;
            }
            let Some((q, i)) = vinc(b, p) else {
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
fn next_word_start(b: &[u8], pos: usize, big: bool) -> usize {
    word_fwd(b, pos, big, 1, false)
}

/// Start of the current/previous word / WORD — Vim `b` / `B` (`bck_word`). An empty line is a stop (#746).
fn prev_word_start(b: &[u8], pos: usize, big: bool) -> usize {
    let Some((mut p, _)) = vdec(b, pos.min(b.len())) else {
        return 0;
    };
    // Skip the white space before the word; an empty line is a word of its own.
    while vcls(b, p, big) == 0 {
        if on_empty_line(b, p) {
            return p;
        }
        match vdec(b, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    // Back to the start of this word, then one forward over the overshoot.
    let c = vcls(b, p, big);
    while vcls(b, p, big) == c {
        match vdec(b, p) {
            Some((q, _)) => p = q,
            None => return p,
        }
    }
    vinc(b, p).map_or(p, |(q, _)| q)
}

/// The landing of Vim `ge` / `gE` (`bckend_word`): the last char of the previous word, or an empty line
/// on the way (#746). Stays at the buffer start when there is no earlier word.
fn prev_word_end(b: &[u8], pos: usize, big: bool) -> usize {
    let mut p = pos.min(b.len());
    let sclass = vcls(b, p, big);
    let Some((q, _)) = vdec(b, p) else {
        return p;
    };
    p = q;
    if sclass != 0 {
        while vcls(b, p, big) == sclass {
            match vdec(b, p) {
                Some((q, _)) => p = q,
                None => return p,
            }
        }
    }
    while vcls(b, p, big) == 0 {
        if on_empty_line(b, p) {
            break;
        }
        match vdec(b, p) {
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
    let mut i = next_grapheme(b, pos.min(b.len()));
    while i < b.len() && char_cls(b, i, big) == 0 {
        i = next_grapheme(b, i);
    }
    if i >= b.len() {
        return b.len();
    }
    run_end(b, i, big)
}

/// One past the run of clusters from `i` that share the class of the one at `i`.
fn run_end(b: &[u8], mut i: usize, big: bool) -> usize {
    let cw = char_cls(b, i, big);
    while i < b.len() && char_cls(b, i, big) == cw {
        i = next_grapheme(b, i);
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
    if char_cls(b, pos, big) == 0 {
        return pos; // on blank: not the cw special case — caller uses the ordinary word span
    }
    run_end(b, pos, big)
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

/// The keyword span under (or, failing that, forward on the current line from) the cursor — Vim `*`/`#`/
/// `gd` (nvim's `find_ident_at_pos`). A keyword is a run of characters of one Vim class of 2 or more
/// ([`crate::charclass::vim_class`]), so "API를" holds two keywords, "API" and "를", as for nvim (#745).
/// Returns `None` when the current line has no keyword at or after the cursor.
pub(crate) fn word_under_cursor(b: &[u8], cur: usize) -> Option<(usize, usize)> {
    use crate::charclass::{vim_class, WORD};
    if b.is_empty() {
        return None;
    }
    let i = cur.min(b.len() - 1);
    let (ls, le) = (line_start(b, i), line_end(b, i));
    let Ok(line) = std::str::from_utf8(&b[ls..le]) else {
        return byte_word_under_cursor(b, i);
    };
    let mut col = (i - ls).min(line.len());
    while !line.is_char_boundary(col) {
        col -= 1;
    }
    // 1. The first keyword character at or after the cursor.
    let (off, c) = line[col..]
        .char_indices()
        .find(|&(_, c)| vim_class(c) >= WORD)?;
    let this = vim_class(c);
    let at = col + off;
    // 2. Back to the start of the run of that class, 3. on to its end.
    let start = line[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| vim_class(c) == this)
        .last()
        .map_or(at, |(j, _)| j);
    let end = line[at..]
        .char_indices()
        .find(|&(_, c)| vim_class(c) != this)
        .map_or(line.len(), |(j, _)| at + j);
    Some((ls + start, ls + end))
}

/// [`word_under_cursor`] on a line that is not UTF-8: a keyword is a run of alnum, `_` and non-ASCII
/// bytes.
fn byte_word_under_cursor(b: &[u8], cur: usize) -> Option<(usize, usize)> {
    let mut i = cur;
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
/// caret is not keyword-class or the caret is at a line/buffer start). A keyword is a run of grapheme
/// clusters of one Vim class of 2 or more ([`crate::charclass::vim_class`]), as nvim's
/// `get_normal_compl_info` reads it, so the base of "API를" is "를" (#1136).
pub(crate) fn keyword_before_cursor(b: &[u8], cur: usize) -> (usize, &str) {
    let cur = cur.min(b.len());
    let ls = line_start(b, cur);
    let Ok(line) = std::str::from_utf8(&b[ls..cur]) else {
        return byte_keyword_before_cursor(b, cur);
    };
    let mut start = cur;
    let mut class = None;
    for (i, g) in line.grapheme_indices(true).rev() {
        let k = keyword_class(g);
        if k < crate::charclass::WORD || class.is_some_and(|c| c != k) {
            break;
        }
        class = Some(k);
        start = ls + i;
    }
    (start, &line[start - ls..])
}

/// The Vim class of a grapheme cluster (its first char's), 0 for a line break.
fn keyword_class(g: &str) -> u32 {
    match g.chars().next() {
        None | Some('\n') => 0,
        Some(c) => crate::charclass::vim_class(c),
    }
}

/// [`keyword_before_cursor`] for a line that is not UTF-8: a run of word-class bytes.
fn byte_keyword_before_cursor(b: &[u8], cur: usize) -> (usize, &str) {
    let mut s = cur;
    while s > 0 && class(b[s - 1]) == Class::Word {
        s -= 1;
    }
    (s, std::str::from_utf8(&b[s..cur]).unwrap_or(""))
}

/// Every keyword in `b` as `(start, text)`, in positional order: runs of grapheme clusters of one Vim
/// class of 2 or more (nvim `find_word_end`); a buffer that is not UTF-8 falls back to word-class bytes.
fn keyword_runs(b: &[u8]) -> Vec<(usize, &str)> {
    let mut runs: Vec<(usize, &str)> = Vec::new();
    let Ok(text) = std::str::from_utf8(b) else {
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
        return runs;
    };
    let mut open: Option<(usize, u32)> = None;
    for (i, g) in text.grapheme_indices(true) {
        let k = keyword_class(g);
        if let Some((s, c)) = open {
            if c == k {
                continue;
            }
            runs.push((s, &text[s..i]));
            open = None;
        }
        if k >= crate::charclass::WORD {
            open = Some((i, k));
        }
    }
    if let Some((s, _)) = open {
        runs.push((s, &text[s..]));
    }
    runs
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
    let runs = keyword_runs(b);
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

/// The end of the leading indent (spaces/tabs) on the byte range `[start, end)` — the first offset whose
/// byte is neither a space nor a tab, clamped to `end` when the whole range is blank. Used by whole-line
/// completion, which IGNORES leading indent on both the typed prefix and every candidate line.
fn indent_end(b: &[u8], start: usize, end: usize) -> usize {
    let mut i = start;
    while i < end && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

/// Insert-mode WHOLE-LINE completion source for `i_CTRL-X CTRL-L` (F-003, current-buffer only), in Vim's
/// scan order. Returns `(base, candidates)`: `base` is the caret line's content AFTER its leading indent,
/// up to the caret (the text being completed); each candidate is another buffer line's content after its
/// OWN leading indent — indent is IGNORED for both the match and the inserted text (`:help i_CTRL-X_CTRL-L`,
/// "Indent is ignored") — that STARTS WITH `base`. The order mirrors Vim's backward search: the line just
/// ABOVE the caret, upward to the top, then WRAPPING from the bottom line upward back to the line just
/// below the caret; the caret's own line is never a candidate, and later DUPLICATES (by post-indent
/// content, so lines differing only in indent collapse) are removed — first in scan order wins. Matching is
/// case-sensitive (nvim default `noignorecase`; the 'ignorecase'/'infercase' interaction is out of scope
/// for this slice). C-P walks the same list backward — the caller cycles the index, so only one order is
/// built. Verified against nvim v0.12.4.
pub(crate) fn line_completions(b: &[u8], cur: usize) -> (String, Vec<String>) {
    // `base`: the caret line's post-indent text up to the caret (empty when the caret sits within indent).
    let ls = crate::pos::line_start(b, cur);
    let bi = indent_end(b, ls, cur);
    let base = std::str::from_utf8(&b[bi..cur]).unwrap_or("").to_string();
    // Line geometry: `n` rows total, the caret on row `cr`. Scan backward from `cr` with wraparound.
    // The slot after a final newline is a row only while the caret is on it (a line being typed); else it
    // is no line and offers no empty candidate (#677).
    let cr = crate::pos::line_of(b, cur);
    let last = crate::pos::line_of(b, b.len());
    let n = if b.last() == Some(&b'\n') && cr < last {
        last
    } else {
        last + 1
    };
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 1..n {
        let row = (cr + n - i) % n; // cr-1, cr-2, …, 0, n-1, …, cr+1 — every row but the caret's
        let rs = crate::pos::nth_line_start(b, row);
        let re = crate::pos::line_end(b, rs);
        let ri = indent_end(b, rs, re);
        let Ok(content) = std::str::from_utf8(&b[ri..re]) else {
            continue;
        };
        if !content.starts_with(&base) {
            continue;
        }
        if seen.insert(content.to_string()) {
            out.push(content.to_string());
        }
    }
    (base, out)
}

/// Whether the line starting at `ls` is blank — empty or (a start pointing at) a newline / end of buffer.
/// Paragraph boundaries in Vim are truly-empty lines (a whitespace-only line is *not* a boundary).
fn is_blank_line(b: &[u8], ls: usize) -> bool {
    ls >= b.len() || b[ls] == b'\n'
}

/// The starts of the buffer's lines in Vim's model (the slot after a final newline is not a line).
fn line_starts(b: &[u8]) -> Vec<usize> {
    let end = content_end(b);
    let mut v = vec![0];
    v.extend(
        b.iter()
            .enumerate()
            .filter(|&(i, &c)| c == b'\n' && i < end)
            .map(|(i, _)| i + 1),
    );
    v
}

/// The 0-based line of `pos` among `starts`.
fn line_of_starts(starts: &[usize], pos: usize) -> usize {
    starts.partition_point(|&s| s <= pos).saturating_sub(1)
}

/// The paragraph object (`ip` / `ap`) with `count`, as Vim's `current_par` resolves it for an operator: the
/// 0-based first and last line, or `None` when it fails (`count` paragraphs do not fit before the end, or
/// `ap` starts on blank lines with no paragraph after them, #1089). A white line holds only blanks; a
/// non-white run also stops at a line starting with a form feed (`startPS`).
fn paragraph_lines(b: &[u8], cur: usize, count: u32, include: bool) -> Option<(usize, usize)> {
    let starts = line_starts(b);
    let last = starts.len() - 1;
    let white = |l: usize| {
        let ls = starts[l];
        b[ls..line_end(b, ls)]
            .iter()
            .all(|&c| c == b' ' || c == b'\t')
    };
    let start_ps = |l: usize| {
        let ls = starts[l];
        ls >= b.len() || b[ls] == b'\n' || b[ls] == 0x0C
    };
    let mut start = line_of_starts(&starts, cur);
    let white_in_front = white(start);
    while start > 0 {
        if white_in_front {
            if !white(start - 1) {
                break;
            }
        } else if white(start - 1) || start_ps(start) {
            break;
        }
        start -= 1;
    }
    // `next` is the first line not taken yet (Vim's `end_lnum + 1`): past the white lines at the start.
    let mut next = start;
    while next <= last && white(next) {
        next += 1;
    }
    let mut i = count.max(1);
    if !include && white_in_front {
        i -= 1;
    }
    while i > 0 {
        i -= 1;
        if next > last {
            return None;
        }
        let do_white = !include && white(next);
        if include || !do_white {
            next += 1;
            while next <= last && !white(next) && !start_ps(next) {
                next += 1;
            }
        }
        if i == 0 && white_in_front && include {
            break;
        }
        if include || do_white {
            while next <= last && white(next) {
                next += 1;
            }
        }
    }
    let end = next - 1;
    // `ap` with no white lines after it takes the ones before it.
    if !white_in_front && !white(end) && include {
        while start > 0 && white(start - 1) {
            start -= 1;
        }
    }
    Some((start, end))
}

/// The paragraph object as a linewise byte range `[s, e)` covering whole lines ([`paragraph_lines`]); a
/// failed object is the empty range at the cursor.
fn paragraph_span(b: &[u8], cur: usize, count: u32, around: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let Some((first, last)) = paragraph_lines(b, cur, count, around) else {
        return (cur, cur);
    };
    let starts = line_starts(b);
    let le = line_end(b, starts[last]);
    (starts[first], if le < b.len() { le + 1 } else { le })
}

/// Whether the paragraph object fails ([`paragraph_lines`]).
fn paragraph_fails(b: &[u8], cur: usize, count: u32, around: bool) -> bool {
    !b.is_empty() && paragraph_lines(b, cur, count, around).is_none()
}

/// One `}` step (Vim `findpar`): the start of the next blank line after the cursor's line that follows a
/// non-blank one — from a blank line the run of blanks is skipped first (#748) — or `None` when the scan
/// runs off the last line.
fn next_para_boundary(b: &[u8], pos: usize) -> Option<usize> {
    let mut ls = line_start(b, pos);
    let mut did_skip = false;
    loop {
        if !is_blank_line(b, ls) {
            did_skip = true;
        }
        let le = line_end(b, ls);
        if le + 1 >= b.len() {
            return None;
        }
        ls = le + 1;
        if did_skip && is_blank_line(b, ls) {
            return Some(ls);
        }
    }
}

/// One `{` step: the start of the previous blank line before the cursor's line that precedes a non-blank
/// one (the mirror of [`next_para_boundary`]: a run of blanks above the cursor is skipped first), or `None`
/// when the scan runs off the first line.
fn prev_para_boundary(b: &[u8], pos: usize) -> Option<usize> {
    let mut ls = line_start(b, pos);
    let mut did_skip = !is_blank_line(b, ls);
    while ls > 0 {
        let prev_ls = line_start(b, ls - 1);
        if did_skip && is_blank_line(b, prev_ls) {
            return Some(prev_ls);
        }
        if !is_blank_line(b, prev_ls) {
            did_skip = true;
        }
        ls = prev_ls;
    }
    None
}

/// `{count}}` / `{count}{`: `count` paragraph steps. A step running off the buffer lands on its edge —
/// for `}` the LAST CHAR of the last line (never its newline or the phantom slot after it, #677; Vim's `}`
/// turns inclusive there), for `{` the buffer start — but only as the final step: with count left over the
/// motion fails and stays put (Vim `findpar` returns false, so `d2}` over one paragraph is a no-op).
/// Returns the landing and whether it is that end-of-buffer edge, or `None` when the motion fails.
fn paragraph_landing(b: &[u8], cur: usize, forward: bool, n: u32) -> Option<(usize, bool)> {
    let mut c = cur;
    for k in 0..n {
        let step = if forward {
            next_para_boundary(b, c)
        } else {
            prev_para_boundary(b, c)
        };
        match step {
            Some(t) => c = t,
            None if k + 1 < n => return None,
            None if forward => {
                let le = content_end(b); // the last line's end
                let ls = line_start(b, le);
                return Some((if le > ls { prev_grapheme(b, le) } else { le }, true));
            }
            None => return Some((0, true)),
        }
    }
    Some((c, false))
}

fn paragraph_target(b: &[u8], cur: usize, forward: bool, n: u32) -> usize {
    paragraph_landing(b, cur, forward, n).map_or(cur, |(t, _)| t)
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

/// Pull an on-char (Vim Normal/Visual) caret off the phantom slot after a buffer's final newline (#677).
/// That slot is not a line in Vim's model, so a cursor left there by a motion running off the end (`w`,
/// `}`, `)`, `g_`, …), a linewise delete of the last line, or an undo would make the next typed text land
/// on an extra unterminated line. It moves onto the last line's last char (its newline when that line is
/// empty), where Vim leaves it. Any other position, and every position of an unterminated buffer, is
/// returned unchanged.
#[must_use]
pub fn off_phantom(b: &[u8], pos: usize) -> usize {
    if pos < b.len() || b.last() != Some(&b'\n') {
        return pos;
    }
    let le = b.len() - 1;
    if le > line_start(b, le) {
        prev_grapheme(b, le)
    } else {
        le
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
/// rides each line's end. `down` picks the direction; `count` repeats. `want` is a virtual (screen) column,
/// so a `<Tab>` (`ts`) or a wide char counts its cells, as nvim's `curswant` does (#1090).
pub(crate) fn vmove(
    b: &[u8],
    cursor: usize,
    count: u32,
    down: bool,
    want: usize,
    ts: usize,
) -> usize {
    let mut c = cursor.min(b.len());
    for _ in 0..count.max(1) {
        if down {
            let le = line_end(b, c);
            if le >= content_end(b) {
                break; // don't descend onto the phantom line after a trailing '\n'
            }
            c = crate::editor::want_col(b, le + 1, want, ts);
        } else {
            let ls = line_start(b, c);
            if ls == 0 {
                break;
            }
            c = crate::editor::want_col(b, line_start(b, ls - 1), want, ts);
        }
    }
    c
}

/// The bare-move landing for a char-search (`f`/`F`/`t`/`T`), or `None` if the `count`-th match does not
/// exist on the current line. `f`/`F` land on the char; `t`/`T` land one boundary short of it. Search is
/// confined to the current line (Vim never crosses a newline for these). A `;`/`,` repeat (`repeat`) of a
/// `t`/`T` with count 1 does not match the char right next to the cursor, which is where the last `t`
/// stopped short of (Vim `searchc` without `cpo-;`).
fn find_char_target(
    b: &[u8],
    cursor: usize,
    ch: char,
    forward: bool,
    till: bool,
    repeat: bool,
    count: u32,
) -> Option<usize> {
    let n = count.max(1) as usize;
    let ls = line_start(b, cursor);
    let le = line_end(b, cursor); // newline / end position for this line
    let line = std::str::from_utf8(b.get(ls..le)?).ok()?;
    // A target matches the FIRST char of a cluster only: `f` to a combining mark, an emoji modifier or the
    // second flag letter finds nothing inside a cluster, as nvim v0.12.6 (`f\u{301}` on `xáb` fails; #909).
    let starts = line
        .grapheme_indices(true)
        .filter_map(|(i, g)| g.chars().next().map(|c| (ls + i, c)));
    // `t`/`T` stop one CHARACTER short of the match: a base with its marks, a flag, a ZWJ sequence (#909).
    let skip = (repeat && till && n == 1).then(|| {
        if forward {
            next_grapheme(b, cursor)
        } else {
            prev_grapheme(b, cursor)
        }
    });
    if forward {
        // Matches strictly after the cursor, in order; take the n-th.
        let hit = starts
            .filter(|&(pos, c)| pos > cursor && c == ch && Some(pos) != skip)
            .nth(n - 1)?
            .0;
        Some(if till { prev_grapheme(b, hit) } else { hit })
    } else {
        // Matches strictly before the cursor; take the n-th counting backward.
        let hit = starts
            .filter(|&(pos, c)| pos < cursor && c == ch && Some(pos) != skip)
            .rev()
            .nth(n - 1)?
            .0;
        Some(if till { next_grapheme(b, hit) } else { hit })
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
        let p = prev_grapheme(b, i);
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

/// `findmatchlimit` restricted to the two uses Vim's `nv_bracket_block` makes of it for the method
/// motions: find the single enclosing UNMATCHED brace of the class the cursor is in. `[m`/`[M` search
/// BACKWARD for an unmatched `{`; `]m`/`]M` search FORWARD for an unmatched `}`. Returns `None` when
/// there is no such brace (i.e. `unmatched_bracket` reports a no-op by returning `cursor`).
fn find_enclosing_brace(b: &[u8], cursor: usize, forward: bool) -> Option<usize> {
    let t = unmatched_bracket(b, cursor, b'{', b'}', forward, 1);
    if t == cursor {
        None
    } else {
        Some(t)
    }
}

/// Vim `]m`/`[m`/`]M`/`[M` — the method (brace-block) motions. This is a faithful port of Vim's
/// `nv_bracket_block` (`normal.c`): ruse has NO language model, so the "method" is defined purely by
/// brace structure, exactly as Vim's own algorithm defines it via `findmatchlimit` over `{`/`}`.
///
/// `forward` is true for `]m`/`]M` (search right), false for `[m`/`[M`. `end` is true for `]M`/`[M`
/// (the method END, a `}`), false for `]m`/`[m` (the method START, a `{`).
///
/// The algorithm has two phases, mirroring Vim:
///   * Phase 1 walks OUT of the cursor's nesting to the outermost enclosing brace (`new_pos`), keeping
///     the one level in (`prev_pos`), by repeatedly finding the enclosing unmatched `{` (backward cmds)
///     or `}` (forward cmds).
///   * Phase 2 does a directional char-by-char scan honoring `norm` (true for `[m`/`]M`) to land on the
///     wanted class/method boundary, jumping over other methods via their matching brace.
///
/// Returns the target byte offset, or `cursor` for a no-op / error (Vim beeps and does not move). The
/// result is always an ASCII brace position → a char boundary.
///
/// DIVERGENCE (documented non-goal): braces inside string literals and comments are treated literally.
/// nvim's `findmatch` skips them; ruse has no syntax model, so `]m` over a buffer with a `}` inside a
/// string will stop on that `}`. Verified-identical everywhere else against nvim v0.12.4.
fn method_target(b: &[u8], cursor: usize, forward: bool, end: bool, count: u32) -> usize {
    let old_pos = cursor.min(b.len());
    // findc: '{' for the backward commands, '}' for the forward commands (Vim: cmdchar '[' vs ']').
    let findc_is_open = !forward;
    // norm is TRUE for "[m" and "]M": ((findc == '{') == (nchar == 'm')).
    // nchar == 'm' ⇔ !end. So norm = (findc_is_open == !end).
    let norm = findc_is_open == !end;

    // --- Phase 1: walk out to the outermost enclosing brace. ---
    let mut new_pos: Option<usize> = None;
    let mut prev_pos: Option<usize> = None;
    let mut cur = old_pos;
    // Vim loops 9999 times; nesting depth is bounded by buffer length, so an unbounded walk-out is fine.
    while let Some(pos) = find_enclosing_brace(b, cur, forward) {
        prev_pos = new_pos;
        cur = pos;
        new_pos = Some(pos);
    }

    // --- Phase 2: directional scan for the target boundary. ---
    let mut n = count.max(1) as i64;
    let mut pos: Option<usize>;
    if let Some(pp) = prev_pos {
        // We were inside a method: start from the brace one level inside the class.
        pos = Some(pp);
        cur = pp;
        if norm {
            n -= 1;
        }
    } else {
        pos = None;
        cur = old_pos;
    }

    while n > 0 {
        loop {
            // findc == '{' (backward cmds) scans backward; else forward. Move one char boundary.
            let moved = if findc_is_open {
                if cur == 0 {
                    None
                } else {
                    Some(prev_boundary(b, cur))
                }
            } else {
                let nx = next_boundary(b, cur);
                if nx >= b.len() {
                    None
                } else {
                    Some(nx)
                }
            };
            let Some(nc) = moved else {
                // Ran off the buffer edge.
                if pos.is_none() {
                    return old_pos; // error: nothing found, no move.
                }
                n = 0;
                break;
            };
            cur = nc;
            let c = b[cur];
            if c == b'{' || c == b'}' {
                if (c == if findc_is_open { b'{' } else { b'}' } && norm) || (n == 1 && !norm) {
                    // Found the class boundary we want, or the count-th method boundary.
                    new_pos = Some(cur);
                    pos = Some(cur);
                    n = 0;
                } else if new_pos.is_none() {
                    // Started outside any class and just crossed in — use this boundary.
                    new_pos = Some(cur);
                    pos = Some(cur);
                } else {
                    // Found the start/end of ANOTHER method: jump to its matching brace and go on.
                    match find_enclosing_brace(b, cur, forward) {
                        Some(m) => {
                            pos = Some(m);
                            cur = m;
                        }
                        None => {
                            pos = None;
                            n = 0;
                        }
                    }
                }
                break;
            }
        }
        n -= 1;
    }

    match pos {
        Some(p) => p,
        None => old_pos, // error (Vim beeps): stay put.
    }
}

/// Whether a motion is one of the method (brace-block) family, with `(forward, end)` flags.
fn method_spec(m: Motion) -> Option<(bool, bool)> {
    match m {
        Motion::MethodStartFwd => Some((true, false)),
        Motion::MethodStartBack => Some((false, false)),
        Motion::MethodEndFwd => Some((true, true)),
        Motion::MethodEndBack => Some((false, true)),
        _ => None,
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

/// Whether motion `m` FAILS from `cursor` (Vim's `clearopbeep`: the command is cancelled, the cursor stays
/// and the typeahead is flushed, #1085). `op` is an operator waiting for the motion; `visual` is a Visual
/// selection, where `l` may still step onto the line break. Checked against nvim v0.12.6:
/// - `h`/`l`/`<BS>`/`<Space>` fail when they cannot move at all (a count moves as far as it can); under an
///   operator `h`/`l` are a silent no-op instead (`dh` at column 0).
/// - `j`/`k`/`+`/`-`/`<CR>` fail only on the last (first) line; a count past the edge stops there.
/// - `w`/`W`/`b`/`B`/`ge`/`gE` fail when they cannot move; `e`/`E` when the last count step cannot. Under an
///   operator `w`/`W` and `e`/`E` reach the buffer end instead (`$dw`, `$de`).
/// - `f`/`t`/`F`/`T`/`;`/`,` without the count-th match, `}`/`{`/`)`/`(` with count left over, `%` without a
///   bracket, `[(`/`])`/`[{`/`]}` without an enclosing one, and `{count}$` (count > 1) on the last line.
///
/// Every other motion (text objects, line jumps, `0`, `^`, `$`, `|`, sections, …) never fails here.
#[must_use]
pub(crate) fn fails(
    b: &[u8],
    cursor: usize,
    m: Motion,
    count: u32,
    op: bool,
    visual: bool,
) -> bool {
    let n = count.max(1);
    let cur = cursor.min(b.len());
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let last_line = le >= content_end(b);
    // Where the motion leaves an on-char caret: never on a non-empty line's break or the phantom slot.
    let clamp = |t: usize| {
        let t = off_phantom(b, t.min(b.len()));
        let (tls, tle) = (line_start(b, t), line_end(b, t));
        if t == tle && tle > tls {
            prev_grapheme(b, tle)
        } else {
            t
        }
    };
    let still = || clamp(target(b, cur, m, count)) == clamp(cur);
    match m {
        Motion::Left => !op && cur <= ls,
        Motion::Right if visual => !op && cur >= le,
        Motion::Right => !op && next_grapheme(b, cur) >= le,
        Motion::WrapLeft | Motion::WrapRight => !op && still(),
        Motion::Up | Motion::UpFirstNonBlank => ls == 0,
        Motion::Down | Motion::DownFirstNonBlank => last_line,
        Motion::WordFwd | Motion::BigWordFwd => !op && still(),
        Motion::WordBack | Motion::BigWordBack | Motion::WordEndBack | Motion::BigWordEndBack => {
            still()
        }
        Motion::WordEnd | Motion::BigWordEnd => {
            !op && {
                let before = if n > 1 { target(b, cur, m, n - 1) } else { cur };
                clamp(target(b, cur, m, n)) == clamp(before)
            }
        }
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat,
        } => find_char_target(b, cur, ch, forward, till, repeat, n).is_none(),
        Motion::ParagraphFwd | Motion::ParagraphBack => {
            paragraph_landing(b, cur, m == Motion::ParagraphFwd, n).is_none()
        }
        Motion::SentenceFwd | Motion::SentenceBack => {
            crate::textobj::sentence_motion_fails(b, cur, m == Motion::SentenceFwd, n)
        }
        Motion::MatchBracket => match_bracket(b, cur).is_none(),
        Motion::InnerParagraph | Motion::AParagraph => {
            paragraph_fails(b, cur, n, m == Motion::AParagraph)
        }
        Motion::UnmatchedParenBack
        | Motion::UnmatchedParenFwd
        | Motion::UnmatchedBraceBack
        | Motion::UnmatchedBraceFwd => still(),
        // `{count}$` moves `count - 1` lines down first, failing like `j` only on the last line (`5$` on
        // the line above it still reaches the last line's end).
        Motion::LineEnd => n > 1 && last_line,
        _ => false,
    }
}

/// The cursor target for a bare move (`w`, `3l`, `k`, …), applying `count`.
#[must_use]
pub fn target(b: &[u8], cursor: usize, m: Motion, count: u32) -> usize {
    let n = count.max(1);
    let cur0 = cursor.min(b.len());
    // Char-search resolves the count-th match directly (repeating a single `t` step would stick in place).
    if let Motion::FindChar {
        ch,
        forward,
        till,
        repeat,
    } = m
    {
        return find_char_target(b, cur0, ch, forward, till, repeat, count).unwrap_or(cur0);
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
    // A terminated buffer's last byte is its final newline, so a count past EOF lands on the last line's last
    // char instead (#677).
    if m == Motion::GotoByte {
        let t = off_phantom(b, (n as usize).saturating_sub(1).min(b.len()));
        return snap(b, t.min(b.len().saturating_sub(1)));
    }
    // Bracket match jumps to a single computed position (count is not a repeat; `count%` is deferred).
    if m == Motion::MatchBracket {
        return match_bracket(b, cur0).unwrap_or(cur0);
    }
    // `{count}%`: the count is a PERCENTAGE — go to line `(count*line_count + 99)/100` (Vim), first
    // non-blank. `line_count` follows the terminator model (a trailing `\n` is not a new line).
    if m == Motion::GotoPercent {
        let nl = crate::pos::count_newlines(b);
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
    // Stops at the last line, never the phantom slot after a final newline (#677).
    if m == Motion::LineLastNonBlank {
        return last_non_blank(b, lines_down(b, cur0, n - 1));
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
    // Method (brace-block) motions (`]m`/`[m`/`]M`/`[M`): the count is baked into the two-phase scan
    // (Vim `nv_bracket_block`), which resolves directly to a single target (or `cur0` for a no-op).
    if let Some((forward, end)) = method_spec(m) {
        return method_target(b, cur0, forward, end, n);
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
    // `{count}$` / `{count}<End>`: the end of the line `count - 1` lines down (stopping at the last line).
    if m == Motion::LineEnd && n > 1 {
        return target(b, lines_down(b, cur0, n - 1), m, 1);
    }
    // `)` / `(`: Vim's `findsent`, ported in `textobj` for `is`/`as`, where an empty line is a sentence of
    // its own (`)` from `foo bar` stops on the blank line below it).
    if matches!(m, Motion::SentenceFwd | Motion::SentenceBack) {
        return crate::textobj::sentence_motion(b, cur0, m == Motion::SentenceFwd, n);
    }
    // `}` / `{`: a count that runs out of paragraphs fails as a whole (see `paragraph_target`).
    if matches!(m, Motion::ParagraphFwd | Motion::ParagraphBack) {
        return paragraph_target(b, cur0, m == Motion::ParagraphFwd, n);
    }
    let mut c = cur0;
    for _ in 0..n {
        let prev = c;
        c = match m {
            // `h`/`l` are bounded by the line; the count stops at its edge.
            // A step is one grapheme cluster (F-002, #696): a composed char or a ZWJ emoji is one char.
            Motion::Left if c > line_start(b, c) => prev_grapheme(b, c),
            Motion::Right if c < line_end(b, c) => next_grapheme(b, c),
            Motion::Left | Motion::Right => c,
            Motion::WrapLeft => wrap_left(b, c),
            Motion::WrapRight => wrap_right(b, c),
            Motion::EmacsCharBack => prev_boundary(b, c),
            Motion::EmacsCharFwd => next_boundary(b, c),
            Motion::Up => up(b, c),
            Motion::Down => down(b, c),
            Motion::LineStart => line_start(b, c),
            // Vim `^` / Emacs `back-to-indentation` (M-m): the first non-blank of the line (or the line end
            // when the line is all blank). Count-agnostic — repeating stays on the same line.
            Motion::LineFirstNonBlank => first_non_blank(b, line_start(b, c)),
            // A bare `$` lands ON the last char of the line (Vim never rests the cursor past it in Normal),
            // unlike the `d$` operator span which reaches the line end — see `char_span`. `{count}$` is the
            // end of the line `count - 1` below (resolved by the early return above).
            Motion::LineEnd => {
                let ls = line_start(b, c);
                let le = line_end(b, c);
                if le > ls {
                    prev_grapheme(b, le)
                } else {
                    le
                }
            }
            Motion::WordFwd => next_word_start(b, c, false),
            Motion::WordBack => prev_word_start(b, c, false),
            Motion::WordEnd => prev_grapheme(b, word_end_excl(b, c, false)), // land ON the last char
            Motion::WordEndBack => prev_word_end(b, c, false),               // Vim `ge`
            Motion::EmacsWordFwd => emacs_word_fwd(b, c), // Emacs forward-word: two-class, land AFTER the word
            Motion::EmacsWordBack => emacs_word_back(b, c), // Emacs backward-word: two-class mirror
            Motion::BigWordFwd => next_word_start(b, c, true),
            Motion::BigWordBack => prev_word_start(b, c, true),
            Motion::BigWordEnd => prev_grapheme(b, word_end_excl(b, c, true)),
            Motion::BigWordEndBack => prev_word_end(b, c, true), // Vim `gE`
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
            | Motion::MethodStartFwd
            | Motion::MethodStartBack
            | Motion::MethodEndFwd
            | Motion::MethodEndBack
            | Motion::Pair { .. }
            | Motion::Quote { .. }
            | Motion::Tag { .. }
            | Motion::ParagraphFwd
            | Motion::ParagraphBack
            | Motion::SentenceFwd
            | Motion::SentenceBack
            | Motion::Line => c,
        };
        // Every step is a pure function of `(b, c)`, so a step that did not move never will: stop. Without
        // this a hostile count (`emacs_kill_word 4294967295` in a trace, or a huge `C-u` prefix) spun for
        // minutes at the buffer edge (#907).
        if c == prev {
            break;
        }
    }
    c
}

/// The start of the line `n` lines below `pos`'s line, stopping at the last line. A trailing newline
/// does not open a further line (the terminator model).
fn lines_down(b: &[u8], pos: usize, n: u32) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..n {
        let le = line_end(b, ls);
        if le + 1 >= b.len() {
            break;
        }
        ls = le + 1;
    }
    ls
}

/// One bare `<Space>` step (`'whichwrap'` `s`): the next char on the line, else the first char of the next
/// line (an empty line is a stop of its own). The buffer's last char stays put.
fn wrap_right(b: &[u8], c: usize) -> usize {
    let le = line_end(b, c);
    let n = next_boundary(b, c);
    if n < le {
        n
    } else if le + 1 < b.len() {
        le + 1
    } else {
        c
    }
}

/// One bare `<BS>` step (`'whichwrap'` `b`): the previous char on the line, else the last char of the
/// previous line (its newline when that line is empty). The buffer's first char stays put.
fn wrap_left(b: &[u8], c: usize) -> usize {
    let ls = line_start(b, c);
    if c > ls {
        return prev_boundary(b, c);
    }
    if ls == 0 {
        return c;
    }
    let prev_nl = ls - 1;
    if prev_nl > line_start(b, prev_nl) {
        prev_boundary(b, prev_nl)
    } else {
        prev_nl
    }
}

/// Apply the pure step `f` up to `n` times from `start`, stopping early at a fixpoint: a step that did not
/// move never will, so a hostile count costs at most one step per byte instead of `n` steps (#907).
fn repeat_step(n: u32, start: usize, f: impl Fn(usize) -> usize) -> usize {
    let mut c = start;
    for _ in 0..n {
        let next = f(c);
        if next == c {
            break;
        }
        c = next;
    }
    c
}

/// The charwise byte range `[start, end)` an operator (`d`/`c`) covers for a motion + count. Backward
/// motions produce a range ending at the cursor; `WordEnd` is inclusive (Vim `de`).
///
/// A Vim span never takes a terminated buffer's final newline (#677): Vim cannot delete the last line's
/// end, so a motion running off the end (`d)`, `c}`, `de` past the last word) stops at the last line's end,
/// which covers its last char. The linewise paragraph objects (whole lines) and the Emacs motions (whose
/// point may sit after the final newline, so `kill-word` there can take it) are exempt.
#[must_use]
pub fn char_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
    let (s, e) = char_span_raw(b, cursor, m, count);
    let exempt = matches!(
        m,
        Motion::InnerParagraph
            | Motion::AParagraph
            | Motion::EmacsCharFwd
            | Motion::EmacsCharBack
            | Motion::EmacsWordFwd
            | Motion::EmacsWordBack
    );
    if exempt {
        (s, e)
    } else {
        let ce = content_end(b);
        (s.min(ce), e.min(ce))
    }
}

fn char_span_raw(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
    let cur = cursor.min(b.len());
    let n = count.max(1);
    match m {
        // forward / rightward → [cursor, target). `d<Space>` stays on the line exactly like `dl` (nvim).
        Motion::Right | Motion::WrapRight => (cur, target(b, cur, Motion::Right, n)),
        Motion::EmacsCharFwd => (cur, target(b, cur, m, n)),
        // `d<BS>` counts the newline as one step, so at column 0 it joins onto the line above (nvim).
        Motion::WrapLeft => (repeat_step(n, cur, |c| prev_boundary(b, c)), cur),
        // `dw`/`dW`: the LAST word moved over does not cross its line's newline — the operated text ends at
        // the end of that line, through any trailing white space — so `dw` on the last word EMPTIES the line
        // instead of joining it to the next (nvim: `dw` on ["foo   ","bar"] → ["","bar"]). Earlier counts
        // cross line breaks (`3dw`, #742), and an empty line is a word of its own (#746): `dw` there ends at
        // the next line's start, which the exclusive-linewise rule in `op_span` turns into a linewise delete.
        Motion::WordFwd | Motion::BigWordFwd => {
            (cur, word_fwd(b, cur, m == Motion::BigWordFwd, n, true))
        }
        // `d$` reaches the line end (deletes the last char too), unlike the bare `$` move which stops on it.
        // `d{count}$` reaches the end of the line `count - 1` below.
        Motion::LineEnd => (cur, line_end(b, lines_down(b, cur, n - 1))),
        // Paragraph motions as a charwise span (`c}` / a Visual `}` extend): forward → [cursor, target),
        // backward → [target, cursor). The operator's exclusive-linewise reshaping lives in `editor::op_span`.
        // At the end of the buffer `}` lands ON the last char and is inclusive (Vim), so the span takes it; a
        // blank-line landing is a line start (its `\n`), the exclusive end.
        Motion::ParagraphFwd => match paragraph_landing(b, cur, true, n) {
            None => (cur, cur),
            Some((t, true)) if t < b.len() && b[t] != b'\n' => (cur, next_grapheme(b, t)),
            Some((t, _)) => (cur, t),
        },
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
        // Method motions are EXCLUSIVE charwise like the unmatched-bracket ones: forward (`]m`/`]M`) →
        // [cursor, target), backward (`[m`/`[M`) → [target, cursor). The exclusive-linewise reduction
        // (a target at column 0 → whole lines) lives in `editor::op_span`. No-op target collapses there.
        Motion::MethodStartFwd | Motion::MethodEndFwd => (cur, target(b, cur, m, n)),
        Motion::MethodStartBack | Motion::MethodEndBack => (target(b, cur, m, n), cur),
        // inclusive end-of-word (Vim `de` / `dE`).
        Motion::WordEnd | Motion::BigWordEnd => {
            // Each step starts from the previous landing (the last char of a word), not its exclusive end, so
            // `2de` on `foo.bar` takes `foo.` (the `.` is a word of its own).
            let big = m == Motion::BigWordEnd;
            let land = repeat_step(n, cur, |p| prev_grapheme(b, word_end_excl(b, p, big)));
            (cur, next_grapheme(b, land).max(cur))
        }
        // Emacs `kill-word` (`M-d`): `[cursor, emacs_word_fwd)` — the two-class forward-word span, which
        // (unlike Vim `de`) skips a leading non-word run and stops at `_`/punctuation boundaries.
        Motion::EmacsWordFwd => (cur, repeat_step(n, cur, |e| emacs_word_fwd(b, e))),
        // backward / leftward → [target, cursor)
        Motion::Left
        | Motion::EmacsCharBack
        | Motion::WordBack
        | Motion::EmacsWordBack
        | Motion::BigWordBack
        | Motion::LineStart => (target(b, cur, m, n), cur),
        // `ge`/`gE` are INCLUSIVE backward: span from the previous word-end (landed-on char included) up to
        // and including the cursor char (Vim `dge` on `r` of "foo bar" → "fo").
        Motion::WordEndBack | Motion::BigWordEndBack => {
            let big = m == Motion::BigWordEndBack;
            let e = repeat_step(n, cur, |e| prev_word_end(b, e, big));
            (e, next_grapheme(b, cur))
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
            (cur.min(t), next_grapheme(b, cur.max(t)))
        }
        // char-search: forward includes through the landing char (`dfx` incl. x, `dtx` up to x); backward
        // spans from the landing to the cursor. A missing match is a no-op range.
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat,
        } => match find_char_target(b, cur, ch, forward, till, repeat, n) {
            None => (cur, cur),
            Some(t) if forward => (cur, next_grapheme(b, t)),
            Some(t) => (t, cur),
        },
        // bracket match: inclusive of both brackets and everything between (Vim `d%`). No match → no-op.
        Motion::MatchBracket => match match_bracket(b, cur) {
            Some(m) if m != cur => {
                let (lo, hi) = (cur.min(m), cur.max(m));
                (lo, next_grapheme(b, hi))
            }
            _ => (cur, cur),
        },
        // Word, sentence, bracket and quote objects: Vim's operator-pending range (see `textobj`), before
        // the exclusive-linewise reduction an operator applies (`editor::op_span`).
        Motion::InnerWord
        | Motion::AWord
        | Motion::InnerBigWord
        | Motion::ABigWord
        | Motion::InnerSentence
        | Motion::ASentence
        | Motion::Pair { .. }
        | Motion::Quote { .. } => {
            let (s, e, _) = crate::textobj::op_range(b, cur, m, n);
            (s, e)
        }
        Motion::InnerParagraph => paragraph_span(b, cur, n, false),
        Motion::AParagraph => paragraph_span(b, cur, n, true),
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
mod word_class_tests {
    //! Word motions split where Vim's character class changes and step by grapheme cluster (#1136); the
    //! byte offsets are nvim 0.12.6's (`wcls_*` oracle fixtures).
    use super::{target, Motion};

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
mod vim_line_model_tests {
    //! The v0.1.3 motion cluster at the `target`/`char_span` level: empty lines as words (#746), counted
    //! `dw` (#742), multibyte `e` (#741), `}` from a blank line (#748), `;` after `t` (#802).
    use super::{char_span, off_phantom, target, Motion};

    fn till(forward: bool, repeat: bool) -> Motion {
        Motion::FindChar {
            ch: 'x',
            forward,
            till: true,
            repeat,
        }
    }

    #[test]
    fn word_motions_stop_on_empty_lines() {
        let b = b"foo\n\nbar"; // empty line at 4
        assert_eq!(target(b, 0, Motion::WordFwd, 1), 4);
        assert_eq!(target(b, 0, Motion::WordFwd, 2), 5);
        assert_eq!(target(b, 5, Motion::WordBack, 1), 4);
        assert_eq!(target(b, 5, Motion::BigWordBack, 1), 4);
        assert_eq!(target(b, 5, Motion::WordEndBack, 1), 4);
        // `e` does not stop there (Vim `end_word` skips empty lines).
        assert_eq!(target(b, 2, Motion::WordEnd, 1), 7);
        // `dw` on the empty line reaches the next line's start (made linewise by the operator).
        assert_eq!(char_span(b, 4, Motion::WordFwd, 1), (4, 5));
    }

    #[test]
    fn counted_dw_crosses_lines_until_its_last_word() {
        let b = b"foo bar\nbaz qux";
        assert_eq!(char_span(b, 0, Motion::WordFwd, 3), (0, 12)); // through "baz "
        assert_eq!(char_span(b, 0, Motion::WordFwd, 2), (0, 7)); // last word "bar" ends its line
        assert_eq!(char_span(b, 4, Motion::WordFwd, 1), (4, 7));
    }

    #[test]
    fn e_advances_over_multibyte_words() {
        let b = "가나 다라".as_bytes(); // 가0 나3 sp6 다7 라10
        assert_eq!(target(b, 0, Motion::WordEnd, 1), 3);
        assert_eq!(target(b, 3, Motion::WordEnd, 1), 10);
        assert_eq!(char_span(b, 3, Motion::WordEnd, 1), (3, 13));
    }

    #[test]
    fn paragraph_from_a_blank_line_skips_the_blank_run() {
        let b = b"a\n\n\nb\n\nc"; // blanks at 2, 3 and 6
        assert_eq!(target(b, 2, Motion::ParagraphFwd, 1), 6);
        assert_eq!(target(b, 6, Motion::ParagraphBack, 1), 3);
        // A count that runs out of paragraphs fails; the last step alone lands on the last char.
        assert_eq!(target(b"a\nbc\n", 0, Motion::ParagraphFwd, 2), 0);
        assert_eq!(target(b"a\nbc\n", 0, Motion::ParagraphFwd, 1), 3);
        assert_eq!(char_span(b"a\nbc\n", 0, Motion::ParagraphFwd, 1), (0, 4));
    }

    #[test]
    fn repeated_till_skips_the_adjacent_match() {
        let b = b"axbxcxd";
        assert_eq!(
            target(b, 0, till(true, false), 1),
            0,
            "`tx` stops before the x at 1"
        );
        assert_eq!(
            target(b, 0, till(true, true), 1),
            2,
            "`;` goes on to before the next x"
        );
        assert_eq!(
            target(b, 2, till(true, true), 2),
            4,
            "`2;` counts the adjacent match"
        );
        assert_eq!(target(b, 6, till(false, true), 1), 4, "`;` after `Tx`");
    }

    #[test]
    fn off_phantom_moves_only_the_slot_after_a_final_newline() {
        assert_eq!(off_phantom(b"ab\n", 3), 1);
        assert_eq!(off_phantom(b"a\n\n", 3), 2);
        assert_eq!(
            off_phantom(b"ab", 2),
            2,
            "an unterminated end is the last line's end"
        );
        assert_eq!(off_phantom(b"", 0), 0);
        assert_eq!(
            off_phantom("a가\n".as_bytes(), 5),
            1,
            "lands on a char boundary"
        );
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

#[cfg(test)]
mod method_motion_tests {
    //! Vim `]m`/`[m`/`]M`/`[M` — method (brace-block) motions. Every expected byte offset is pinned to
    //! nvim v0.12.4 (captured headless: cursor set by byte offset, `normal! <keys>`, cursor read back as a
    //! byte offset). A faithful port of Vim's `nv_bracket_block`; see `method_target`.
    use super::{target, Motion};

    // A class with two methods. Byte map (nvim-verified landing offsets reference these):
    //   byte  0 R1: "class Foo {"      → open-brace at byte 10
    //   byte 12 R2: "    void a() {"   → open-brace at byte 25
    //   byte 27 R3: "        x;"       → `x` at byte 35
    //   byte 38 R4: "    }"            → close-brace at byte 42
    //   byte 44 R5: "    void b() {"   → open-brace at byte 57
    //   byte 59 R6: "        y;"
    //   byte 70 R7: "    }"            → close-brace at byte 74
    //   byte 76 R8: "}"                → close-brace at byte 76
    const CLASS: &[u8] =
        b"class Foo {\n    void a() {\n        x;\n    }\n    void b() {\n        y;\n    }\n}\n";
    // A flat sequence of top-level `{}` blocks.
    //   byte  0 R1: "void a() {"  → open-brace at byte 9
    //   byte 11 R2: "    x;"
    //   byte 18 R3: "}"           → close-brace at byte 18
    //   byte 20 R4: "void b() {"  → open-brace at byte 29
    //   byte 38 R6: "}"
    const FLAT: &[u8] = b"void a() {\n    x;\n}\nvoid b() {\n    y;\n}\n";

    fn t(b: &[u8], cur: usize, m: Motion, count: u32) -> usize {
        target(b, cur, m, count)
    }

    #[test]
    fn class_forward_start_walks_every_open_then_the_top_close() {
        // `]m` visits each open-brace in forward order, then (no more open-brace) the class's top-level close-brace.
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 1),
            10,
            "]m → class open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 2),
            25,
            "2]m → method a open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 3),
            57,
            "3]m → method b open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 4),
            76,
            "4]m → class close close-brace"
        );
        // A count past the last boundary fails the whole motion → no move (Vim beeps).
        assert_eq!(t(CLASS, 0, Motion::MethodStartFwd, 5), 0, "5]m → no-op");
    }

    #[test]
    fn class_forward_end_walks_every_close_then_stops() {
        // `]M` visits each method-end close-brace, plus the top-level open-brace as the class boundary.
        assert_eq!(
            t(CLASS, 0, Motion::MethodEndFwd, 1),
            10,
            "]M → class open-brace (class boundary)"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodEndFwd, 2),
            42,
            "2]M → method a close-brace"
        );
    }

    #[test]
    fn from_inside_a_method_both_directions() {
        // Cursor on `x` (byte 35), inside method a's body.
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartFwd, 1),
            57,
            "]m → method b open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartBack, 1),
            25,
            "[m → method a open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodEndFwd, 1),
            42,
            "]M → method a close-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodEndBack, 1),
            10,
            "[M → class open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartBack, 2),
            10,
            "2[m → class open-brace"
        );
    }

    #[test]
    fn flat_top_level_blocks() {
        // Flat blocks: `]m` reaches the first open-brace, then that block's top-level close-brace, then fails (Vim treats
        // the level-0 close-brace as end-of-class, so it does not roll on into the next block).
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 1),
            9,
            "]m → first open-brace"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 2),
            18,
            "2]m → its top-level close-brace"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 3),
            0,
            "3]m → no-op (past class end)"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodEndFwd, 1),
            9,
            "]M → first open-brace (class boundary)"
        );
    }

    #[test]
    fn no_braces_is_a_noop() {
        let b = b"hello world\nno braces here\n";
        assert_eq!(t(b, 0, Motion::MethodStartFwd, 1), 0);
        assert_eq!(t(b, 0, Motion::MethodEndBack, 1), 0);
        assert_eq!(t(b, 5, Motion::MethodStartBack, 1), 5);
    }
}
