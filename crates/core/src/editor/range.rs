//! Pure range/motion geometry for the plan pipeline: the byte-span helpers that turn a
//! `(motion, count)` (or a Visual selection's two ends) into the `[start, end)` range an operator
//! covers, plus the char-boundary walkers. Every function here is pure over `&[u8]` + primitives —
//! it touches no `View`/`EditorState` state — so it lives beside `mod.rs`'s hub and is re-exported
//! `pub(crate)` (`planner.rs`'s `use super::*` and `View::{selection_span,block_spans}` resolve
//! these names through that re-export).

use crate::command::ForcedWise;
use crate::motion::{
    self, at_col, col_of, line_end, line_start, next_boundary, prev_boundary, Motion,
};

/// `curswant` sentinel meaning "the end of whatever line you land on" — Vim's MAXCOL, set by `$`.
pub(crate) const MAXCOL: usize = usize::MAX;

/// Whether a motion is a text object (a range around the cursor), as opposed to a bare cursor movement. In a
/// selection mode these set both ends of the selection; everywhere else they are operator operands.
pub(crate) fn is_text_object(m: Motion) -> bool {
    matches!(
        m,
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
    )
}

/// A resolved operator span: nvim's `oparg_T` once `do_pending_operator` has reshaped it. `[s, e)` covers
/// whole lines (each with its trailing newline where present) when `linewise`, else the exact chars.
/// `adjusted` is nvim's `oap->end_adjusted`: Vim's exclusive-linewise rule moved the span's end (the cursor
/// then rests on the first non-blank after a linewise delete, and `gq` one line further, #1071).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpSpan {
    pub(crate) s: usize,
    pub(crate) e: usize,
    pub(crate) linewise: bool,
    pub(crate) adjusted: bool,
    /// The motion failed (`dj` on the last line, `3dd` there): the operator is a no-op, and `c` stays in
    /// Normal.
    pub(crate) failed: bool,
}

impl OpSpan {
    fn new((s, e, linewise): (usize, usize, bool), adjusted: bool) -> OpSpan {
        OpSpan {
            s,
            e,
            linewise,
            adjusted,
            failed: false,
        }
    }

    fn failed(cur: usize) -> OpSpan {
        OpSpan {
            failed: true,
            ..OpSpan::new((cur, cur, true), false)
        }
    }
}

/// Vim's exclusive-linewise reduction of an exclusive span `[lo, hi)` as an [`OpSpan`], recording whether it
/// moved the end (`end_adjusted`).
pub(crate) fn exclusive_op_span(b: &[u8], lo: usize, hi: usize) -> OpSpan {
    let r = exclusive_linewise(b, lo, hi);
    OpSpan::new(r, r.2 || (lo < hi && r.1 != hi))
}

/// The byte range `[s, e)` a `delete`/`yank` operator covers for a motion + count, plus whether the removed
/// span is **linewise** (its register geometry and paste shape). See [`op_span_full`].
pub(crate) fn op_span(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    let sp = op_span_full(b, cur, m, count);
    (sp.s, sp.e, sp.linewise)
}

/// Exclusive charwise motions whose operator span (not just their target) takes [`exclusive_linewise`] in
/// [`op_span_full`]. `w`/`W` stop at their line end under an operator, so the span, not the bare target, is
/// what the rule reshapes. `<BS>` is exempt (Vim sets `CA_NO_ADJ_OP_END` for it), as are the Emacs motions.
fn takes_exclusive_rule_on_span(m: Motion) -> bool {
    matches!(
        m,
        Motion::WordFwd
            | Motion::BigWordFwd
            | Motion::WordBack
            | Motion::BigWordBack
            | Motion::SentenceFwd
            | Motion::SentenceBack
            | Motion::GotoByte
    )
}

