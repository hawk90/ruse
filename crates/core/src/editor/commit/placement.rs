//! Where a committed change and an undo step leave things: the `[`/`]` span of a plan's edits, the
//! cursor a change records as its start for `u`/`CTRL-R` (Vim's `uh_cursor`), and the cursor an undo or
//! redo step lands on. Split out of `editor/commit.rs` (#1504).

use super::super::{range, EditorState, Mode};
use super::is_text_entry;
use crate::anchor::Tracked;
use crate::command::SelectKind;
use crate::document::Document;
use crate::edit::{Edit, EditList};
use crate::editor::CaretGravity;
use crate::motion::{at_col, col_of, line_end, line_start, prev_grapheme, snap};

/// The `(start, end)` byte span (half-open, POST-edit coordinates) the `[`/`]` change marks bracket for an
/// [`EditList`]: `start` is the first edit's position (nothing before it moved), and `end` is the end of the
/// LAST edit's inserted text — its position shifted by the cumulative length delta of every earlier edit,
/// plus that edit's inserted byte count. A pure delete yields `end == start` (both marks collapse onto the
/// deletion point, as Vim does). `None` for an empty list.
pub(super) fn edit_bracket(list: &EditList) -> Option<(usize, usize)> {
    let edits = list.edits();
    let first = edits.first()?;
    let last = edits.last()?;
    // Lines put below an unterminated last line arrive as `"\n" + lines` at its end: `[` is the first put
    // line's start, not the old line's end (`yyjp`[`, #1088).
    let start =
        if edits.len() == 1 && first.del == 0 && first.ins.len() > 1 && first.ins[0] == b'\n' {
            first.pos.0 + 1
        } else {
            first.pos.0
        };
    let delta_before_last: isize = edits[..edits.len() - 1].iter().map(Edit::delta).sum();
    let post_last_start = (last.pos.0 as isize + delta_before_last).max(0) as usize;
    Some((start, post_last_start + last.ins.len()))
}

/// Where a change begins for `u`/`C-r` (Vim's `uh_cursor`, #1063): the cursor at the moment Vim saves the
/// first undo entry, which an operator has already moved to the start of its span. Inferred from the mode
/// the change was made in and its first edit, since the plan does not carry the operator span.
pub(super) fn change_start(
    b: &[u8],
    mode: Mode,
    cursor: usize,
    anchor: Option<usize>,
    edits: &EditList,
) -> usize {
    let Some(first) = edits.edits().first() else {
        return cursor;
    };
    let pos = first.pos.0;
    match (mode.selection(), anchor) {
        // A selection operator starts at the selection's start; a linewise one at column 0 of its top line
        // unless the cursor is the top end (nvim 0.12.6: `jlVj>u` -> col 0, `jjlVk>u` -> col 1).
        (Some(SelectKind::Linewise), Some(a)) if a < cursor => line_start(b, a),
        (Some(SelectKind::Linewise), _) => cursor,
        (Some(SelectKind::Charwise), Some(a)) => a.min(cursor),
        (Some(SelectKind::Blockwise), Some(a)) => {
            let (top, bottom) = (a.min(cursor), a.max(cursor));
            let col =
                col_of(b, line_start(b, top), top).min(col_of(b, line_start(b, bottom), bottom));
            at_col(b, line_start(b, top), col)
        }
        (Some(_), None) => pos.min(cursor),
        // A text-entry session starts where the caret was when its first change landed.
        _ if is_text_entry(mode) => cursor,
        _ => {
            let at_line_start = pos == 0 || b[pos - 1] == b'\n';
            let end = pos + first.del;
            // Deleting whole lines: `dd`/`dj` keep the cursor's column, so only the line can move up.
            // The buffer's end is its last line's end, before the final newline (RFC-0024).
            let at_end = end == b.len() || end == crate::motion::content_end(b);
            let whole_lines = first.del > 0
                && ((at_line_start && (b[end - 1] == b'\n' || at_end))
                    || (b.get(pos) == Some(&b'\n') && end == b.len()));
            if first.del > 0 && !whole_lines && pos < cursor {
                // A charwise operator over a backward span (`db`, `d0`, `X`): its start.
                pos
            } else {
                // A linewise operator reaching up (`dk`, `>k`): its first line, at the cursor's column.
                let top = if whole_lines && !at_line_start {
                    pos + 1
                } else {
                    pos
                };
                if top < line_start(b, cursor) {
                    at_col(
                        b,
                        line_start(b, top),
                        col_of(b, line_start(b, cursor), cursor),
                    )
                } else {
                    cursor
                }
            }
        }
    }
}

/// Run one undo/redo step `f` with the view's marks tracked through it. Like nvim's `u_undoredo`, a step
/// first records the cursor `from` on the jumplist (`setpcmark`), so the entry follows the step's own line
/// changes; a step that finds nothing to re-apply records nothing (#1168).
pub(super) fn undo_step(
    st: &mut EditorState,
    from: usize,
    f: impl FnOnce(&mut Document, &mut [Tracked]) -> Option<crate::undo_cursor::UndoCursor>,
) -> Option<crate::undo_cursor::UndoCursor> {
    let saved = (st.view.jumps.clone(), st.view.jump_idx);
    st.view.push_jump(from);
    let placed = st.with_tracked(f);
    if placed.is_none() {
        (st.view.jumps, st.view.jump_idx) = saved;
    }
    placed
}

/// The byte offset for an undo placement: the column on its line (clamped, and pulled off the newline in
/// Normal mode as Vim's `check_cursor` does), or the sticky column when Vim would `beginline`.
pub(super) fn undo_target(
    st: &EditorState,
    placed: crate::undo_cursor::UndoCursor,
    mode: Mode,
) -> usize {
    let b = st.doc.bytes();
    let ls = crate::pos::raw::nth_line_start(b, placed.line);
    let le = line_end(b, ls);
    let at = match placed.col {
        Some(col) => snap(b, (ls + col).min(le)),
        None => range::want_col(b, ls, st.view.curswant.get(), st.view.indent.tabs()),
    };
    if at == le && ls < le && mode == Mode::Normal && st.view.caret == CaretGravity::OnChar {
        prev_grapheme(b, le)
    } else {
        at
    }
}
