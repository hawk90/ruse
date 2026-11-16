//! Where the cursor lands on screen. Split from `render.rs` (#1386).

use super::*;

/// The cursor's place in its window, relative to the window's top-left (gutter included): the rows of
/// the lines from `top` down to the cursor line (closed folds one row, virtual blocks theirs), then the
/// cursor's row and column in its laid-out line. A cursor inside a closed fold shows on the fold's row.
/// Columns saturate at `u16::MAX` on a line wider than that (#899).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cursor_screen(
    text: PaneText<'_>,
    win: &WinView,
    top: usize,
    folds: &[crate::folds::Fold],
    virt: &[highlight::VirtLine],
    pos: usize,
    mode: ruse_core::Mode,
) -> (u16, u16) {
    let sat = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    let pos = pos.min(text.bytes.len());
    let cline = text.idx.line_of(pos);
    let shown = crate::folds::snap_out(folds, cline);
    let mut row = 0usize;
    let mut l = top;
    while l < shown {
        if !crate::folds::hidden(folds, l) {
            let rows = if crate::folds::closed_starting_at(folds, l).is_some() || !win.wrap {
                1
            } else {
                let limit = (win.skip + 1 + (1 << 12)).saturating_mul(win.width);
                let line = text.line_upto(l, PaneText::bytes_for(limit));
                crate::ui::line_layout::rows_upto(line, &win.layout(), limit)
            };
            row += rows - if l == top { win.skip.min(rows - 1) } else { 0 };
            row += virt
                .iter()
                .filter(|v| v.after_line == l)
                .map(|v| usize::from(v.height))
                .sum::<usize>();
        }
        l += 1;
    }
    let gutter = win.gutter();
    if shown != cline || crate::folds::closed_starting_at(folds, cline).is_some() {
        return (sat(row), sat(gutter));
    }
    let v = match win.cursor {
        Some((at, v)) if at == pos => v,
        _ => {
            crate::ui::window_view::cursor_place(
                win,
                text,
                cline,
                pos - text.start(cline),
                win.tab_end(mode),
            )
            .2
        }
    };
    if win.wrap {
        let (r, c) = (v / win.width, v % win.width);
        let skip = if cline == top { win.skip } else { 0 };
        (sat(row + r.saturating_sub(skip)), sat(gutter + c))
    } else {
        (sat(row), sat(gutter + v.saturating_sub(win.leftcol)))
    }
}

/// The cursor's on-screen `(row, col)` in an unwrapped window with no gutter, the caret on a tab at its
/// first cell: the pre-#615 rule its tests pin (F-006 #4, F-031 slice 3a, #899).
#[cfg(test)]
pub(crate) fn cursor_cell(
    bytes: &[u8],
    pos: usize,
    top: usize,
    virt: &[highlight::VirtLine],
    folds: &[crate::folds::Fold],
    line_idx: &crate::line_index::LineIndex,
    tab: u16,
) -> (u16, u16) {
    let win = WinView::plain(1 << 20, usize::from(tab));
    cursor_screen(
        PaneText {
            bytes,
            idx: line_idx,
        },
        &win,
        top,
        folds,
        virt,
        pos,
        ruse_core::Mode::Insert,
    )
}