/// Vim's linewise promotion for a charwise `d{motion}` (nvim `op_delete`): a span over more than one line
/// whose start is in its line's indent and whose end leaves only white space on its last line deletes
/// whole lines instead. `2d$` on `ab|cd|ef` takes the first two lines (#747), `d2w` from the indent of
/// `foo |  bar` takes both, while `3dw` ending mid-line stays charwise. Delete only, not Visual or a forced
/// wise. Returns the whole-line span `[first line start, after the last line's newline)` when it applies.
pub(crate) fn delete_promotes_linewise(b: &[u8], s: usize, e: usize) -> Option<(usize, usize)> {
    if s >= e {
        return None;
    }
    let last = prev_boundary(b, e);
    let ls = line_start(b, s);
    if ls == line_start(b, last) {
        return None; // a single line
    }
    let in_indent = b[ls..s].iter().all(|&c| c == b' ' || c == b'\t');
    let le = line_end(b, last);
    let tail_blank = b[e.min(le)..le].iter().all(|&c| c == b' ' || c == b'\t');
    if !(in_indent && tail_blank) {
        return None;
    }
    Some((ls, if le < b.len() { le + 1 } else { le }))
}

/// Whether the cursor's line is the buffer's last line (a final newline ends it; it opens no further line).
fn on_last_line(b: &[u8], cur: usize) -> bool {
    line_end(b, cur) + 1 >= b.len()
}

/// The operator span for a motion + count. Linewise motions cover whole lines; every exclusive charwise
/// motion takes Vim's exclusive-linewise rule (`:help exclusive-linewise`: an end at column 0 of a later
/// line pulls back to the previous line's end, and the span goes linewise when it starts in the indent),
/// so `d}`, `d(`, `db` and the objects agree with nvim; everything else is the motion's charwise span.
pub(crate) fn op_span_full(b: &[u8], cur: usize, m: Motion, count: u32) -> OpSpan {
    // Whole-lines span from the cursor's line through the target's line, inclusive of the final newline.
    let whole_lines = |t: usize| {
        let start = line_start(b, cur.min(t));
        let le = line_end(b, cur.max(t));
        let end = if le < b.len() { le + 1 } else { le };
        OpSpan::new((start, end, true), false)
    };
    let failed = OpSpan::failed(cur);
    match m {
        // Line jumps (`dG`, `dgg`, `d{n}G`) and first-non-blank line motions (`d+`, `d-`, `d_`) are
        // linewise across every line between the cursor and target.
        Motion::GotoLine
        | Motion::LastLine
        | Motion::DownFirstNonBlank
        | Motion::UpFirstNonBlank
        | Motion::LineUnderscore
        | Motion::GotoPercent => whole_lines(motion::target(b, cur, m, count)),
        // Vertical motions under an operator are linewise (`dj` deletes this line and the next). A motion
        // that cannot move a line (`dj` on the last line) fails the operator entirely (Vim) — a no-op range.
        Motion::Up | Motion::Down => {
            let t = motion::target(b, cur, m, count);
            if line_start(b, t) == line_start(b, cur) {
                failed
            } else {
                whole_lines(t)
            }
        }
        // `dd` / `{count}dd`: whole lines from the cursor's line down. A count past the last line clamps,
        // except on the last line itself, where `{count}dd` (count > 1) fails like nvim's `cursor_down`.
        Motion::Line => {
            if count > 1 && on_last_line(b, cur) {
                return failed;
            }
            let start = line_start(b, cur);
            let mut end = start;
            for _ in 0..count.max(1) {
                let le = line_end(b, end);
                if le < b.len() {
                    end = le + 1;
                } else {
                    end = le;
                    break;
                }
            }
            OpSpan::new((start, end, true), false)
        }
        // Paragraph objects (`dip`/`dap`) are linewise (Vim); `char_span` already returns whole lines.
        Motion::InnerParagraph | Motion::AParagraph => {
            let (s, e) = motion::char_span(b, cur, m, count);
            OpSpan::new((s, e, true), false)
        }
        // `}` takes its span from `char_span`, which makes a landing on the last char at the end of the
        // buffer inclusive.
        Motion::ParagraphFwd => {
            let (s, e) = motion::char_span(b, cur, m, count);
            exclusive_op_span(b, s, e)
        }
        // Paragraph, section, unmatched-bracket and method motions: exclusive charwise between the cursor
        // and the target, reduced by the exclusive-linewise rule.
        m if takes_exclusive_linewise(m) => {
            let t = motion::target(b, cur, m, count);
            exclusive_op_span(b, cur.min(t), cur.max(t))
        }
        // Word, sentence, bracket and quote objects: Vim's object range, reduced by the exclusive-linewise
        // rule — so `di{` over a block whose closer sits alone (after any indent) on its line deletes the
        // whole inner lines linewise, and `dis` on a sentence ending its line takes that line.
        m if crate::textobj::owns(m) => {
            let (s, e, exclusive_at_col0) = crate::textobj::op_range(b, cur, m, count);
            if exclusive_at_col0 {
                exclusive_op_span(b, s, e)
            } else {
                OpSpan::new((s, e, false), false)
            }
        }
        // The exclusive word, sentence and byte motions take the same rule on their span (`jdb` from
        // column 0 deletes the line above, `dw` on an empty line deletes that line, #746); every other
        // motion keeps its charwise span.
        _ => {
            let (s, e) = motion::char_span(b, cur, m, count);
            if takes_exclusive_rule_on_span(m) {
                exclusive_op_span(b, s, e)
            } else {
                OpSpan::new((s, e, false), false)
            }
        }
    }
}

