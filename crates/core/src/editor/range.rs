//! Pure range/motion geometry for the plan pipeline: the byte-span helpers that turn a
//! `(motion, count)` (or a Visual selection's two ends) into the `[start, end)` range an operator
//! covers, plus the char-boundary walkers. Every function here is pure over `&[u8]` + primitives —
//! it touches no `View`/`EditorState` state — so it lives beside `mod.rs`'s hub and is re-exported
//! `pub(crate)` (`planner.rs`'s `use super::*` and `View::{selection_span,block_spans}` resolve
//! these names through that re-export).

use crate::command::ForcedWise;
use crate::motion::{self, line_end, line_start, next_boundary, prev_boundary, Motion};

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
    curswant: &super::Want,
    ts: usize,
    yank: bool,
) -> usize {
    match m {
        Motion::Up => motion::vmove(b, cur, count, false, curswant.get(), ts),
        Motion::Down => motion::vmove(b, cur, count, true, curswant.get(), ts),
        Motion::GotoLine | Motion::LastLine | Motion::GotoPercent => want_col(
            b,
            line_start(b, motion::target(b, cur, m, count)),
            curswant.get(),
            ts,
        ),
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
                want_col(b, ls, curswant.get(), ts)
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
    curswant: &super::Want,
    ts: usize,
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
    let landing = op_landing(b, cur, m, count, curswant, ts, false);
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
                exclusive_op_span(b, s0, motion::prev_grapheme(b, e0).max(s0))
            } else {
                // `dvb`/`dvt`: now inclusive — the end char is taken too.
                OpSpan::new(
                    (s0, motion::next_grapheme(b, e0).min(b.len()), false),
                    false,
                )
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
                let next = motion::word_end_excl(b, motion::prev_grapheme(b, e), big);
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

// --- Blockwise geometry in VIRTUAL (screen) columns -------------------------------------------------
// One model for every `CTRL-V` operation — yank/delete/change, `~`/`u`/`U`/`g?`/`r`, block insert, block
// paste, Visual put and the Visual dot-repeat — so they all agree with Vim's `block_prep`. A column here is a
// display cell: a `<Tab>` spans to the next tabstop and a wide (CJK) character spans two cells.

/// One display cluster at byte `i` of a line ending at `le`, starting at virtual column `v`: its end byte
/// and its width in cells. A grapheme cluster is one cell run (a combining mark rides on its base); a
/// `<Tab>` reaches the next multiple of `ts`; anything else is its base char's terminal width, at least 1
/// (the renderer's rule, `screen.rs`).
pub(crate) fn cluster_cells(b: &[u8], i: usize, le: usize, v: usize, ts: usize) -> (usize, usize) {
    use unicode_width::UnicodeWidthChar;
    if b[i] == b'\t' {
        let ts = ts.max(1);
        return (i + 1, ts - v % ts);
    }
    // An ASCII char not followed by a combining mark is one cell: skip the line-wide grapheme scan, which
    // made walking a long line quadratic (the wanted column is measured after every command, #1090).
    if b[i] < 0x80 && b.get(i + 1).is_none_or(|&n| n < 0x80 || i + 1 >= le) {
        return (i + 1, 1);
    }
    let j = grapheme_end(b, i, le);
    let w = std::str::from_utf8(&b[i..j])
        .ok()
        .and_then(|s| s.chars().next())
        .and_then(UnicodeWidthChar::width)
        .unwrap_or(1)
        .max(1);
    (j, w)
}

/// The end of the grapheme cluster that starts at the cluster boundary `i` of a line ending at `le`.
/// Segments a small window after `i`, doubled until the cluster ends inside it, so the cost is the
/// cluster's length, not the line's (#1109; `motion::next_grapheme` re-segmented the whole line then). A
/// window cut inside a character is trimmed to the last whole one; bytes that are not UTF-8 are one cluster
/// each, as the motions count them since #1232.
fn grapheme_end(b: &[u8], i: usize, le: usize) -> usize {
    use unicode_segmentation::GraphemeCursor;
    let le = le.min(b.len()).max(i + 1);
    let mut window = 8; // two 4-byte characters: most clusters end inside it
    loop {
        let end = (i + window).min(le);
        // The window's valid prefix, and whether no more text can join a cluster at its end: the window
        // reached the line end, or an invalid byte (not just a character cut by the window) follows.
        let (s, last) = match std::str::from_utf8(&b[i..end]) {
            Ok(s) => (s, end == le),
            Err(e) => {
                let s = std::str::from_utf8(&b[i..i + e.valid_up_to()]).unwrap_or_default();
                (s, end == le || e.error_len().is_some())
            }
        };
        if s.is_empty() {
            return motion::next_boundary(b, i).clamp(i + 1, le);
        }
        let mut gc = GraphemeCursor::new(0, s.len(), true);
        match gc.next_boundary(s, 0) {
            // A boundary inside the window is final; one at its end only when nothing can follow.
            Ok(Some(rel)) if rel < s.len() || last => return i + rel,
            _ if last => return i + s.len(),
            _ => window *= 2,
        }
    }
}

/// The virtual columns `[start, end]` (inclusive) the cluster at `pos` covers on its line; at the line end
/// (no char there) both are the column just past the line's last cell, as Vim's `getvvcol` reports on NUL.
pub(crate) fn vcols_at(b: &[u8], pos: usize, ts: usize) -> (usize, usize) {
    let ls = line_start(b, pos);
    let le = line_end(b, ls);
    let (mut i, mut v) = (ls, 0);
    while i < le {
        let (j, w) = cluster_cells(b, i, le, v, ts);
        if pos < j {
            return (v, v + w - 1);
        }
        i = j;
        v += w;
    }
    (v, v)
}

/// The cells a whole line (from `ls`) spans: the virtual column just past its last character.
pub(crate) fn line_cells(b: &[u8], ls: usize, ts: usize) -> usize {
    vcols_at(b, line_end(b, ls), ts).0
}

/// The cells `text` spans when laid out from column 0 (a register row's width).
pub(crate) fn text_cells(text: &[u8], ts: usize) -> usize {
    let (mut i, mut v) = (0, 0);
    while i < text.len() {
        let (j, w) = cluster_cells(text, i, text.len(), v, ts);
        i = j;
        v += w;
    }
    v
}

/// The byte of the character covering virtual column `vcol` on the line starting at `ls` — Vim's
/// `coladvance` in Normal/Visual: past the last character it stops ON that last character (an empty line
/// gives its start).
pub(crate) fn at_vcol(b: &[u8], ls: usize, vcol: usize, ts: usize) -> usize {
    let le = line_end(b, ls);
    let (mut i, mut v) = (ls, 0);
    let mut last = ls;
    while i < le {
        let (j, w) = cluster_cells(b, i, le, v, ts);
        if vcol < v + w {
            return i;
        }
        last = i;
        i = j;
        v += w;
    }
    last
}

/// Where a vertical move lands for the wanted column `want` (`curswant`, a virtual column) on the line at
/// `ls` — Vim's `coladvance`: the character covering that cell (a `<Tab>` or a wide char spans several), or
/// the line end when the line is shorter or `want` is [`MAXCOL`]. The caller pulls a Normal-mode caret off
/// the line end (#1090).
pub(crate) fn want_col(b: &[u8], ls: usize, want: usize, ts: usize) -> usize {
    let le = line_end(b, ls);
    let (mut i, mut v) = (ls, 0);
    while i < le {
        let (j, w) = cluster_cells(b, i, le, v, ts);
        if want < v + w {
            return i;
        }
        i = j;
        v += w;
    }
    le
}

/// The wanted column (`curswant`) a caret at `pos` sets: its virtual column. On a `<Tab>` an on-char caret
/// (`tab_end`: Normal, or Visual at or past the anchor) stands on the tab's LAST cell, as Vim draws and
/// counts it (`getvcol`'s cursor column); otherwise the first (#1090).
pub(crate) fn wanted_vcol(b: &[u8], pos: usize, ts: usize, tab_end: bool) -> usize {
    let (s, e) = vcols_at(b, pos, ts);
    if tab_end && b.get(pos) == Some(&b'\t') {
        e
    } else {
        s
    }
}

/// One line's slice of a block (Vim's `struct block_def`). Columns `lo..=hi` are the block's; a character
/// that crosses an edge (a `<Tab>` or a wide char) "straddles" it and is split into spaces by the ops.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct BlockRow {
    /// The line's start and end bytes.
    pub ls: usize,
    pub le: usize,
    /// The characters WHOLLY inside the block (what a yank copies and `~`/`r` change).
    pub s: usize,
    pub e: usize,
    /// The characters the block touches at all, straddlers included (what a delete removes).
    pub ds: usize,
    pub de: usize,
    /// Cells of a straddler on the left / right that lie INSIDE the block (a yank keeps them as spaces).
    pub in_l: usize,
    pub in_r: usize,
    /// Cells of a straddler on the left / right that lie OUTSIDE the block (a delete leaves them as spaces).
    pub out_l: usize,
    pub out_r: usize,
    /// The virtual column just past the line's last character.
    pub cells: usize,
}

impl BlockRow {
    /// The line ends strictly before the block's left edge.
    pub fn short(&self, lo: usize) -> bool {
        self.cells < lo
    }
}

/// A block: its rows (top first) and its inclusive virtual-column bounds. `hi` is [`MAXCOL`] for a `$`
/// block, whose rows each run to their own line end.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Block {
    pub rows: Vec<BlockRow>,
    pub lo: usize,
    pub hi: usize,
}

