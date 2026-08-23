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
    )
}

/// The byte range `[s, e)` a `delete`/`yank` operator covers for a motion + count, plus whether the removed
/// span is **linewise** (its register geometry and paste shape). Linewise motions delete whole lines;
/// paragraph motions (`d}`) become linewise per Vim's exclusive-linewise rule; everything else is charwise.
pub(crate) fn op_span(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    // Whole-lines span from the cursor's line through the target's line, inclusive of the final newline.
    let whole_lines = |t: usize| {
        let start = line_start(b, cur.min(t));
        let le = line_end(b, cur.max(t));
        let end = if le < b.len() { le + 1 } else { le };
        (start, end, true)
    };
    match m {
        // Line jumps (`dG`, `dgg`, `d{n}G`) and first-non-blank line motions (`d+`, `d-`, `d_`) are
        // linewise across every line between the cursor and target.
        Motion::GotoLine
        | Motion::LastLine
        | Motion::DownFirstNonBlank
        | Motion::UpFirstNonBlank
        | Motion::LineUnderscore => whole_lines(motion::target(b, cur, m, count)),
        // Vertical motions under an operator are linewise (`dj` deletes this line and the next). A motion
        // that cannot move a line (`dj` on the last line) fails the operator entirely (Vim) — a no-op range.
        Motion::Up | Motion::Down => {
            let t = motion::target(b, cur, m, count);
            if line_start(b, t) == line_start(b, cur) {
                (cur, cur, true)
            } else {
                whole_lines(t)
            }
        }
        // `dd` / `{count}dd`: whole lines from the cursor's line down.
        Motion::Line => {
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
            (start, end, true)
        }
        // Paragraph objects (`dip`/`dap`) are linewise (Vim); `char_span` already returns whole lines.
        Motion::InnerParagraph | Motion::AParagraph => {
            let (s, e) = motion::char_span(b, cur, m, count);
            (s, e, true)
        }
        // Paragraph motions (`d}`/`d{`) are exclusive charwise, but Vim's exclusive-linewise rule can turn
        // them linewise — shared with forced-charwise on a linewise motion (see `exclusive_linewise`).
        Motion::ParagraphFwd | Motion::ParagraphBack => {
            let t = motion::target(b, cur, m, count);
            exclusive_linewise(b, cur.min(t), cur.max(t))
        }
        // `di(`/`di{`/`di[`/`di<` on a block whose delimiters sit on their OWN lines is LINEWISE (Vim):
        // delete the whole inner lines, register linewise. Otherwise (single-line, or content sharing the
        // open/close line) it stays charwise. `a(` is never reshaped — see `linewise_inner_block`.
        Motion::Pair { around: false, .. } => {
            if let Some((s, e)) = linewise_inner_block(b, cur, m) {
                (s + 1, e, true) // [first-inner-line-start, close): whole inner lines incl the final '\n'
            } else {
                let (s, e) = motion::char_span(b, cur, m, count);
                (s, e, false)
            }
        }
        // Everything else is the motion's charwise span.
        _ => {
            let (s, e) = motion::char_span(b, cur, m, count);
            (s, e, false)
        }
    }
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
/// bit `o_v` toggles. Only the directional motions matter here; text objects carry their own exact span and
/// are left untoggled by [`forced_span`]. Kept in sync with `motion::char_span`'s inclusivity by hand.
pub(crate) fn motion_inclusive(m: Motion) -> bool {
    matches!(
        m,
        Motion::WordEnd
            | Motion::BigWordEnd
            | Motion::LineEnd
            | Motion::MatchBracket
            | Motion::FindChar { forward: true, .. }
    )
}

/// The span for an operator whose motion wise is FORCED (Vim `o_v`/`o_V`). `Linewise` expands the motion's
/// reach to whole lines; `Charwise` makes it charwise — turning a linewise motion into an inclusive span to
/// the target, or toggling a charwise motion's exclusive/inclusive edge. Text objects (which already carry
/// an exact span) are only reshaped for `Linewise`; `Charwise` leaves their edge alone.
pub(crate) fn forced_span(
    b: &[u8],
    cur: usize,
    m: Motion,
    count: u32,
    wise: ForcedWise,
) -> (usize, usize, bool) {
    let (s0, e0, was_line) = op_span(b, cur, m, count);
    match wise {
        ForcedWise::Linewise => {
            // Whole lines from the cursor's line THROUGH the line the motion lands on (Vim `dV}` includes
            // the blank paragraph line the `}` reaches). Text objects have no bare-move target, so expand
            // their own span instead.
            if is_text_object(m) || motion::target(b, cur, m, count) == cur && !was_line {
                if s0 >= e0 {
                    return (cur, cur, true);
                }
                let start = line_start(b, s0);
                let le = line_end(b, (e0 - 1).max(s0));
                return (start, if le < b.len() { le + 1 } else { le }, true);
            }
            let t = motion::target(b, cur, m, count);
            let (lo, hi) = (cur.min(t), cur.max(t));
            let start = line_start(b, lo);
            let le = line_end(b, hi);
            (start, if le < b.len() { le + 1 } else { le }, true)
        }
        ForcedWise::Charwise => {
            if was_line {
                // A linewise motion forced charwise becomes EXCLUSIVE [cur, target) and then takes Vim's
                // exclusive-linewise reduction — so `dvj` deletes the first line linewise, exactly as `d}`
                // would (both go through `exclusive_linewise`).
                let t = motion::target(b, cur, m, count);
                exclusive_linewise(b, cur.min(t), cur.max(t))
            } else if is_text_object(m) {
                (s0, e0, false)
            } else if motion_inclusive(m) {
                (s0, prev_boundary(b, e0).max(s0), false)
            } else {
                (s0, next_boundary(b, e0).min(b.len()), false)
            }
        }
        // Blockwise force never reaches here — the `OpForced` arm routes it to `block_op` (a rectangle is
        // not a single span, which is all `forced_span` can return).
        ForcedWise::Blockwise => {
            unreachable!("forced blockwise is handled by block_op before forced_span")
        }
    }
}

/// Vim's "linewise inner block": for `i(`/`i{`/`i[`/`i<` when the open delimiter is the LAST char on its
/// line (a newline immediately follows it) AND the close delimiter is the FIRST on its line (a newline
/// immediately precedes it), the inner object is LINEWISE — whole inner lines, like `cc`/`dd` — rather than
/// charwise. Returns the inner charwise span `(s, e)` (`s` = the newline just after the open, `e` = the
/// close delimiter) when the condition holds, else `None`. `a(` (around) is never linewise, so this only
/// fires for `around: false`. Empty/single-line/inline-content pairs return `None` (stay charwise).
pub(crate) fn linewise_inner_block(b: &[u8], cur: usize, m: Motion) -> Option<(usize, usize)> {
    let Motion::Pair {
        around: false,
        open,
        close,
    } = m
    else {
        return None;
    };
    if !open.is_ascii() || !close.is_ascii() {
        return None;
    }
    let (s, e) = motion::char_span(b, cur, m, 1); // == pair_span interior (open+1, close)
    if s < e && b.get(s) == Some(&b'\n') && b.get(e - 1) == Some(&b'\n') {
        Some((s, e))
    } else {
        None
    }
}

/// The byte range a `change` operator covers: for `Motion::Line` it is the *content* of the line(s) (the
/// newline is kept so `cc` leaves an empty line to type into); else the same charwise span as delete.
pub(crate) fn change_range(b: &[u8], cur: usize, m: Motion, count: u32) -> (usize, usize) {
    // Line jumps under change keep the final newline, leaving an empty line to type into (as `cc` does).
    if matches!(
        m,
        Motion::GotoLine
            | Motion::LastLine
            | Motion::DownFirstNonBlank
            | Motion::UpFirstNonBlank
            | Motion::LineUnderscore
    ) {
        let t = motion::target(b, cur, m, count);
        let start = line_start(b, cur.min(t));
        return (start, line_end(b, cur.max(t)));
    }
    // `cw`/`cW` special case (Vim): when the cursor is on a non-blank, change up to the END OF THE WORD,
    // NOT the start of the next word (so no trailing whitespace is eaten) — and, unlike `ce`, changing the
    // LAST char of a word changes only that char rather than jumping into the next word. On a blank cursor
    // the special case does not apply and the ordinary word span is used.
    if matches!(m, Motion::WordFwd | Motion::BigWordFwd) {
        let big = m == Motion::BigWordFwd;
        let end = motion::current_word_end_excl(b, cur, big);
        if end > cur {
            // Extend through `count-1` further word-ends (Vim `c{n}w`).
            let mut e = end;
            for _ in 1..count.max(1) {
                e = motion::word_end_excl(b, e.saturating_sub(1), big);
            }
            return (cur, e);
        }
        // Cursor on blank → fall through to the ordinary span below.
    }
    if m != Motion::Line {
        return motion::char_span(b, cur, m, count);
    }
    let start = line_start(b, cur);
    let mut pos = cur;
    for _ in 1..count.max(1) {
        let le = line_end(b, pos);
        if le < b.len() {
            pos = le + 1;
        } else {
            break;
        }
    }
    (start, line_end(b, pos))
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
        // Inclusive of the char under `hi`.
        let end = if hi < b.len() {
            next_boundary(b, hi)
        } else {
            hi
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

/// Like [`advance_n`] but reports whether the FULL `count` chars fit within `[from, limit)`. The bool
/// is false when fewer than `count` chars remain — the signal `{count}r` uses to become a clean no-op.
pub(crate) fn advance_n_checked(b: &[u8], from: usize, count: u32, limit: usize) -> (usize, bool) {
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            return (end, false);
        }
        end = next_boundary(b, end);
    }
    (end, true)
}
