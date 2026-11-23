//! Motions and operators: a counted motion, `d`/`c`/`y`, case and forced-type operators,
//! shifts, reindent, format, comment and the indent setters.
//!
//! Split out of [`super::plan_cmd`] (#1349); [`plan`] routes each operator family to its own planner
//! (#1479), and the arms are as they were there.

use super::{
    apply_op, beep, block_op, delete_quirk, nop, plan_comment, plan_format_lines, plan_reindent,
    plan_set_indents, plan_shift,
};
use crate::editor::{
    block_geom, change_span, exclusive_end_adjusted, forced_span, is_text_object, op_landing,
    op_span, op_span_full, want_col,
};
use crate::motion::{self, line_end, line_start, prev_grapheme};
use crate::{
    CaretGravity, Command, EditorState, ForcedWise, GroupHint, MarkOp, Mode, Motion, OpKind, Plan,
    SelectKind,
};

pub(super) fn plan(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    memo: &std::cell::OnceCell<usize>,
    cmd: &Command,
) -> Plan {
    match cmd {
        Command::Move(count, m) => plan_move(st, b, cur, memo, count, m),
        Command::Delete(..)
        | Command::Change(..)
        | Command::Yank(..)
        | Command::CaseMotion { .. }
        | Command::OpForced { .. } => plan_span_op(st, b, cur, hint, cmd),
        Command::ShiftRight(..)
        | Command::ShiftLeft(..)
        | Command::ShiftMotion { .. }
        | Command::Reindent { .. }
        | Command::Format { .. }
        | Command::Comment { .. } => plan_line_op(st, b, cur, hint, cmd),
        Command::SetIndents { .. } | Command::SetIndentCols { .. } | Command::InsertReindent => {
            plan_indent_set(st, cur, hint, cmd)
        }
        _ => unreachable!("planner::operator plans only its own commands"),
    }
}

/// The span operators: `d`/`c`/`y`, the case operators and the forced-wise forms, each applied to its
/// motion's span through [`apply_op`] (or [`block_op`] for a forced block).
fn plan_span_op(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::Delete(count, m) => {
            // Vi's "d" quirk (nvim `op_delete`): a charwise delete over several lines that starts in the
            // indent and leaves only blanks after its end goes linewise (`daw` on an empty line, `das` on
            // a whole-line `dat` tag block, `2d$` over whole lines, #747). Visual deletes never take it.
            let sp = delete_quirk(b, op_span_full(b, cur, *m, *count), is_text_object(*m));
            if sp.s >= sp.e && crate::textobj::owns(*m) {
                // An object that selects nothing (`di(` on `(\n)`) still leaves the cursor at its start.
                return nop(sp.s, st.view.mode);
            }
            let landing = view_landing(st, b, cur, *m, *count, false);
            apply_op(st, b, cur, MarkOp::Delete, sp, landing, hint)
        }
        Command::Change(count, m) => plan_change(st, b, cur, *count, m, hint),
        Command::Yank(count, m) => {
            let sp = op_span_full(b, cur, *m, *count);
            let landing = view_landing(st, b, cur, *m, *count, true);
            apply_op(st, b, cur, MarkOp::Yank, sp, landing, hint)
        }
        Command::CaseMotion {
            count,
            motion,
            case,
        } => {
            let sp = op_span_full(b, cur, *motion, *count);
            let landing = view_landing(st, b, cur, *motion, *count, false);
            apply_op(st, b, cur, MarkOp::Case(*case), sp, landing, hint)
        }
        // Forced-wise operator (`dvj`, `dVe`, `yv}`): reshape the span (see `forced_span`), then apply the
        // operator like its plain form.
        Command::OpForced {
            op,
            count,
            motion,
            wise,
        } => {
            // Forced blockwise (`d<C-v>j`): the cursor and the motion target are the block's two corners.
            if *wise == ForcedWise::Blockwise {
                let target = motion::target(b, cur, *motion, *count);
                let block = block_geom(b, cur, target, st.view.indent.tabs(), false);
                return block_op(b, &block, *op, hint);
            }
            let sp = forced_span(
                b,
                cur,
                *motion,
                *count,
                *wise,
                &st.view.curswant,
                st.view.indent.tabs(),
            );
            if sp.s >= sp.e && *op == OpKind::Change && !sp.linewise {
                // An empty charwise change still drops into Insert (Vim).
                return nop(sp.s, Mode::Insert);
            }
            let landing = view_landing(st, b, cur, *motion, *count, *op == OpKind::Yank);
            apply_op(st, b, cur, mark_op(*op), sp, landing, hint)
        }
        _ => unreachable!("plan_span_op plans only the span operators"),
    }
}