/// Where the cursor stands once the operator's motion has run (nvim's cursor at `do_pending_operator`), the
/// other end of the operated range from the cursor. Vertical and line-jump motions keep the wanted column
/// `curswant` (`'nostartofline'`); `{count}dd`-style line operators land on the last line's first non-blank,
/// except yank, which keeps the column. A text object lands on its start.
pub(crate) fn op_landing(
    b: &[u8],
    cur: usize,
    m: Motion,
    count: u32,
    curswant: usize,
    yank: bool,
) -> usize {
    match m {
        Motion::Up => motion::vmove(b, cur, count, false, curswant),
        Motion::Down => motion::vmove(b, cur, count, true, curswant),
        Motion::GotoLine | Motion::LastLine | Motion::GotoPercent => {
            at_col(b, line_start(b, motion::target(b, cur, m, count)), curswant)
        }
        Motion::Line => {
            let mut ls = line_start(b, cur);
            for _ in 1..count.max(1) {
                let le = line_end(b, ls);
                if le + 1 >= b.len() {
                    break;
                }
                ls = le + 1;
            }
            if yank {
                at_col(b, ls, curswant)
            } else {
                motion::first_non_blank(b, ls)
            }
        }
        m if is_text_object(m) => op_span_full(b, cur, m, count).s,
        _ => motion::target(b, cur, m, count),
    }
}

/// Whether `m` is an exclusive charwise motion that takes Vim's exclusive-linewise reduction under an
/// operator ([`exclusive_linewise`]).
fn takes_exclusive_linewise(m: Motion) -> bool {
    matches!(
        m,
        // Paragraph motions (`d}`/`d{`) and section motions (`d]]`/`d[[`/`d][`/`d[]`) are exclusive
        // charwise, but Vim's exclusive-linewise rule can turn them linewise — shared with forced-charwise
        // on a linewise motion (see `exclusive_linewise`). (nvim quirk NOT reproduced: a section-FORWARD
        // operator that runs OFF the end of the file — `d]]` with no further `{` — makes delete
        // linewise-through-EOF but yank charwise; ruse uses one span for d/y, so both stop at the last
        // line's start here. Deliberate v0 divergence, documented in the change record.)
        Motion::ParagraphFwd
            | Motion::ParagraphBack
            | Motion::SectionFwd
            | Motion::SectionBack
            | Motion::SectionEndFwd
            | Motion::SectionEndBack
            // Unmatched-bracket motions (`d[(`/`d])`/`d[{`/`d]}`) are exclusive charwise and take the same
            // exclusive-linewise reduction (verified against nvim: `d])` landing on a `)` at column 0
            // deletes whole lines; a mid-line landing stays charwise). A no-op target collapses to an empty
            // span.
            | Motion::UnmatchedParenBack
            | Motion::UnmatchedParenFwd
            | Motion::UnmatchedBraceBack
            | Motion::UnmatchedBraceFwd
            // Method motions (`d]m`/`d[m`/`d]M`/`d[M`) are exclusive charwise and take the same
            // exclusive-linewise reduction (verified against nvim: a landing on a `}` at column 0 deletes
            // whole lines; a mid-line landing stays charwise). A no-op target collapses to an empty span.
            | Motion::MethodStartFwd
            | Motion::MethodStartBack
            | Motion::MethodEndFwd
            | Motion::MethodEndBack
    )
}

