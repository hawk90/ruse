//! The frame renderer: paint the workspace into a fresh `screen::Screen`, diff it against the previous
//! frame, and emit only the changed cells (F-006). Pure over the screen buffer and `Write`.

use std::io::{self, Write};

use crossterm::{queue, terminal};

use ruse_core::{Command, Mode, SelectKind, SplitDir, Workspace};

use crate::ui::layout::Rect;
use crate::ui::window_view::{PaneText, WinView};
use crate::{graphics, highlight, screen};

mod caret;
mod chrome;
mod columns;
mod flush;
mod image;
mod status;
mod window;

pub(crate) use caret::*;
pub(crate) use chrome::*;
#[cfg(test)]
use columns::*;
pub(crate) use flush::*;
use image::*;
use status::*;
pub(crate) use window::*;

/// The schema default tabstop (`editor.tab_width`), in display columns — the fixed width the renderer
/// tests paint at. Live painting reads each window's own `'tabstop'` from its `WinView`.
#[cfg(test)]
pub(crate) const TAB_WIDTH: u16 = 4;

/// The terminal grids the renderer may paint (F-011), keyed by their placeholder buffer id. Unix-only; on
/// other targets it is an empty map (there are no terminals) so `render`'s signature stays platform-neutral.
#[cfg(unix)]
pub(crate) type TermViews<'a> =
    std::collections::HashMap<ruse_core::DocumentId, &'a crate::term_grid::Grid>;
#[cfg(not(unix))]
pub(crate) type TermViews<'a> = std::collections::HashMap<ruse_core::DocumentId, &'a ()>;

/// Paint a terminal's VT grid into `rect` (F-011 slice 2): one grid cell per screen cell, with its full
/// style. Continuation cells (right half of a wide glyph) are skipped — the wide glyph already covered them.
#[cfg(unix)]
fn paint_grid(cur: &mut screen::Screen, rect: Rect, grid: &crate::term_grid::Grid) {
    let (grows, gcols) = grid.size();
    for r in 0..grows.min(rect.h) {
        for c in 0..gcols.min(rect.w) {
            if let Some((text, style)) = grid.cell(r, c) {
                let g = if text.is_empty() { " " } else { text };
                cur.put_styled(rect.y + r, rect.x + c, g, style);
            }
        }
    }
}

/// The pattern a search command carries (turns on hlsearch for it), else `None` (F-009 #1).
pub(crate) fn search_pattern(cmd: &Command) -> Option<String> {
    match cmd {
        Command::Search { pattern, .. } => Some(pattern.clone()),
        Command::SearchNext(p) | Command::SearchPrev(p) => Some(p.clone()),
        // `gd`/`gD` resolve to this; like `*`, the landing turns on hlsearch for the whole-word pattern.
        Command::GotoFirstMatch(p) => Some(p.clone()),
        _ => None,
    }
}