/// The linewise operators: `>`/`<` (counted and over a motion), `=`, `gq`/`gw` and `gc`, each over every
/// LINE its motion touches.
fn plan_line_op(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        // `{count}>>` moves `count - 1` lines down first (nvim `nv_lineop` → `cursor_down`), so on the
        // last line a count above 1 fails like `3dd` there, shifting nothing (#1159); a count past the
        // end from a line above still shifts the lines that exist.
        Command::ShiftRight(count) | Command::ShiftLeft(count)
            if *count > 1
                && st.view.caret == CaretGravity::OnChar
                && st.view.mode == Mode::Normal
                && line_end(b, cur) >= crate::motion::content_end(b) =>
        {
            beep(cur, st.view.mode)
        }
        Command::ShiftRight(count) => plan_shift(st, cur, *count, true, hint),
        Command::ShiftLeft(count) => plan_shift(st, cur, *count, false, hint),
        Command::ShiftMotion {
            left,
            count,
            motion,
        } => {
            // Resolve the motion's byte span, then shift every LINE it touches (Vim `>` is always linewise).
            let (s, e, _) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let (first_line, last_line) = touched_lines(b, s, e);
                let lines = (last_line - first_line + 1) as u32;
                plan_shift(st, line_start(b, s), lines, !*left, hint)
            }
        }
        Command::Reindent { count, motion } => {
            // `=` over a motion — reindent every LINE it touches (always linewise, like `>`).
            let (s, e, _) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let (first, last) = touched_lines(b, s, e);
                plan_reindent(st, first, last, hint)
            }
        }
        Command::Format {
            count,
            motion,
            keep_cursor,
        } => plan_format(st, cur, *count, *motion, *keep_cursor, hint),
        Command::Comment { count, motion } => {
            // `gc` over a motion: every LINE it touches; the cursor goes to the start of the range
            // (`g@`'s `'[`), keeping its column over a linewise motion.
            let (s, e, linewise) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let landing = view_landing(st, b, cur, *motion, *count, true);
                let at = if linewise { cur.min(landing) } else { s };
                let (first, last) = touched_lines(b, s, e);
                plan_comment(st, first, last, at, hint)
            }
        }
        _ => unreachable!("plan_line_op plans only the linewise operators"),
    }
}

/// The indent setters the frontend resolves into per-line levels or columns.
fn plan_indent_set(st: &EditorState, cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::SetIndents {
            first_line,
            last_line,
            levels,
        } => {
            let sw = st.view.indent.shift_width();
            let cols: Vec<Option<usize>> = levels.iter().map(|l| Some(l * sw)).collect();
            plan_set_indents(st, *first_line, *last_line, &cols, hint)
        }
        Command::SetIndentCols {
            first_line,
            last_line,
            cols,
        } => plan_set_indents(st, *first_line, *last_line, cols, hint),
        // Resolved by the frontend from the buffer's indenter; unresolved it does nothing (nvim without one).
        Command::InsertReindent => nop(cur, st.view.mode),
        _ => unreachable!("plan_indent_set plans only the indent setters"),
    }
}

/// [`op_landing`] with the view's wanted column and tab stops.
fn view_landing(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    m: Motion,
    count: u32,
    yank: bool,
) -> usize {
    op_landing(
        b,
        cur,
        m,
        count,
        &st.view.curswant,
        st.view.indent.tabs(),
        yank,
    )
}

/// The first and last line the non-empty byte span `s..e` touches.
fn touched_lines(b: &[u8], s: usize, e: usize) -> (usize, usize) {
    (
        crate::pos::raw::line_of(b, s),
        crate::pos::raw::line_of(b, e - 1),
    )
}