/// Whether the exclusive-linewise rule ADJUSTS the end of `m`'s operator span (nvim's `oap->end_adjusted`).
/// `gq` then leaves the cursor on the line below the formatted block (`gq}`, #1071).
pub(crate) fn exclusive_end_adjusted(b: &[u8], cur: usize, m: Motion, count: u32) -> bool {
    op_span_full(b, cur, m, count).adjusted
}

/// Vim's exclusive-linewise reduction for an EXCLUSIVE charwise span `[lo, hi)`: if the end sits at column
/// 0 of a line and the start is at/before that start line's first non-blank, the span becomes whole lines
/// (linewise); otherwise the end pulls back one byte (charwise). Empty span → a no-op. Shared by the `d}`
/// paragraph motion and forced-charwise on a linewise motion (`dvj`), which is why they agree.
pub(crate) fn exclusive_linewise(b: &[u8], lo: usize, hi: usize) -> (usize, usize, bool) {
    if lo >= hi {
        (lo, lo, false)
    } else if hi > 0 && hi == line_start(b, hi) {
        if lo <= motion::first_non_blank(b, lo) {
            (line_start(b, lo), hi, true)
        } else {
            (lo, hi - 1, false)
        }
    } else {
        (lo, hi, false)
    }
}

/// Whether a charwise motion is INCLUSIVE (its last char is part of the operated span) in Vim terms — the
/// bit `o_v` toggles. Text objects carry their own exact span and are left untoggled by [`forced_span`].
pub(crate) fn motion_inclusive(m: Motion) -> bool {
    matches!(
        m,
        Motion::WordEnd
            | Motion::BigWordEnd
            | Motion::WordEndBack
            | Motion::BigWordEndBack
            | Motion::LineEnd
            | Motion::LineLastNonBlank
            | Motion::MatchBracket
            | Motion::FindChar { forward: true, .. }
    )
}