impl Block {
    /// The block's width in cells — a blockwise register's width (Vim `y_width + 1`): `hi - lo + 1`, or
    /// for a `$` block the longest row.
    pub fn width(&self) -> usize {
        if self.hi == MAXCOL {
            self.rows
                .iter()
                .map(|r| r.cells.saturating_sub(self.lo))
                .max()
                .unwrap_or(0)
        } else {
            self.hi - self.lo + 1
        }
    }

    /// The `[start, end)` byte range per row the selection highlights (the characters it touches).
    pub fn spans(&self) -> Vec<(usize, usize)> {
        self.rows.iter().map(|r| (r.ds, r.de)).collect()
    }

    /// The text a block yank captures, one row per line joined by `\n`: each row's whole characters, with
    /// the inside cells of a straddler as spaces, and a line that ends before the block as `width` spaces
    /// (Vim's `block_prep` with `is_del` false).
    pub fn yank_text(&self, b: &[u8]) -> Vec<u8> {
        let width = self.width();
        let mut text = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if i > 0 {
                text.push(b'\n');
            }
            if r.short(self.lo) {
                // Vim fills `end_vcol - start_vcol + 1` cells, and a `$` block's `end_vcol` is the column
                // just past its longest line — one more than its width.
                let fill = width + usize::from(self.hi == MAXCOL);
                text.extend(std::iter::repeat_n(b' ', fill));
                continue;
            }
            text.extend(std::iter::repeat_n(b' ', r.in_l));
            text.extend_from_slice(&b[r.s..r.e]);
            text.extend(std::iter::repeat_n(b' ', r.in_r));
        }
        text
    }
}

