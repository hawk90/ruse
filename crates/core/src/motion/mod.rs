//! Motions — the geometry half of the editing grammar (C-EDITLANG). A motion resolves, from the cursor, to
//! a target position (a bare move like `w`) or to a byte range (an operator like `dw`). It also owns the
//! shared char-boundary / line helpers used across the editor.
//!
//! Word motions come in two flavors: small-word (`w`/`b`/`e`) split where Vim's character class changes
//! (blank / punctuation / word / emoji / one class per script, [`crate::charclass::vim_class`], #1136), and
//! WORD (`W`/`B`/`E`) split on blanks only. They step one grapheme cluster at a time, as nvim's `inc()` /
//! `dec()` do, and a cluster takes the class of its first char. All positions land on char boundaries.

mod boundary;
mod bracket;
mod completion;
mod cursor_word;
mod find_char;
mod line;
mod paragraph;
mod tag;
mod word;

// The helpers the rest of the crate reaches as `crate::motion::*` (and `ruse_core::motion::off_phantom`).
pub(crate) use boundary::{
    at_bol, at_col, at_nul, char_len_at, col_of, next_boundary, next_grapheme, prev_boundary,
    prev_grapheme, snap, vcol_of, Steps,
};
// Only tests outside `motion` ask whether a position is a boundary.
#[cfg(test)]
pub(crate) use boundary::is_boundary;
pub(crate) use completion::{keyword_completions, line_completions};
pub(crate) use cursor_word::{cword_under_cursor, keyword_before_cursor, word_under_cursor};
pub use line::off_phantom;
pub(crate) use line::{content_end, first_non_blank, first_non_blank_fix, off_phantom_at, vmove};
pub(crate) use word::{char_cls, current_word_end_excl, word_end_excl};

use bracket::{bracket_block_target, match_bracket};
use find_char::find_char_target;
use line::{
    column_target, down, last_non_blank, line_delta_target, line_jump_target, lines_down, up,
    wrap_left, wrap_right,
};
use paragraph::{
    paragraph_fails, paragraph_landing, paragraph_span, paragraph_target, section_target,
};
use tag::tag_span;
pub(crate) use tag::tag_visual;
use word::{
    emacs_word_back, emacs_word_fwd, next_word_start, prev_word_end, prev_word_start, word_end_in,
    word_fwd,
};

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
    /// Exclusive charwise under an operator. The editor resolves the display column (it knows `'tabstop'`)
    /// and hands this layer the cluster count that reaches it; here `count` steps clusters (#1251).
    Column,
    /// Vim `gM` — go to the char at `count` PERCENT of the line's display width (bare `gM` = 50%; a count
    /// over 100 is 50% too), as nvim measures it. The editor resolves it to a [`Motion::Column`] (#1251); on
    /// its own this layer takes `count` percent of the line's clusters. Exclusive charwise under an
    /// operator (`dgM`), like `|`.
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
    /// section / unmatched-brace motions. See [`bracket::method_target`] for the exact algorithm and the
    /// string/comment divergence.
    MethodStartFwd,
    /// Vim `[m` — go to the `count`-th previous method start. The backward mirror of [`Motion::MethodStartFwd`].
    MethodStartBack,
    /// Vim `]M` — go to the `count`-th next method END (the matching `}`). As [`Motion::MethodStartFwd`].
    MethodEndFwd,
    /// Vim `[M` — go to the `count`-th previous method end. The backward mirror of [`Motion::MethodEndFwd`].
    MethodEndBack,
}