/// The span for an operator whose motion wise is FORCED (Vim `o_v`/`o_V`), as nvim's `do_pending_operator`
/// reshapes it. `o_V` covers every line from the cursor's to the line the motion ends on; `o_v` makes a
/// linewise motion EXCLUSIVE charwise between the cursor and where the motion lands (the column kept by
/// `curswant`, so `ldvG` deletes up to the same column of the last line), and toggles a charwise motion's
/// exclusive/inclusive edge. An exclusive result then takes the exclusive-linewise rule. Text objects (which
/// already carry an exact span) are only reshaped for `o_V`; `o_v` leaves their edge alone.
pub(crate) fn forced_span(
    b: &[u8],
    cur: usize,
    m: Motion,
    count: u32,
    wise: ForcedWise,
    curswant: usize,
) -> OpSpan {
    let plain = op_span_full(b, cur, m, count);
    let whole = |lo: usize, hi: usize| {
        let le = line_end(b, hi);
        OpSpan::new(
            (
                line_start(b, lo),
                if le < b.len() { le + 1 } else { le },
                true,
            ),
            false,
        )
    };
    // The linewise motions; a failed one (`j` on the last line) fails the forced form too.
    let line_motion = matches!(
        m,
        Motion::GotoLine
            | Motion::LastLine
            | Motion::DownFirstNonBlank
            | Motion::UpFirstNonBlank
            | Motion::LineUnderscore
            | Motion::GotoPercent
            | Motion::Up
            | Motion::Down
            | Motion::Line
    );
    if line_motion && plain.failed {
        return plain;
    }
    let landing = op_landing(b, cur, m, count, curswant, false);
    // A charwise motion's own span and whether it is inclusive (its end char is operated on).
    let (s0, e0) = motion::char_span(b, cur, m, count);
    let inclusive = motion_inclusive(m) || e0 > cur.max(motion::target(b, cur, m, count));
    match wise {
        ForcedWise::Linewise => {
            if is_text_object(m) {
                if plain.s >= plain.e {
                    return OpSpan::new((cur, cur, true), false);
                }
                return whole(plain.s, (plain.e - 1).max(plain.s));
            }
            if line_motion {
                return whole(cur.min(landing), cur.max(landing));
            }
            if s0 >= e0 {
                return OpSpan::new((cur, cur, true), false);
            }
            // The line the motion ends on: an inclusive motion's last char, an exclusive one's end itself
            // (`dV}` takes the blank line `}` stops at; `dVw` from a line's last word stops at its end).
            let end = if inclusive { prev_boundary(b, e0) } else { e0 };
            whole(s0, end.max(s0))
        }
        ForcedWise::Charwise => {
            if line_motion {
                // `dvj`: exclusive [cursor, landing), then the exclusive-linewise rule.
                exclusive_op_span(b, cur.min(landing), cur.max(landing))
            } else if is_text_object(m) {
                OpSpan::new((plain.s, plain.e, false), false)
            } else if inclusive {
                // `dve`/`dvg_`: now exclusive — the end char is left alone.
                exclusive_op_span(b, s0, prev_boundary(b, e0).max(s0))
            } else {
                // `dvb`/`dvt`: now inclusive — the end char is taken too.
                OpSpan::new((s0, next_boundary(b, e0).min(b.len()), false), false)
            }
        }
        // Blockwise force never reaches here — the `OpForced` arm routes it to `block_op` (a rectangle is
        // not a single span, which is all `forced_span` can return).
        ForcedWise::Blockwise => {
            unreachable!("forced blockwise is handled by block_op before forced_span")
        }
    }
}

/// The span a `change` operator covers: the operator span ([`op_span_full`]) except for Vim's `cw`/`cW`
/// special case — on a non-blank, change up to the END of the word, not the start of the next (so no
/// trailing white space is eaten), and on a word's last char only that char. A linewise span keeps its
/// whole lines; the change planner keeps the first line's indent and the trailing newline.
pub(crate) fn change_span(b: &[u8], cur: usize, m: Motion, count: u32) -> OpSpan {
    if matches!(m, Motion::WordFwd | Motion::BigWordFwd) {
        let big = m == Motion::BigWordFwd;
        let end = motion::current_word_end_excl(b, cur, big);
        if end > cur {
            // Extend through `count-1` further word-ends (Vim `c{n}w`).
            let mut e = end;
            for _ in 1..count.max(1) {
                let next = motion::word_end_excl(b, e.saturating_sub(1), big);
                if next == e {
                    break; // a fixpoint (buffer end): a hostile count must not spin (#907)
                }
                e = next;
            }
            return OpSpan::new((cur, e, false), false);
        }
        // Cursor on blank → fall through to the ordinary span below.
    }
    op_span_full(b, cur, m, count)
}

/// The byte range `[s, e)` a Visual selection covers, from its anchor and active (cursor) ends. Charwise
/// includes the character under the higher end (Vim's inclusive selection); linewise spans whole lines
/// including the trailing newline where present.
pub(crate) fn selection_range(
    b: &[u8],
    anchor: usize,
    cursor: usize,
    line: bool,
) -> (usize, usize) {
    let lo = anchor.min(cursor);
    let hi = anchor.max(cursor);
    if line {
        let start = line_start(b, lo);
        let le = line_end(b, hi);
        let end = if le < b.len() { le + 1 } else { le };
        (start, end)
    } else {
        // Inclusive of the char under `hi`: a whole grapheme, or the line break when `hi` rests past a line's
        // last char (`v$`) — except the buffer's final newline, which opens no further line (nvim includes
        // the break only when a line follows).
        let end = if hi >= b.len() || (b[hi] == b'\n' && hi + 1 == b.len()) {
            hi
        } else if b[hi] == b'\n' {
            hi + 1
        } else {
            motion::next_grapheme(b, hi)
        };
        (lo, end)
    }
}