/// A counted motion (`Move`): in a selection mode a text object sets both ends; otherwise the cursor moves to the target.
fn plan_move(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    memo: &std::cell::OnceCell<usize>,
    count: &u32,
    m: &Motion,
) -> Plan {
    // A text object issued in a selection mode (`viw`, `vi(`) sets BOTH ends: anchor at the object's
    // start, cursor on its last char (inclusive selection). A bare motion only moves the cursor.
    if st.view.mode.selection().is_some() && crate::textobj::owns(*m) {
        return plan_visual_owned_object(st, b, cur, *count, m);
    }
    if st.view.mode.selection().is_some() && is_text_object(*m) {
        return plan_visual_span_object(st, b, cur, *count, m);
    }
    // Vertical motions honour the sticky desired column (curswant) so `j`/`k` keep the wanted
    // column through shorter interior lines; every other motion computes its own landing.
    let ts = st.view.indent.tabs();
    let target = match m {
        Motion::Up => motion::vmove(b, cur, *count, false, st.view.curswant.get(), ts),
        Motion::Down => motion::vmove(b, cur, *count, true, st.view.curswant.get(), ts),
        // `gg`/`G`/`{count}%` keep the wanted column too (`'nostartofline'`, nvim's
        // `beginline(BL_SOL)`).
        Motion::GotoLine | Motion::LastLine | Motion::GotoPercent => want_col(
            b,
            line_start(b, motion::target(b, cur, *m, *count)),
            st.view.curswant.get(),
            ts,
        ),
        // In charwise Visual `$` goes just past the line's last char, so the selection takes the
        // line break (`v$d` joins the lines, #697). In blockwise Visual too (nvim's `coladvance`
        // puts a Visual `$` one past the end): a motion from there starts past the last char, so
        // `<C-v>$h` rests ON it and the block still ends there, and `ge` finds that word (#1207).
        Motion::LineEnd
            if matches!(
                st.view.mode,
                Mode::Visual {
                    kind: SelectKind::Charwise | SelectKind::Blockwise
                } | Mode::Select {
                    kind: SelectKind::Charwise | SelectKind::Blockwise
                }
            ) =>
        {
            line_end(b, motion::target(b, cur, *m, *count))
        }
        // A Visual `^` on an all-blank line rests on the last blank (`BL_FIX`), not the line break past it,
        // so `v^d` keeps the lines apart (#762). The Emacs profile keeps `back-to-indentation`'s line end.
        Motion::LineFirstNonBlank if st.view.caret == CaretGravity::OnChar => {
            motion::first_non_blank_fix(b, cur)
        }
        _ => *memo.get_or_init(|| motion::target(b, cur, *m, *count)),
    };
    // Normal mode never rests on a non-empty line's trailing newline, whatever the motion: `j`/`k`
    // overshooting a short line (or `$`'s MAXCOL), `w`/`W` past the last word, `l`, `go` onto a
    // newline byte and `^` on an all-blank line (Vim's `BL_FIX`) all pull back onto the last char.
    // The phantom slot after the final newline is handled for every mode in `commit` (#677).
    // Visual may reach the newline (`vl` at the end selects it, as nvim does), Insert rests after the
    // last char, and the Emacs profile (between-char gravity) is never clamped.
    let target = if matches!(st.view.mode, Mode::Normal) && st.view.caret == CaretGravity::OnChar {
        // On a non-empty line's NUL slot (`target == line_end`, past its start), told by
        // the bytes around it rather than by finding the line (#1231).
        if target <= b.len() && motion::at_nul(b, target) && !motion::at_bol(b, target) {
            prev_grapheme(b, target)
        } else {
            target
        }
    } else {
        target
    };
    // `{count}e` past the last word moves to the end and still fails (nvim's `end_word`).
    if matches!(m, Motion::WordEnd | Motion::BigWordEnd)
        && st.view.caret == CaretGravity::OnChar
        && matches!(st.view.mode, Mode::Normal | Mode::Visual { .. })
        && motion::fails_with(
            b,
            cur,
            *m,
            *count,
            false,
            st.view.mode.selection().is_some(),
            memo,
        )
    {
        return beep(target, st.view.mode);
    }
    nop(target, st.view.mode)
}