/// The slice of the line at `ls` that the block `lo..=hi` covers (see [`BlockRow`]).
pub(crate) fn block_row(b: &[u8], ls: usize, lo: usize, hi: usize, ts: usize) -> BlockRow {
    let le = line_end(b, ls);
    let mut r = BlockRow {
        ls,
        le,
        ..BlockRow::default()
    };
    let (mut i, mut v) = (ls, 0);
    let (mut s, mut ds) = (None, None);
    let mut after_left = None; // the byte just past a left straddler
    while i < le {
        let (j, w) = cluster_cells(b, i, le, v, ts);
        let last = v + w - 1;
        if last < lo {
            i = j;
            v += w;
            continue;
        }
        if v > hi {
            break;
        }
        ds.get_or_insert(i);
        r.de = j;
        match (v < lo, last > hi) {
            (false, false) => {
                s.get_or_insert(i);
                r.e = j;
            }
            (true, false) => {
                r.out_l = lo - v;
                r.in_l = last - lo + 1;
                after_left = Some(j);
            }
            (false, true) => {
                r.in_r = hi - v + 1;
                r.out_r = last - hi;
            }
            (true, true) => {
                r.out_l = lo - v;
                r.in_l = hi - lo + 1;
                r.out_r = last - hi;
                after_left = Some(i);
            }
        }
        i = j;
        v += w;
    }
    // Where the inside run sits when it holds no whole character: after a left straddler, or where the walk
    // stopped (the first character right of the block, or the line end).
    let s = s.unwrap_or(after_left.unwrap_or(i));
    r.s = s;
    if r.e < s {
        r.e = s;
    }
    r.ds = ds.unwrap_or(s);
    if r.de < r.ds {
        r.de = r.ds;
    }
    r.cells = {
        let mut cells = v;
        while i < le {
            let (j, w) = cluster_cells(b, i, le, cells, ts);
            i = j;
            cells += w;
        }
        cells
    };
    r
}