/// All matches of `pattern` in `bytes` as byte spans, for the incsearch/hlsearch reverse-video
/// highlight (F-009 #1). Uses the default search options (case-sensitive magic — the search default);
/// an unrepresentable/malformed pattern highlights nothing (never an error path).
#[allow(clippy::too_many_arguments)] // the frame render legitimately needs the full view context
pub(crate) fn render(
    out: &mut io::Stdout,
    ws: &Workspace,
    cmd_line: Option<(char, &str, Option<usize>)>,
    cmd_menu: &CmdMenu<'_>,
    status: &str,
    spans: &[highlight::Span],
    virt_lines: &[highlight::VirtLine],
    rects: &[Rect],
    wins: &[WinView],
    prev: &mut screen::Screen,
    sync: bool,
    focus_hl: &[(usize, usize)],
    palette_rows: &[(String, bool)],
    terminals: &TermViews,
    diagnostics: &[crate::lsp::Diag],
    completion: Option<(&[crate::lsp::protocol::CompletionItem], usize)>,
    graphics_on: bool,
    image_dims: &std::collections::HashMap<String, (u32, u32)>,
    folds: &[crate::folds::Fold],
    line_idx: &crate::line_index::LineIndex,
    pane_idx: &crate::line_index::PaneIndexes,
    showmode: bool,
    conceal_on: bool,
    flags: &str,
    status_serial: u64,
    question: bool,
    pager: &mut crate::ui::message::Pager,
) -> io::Result<Vec<(String, graphics::Placement)>> {
    use crossterm::style::Color;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text_rows = rows.saturating_sub(1);
    // Paint the whole frame into a fresh cell grid; the diff against `prev` emits only what changed.
    let mut cur = screen::Screen::new(cols, rows);
    // The focused pane's visible image blocks (path + screen placement), returned for the graphics pass
    // that draws real pixels after the cell flush (F-031 slice 3b-2b).
    let mut images: Vec<(String, graphics::Placement)> = Vec::new();

    let focus = ws.focused();
    let fbytes = focus.doc.bytes();
    let f = Focused {
        ws,
        doc: focus.view.doc(),
        byte_style: face_map(spans, fbytes.len()),
        conceal: concealed(spans, conceal_on),
        // The focused caret's buffer row — from the revision-cached line index (O(log n)) rather than an
        // O(cursor-offset) newline scan every frame. `line_idx` is refreshed on this same focused buffer.
        caret_line: line_idx.line_of(focus.view.cursor().0.min(fbytes.len())),
        signs: diagnostic_signs(diagnostics, line_idx, fbytes.len()),
        focus_hl,
        virt_lines,
        folds,
        diagnostics,
        line_idx,
        pane_idx,
        wins,
        terminals,
        graphics_on,
        image_dims,
    };

    // The focused cursor's cell in its window (the completion menu's anchor and the terminal cursor).
    let focus_cursor = || match wins.get(ws.focus()) {
        Some(win) => cursor_screen(
            PaneText {
                bytes: fbytes,
                idx: line_idx,
            },
            win,
            focus.view.top(),
            folds,
            virt_lines,
            focus.view.cursor().0,
            focus.view.mode(),
        ),
        None => (0, 0),
    };

    paint_windows(&mut cur, rects, &f, &mut images);
    draw_separators(&mut cur, rects, ws.split_dir(), cols, text_rows, wins);

    paint_palette(&mut cur, palette_rows, cols, text_rows);
    if let Some((items, selected)) = completion.filter(|(items, _)| !items.is_empty()) {
        // Anchor at the focused cursor's screen cell (same math the terminal cursor uses below).
        paint_completion(
            &mut cur,
            items,
            selected,
            focus_cursor(),
            focus_rect(ws, rects, cols, text_rows),
            cols,
            text_rows,
        );
    }
    paint_cmd_menus(&mut cur, cmd_menu, cmd_line, cols, text_rows);

    // The command line shows control chars as `^X` (a literal `<CR>` typed with `c_CTRL-V` is `^M`) and
    // puts the terminal caret on its own caret, not at the end (F-026 line editor).
    let cmd_shown = cmd_line.map(|(prefix, text, caret)| {
        let (shown, col) = crate::input::display_with_caret(text, caret.unwrap_or(usize::MAX));
        (format!("{prefix}{shown}"), col)
    });
    pager.prompt = crate::ui::message::Prompt::None;
    let bar = match &cmd_shown {
        Some((shown, _)) => shown.clone(),
        None => status_text(
            &mut cur,
            ws,
            &StatusInput {
                status,
                flags,
                diagnostics,
                showmode,
                question,
                status_serial,
            },
            pager,
            cols,
            text_rows,
        ),
    };
    // Paint the status / command line into the last row (put_str truncates at the right edge).
    cur.put_str(rows.saturating_sub(1), 0, &bar, Color::Reset, false);

    // Diff the finished grid against the previous frame and emit ONLY the changed runs (F-006 #1),
    // wrapped in synchronized output when supported so a big repaint lands atomically (#3).
    flush_diff(out, &cur, prev, sync)?;
    *prev = cur;

    let at_prompt = (pager.prompt != crate::ui::message::Prompt::None).then_some(bar.as_str());
    place_terminal_cursor(
        out,
        ws,
        rects,
        terminals,
        at_prompt,
        cmd_shown.map(|(_, col)| col),
        focus_cursor,
        (cols, rows, text_rows),
    )?;
    out.flush()?;
    Ok(images)
}

/// The focused window's rectangle (the text area when there is none).
fn focus_rect(ws: &Workspace, rects: &[Rect], cols: u16, text_rows: u16) -> Rect {
    rects.get(ws.focus()).copied().unwrap_or(Rect {
        x: 0,
        y: 0,
        w: cols,
        h: text_rows,
    })
}

#[cfg(test)]
mod display_tests;

/// A `LineIndex` over `bytes`, for the renderer tests.
#[cfg(test)]
fn li(bytes: &[u8]) -> crate::line_index::LineIndex {
    let mut idx = crate::line_index::LineIndex::default();
    idx.refresh(ruse_core::Revision(0), bytes);
    idx
}

// ---- F-031 slice 0: the display-coordinate layout seam (P1/P2/P6) + faces ----

#[cfg(test)]
mod render_tests {
    use super::*;

    /// F-009 #1: a search command carries its pattern for hlsearch; other commands do not.
    #[test]
    fn search_pattern_extracts_only_from_search_commands() {
        assert_eq!(
            search_pattern(&ruse_core::Command::SearchNext("foo".into())),
            Some("foo".to_string())
        );
        assert_eq!(search_pattern(&ruse_core::Command::MoveLeft), None);
    }
}

#[cfg(test)]
mod caret_tests;
#[cfg(test)]
mod columns_tests;
#[cfg(test)]
mod flush_tests;
#[cfg(test)]
mod layout_tests;
#[cfg(test)]
mod nvim_parity_tests;
#[cfg(test)]
mod window_tests;