// Line boundaries are the one implementation in `crate::pos`; re-exported here so motion call sites
// (`line_start`/`line_end`) read unchanged.
pub(crate) use crate::pos::raw::{line_end, line_start};

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
/// - `it`/`at` under an operator outside a tag pair (the block, quote and sentence objects fail through
///   `textobj::object_fails`).
/// - `{count}%` with a count above 100 (`nv_percent`), bare, under an operator and in Visual (#761).
///
/// Every other motion (the other line jumps, `0`, `^`, `$`, `|`, sections, …) never fails here.
///
/// `memo` holds (or receives) the motion's target, `target(b, cursor, m, count)`, so a planner that goes
/// on to move does not run the motion twice (#1231). The line's ends are told by the bytes around a
/// position ([`at_bol`], [`at_nul`]); only the vertical and `{count}$` checks find the line.
#[must_use]
pub(crate) fn fails_with(
    b: &[u8],
    cursor: usize,
    m: Motion,
    count: u32,
    op: bool,
    visual: bool,
    memo: &std::cell::OnceCell<usize>,
) -> bool {
    let n = count.max(1);
    let cur = cursor.min(b.len());
    let last_line = || line_end(b, cur) >= content_end(b);
    // Where the motion leaves an on-char caret: never on a non-empty line's break or the phantom slot.
    let clamp = |t: usize| {
        let t = off_phantom_at(b, t.min(b.len()));
        if at_nul(b, t) && !at_bol(b, t) {
            prev_grapheme(b, t)
        } else {
            t
        }
    };
    let still = || clamp(*memo.get_or_init(|| target(b, cur, m, count))) == clamp(cur);
    match m {
        Motion::Left => !op && at_bol(b, cur),
        Motion::Right if visual => !op && at_nul(b, cur),
        // From the NUL slot the step crosses the `\n`, past the line's end; else it ends on or before it.
        Motion::Right => !op && (at_nul(b, cur) || at_nul(b, next_grapheme(b, cur))),
        Motion::WrapLeft | Motion::WrapRight => !op && still(),
        Motion::Up | Motion::UpFirstNonBlank => line_start(b, cur) == 0,
        Motion::Down | Motion::DownFirstNonBlank => last_line(),
        Motion::WordFwd | Motion::BigWordFwd => !op && still(),
        Motion::WordBack | Motion::BigWordBack | Motion::WordEndBack | Motion::BigWordEndBack => {
            still()
        }
        Motion::WordEnd | Motion::BigWordEnd => {
            !op && {
                let before = if n > 1 { target(b, cur, m, n - 1) } else { cur };
                let t = if n == count {
                    *memo.get_or_init(|| target(b, cur, m, count))
                } else {
                    target(b, cur, m, n)
                };
                clamp(t) == clamp(before)
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
        Motion::LineEnd => n > 1 && last_line(),
        // `{count}dd`/`yy`/`cc`/`>>` move `count - 1` lines down first (`nv_lineop` → `cursor_down`), so on
        // the last line a count above 1 fails like `j` there (#1176).
        Motion::Line => op && n > 1 && last_line(),
        // `{count}%` past 100 percent has no line to go to: nvim beeps instead of clamping (#761).
        Motion::GotoPercent => count > 100,
        // `it`/`at` outside a tag pair (or a count past its nesting) cancel the operator (#1179). The `at`
        // span holds the tags themselves, so it is empty only when there is no pair.
        Motion::Tag { .. } => {
            op && {
                let (s, e) = tag_span(b, cur, true, n);
                s == e
            }
        }
        _ => false,
    }
}

/// The cursor target for a bare move (`w`, `3l`, `k`, …), applying `count`.
///
/// One arm per motion family. Most families read the count as something other than a repeat (an absolute
/// line, a percentage, a column, a line delta) and resolve it in one call; the char / line / word steps
/// repeat [`step`] up to `count` times. The text objects and the linewise motion have no bare-move target.
#[must_use]
pub(crate) fn target(b: &[u8], cursor: usize, m: Motion, count: u32) -> usize {
    let n = count.max(1);
    let cur0 = cursor.min(b.len());
    match m {
        // Char-search resolves the count-th match directly (repeating a single `t` step would stick in place).
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat,
        } => find_char_target(b, cur0, ch, forward, till, repeat, count).unwrap_or(cur0),
        Motion::GotoLine | Motion::LastLine | Motion::GotoByte | Motion::GotoPercent => {
            line_jump_target(b, m, count, n)
        }
        // Bracket match jumps to a single computed position (count is not a repeat; `count%` is deferred).
        Motion::MatchBracket => match_bracket(b, cur0).unwrap_or(cur0),
        // `{count}g_`: the count is `count-1` lines down, then the last non-blank char (not a repeat).
        // Stops at the last line, never the phantom slot after a final newline (#677).
        Motion::LineLastNonBlank => last_non_blank(b, lines_down(b, cur0, n - 1)),
        Motion::DownFirstNonBlank | Motion::UpFirstNonBlank | Motion::LineUnderscore => {
            line_delta_target(b, cur0, m, n)
        }
        Motion::SectionFwd
        | Motion::SectionBack
        | Motion::SectionEndFwd
        | Motion::SectionEndBack => section_target(b, cur0, m, n),
        Motion::UnmatchedParenBack
        | Motion::UnmatchedParenFwd
        | Motion::UnmatchedBraceBack
        | Motion::UnmatchedBraceFwd
        | Motion::MethodStartFwd
        | Motion::MethodStartBack
        | Motion::MethodEndFwd
        | Motion::MethodEndBack => bracket_block_target(b, cur0, m, n),
        Motion::Column | Motion::MidLine => column_target(b, cur0, m, n),
        // `{count}$` / `{count}<End>`: the end of the line `count - 1` lines down (stopping at the last line).
        Motion::LineEnd if n > 1 => target(b, lines_down(b, cur0, n - 1), m, 1),
        // `)` / `(`: Vim's `findsent`, ported in `textobj` for `is`/`as`, where an empty line is a sentence of
        // its own (`)` from `foo bar` stops on the blank line below it).
        Motion::SentenceFwd | Motion::SentenceBack => {
            crate::textobj::sentence_motion(b, cur0, m == Motion::SentenceFwd, n)
        }
        // `}` / `{`: a count that runs out of paragraphs fails as a whole (see `paragraph_target`).
        Motion::ParagraphFwd | Motion::ParagraphBack => {
            paragraph_target(b, cur0, m == Motion::ParagraphFwd, n)
        }
        Motion::Left
        | Motion::Right
        | Motion::WrapLeft
        | Motion::WrapRight
        | Motion::EmacsCharBack
        | Motion::EmacsCharFwd
        | Motion::Up
        | Motion::Down
        | Motion::LineStart
        | Motion::LineFirstNonBlank
        | Motion::LineEnd
        | Motion::WordFwd
        | Motion::WordBack
        | Motion::WordEnd
        | Motion::WordEndBack
        | Motion::EmacsWordFwd
        | Motion::EmacsWordBack
        | Motion::BigWordFwd
        | Motion::BigWordBack
        | Motion::BigWordEnd
        | Motion::BigWordEndBack => {
            // `{count}h` / `{count}l` step through one line cache: a count is not a whole-line scan per step
            // (#1224). Every step is a pure function of `(b, c)`, so `repeat_step` stops at the first step
            // that did not move: without that a hostile count (`emacs_kill_word 4294967295` in a trace, or a
            // huge `C-u` prefix) spun for minutes at the buffer edge (#907).
            let steps = Steps::new(b);
            repeat_step(n, cur0, |c| step(b, &steps, c, m))
        }
        // The text objects and the linewise motion have no bare-move target.
        Motion::InnerWord
        | Motion::AWord
        | Motion::InnerBigWord
        | Motion::ABigWord
        | Motion::InnerParagraph
        | Motion::AParagraph
        | Motion::InnerSentence
        | Motion::ASentence
        | Motion::Pair { .. }
        | Motion::Quote { .. }
        | Motion::Tag { .. }
        | Motion::Line => cur0,
    }
}

/// One bare-move step of a repeating motion (`h`, `l`, `j`, `k`, `0`, `^`, `$`, the word motions) from `c`.
/// Only [`target`]'s stepping arm calls it.
fn step(b: &[u8], steps: &Steps<'_>, c: usize, m: Motion) -> usize {
    match m {
        // `h`/`l` are bounded by the line; the count stops at its edge.
        // A step is one grapheme cluster (F-002, #696): a composed char or a ZWJ emoji is one char.
        Motion::Left if c > steps.line_start(c) => steps.prev(c),
        Motion::Right if c < steps.line_end(c) => steps.next(c),
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
        // end of the line `count - 1` below (resolved by `target` before it steps).
        Motion::LineEnd => {
            let ls = line_start(b, c);
            let le = line_end(b, c);
            if le > ls {
                prev_grapheme(b, le)
            } else {
                le
            }
        }
        Motion::WordFwd => next_word_start(steps, c, false),
        Motion::WordBack => prev_word_start(steps, c, false),
        Motion::WordEnd => steps.prev(word_end_in(steps, c, false)), // land ON the last char
        Motion::WordEndBack => prev_word_end(steps, c, false),       // Vim `ge`
        Motion::EmacsWordFwd => emacs_word_fwd(b, c), // Emacs forward-word: two-class, land AFTER the word
        Motion::EmacsWordBack => emacs_word_back(b, c), // Emacs backward-word: two-class mirror
        Motion::BigWordFwd => next_word_start(steps, c, true),
        Motion::BigWordBack => prev_word_start(steps, c, true),
        Motion::BigWordEnd => steps.prev(word_end_in(steps, c, true)),
        Motion::BigWordEndBack => prev_word_end(steps, c, true), // Vim `gE`
        _ => unreachable!("`target` resolves {m:?} without stepping"),
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
pub(crate) fn char_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
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
        Motion::WordFwd | Motion::BigWordFwd => (
            cur,
            word_fwd(&Steps::new(b), cur, m == Motion::BigWordFwd, n, true),
        ),
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
            let s = Steps::new(b);
            let land = repeat_step(n, cur, |p| s.prev(word_end_in(&s, p, big)));
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
            let s = Steps::new(b);
            let e = repeat_step(n, cur, |e| prev_word_end(&s, e, big));
            (e, next_grapheme(b, cur))
        }
        // `^` is exclusive and stops on an all-blank line's LAST blank (`BL_FIX`), so `0d^` there keeps that
        // blank instead of emptying the line (#762).
        Motion::LineFirstNonBlank => {
            let t = first_non_blank_fix(b, cur);
            (cur.min(t), cur.max(t))
        }
        // `|` / `go` / `gM` are exclusive and can point either way (column / byte / line-%, left or right):
        // ordered pair from the cursor to the absolute target.
        Motion::Column | Motion::GotoByte | Motion::MidLine => {
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
mod vim_line_model_tests {
    //! The v0.1.3 motion cluster at the `target`/`char_span` level: empty lines as words (#746), counted
    //! `dw` (#742), multibyte `e` (#741), `}` from a blank line (#748), `;` after `t` (#802).
    use super::{char_span, target, Motion};

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
        assert_eq!(super::off_phantom_at(b"ab\n", 3), 1);
        assert_eq!(super::off_phantom_at(b"a\n\n", 3), 2);
        assert_eq!(
            super::off_phantom_at(b"ab", 2),
            2,
            "an unterminated end is the last line's end"
        );
        assert_eq!(super::off_phantom_at(b"", 0), 0);
        assert_eq!(
            super::off_phantom_at("a가\n".as_bytes(), 5),
            1,
            "lands on a char boundary"
        );
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
mod line_pass_tests {
    //! #1231: the word helpers, `fails` and the line-end clamps tell a line's ends by the bytes around a
    //! position instead of finding the line. Each must answer exactly as the line-finding version it
    //! replaced, copied here verbatim (`old_*`), on any bytes and at any position.
    use super::word::{on_empty_line, vcls, vdec, vinc};
    use super::*;
    use proptest::prelude::*;

    /// The pre-#1231 `off_phantom_at`, verbatim.
    fn old_off_phantom_at(b: &[u8], pos: usize) -> usize {
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

    fn old_vcls(s: &Steps, p: usize, big: bool) -> u32 {
        if p >= s.line_end(p) {
            return 0;
        }
        char_cls(s.b, p, big)
    }

    /// Whether `p` is on an empty line (Vim `col == 0 && LINEEMPTY`).
    fn old_on_empty_line(s: &Steps, p: usize) -> bool {
        let ls = s.line_start(p);
        p == ls && s.line_end(ls) == ls
    }

    /// Vim `inc()`: the next position and how it got there (0 within the line, 2 onto the line's NUL slot, 1 to
    /// the next line's start), or `None` from the last line's NUL slot.
    fn old_vinc(s: &Steps, p: usize) -> Option<(usize, u8)> {
        let le = s.line_end(p);
        if p < le {
            let q = s.next(p);
            Some((q, if q < le { 0 } else { 2 }))
        } else if le + 1 < s.b.len() {
            Some((le + 1, 1))
        } else {
            None
        }
    }

    /// Vim `dec()`: the previous position (0 within the line, 1 back onto the previous line's NUL slot), or
    /// `None` at the buffer start.
    fn old_vdec(s: &Steps, p: usize) -> Option<(usize, u8)> {
        if p > s.line_start(p) {
            Some((s.prev(p), 0))
        } else if p > 0 {
            Some((p - 1, 1))
        } else {
            None
        }
    }

    fn old_word_fwd(s: &Steps, pos: usize, big: bool, count: u32, eol: bool) -> usize {
        let b = s.b;
        let mut p = pos.min(b.len());
        for left in (0..count.max(1)).rev() {
            let stop = |i: u8| i >= 1 && eol && left == 0;
            let sclass = old_vcls(s, p, big);
            let last_line = s.line_end(p) + 1 >= b.len();
            let Some((q, i)) = old_vinc(s, p) else {
                return p;
            };
            p = q;
            if (i >= 1 && last_line) || stop(i) {
                return p;
            }
            if sclass != 0 {
                while old_vcls(s, p, big) == sclass {
                    let Some((q, i)) = old_vinc(s, p) else {
                        return p;
                    };
                    p = q;
                    if stop(i) {
                        return p;
                    }
                }
            }
            while old_vcls(s, p, big) == 0 {
                if old_on_empty_line(s, p) {
                    break;
                }
                let Some((q, i)) = old_vinc(s, p) else {
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

    #[allow(clippy::too_many_lines)]
    fn old_fails(b: &[u8], cursor: usize, m: Motion, count: u32, op: bool, visual: bool) -> bool {
        let n = count.max(1);
        let cur = cursor.min(b.len());
        let ls = line_start(b, cur);
        let le = line_end(b, cur);
        let last_line = le >= content_end(b);
        // Where the motion leaves an on-char caret: never on a non-empty line's break or the phantom slot.
        let clamp = |t: usize| {
            let t = old_off_phantom_at(b, t.min(b.len()));
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
            Motion::WordBack
            | Motion::BigWordBack
            | Motion::WordEndBack
            | Motion::BigWordEndBack => still(),
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
            // `{count}dd`/`yy`/`cc`/`>>` move `count - 1` lines down first (`nv_lineop` → `cursor_down`), so on
            // the last line a count above 1 fails like `j` there (#1176).
            Motion::Line => op && n > 1 && last_line,
            Motion::GotoPercent => count > 100,
            // `it`/`at` outside a tag pair (or a count past its nesting) cancel the operator (#1179). The `at`
            // span holds the tags themselves, so it is empty only when there is no pair.
            Motion::Tag { .. } => {
                op && {
                    let (s, e) = tag_span(b, cur, true, n);
                    s == e
                }
            }
            _ => false,
        }
    }

    /// Bytes that exercise the line model: blank and empty lines, CR, words, punctuation, multibyte and
    /// combining text, a bracket pair and an invalid byte.
    fn text() -> impl Strategy<Value = Vec<u8>> {
        let piece = prop_oneof![
            Just(b"a".to_vec()),
            Just(b"_".to_vec()),
            Just(b" ".to_vec()),
            Just(b".".to_vec()),
            Just(b"\n".to_vec()),
            Just(b"\n\n".to_vec()),
            Just(b"\r".to_vec()),
            Just(b"(".to_vec()),
            Just(b")".to_vec()),
            Just("가".as_bytes().to_vec()),
            Just("e\u{301}".as_bytes().to_vec()),
            Just(b"\xff".to_vec()),
        ];
        proptest::collection::vec(piece, 0..24).prop_map(|v| v.concat())
    }

    fn motions() -> Vec<Motion> {
        use Motion::*;
        vec![
            Left,
            Right,
            WrapLeft,
            WrapRight,
            Up,
            Down,
            UpFirstNonBlank,
            DownFirstNonBlank,
            WordFwd,
            BigWordFwd,
            WordBack,
            BigWordBack,
            WordEnd,
            BigWordEnd,
            WordEndBack,
            BigWordEndBack,
            LineEnd,
            Line,
            MatchBracket,
            SentenceFwd,
            UnmatchedParenFwd,
            UnmatchedParenBack,
        ]
    }

    proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// The byte tests equal the line-finding ones, at every position and one past the end.
        #[test]
        fn line_end_tests_equal_finding_the_line(b in text()) {
            let s = Steps::new(&b);
            for p in 0..=b.len() + 1 {
                prop_assert_eq!(at_nul(&b, p), p >= line_end(&b, p), "at_nul {}", p);
                if p <= b.len() {
                    prop_assert_eq!(at_bol(&b, p), p == line_start(&b, p), "at_bol {}", p);
                }
                prop_assert_eq!(vcls(&s, p, false), old_vcls(&s, p, false), "vcls {}", p);
                prop_assert_eq!(vcls(&s, p, true), old_vcls(&s, p, true), "vCLS {}", p);
                prop_assert_eq!(on_empty_line(&s, p), old_on_empty_line(&s, p), "empty {}", p);
                prop_assert_eq!(vinc(&s, p), old_vinc(&s, p), "inc {}", p);
                prop_assert_eq!(vdec(&s, p), old_vdec(&s, p), "dec {}", p);
                prop_assert_eq!(off_phantom_at(&b, p), old_off_phantom_at(&b, p), "phantom {}", p);
                // The planner's Normal-mode clamp onto a non-empty line's last char.
                let old = p == line_end(&b, p) && line_end(&b, p) > line_start(&b, p);
                let new = p <= b.len() && at_nul(&b, p) && !at_bol(&b, p);
                prop_assert_eq!(new, old, "clamp {}", p);
            }
        }

        /// `w`/`W`, bare and under an operator, with any count, equal the pre-#1231 walk.
        #[test]
        fn word_fwd_equals_the_line_finding_walk(b in text(), count in 1u32..4) {
            let s = Steps::new(&b);
            for p in 0..=b.len() {
                for big in [false, true] {
                    for eol in [false, true] {
                        prop_assert_eq!(
                            word_fwd(&s, p, big, count, eol),
                            old_word_fwd(&s, p, big, count, eol),
                            "w at {} big={} eol={} count={}", p, big, eol, count
                        );
                    }
                }
            }
        }

        /// `fails` (sharing the target through its memo) equals the pre-#1231 one for every motion, from
        /// every char boundary, bare, under an operator and in Visual.
        #[test]
        fn fails_equals_the_line_finding_check(b in text(), count in 0u32..4) {
            for p in (0..=b.len()).filter(|&p| is_boundary(&b, p)) {
                for m in motions() {
                    for (op, visual) in [(false, false), (true, false), (false, true)] {
                        let memo = std::cell::OnceCell::new();
                        let new = fails_with(&b, p, m, count, op, visual, &memo);
                        prop_assert_eq!(
                            new,
                            old_fails(&b, p, m, count, op, visual),
                            "{:?} at {} count={} op={} visual={}", m, p, count, op, visual
                        );
                        if let Some(&t) = memo.get() {
                            prop_assert_eq!(t, target(&b, p, m, count), "memo {:?} at {}", m, p);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod invalid_byte_tests {
    //! #1243: a byte that belongs to no valid UTF-8 char is one char, with the class of the Latin-1 char of
    //! its value (0xA0 blank, 0xB5 and 0xC0-0xFF keyword, the rest punctuation), for the column count and
    //! the keyword lookups, as nvim v0.12.6 reads it.
    use super::{col_of, keyword_before_cursor, keyword_completions, word_under_cursor};

    #[test]
    fn col_of_counts_each_invalid_byte_as_a_char() {
        assert_eq!(col_of(b"a\xff\xc3\xa9\xffb", 0, 6), 5);
        assert_eq!(col_of(b"\xea\xb0ab", 0, 2), 2);
        assert_eq!(col_of(b"x\x80\xcc\x81y", 0, 4), 3);
        assert_eq!(col_of("한글".as_bytes(), 0, 6), 2);
    }

    #[test]
    fn word_under_cursor_reads_the_latin1_class() {
        // 0xFF is a keyword char: it joins the ASCII letters around it.
        assert_eq!(word_under_cursor(b"foo\xffbar x", 0), Some((0, 7)));
        // 0x80 is punctuation: it ends the word, and a cursor on it finds the next one.
        assert_eq!(word_under_cursor(b"ab\x80cd", 0), Some((0, 2)));
        assert_eq!(word_under_cursor(b"\x80ab ab", 0), Some((1, 3)));
        // A cut sequence: its lead 0xE9 is a keyword char, its 0xB0 punctuation.
        assert_eq!(word_under_cursor(b"\xe9\xb0ab", 0), Some((0, 1)));
        assert_eq!(word_under_cursor(b"\x80 \xa0", 0), None);
    }

    #[test]
    fn keyword_before_cursor_reads_the_latin1_class() {
        assert_eq!(
            keyword_before_cursor(b"x foo\xffba", 9),
            (2, &b"foo\xffba"[..])
        );
        assert_eq!(keyword_before_cursor(b"foo\x80ba", 6), (4, &b"ba"[..]));
        // Candidates split at 0x80; one holding a byte that is not UTF-8 is offered spelled, as nvim
        // offers it (#1252).
        let b = b"foo\x80bar fo\xffx food\nfo";
        let ff = crate::pattern::spell(b"fo\xffx").into_owned();
        assert_eq!(
            keyword_completions(b, b.len(), b"fo"),
            vec!["foo", &ff, "food"]
        );
        assert_eq!(&*crate::pattern::unspell(&ff), b"fo\xffx");
        // <cword> looks for a keyword with the Latin-1 class: 0x80 is punctuation, so `x` is found.
        assert_eq!(
            super::cword_under_cursor(b"+\x80+ x", 0, false),
            Some((4, 5))
        );
        assert_eq!(
            super::cword_under_cursor(b"+ \xffa", 0, false),
            Some((2, 4))
        );
    }
}
