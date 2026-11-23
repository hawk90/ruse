//! Where the cursor lands on screen. Split from `render.rs` (#1386).

use crossterm::cursor;

use super::{focus_rect, highlight, io, queue, PaneText, Rect, TermViews, WinView, Workspace};

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

/// Put the terminal cursor where it belongs after the flush: after a hit-enter prompt, on the command
/// line's caret, on a focused terminal's grid cursor (or hidden), or on the focused view's cursor cell.
#[allow(clippy::too_many_arguments)]
pub(super) fn place_terminal_cursor(
    out: &mut io::Stdout,
    ws: &Workspace,
    rects: &[Rect],
    terminals: &TermViews<'_>,
    at_prompt: Option<&str>,
    cmd_caret: Option<usize>,
    focus_cursor: impl Fn() -> (u16, u16),
    (cols, rows, text_rows): (u16, u16, u16),
) -> io::Result<()> {
    if let Some(bar) = at_prompt {
        // nvim leaves the cursor after the prompt.
        let col = u16::try_from(crate::ui::message::width(bar))
            .unwrap_or(u16::MAX)
            .min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(col, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else if let Some(caret_col) = cmd_caret {
        let ccol = (u16::try_from(caret_col + 1).unwrap_or(u16::MAX)).min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(ccol, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else {
        // The focused view owns the cursor; its DISPLAY column is grapheme-cluster / width based
        // (F-006 #4), offset into the focused window's rectangle (F-007).
        let frect = focus_rect(ws, rects, cols, text_rows);
        // F-011: a focused terminal owns the cursor at its GRID position (and may hide it, e.g. in vim).
        #[cfg(unix)]
        if let Some(&grid) = terminals.get(&ws.focused_buffer()) {
            if grid.cursor_visible() {
                let (gr, gc) = grid.cursor();
                let sr = (frect.y + gr).min(rows.saturating_sub(1));
                let sc = (frect.x + gc).min(cols.saturating_sub(1));
                queue!(out, cursor::MoveTo(sc, sr), cursor::Show)?;
            } else {
                queue!(out, cursor::Hide)?;
            }
            return Ok(());
        }
        let (row, col) = focus_cursor();
        let screen_row = (frect.y + row).min(rows.saturating_sub(1));
        let screen_col = frect.x.saturating_add(col).min(cols.saturating_sub(1)); // #899
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    Ok(())
}
