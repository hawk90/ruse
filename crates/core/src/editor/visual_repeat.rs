//! Visual-mode dot-repeat (`:help visual-repeat`, #794): the geometry the last selection operator covered
//! ([`VisualOpGeom`]) and the step that records it or rebuilds an equivalent selection at the caret before
//! the operator plans. Moved out of `editor/mod.rs` (#1495).

use super::{at_vcol, block_geom, range, vcols_at, Want};
use crate::motion::{line_end, line_start};
use crate::{Command, EditorState, Mode, SelectKind};

/// The AMOUNT of text the last Visual-selection operator covered, kept so dot-repeat (`.`) can rebuild an
/// equivalent selection at the caret (`:help visual-repeat`, #794): the same number of lines; within one
/// line the same number of screen cells; for a multi-line charwise selection the same end column on the
/// last line; for a block the same width. Columns are VIRTUAL (screen) columns, as in Vim (#1064): a `<Tab>`
/// spans to its tabstop and a wide character two cells (see [`vcols_at`](range::vcols_at)).
#[derive(Clone, Copy, Debug)]
pub(super) struct VisualOpGeom {
    pub(super) kind: SelectKind,
    /// Lines spanned (>= 1).
    pub(super) lines: usize,
    /// Charwise one-line: cell count. Charwise multi-line: the end's last cell column. Blockwise: width.
    pub(super) cols: usize,
    /// `$` extended the selection (curswant at MAXCOL): the repeat runs to each line's end.
    pub(super) to_eol: bool,
}

/// Whether `cmd` operates on the live Visual/Select selection — the commands whose dot-repeat (issued in
/// Normal, with no selection) rebuilds one from [`VisualOpGeom`]. Block `I`/`A`/`c` has its own
/// [`BlockGeom`] path in the planner.
fn is_selection_op(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::DeleteSelection
            | Command::ChangeSelection
            | Command::ReplaceSelection(_)
            | Command::ReplaceSelectionChar(_)
            | Command::CaseSelection(_)
            | Command::PasteSelection { .. }
            | Command::IncrementSelection { .. }
            | Command::VisualOp(_)
    )
}

/// Before a selection operator runs: record the live selection's geometry, or — for a dot-repeat, which
/// arrives in Normal with no selection — rebuild one of the recorded size starting at the caret, so the
/// operator then plans exactly as if the user had re-selected that text (Vim's `redo_VIsual`).
pub(super) fn prepare_selection_op(st: &mut EditorState, cmd: &Command) {
    if !is_selection_op(cmd) {
        return;
    }
    let b = st.doc.bytes();
    let cur = st.view.cursor;
    match (st.view.mode.selection(), st.view.anchor) {
        (Some(kind), Some(anchor)) => {
            let (lo, hi) = (anchor.min(cur), anchor.max(cur));
            let lines = crate::pos::raw::line_of(b, hi) - crate::pos::raw::line_of(b, lo) + 1;
            let ts = st.view.indent.tabs();
            let cols = match kind {
                SelectKind::Charwise if lines == 1 => {
                    vcols_at(b, hi, ts).1 + 1 - vcols_at(b, lo, ts).0
                }
                SelectKind::Charwise => vcols_at(b, hi, ts).1,
                SelectKind::Linewise => 0,
                SelectKind::Blockwise => block_geom(b, anchor, cur, ts, false).width(),
            };
            st.view.last_visual_op = Some(VisualOpGeom {
                kind,
                lines,
                cols,
                to_eol: st.view.curswant == range::MAXCOL,
            });
        }
        (None, _) => {
            let Some(g) = st.view.last_visual_op else {
                return;
            };
            let ts = st.view.indent.tabs();
            let ls = line_start(b, cur);
            let start_col = vcols_at(b, cur, ts).0;
            let end_ls =
                crate::pos::raw::nth_line_start(b, crate::pos::raw::line_of(b, cur) + g.lines - 1);
            let end = match g.kind {
                _ if g.to_eol => line_end(b, end_ls),
                SelectKind::Charwise if g.lines == 1 => at_vcol(b, ls, start_col + g.cols - 1, ts),
                SelectKind::Charwise => at_vcol(b, end_ls, g.cols, ts),
                SelectKind::Linewise => end_ls,
                SelectKind::Blockwise => at_vcol(b, end_ls, start_col + g.cols - 1, ts),
            };
            st.view.mode = Mode::Visual { kind: g.kind };
            st.view.anchor = Some(cur);
            st.view.cursor = end;
            if g.to_eol {
                st.view.curswant = Want::col(range::MAXCOL);
            }
        }
        (Some(_), None) => {}
    }
}