/// The block a `CTRL-V` selection with corners `anchor` and `cursor` covers (Vim `do_pending_operator`):
/// the left edge is the lower of the corners' first cells, the right edge the higher of their last cells
/// (so a corner on a `<Tab>` or a wide char takes all of it), or [`MAXCOL`] when `to_eol` (`$`).
pub(crate) fn block_geom(b: &[u8], anchor: usize, cursor: usize, ts: usize, to_eol: bool) -> Block {
    let (a0, a1) = vcols_at(b, anchor, ts);
    let (c0, c1) = vcols_at(b, cursor, ts);
    let hi = if to_eol { MAXCOL } else { a1.max(c1) };
    block_from(
        b,
        anchor.min(cursor),
        anchor.max(cursor),
        a0.min(c0),
        hi,
        ts,
    )
}

/// A block of `width` cells and `height` rows whose top-left corner is the first cell of the character at
/// `cursor` — what a dot-repeat rebuilds at the caret (nvim reuses the original size there). Rows stop at the
/// end of the buffer, so fewer than `height` come back near the last line.
pub(crate) fn block_at(
    b: &[u8],
    cursor: usize,
    width: usize,
    height: usize,
    ts: usize,
    to_eol: bool,
) -> Block {
    let lo = vcols_at(b, cursor, ts).0;
    let hi = if to_eol {
        MAXCOL
    } else {
        lo + width.max(1) - 1
    };
    let ls = line_start(b, cursor);
    let last =
        crate::pos::raw::nth_line_start(b, crate::pos::raw::line_of(b, ls) + height.max(1) - 1);
    block_from(b, ls, last.max(ls), lo, hi, ts)
}

/// The rows of the block `lo..=hi` on every line from the one holding `top` down to the one holding `bot`.
fn block_from(b: &[u8], top: usize, bot: usize, lo: usize, hi: usize, ts: usize) -> Block {
    let bot_ls = line_start(b, bot);
    let mut rows = Vec::new();
    let mut rs = line_start(b, top);
    loop {
        rows.push(block_row(b, rs, lo, hi, ts));
        let le = line_end(b, rs);
        if rs >= bot_ls || le >= b.len() {
            break;
        }
        rs = le + 1;
    }
    Block { rows, lo, hi }
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
    let steps = motion::Steps::new(b);
    let mut end = from;
    for _ in 0..count {
        if end >= limit {
            return (end, false);
        }
        let nb = steps.next(end).min(limit);
        if nb == end {
            return (end, false);
        }
        end = nb;
    }
    (end, true)
}

#[cfg(test)]
mod grapheme_end_tests {
    use super::*;

    /// #1109: the windowed segmentation agrees with whole-line segmentation on every cluster, including
    /// clusters longer than the first window and flag pairs that need the text before them.
    #[test]
    fn windowed_clusters_match_whole_line_segmentation() {
        let zwj_family = "👨\u{200d}👩\u{200d}👧\u{200d}👦".repeat(3); // > 64 bytes, joined
        let marks = format!("a{}", "\u{301}".repeat(40)); // one base and 40 combining marks: 81 bytes
        for line in [
            "가나다 abc",
            "e\u{301}x\u{301}\u{302}",
            "🇰🇷🇯🇵🇺🇸x",
            zwj_family.as_str(),
            marks.as_str(),
            "\t한\tx",
        ] {
            let b = format!("{line}\nnext").into_bytes();
            let le = line.len();
            let mut i = 0;
            while i < le {
                let want = motion::next_grapheme(&b, i).clamp(i + 1, le);
                assert_eq!(grapheme_end(&b, i, le), want, "{line:?} at {i}");
                i = want;
            }
        }
    }

    #[test]
    fn invalid_bytes_are_one_cluster_each() {
        let b = b"\xffa\xfe\n";
        assert_eq!(grapheme_end(b, 0, 3), 1);
        assert_eq!(grapheme_end(b, 1, 3), 2);
        assert_eq!(grapheme_end(b, 2, 3), 3);
    }
}