/// A text object the `textobj` module owns, issued in a selection mode (`viw`, `va(`, `vis`).
fn plan_visual_owned_object(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    m: &Motion,
) -> Plan {
    // Word, sentence, bracket and quote objects follow Vim's Visual rules: on a one-char area
    // they select the object, on a wider one they EXTEND it (`viwiw`, `va(a(`, `visis`). They
    // turn a linewise selection charwise; bracket and sentence objects any selection.
    let anchor = st.view.anchor.unwrap_or(cur);
    let Some((a, c)) = crate::textobj::visual(b, anchor, cur, *m, count) else {
        return nop(cur, st.view.mode);
    };
    let to_charwise = st.view.mode.selection() == Some(SelectKind::Linewise)
        || matches!(
            m,
            Motion::Pair { .. } | Motion::InnerSentence | Motion::ASentence
        );
    let mode = match st.view.mode {
        Mode::Select { .. } if to_charwise => Mode::Select {
            kind: SelectKind::Charwise,
        },
        _ if to_charwise => Mode::Visual {
            kind: SelectKind::Charwise,
        },
        mode => mode,
    };
    Plan {
        set_anchor: Some(a),
        ..nop(c, mode)
    }
}

/// Any other text object issued in a selection mode (`vip`, `vap`): it selects its character span.
fn plan_visual_span_object(st: &EditorState, b: &[u8], cur: usize, count: u32, m: &Motion) -> Plan {
    let (s, e) = motion::char_span(b, cur, *m, count);
    if s >= e {
        return nop(cur, st.view.mode);
    }
    // `ip`/`ap` are LINEWISE objects: selecting one in Visual switches the selection to
    // linewise (Vim), so a following `d`/`y` gets linewise register geometry. Other objects
    // (`iw`, `i(`, `is`, …) keep the current visual kind.
    let obj_mode = if matches!(m, Motion::InnerParagraph | Motion::AParagraph) {
        match st.view.mode {
            Mode::Select { .. } => Mode::Select {
                kind: SelectKind::Linewise,
            },
            _ => Mode::Visual {
                kind: SelectKind::Linewise,
            },
        }
    } else {
        st.view.mode
    };
    Plan {
        set_anchor: Some(s),
        ..nop(motion::prev_grapheme(b, e), obj_mode)
    }
}

/// `c{motion}` / `cc`: delete the change span, capture it to a register, and enter Insert. A linewise span
/// (`cc`/`S`, `cj`, `c}` reduced by the exclusive-linewise rule, `cip`, a linewise inner block) keeps the
/// first line's indent and the trailing newline ([`plan_linewise_change`]); a failed linewise motion stays
/// in Normal.
fn plan_change(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    m: &Motion,
    hint: GroupHint,
) -> Plan {
    let sp = change_span(b, cur, *m, count);
    let landing = view_landing(st, b, cur, *m, count, false);
    apply_op(st, b, cur, MarkOp::Change, sp, landing, hint)
}

/// The `MarkOp` form of a delete / change / yank.
fn mark_op(op: OpKind) -> MarkOp {
    match op {
        OpKind::Delete => MarkOp::Delete,
        OpKind::Change => MarkOp::Change,
        OpKind::Yank => MarkOp::Yank,
    }
}

/// `gq`/`gw` {motion} — format the motion's whole lines (nvim's `op_format`). `gw` (`keep_cursor`) keeps
/// the caret on its text; `gq` leaves it on the first non-blank of the last formatted line.
fn plan_format(
    st: &EditorState,
    cur: usize,
    count: u32,
    motion: Motion,
    keep_cursor: bool,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let (s, e, _) = op_span(b, cur, motion, count);
    if s >= e {
        return nop(cur, st.view.mode);
    }
    // Through the last touched line (exclusive of its trailing newline).
    plan_format_lines(
        st,
        cur,
        line_start(b, s),
        line_end(b, e - 1),
        keep_cursor,
        exclusive_end_adjusted(b, cur, motion, count),
        hint,
    )
}