/// The geometry of a BLOCKWISE selection whose two corners are the byte offsets `anchor` and `cursor`.
/// Returns one `[start, end)` byte range per line the rectangle crosses (top line first), plus the block's
/// inclusive char-column bounds `(col_lo, col_hi)`.
///
/// Columns are CHAR columns (via [`col_of`]/[`at_col`]) — correct for ASCII and multibyte code points;
/// tab/wide-char VISUAL columns are a documented follow-up (the same curswant family as the i_CTRL-O gap).
/// Each row's range is clamped to that line's own length, so a line shorter than `col_lo` yields an empty
/// range at its end (Vim: short lines contribute nothing to a block delete/yank).
pub(crate) fn block_rows(
    b: &[u8],
    anchor: usize,
    cursor: usize,
) -> (Vec<(usize, usize)>, usize, usize) {
    let a_ls = line_start(b, anchor);
    let c_ls = line_start(b, cursor);
    let a_col = col_of(b, a_ls, anchor);
    let c_col = col_of(b, c_ls, cursor);
    let col_lo = a_col.min(c_col);
    let col_hi = a_col.max(c_col);
    let top_ls = a_ls.min(c_ls);
    let bot_ls = a_ls.max(c_ls);

    let mut rows = Vec::new();
    let mut rs = top_ls;
    loop {
        // `at_col` clamps to the line end, so on a short line `s == e == line_end` (an empty slice).
        let s = at_col(b, rs, col_lo);
        let e = at_col(b, rs, col_hi + 1);
        rows.push((s, e));
        let le = line_end(b, rs);
        if rs >= bot_ls || le >= b.len() {
            break;
        }
        rs = le + 1;
    }
    (rows, col_lo, col_hi)
}

/// Rebuild a blockwise selection of a given `width` (inclusive char columns) and `height` (rows) whose
/// TOP-LEFT corner is the char position of `cursor` — the geometry a dot-repeat (`.`) of a block
/// insert/append/change reconstructs at the caret (nvim reuses the original block size, positioned at the
/// cursor). Returns the same shape as [`block_rows`]: one `[start, end)` byte range per row (clamped to that
/// line's own length, so a short line yields an empty range at its end), plus the inclusive column bounds
/// `(col_lo, col_hi)`. Rows stop at end-of-buffer, so fewer than `height` rows are returned near the last line.
pub(crate) fn block_rows_at(
    b: &[u8],
    cursor: usize,
    width: usize,
    height: usize,
) -> (Vec<(usize, usize)>, usize, usize) {
    let ls = line_start(b, cursor);
    let col_lo = col_of(b, ls, cursor);
    let col_hi = col_lo + width.saturating_sub(1);
    let mut rows = Vec::new();
    let mut rs = ls;
    for _ in 0..height.max(1) {
        let s = at_col(b, rs, col_lo);
        let e = at_col(b, rs, col_hi + 1);
        rows.push((s, e));
        let le = line_end(b, rs);
        if le >= b.len() {
            break; // no further line to extend the block onto
        }
        rs = le + 1;
    }
    (rows, col_lo, col_hi)
}

/// Walk `count` char boundaries forward from `from`, never past `limit` (typically the line end).
/// Returns the end byte offset; fewer than `count` chars available stops at `limit` (Vim's EOL clamp).
pub(crate) fn advance_n(b: &[u8], from: usize, count: u32, limit: usize) -> usize {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            break;
        }
        let nb = next_boundary(b, end).min(limit);
        if nb == end {
            break;
        }
        end = nb;
    }
    end
}

/// Walk `count` GRAPHEME clusters forward from `from`, never past `limit` (the line end): Vim's `x`, `r`
/// and `~` step over a base char with its composing marks, or a ZWJ emoji sequence, as one character.
/// Returns the end and whether all `count` fit.
pub(crate) fn advance_graphemes(b: &[u8], from: usize, count: u32, limit: usize) -> (usize, bool) {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            return (end, false);
        }
        let nb = motion::next_grapheme(b, end).min(limit);
        if nb == end {
            return (end, false);
        }
        end = nb;
    }
    (end, true)
}
