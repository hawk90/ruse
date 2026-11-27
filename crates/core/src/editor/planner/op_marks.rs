//! The change marks `'[` `']` `'.` that nvim's non-block operators leave where they differ from the
//! default (the edit span's ends and the cursor), as [`OpMarks`] on the plan (#1302): shifts, joins, `gq`,
//! linewise and multi-line deletes. Each planner names them in lines of the text after the command; see
//! `commit::resolve_op_marks`.

use crate::editor::{MarkAt, MarkCol, OpMarks};
use crate::motion::line_start;
use crate::{Plan, SelectKind};

/// `pos`'s 0-based line and byte column in `b`.
pub(super) fn line_col(b: &[u8], pos: usize) -> (usize, usize) {
    let pos = pos.min(b.len());
    (crate::pos::raw::line_of(b, pos), pos - line_start(b, pos))
}

/// `p` with its `'[` on `pos`'s byte column (an offset of the text before the command), its line kept: nvim's
/// line operators set `'[` from `oap->start`, the raw position the operator started at (the earlier of the
/// cursor and the motion's end), not from the edited bytes. A plan without marks is returned as it is.
pub(super) fn start_col_at(p: Plan, b: &[u8], pos: usize) -> Plan {
    let Some(mut m) = p.op_marks else {
        return p;
    };
    m.start.col = MarkCol::Byte(line_col(b, pos).1);
    Plan {
        op_marks: Some(m),
        ..p
    }
}

/// The change marks of nvim's `op_delete` for a line- or charwise delete of `[s, e)` (not a block one):
/// `'[` and `']` both on `oap->start` (`start`, the raw position the operator started at; a linewise
/// delete's start line is `s`'s); `'.` where its last `changed_lines` put it. A linewise delete ends in
/// `del_lines`, so `'.` is column 0 of the first deleted line's number; a charwise delete over several
/// lines ends in `do_join`'s `del_lines`, so `'.` is column 0 of the line after the start line, which may be
/// past the buffer's end. `None` for a charwise delete within one line, which the default marks cover.
pub(super) fn delete_marks(
    b: &[u8],
    s: usize,
    e: usize,
    linewise: bool,
    start: usize,
) -> Option<OpMarks> {
    let (line, _) = line_col(b, s);
    let (_, col) = line_col(b, start);
    let at = MarkAt::new(line, MarkCol::Byte(col));
    if linewise {
        return Some(OpMarks {
            start: at,
            end: at,
            dot: MarkAt::new(line, MarkCol::Byte(0)),
        });
    }
    b[s..e.min(b.len())].contains(&b'\n').then(|| OpMarks {
        start: at,
        end: at,
        dot: MarkAt::new(line + 1, MarkCol::Byte(0)),
    })
}

/// `p` with `marks` as its change marks, when there are any.
pub(super) fn with_marks(p: Plan, marks: Option<OpMarks>) -> Plan {
    match marks {
        Some(m) => Plan {
            op_marks: Some(m),
            ..p
        },
        None => p,
    }
}

/// Where nvim's Visual operator starts (`oap->start`), whose raw column its `'[` takes: the earlier of the
/// Visual start and the cursor, the Visual start taken at column 0 in a linewise area (`VIsual.col = 0`
/// comes first, so `$V>` starts at column 0 of its line). A dot-repeat (`redo`, whose area
/// `prepare_selection_op` rebuilt from the caret at `anchor`) starts at the caret, column and all (nvim's
/// `redo_VIsual_busy`: `oap->start = curwin->w_cursor`).
pub(super) fn visual_op_start(
    b: &[u8],
    anchor: usize,
    cur: usize,
    kind: SelectKind,
    redo: bool,
) -> usize {
    match kind {
        _ if redo => anchor,
        SelectKind::Linewise => line_start(b, anchor).min(cur),
        _ => anchor.min(cur),
    }
}

/// The change marks of an INCLUSIVE charwise yank of `[s, e)`: `'[` on `s`, `']` on the last byte of the
/// last char, `e - 1`, as nvim's `mb_adjust_opend` moves an inclusive `oap->end` there and `op_yank` sets
/// `b_op_end` from it (#1613). `'.` is unused: a yank records none. `None` for an empty span and for one
/// that takes a newline (`v$y`, whose `']` nvim's `decl` puts on the char before it, the first byte that
/// [`yank_mark_span`] gives): the default marks cover those.
pub(super) fn inclusive_yank_marks(b: &[u8], (s, e): (usize, usize)) -> Option<OpMarks> {
    if e <= s || e > b.len() || b[e - 1] == b'\n' {
        return None;
    }
    let (sl, sc) = line_col(b, s);
    let (el, ec) = line_col(b, e - 1);
    Some(OpMarks {
        start: MarkAt::new(sl, MarkCol::Byte(sc)),
        end: MarkAt::new(el, MarkCol::Byte(ec)),
        dot: MarkAt::new(sl, MarkCol::Byte(sc)),
    })
}

/// The span a charwise yank of `[s, e)` marks with `'[` `']`: a yank that takes a non-empty line's
/// newline (`v$y`, `lv$jy`) ends its `']` on that line's last char, as nvim's `op_yank` does (`decl` of the
/// end skips the NUL), not on the newline (#1302).
pub(super) fn yank_mark_span(b: &[u8], s: usize, e: usize) -> (usize, usize) {
    let e = e.min(b.len());
    if e > s + 1 && b[e - 1] == b'\n' && e - 1 > line_start(b, e - 1) {
        (s, e - 1)
    } else {
        (s, e)
    }
}
