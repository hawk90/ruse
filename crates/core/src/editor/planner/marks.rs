//! Marks and jumps: the change list, the `[`/`]`, `<`/`>` and context marks, named marks, the
//! jumplist and an operator to a mark.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{apply_op, beep, delete_quirk, mark_line_target, nop, plan_op_over_span};
use crate::editor::{exclusive_op_span, Action};
use crate::motion::{self, line_end, line_start, prev_grapheme};
use crate::{BytePos, Command, EditorState, GroupHint, MarkOp, Mode, Plan, SelectKind};

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::GotoLastChange => {
            // `` `. `` — move to the last change position (snapped into range). No-op before any edit.
            match st.view.last_change() {
                Some(BytePos(pos)) => nop(motion::snap(b, pos), st.view.mode),
                None => beep(cur, st.view.mode),
            }
        }
        // `'.` — LINEWISE to the first non-blank of the last change's line. No-op before any edit.
        Command::GotoLastChangeLine => match st.view.last_change() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `[ `` — to the FIRST char of the last changed/yanked text. No-op before any change/yank.
        Command::GotoChangeMarkStart => match st.view.change_mark_start() {
            Some(BytePos(pos)) => nop(mark_char_target(b, pos, st.view.mode), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `] `` — to the LAST char of the last changed/yanked text (EOL-clamped). No-op if unset.
        Command::GotoChangeMarkEnd => match st.view.change_mark_end() {
            Some(BytePos(pos)) => nop(mark_char_target(b, pos, st.view.mode), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `'[` — LINEWISE to the first non-blank of the `[` mark's line. No-op if unset.
        Command::GotoChangeMarkStartLine => match st.view.change_mark_start() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `']` — LINEWISE to the first non-blank of the `]` mark's line. No-op if unset.
        Command::GotoChangeMarkEndLine => match st.view.change_mark_end() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `< `` — to the START of the last visual selection. Charwise/blockwise: the byte-min end (the
        // block's top corner is that end). Linewise: column 0 of the first selected line (Vim). No-op if
        // no visual selection has ended. Verified against nvim v0.12.4 (parity fixtures).
        Command::GotoVisualMarkStart => match st.view.last_visual_bounds() {
            Some((BytePos(s), _e, kind)) => {
                let pos = if kind == SelectKind::Linewise {
                    line_start(b, s)
                } else {
                    s
                };
                nop(mark_char_target(b, pos, st.view.mode), st.view.mode)
            }
            None => beep(cur, st.view.mode),
        },
        // `` `> `` — to the END of the last visual selection. Charwise/blockwise: the byte-max end. Linewise:
        // the last char of the last selected line (Vim clamps its MAXCOL `>` mark onto the last char). No-op
        // if unset. Verified against nvim v0.12.4.
        Command::GotoVisualMarkEnd => match st.view.last_visual_bounds() {
            Some((_s, BytePos(e), kind)) => {
                let pos = if kind == SelectKind::Linewise {
                    line_end(b, e)
                } else {
                    e
                };
                nop(mark_char_target(b, pos, st.view.mode), st.view.mode)
            }
            None => beep(cur, st.view.mode),
        },
        // `'<` — LINEWISE to the first non-blank of the FIRST selected line. No-op if unset.
        Command::GotoVisualMarkStartLine => match st.view.last_visual_bounds() {
            Some((BytePos(s), _e, _kind)) => nop(mark_line_target(b, s), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `'>` — LINEWISE to the first non-blank of the LAST selected line. No-op if unset.
        Command::GotoVisualMarkEndLine => match st.view.last_visual_bounds() {
            Some((_s, BytePos(e), _kind)) => nop(mark_line_target(b, e), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `` `` — jump to the context mark (position before the latest jump). No-op before any jump. It is
        // itself a jump (`apply_command` records the leave position AFTER commit), so a repeat toggles.
        Command::GotoContextMark => match st.view.context_mark() {
            Some(BytePos(pos)) => nop(motion::snap(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `''` — LINEWISE to the first non-blank of the context mark's line. No-op before any jump.
        Command::GotoContextMarkLine => match st.view.context_mark() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `g;`/`g,` — step the change list. The cursor here is a placeholder; `commit` steps `change_idx`
        // (a mutation the pure planner cannot make) and overrides it with the resolved change position.
        Command::GotoOlderChange => moved(Action::JumpChange { older: true }, cur, st.view.mode),
        Command::GotoNewerChange => moved(Action::JumpChange { older: false }, cur, st.view.mode),
        // `CTRL-O`/`CTRL-I` — step the jumplist. `commit` mutates it (the planner is pure); the placeholder
        // cursor is overridden with the resolved jump position.
        Command::GotoOlderJump => moved(Action::JumpList { older: true }, cur, st.view.mode),
        Command::GotoNewerJump => moved(Action::JumpList { older: false }, cur, st.view.mode),
        // `m{a-z}` — install a named mark at the cursor. The cursor stays; `commit` writes the mark table.
        Command::SetNamedMark(ch) => moved(Action::SetNamedMark { ch: *ch }, cur, st.view.mode),
        // `` `{a-z} `` — jump to a named mark (snapped). No-op if unset.
        // A mark past a line's last char (`` `^ `` after `A`) lands on that char, as Vim's `check_cursor`.
        Command::GotoNamedMark(ch) => match st.view.named_mark(*ch) {
            Some(BytePos(pos)) => nop(mark_char_target(b, pos, st.view.mode), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `'{a-z}` — LINEWISE to the first non-blank of a named mark's line. No-op if unset.
        Command::GotoNamedMarkLine(ch) => match st.view.named_mark(*ch) {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` d`{a-z} `` / `d'{a-z}` (and `c`/`y`): operate from the cursor to a named mark. `` ` `` is
        // exclusive charwise; `'` is linewise over the line range. No-op if the mark is unset.
        Command::OpToMark { op, name, linewise } => {
            let Some(BytePos(mark)) = st.view.named_mark(*name) else {
                return beep(cur, st.view.mode);
            };
            // A mark left past a shortened line's end rests on its last char, as `` `a `` alone does (Vim's
            // `check_cursor` under the operator, #728), so `` d`a `` from below takes that char's line tail.
            let mark = mark_char_target(b, mark, Mode::Normal);
            let (lo, hi) = (cur.min(mark), cur.max(mark));
            if *linewise {
                // `'a` lands on the first non-blank of the mark's line.
                let landing = mark_line_target(b, mark);
                return plan_op_over_span(st, b, cur, *op, lo, hi, true, false, landing, hint);
            }
            // `` `a `` is an exclusive charwise motion: Vim's exclusive-linewise rule (a mark at column 0 of
            // a later line pulls the end back, linewise from the indent, #710), then — for `d` — Vi's
            // delete quirk. The line operators keep the plain span and take only the adjusted flag.
            let sp = exclusive_op_span(b, lo, hi);
            if matches!(
                op,
                MarkOp::Shift { .. } | MarkOp::Reindent | MarkOp::Format { .. } | MarkOp::Comment
            ) {
                return plan_op_over_span(st, b, cur, *op, lo, hi, false, sp.adjusted, mark, hint);
            }
            let sp = if *op == MarkOp::Delete {
                delete_quirk(b, sp, false)
            } else {
                sp
            };
            apply_op(st, b, cur, *op, sp, mark, hint)
        }
        _ => unreachable!("planner::marks plans only its own commands"),
    }
}

/// A non-editing plan carrying an arbitrary `action` — the jumplist / changelist / named-mark steps whose
/// resolution happens in `commit` (the planner supplies a placeholder cursor the action then overrides).
/// Same field shape as [`nop`] but the action is not forced to `Action::Nop`.
fn moved(action: Action, cursor: usize, mode: Mode) -> Plan {
    Plan {
        action,
        ..nop(cursor, mode)
    }
}

/// A charwise mark jump target: snap `pos` to a char boundary, then apply Vim's Normal-mode end-of-line
/// clamp — a mark that lands on (or past) the line's newline pulls back onto the line's last char. This is
/// what makes `` `] `` land on the last inserted/changed char even when the stored mark is the exclusive
/// insert end-caret or the trailing '\n' of a linewise span (matching Neovim's on-jump clamp).
///
/// In Visual mode Vim's `check_cursor` lets the cursor rest one past the last char (`'selection'`
/// inclusive), so there a mark past a shortened line's end lands on its newline and the selection takes
/// it (#728).
pub(in crate::editor) fn mark_char_target(b: &[u8], pos: usize, mode: Mode) -> usize {
    let p = motion::snap(b, pos.min(b.len()));
    let le = line_end(b, p);
    let ls = line_start(b, p);
    if mode.selection().is_some() {
        p.min(le)
    } else if p >= le && ls < le {
        prev_grapheme(b, le)
    } else {
        p
    }
}
