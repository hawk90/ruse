//! The frame renderer: paint the workspace into a fresh `screen::Screen`, diff it against the previous
//! frame, and emit only the changed cells (F-006). Pure over the screen buffer and `Write`.

use std::io::{self, Write};

use crossterm::style::Print;
use crossterm::{cursor, queue, terminal};

use ruse_core::{Command, Mode, SelectKind, SplitDir, Workspace};

use crate::ui::layout::Rect;
use crate::ui::window_view::{PaneText, WinView};
use crate::{graphics, highlight, screen};

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

/// Advance a display column past one grapheme cluster `g` — the ONE column rule shared by painting
/// ([`paint_pane`]), the caret ([`cursor_cell`] via [`line_display_col`]), and the inverse
/// ([`line_byte_at_col`]), so the three can never drift. `col` is relative to the line's left edge; a tab
/// expands to the next `tab` stop, every other cluster advances by its display width. Conceal and
/// virt_text are handled in [`paint_pane`]'s walk (skip / inject), so this stays the pure column rule.
///
/// The column SATURATES at `u16::MAX` (#899): a line wider than 65535 cells (a minified file, a long log
/// line) is legal buffer text, and plain `+` overflowed there — a panic in debug, a wrapped caret column
/// in release. Every cell past the terminal's width is clipped anyway, so pinning at the ceiling is exact
/// for painting and keeps the caret off-screen-right rather than wrapping it back to the left.
#[inline]
pub(crate) fn advance_col(col: u16, g: &str, tab: u16) -> u16 {
    if g == "\t" {
        let tab = tab.max(1);
        col.saturating_add(tab - (col % tab))
    } else {
        col.saturating_add(screen::cluster_width(g))
    }
}

/// The display column (0-based, from the line's left edge) of the grapheme boundary at byte offset
/// `line_byte` within a single line `line` (no trailing `\n`). `line_byte` past the end clamps to the
/// end. The caret's column ([`cursor_cell`]) and the P1/P2 coordinate tests go through here.
#[cfg(test)]
pub(crate) fn line_display_col(line: &str, line_byte: usize, tab: u16) -> u16 {
    use unicode_segmentation::UnicodeSegmentation;
    let mut col: u16 = 0;
    for (i, g) in line.grapheme_indices(true) {
        if i >= line_byte {
            break;
        }
        col = advance_col(col, g, tab);
    }
    col
}

/// Inverse of [`line_display_col`]: the byte offset of the grapheme boundary at or just past display
/// column `col` within `line`. A `col` beyond the line maps to its end. The P1 round-trip test exercises
/// it now; a later slice wires it into mouse/click resolution (`byte_of(row, col)`), so it is the seam's
/// other half kept beside its forward map rather than being introduced late.
#[allow(dead_code)] // exercised by the P1 test now; the click/byte_of path lands in a later F-031 slice
pub(crate) fn line_byte_at_col(line: &str, col: u16, tab: u16) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    let mut cur: u16 = 0;
    for (i, g) in line.grapheme_indices(true) {
        if cur >= col {
            return i;
        }
        cur = advance_col(cur, g, tab);
    }
    line.len()
}

/// What one window paints (F-006; wrap, gutter and `'list'` #615, #616).
pub(crate) struct PaneInput<'a> {
    pub text: PaneText<'a>,
    pub win: &'a WinView,
    /// The syntax FACE per byte (fg + bold/italic; empty ⇒ default) — F-031 slice 0.
    pub byte_style: &'a [screen::CellStyle],
    pub top: usize,
    /// The Visual selection (reverse video), charwise/linewise or blockwise rows.
    pub sel: Option<(usize, usize)>,
    pub block: Option<&'a [(usize, usize)]>,
    /// The search / `:s///c` matches, also in reverse video.
    pub hl: &'a [(usize, usize)],
    /// The diagnostic ranges (F-014), underlined.
    pub underline: &'a [(usize, usize)],
    /// Byte ranges HIDDEN from layout except on `caret_line` (F-031 reveal-at-point), with the text painted
    /// in their place.
    pub conceal: &'a [(usize, usize, Option<&'a str>)],
    pub caret_line: usize,
    /// This window's cursor line, which `'relativenumber'` counts from.
    pub cursor_line: usize,
    /// Virtual blocks (image placeholders) below lines (F-031 slice 3a).
    pub virt: &'a [highlight::VirtLine],
    pub folds: &'a [crate::folds::Fold],
    /// `(line, sign text, colour)` for the sign column (#616): the diagnostics' `E`/`W`/`I`/`H`.
    pub signs: &'a [(usize, char, crossterm::style::Color)],
    /// The window is in Visual mode: `'cursorline'` leaves the text alone then (nvim).
    pub visual: bool,
    pub graphics_on: bool,
    pub image_dims: &'a std::collections::HashMap<String, (u32, u32)>,
}

/// The colour nvim's `NonText` / `Whitespace` / `LineNr` groups get here.
const DIM: crossterm::style::Color = crossterm::style::Color::DarkGrey;
/// The background of nvim's `CursorLine` / `CursorColumn` (its default `NvimDarkGrey3`) and
/// `ColorColumn` (`NvimDarkGrey4`), as the nearest 256-colour cells (#616).
const CURSORLINE_BG: crossterm::style::Color = crossterm::style::Color::AnsiValue(236);
const COLORCOLUMN_BG: crossterm::style::Color = crossterm::style::Color::AnsiValue(239);

/// The frame overlays of one painted line (#616): its screen rows, which of its own rows they are,
/// and its laid-out rows.
struct LineRows {
    line: usize,
    /// The first screen row and how many show.
    srow: u16,
    shown: usize,
    /// The line's own row the first shown one is (the rows `skip` leaves out above).
    skip: usize,
    /// The line's rows when laid out (1 for a closed fold).
    rows: usize,
    fold: bool,
}

/// Paint the `'cursorline'`, `'cursorcolumn'` and `'colorcolumn'` backgrounds and the `CursorLineNr`
/// gutter of one line, as nvim v0.12.6 places them: the columns are virtual columns, so a wrapped line
/// shows them on the row holding that column and nowhere past its last row.
fn paint_line_overlays(cur: &mut screen::Screen, rect: Rect, p: &PaneInput<'_>, lr: &LineRows) {
    let win = p.win;
    let gutter = u16::try_from(win.gutter()).unwrap_or(u16::MAX).min(rect.w);
    let (tx, x1) = (rect.x + gutter, rect.x + rect.w);
    let tw = usize::from(rect.w - gutter);
    let leftcol = if win.wrap { 0 } else { win.leftcol };
    let cursor_vcol = win.cursor.map_or(0, |(_, v)| v);
    let is_cursor_line = crate::folds::snap_out(p.folds, p.cursor_line) == lr.line;
    // The screen cell of virtual column `v` on this line, if it shows.
    let cell = |v: usize| -> Option<(u16, u16)> {
        let (r, c) = if win.wrap {
            (v / tw.max(1), v % tw.max(1))
        } else if v >= leftcol && v - leftcol < tw {
            (0, v - leftcol)
        } else {
            return None;
        };
        (r >= lr.skip && r < lr.rows && r - lr.skip < lr.shown)
            .then(|| (lr.srow + (r - lr.skip) as u16, tx + c as u16))
    };
    if let (Some(o), true) = (win.cursorline, is_cursor_line) {
        let crow = if lr.fold || !win.wrap {
            0
        } else {
            (cursor_vcol / tw.max(1)).saturating_sub(lr.skip)
        };
        if !p.visual && (o.line || o.screenline) {
            for r in 0..lr.shown {
                if o.line || r == crow {
                    for x in tx..x1 {
                        cur.set_bg(lr.srow + r as u16, x, CURSORLINE_BG);
                    }
                }
            }
        }
        if o.number && win.num_w > 0 {
            let x0 = rect.x + (win.fold_w + win.sign_w) as u16;
            for r in 0..lr.shown {
                // The number's row, or every row of the line with `line`.
                if (o.line || r + lr.skip == 0) && !win.signs_in_number {
                    for x in x0..(x0 + win.num_w as u16).min(x1) {
                        cur.set_bold(lr.srow + r as u16, x);
                    }
                }
            }
        }
    }
    if win.cursorcolumn && !is_cursor_line && !lr.fold {
        if let Some((y, x)) = cell(cursor_vcol) {
            cur.set_bg(y, x, CURSORLINE_BG);
        }
    }
    for &v in &win.colorcolumns {
        if let Some((y, x)) = cell(v) {
            cur.set_bg(y, x, COLORCOLUMN_BG);
        }
    }
}

/// Paint one window into its `rect`: from line `top`, each line laid out by [`line_layout`] (soft wrap,
/// `'showbreak'`, `'breakindent'`, `'linebreak'`, `'list'`, or the columns from `leftcol` without
/// `'wrap'`), behind the sign and number columns. A closed fold's start row shows its summary; a last line
/// that does not fit shows as `'display'` says. One GRAPHEME CLUSTER per cell at its display width.
pub(crate) fn paint_window(
    cur: &mut screen::Screen,
    rect: Rect,
    p: &PaneInput<'_>,
    out_images: &mut Vec<(String, graphics::Placement)>,
) {
    use crate::ui::line_layout::{CellKind, CellText};
    use crate::ui::window_view::LastLine;
    use unicode_segmentation::UnicodeSegmentation;
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let win = p.win;
    let gutter = u16::try_from(win.gutter()).unwrap_or(u16::MAX).min(rect.w);
    let (tx, x1) = (rect.x + gutter, rect.x + rect.w);
    let tw = usize::from(rect.w - gutter);
    let lay_opts = win.layout();
    let count = p.text.line_count();
    let leftcol = if win.wrap { 0 } else { win.leftcol };
    let paint_block = |cur: &mut screen::Screen, disp_row: u16, vb: &highlight::VirtLine| match (
        p.graphics_on,
        &vb.path,
    ) {
        // A graphics terminal shows the real image ONLY when its pixel dimensions were read; a missing
        // or unreadable path paints a text "not found" band instead of a blank placeholder grid.
        (true, Some(path)) => match p.image_dims.get(path).copied() {
            Some(dims) => paint_image_cells(cur, rect, disp_row, vb.height, path, Some(dims)),
            None => paint_missing_image(cur, rect, disp_row, vb.height, path),
        },
        _ => paint_virt_block(cur, rect, disp_row, vb),
    };
    let mut buf = [0u8; 4];
    let lastline: &str = win.fill.lastline.encode_utf8(&mut buf);
    let lastline = lastline.to_string();
    let mut row: u16 = 0;
    let mut line = p.top;
    let mut first = true;
    let mut cut = false;
    while row < rect.h && line < count {
        if crate::folds::hidden(p.folds, line) {
            line += 1;
            continue;
        }
        let srow = rect.y + row;
        let ls = p.text.start(line);
        // Only what the window can show is laid out (a huge line costs its visible rows).
        let skip = if first { win.skip } else { 0 };
        let max_vcol = if win.wrap {
            (skip + usize::from(rect.h - row) + 1).saturating_mul(tw.max(1))
        } else {
            leftcol + tw + 1
        };
        let text = p.text.line_upto(line, PaneText::bytes_for(max_vcol));
        if let Some(fold) = crate::folds::closed_starting_at(p.folds, line) {
            // A closed fold's start row shows its one-line summary in place of the text.
            paint_gutter(cur, rect, srow, line, 0, p);
            let mut c = tx;
            for gph in crate::folds::summary(fold, &String::from_utf8_lossy(text)).graphemes(true) {
                if c >= x1 {
                    break;
                }
                c = cur.put(srow, c, gph, DIM, false);
            }
            // 'fillchars' `fold` fills the rest of the row.
            let mut fb = [0u8; 4];
            let fill: &str = win.fill.fold.encode_utf8(&mut fb);
            while c < x1 {
                c = cur.put(srow, c, fill, DIM, false);
            }
            paint_line_overlays(
                cur,
                rect,
                p,
                &LineRows {
                    line,
                    srow,
                    shown: 1,
                    skip: 0,
                    rows: 1,
                    fold: true,
                },
            );
            row += 1;
        } else {
            // F-031 conceal: hidden on every line but the caret's, which is revealed so its markup stays
            // editable.
            let conceal: Vec<(usize, usize, Option<&str>)> = if line == p.caret_line {
                Vec::new()
            } else {
                p.conceal
                    .iter()
                    .filter(|&&(s, e, _)| s >= ls && s < ls + text.len().max(1) && e > s)
                    .map(|&(s, e, v)| (s - ls, (e - ls).min(text.len()), v))
                    .collect()
            };
            // The rows above the window (`skip`) or the columns left of it are counted, not laid out.
            let min_vcol = if win.wrap {
                skip.saturating_mul(tw.max(1))
            } else {
                leftcol
            };
            let lay = crate::ui::line_layout::layout_from(
                text,
                &lay_opts,
                &conceal,
                min_vcol,
                max_vcol,
                usize::MAX,
            );
            let rows = lay.rows.saturating_sub(skip).max(1);
            let avail = usize::from(rect.h - row);
            // The first line always paints; a later one that does not fit shows as 'display' says.
            let partial = rows > avail && !first;
            if partial && win.lastline == LastLine::AtRows {
                for r in 0..avail {
                    cur.put(srow + r as u16, tx, &lastline, DIM, false);
                }
                cut = true;
                break;
            }
            let shown = rows.min(avail);
            for r in 0..shown {
                paint_gutter(cur, rect, srow + r as u16, line, r + skip, p);
            }
            let truncate_row = (partial && win.lastline == LastLine::Truncate).then_some(shown - 1);
            for c in &lay.cells {
                let (r, col) = if win.wrap {
                    lay.place(c.vcol)
                } else {
                    (0, c.vcol)
                };
                if r < skip || r - skip >= shown || Some(r - skip) == truncate_row {
                    continue;
                }
                let srow = srow + (r - skip) as u16;
                // Without 'wrap' the window shows the columns from `leftcol`: a wide glyph cut at the left
                // edge shows `<`, one cut at the right edge `>`.
                let (col, text, width, kind) = if win.wrap {
                    (col, c.text.clone(), c.width, c.kind)
                } else if col + c.width <= leftcol || col >= leftcol + tw {
                    continue;
                } else if col < leftcol {
                    (0, CellText::Char('<'), 1, CellKind::NonText)
                } else if col + c.width > leftcol + tw {
                    (col - leftcol, CellText::Char('>'), 1, CellKind::NonText)
                } else {
                    (col - leftcol, c.text.clone(), c.width, c.kind)
                };
                if col >= tw {
                    continue;
                }
                let x = tx + col as u16;
                let byte = c.byte.map(|b| ls + b);
                let style = cell_style(p, byte, kind);
                let mut buf = [0u8; 4];
                let g: &str = match &text {
                    CellText::Str(s) => s,
                    CellText::Char(ch) => ch.encode_utf8(&mut buf),
                    CellText::Blank => " ",
                };
                if col + width > tw {
                    // A wide glyph with no room for its second half (a window narrower than it).
                    cur.put_styled(srow, x, " ", &style);
                } else {
                    cur.put_styled(srow, x, g, &style);
                }
            }
            if !win.wrap {
                if let Some(lc) = win.list.as_ref() {
                    // 'listchars' `extends` / `precedes`: the line goes on past the right / left edge.
                    let past_right = lay.cells.iter().any(|c| c.vcol >= leftcol + tw);
                    if let (Some(ch), true) = (lc.extends, past_right) {
                        let mut buf = [0u8; 4];
                        cur.put(
                            srow,
                            tx + (tw - 1) as u16,
                            ch.encode_utf8(&mut buf),
                            DIM,
                            false,
                        );
                    }
                    if let (Some(ch), true) = (lc.precedes, leftcol > 0 && lay.total > leftcol) {
                        let mut buf = [0u8; 4];
                        cur.put(srow, tx, ch.encode_utf8(&mut buf), DIM, false);
                    }
                }
            }
            paint_line_overlays(
                cur,
                rect,
                p,
                &LineRows {
                    line,
                    srow,
                    shown,
                    skip,
                    rows: lay.rows,
                    fold: false,
                },
            );
            if partial {
                let last = srow + (shown - 1) as u16;
                match win.lastline {
                    LastLine::Lastline => {
                        for k in 0..3.min(tw) {
                            cur.put(last, x1 - 1 - k as u16, &lastline, DIM, false);
                        }
                    }
                    _ => {
                        for k in 0..3.min(tw) {
                            cur.put(last, tx + k as u16, &lastline, DIM, false);
                        }
                    }
                }
                cut = true;
                break;
            }
            row += shown as u16;
        }
        first = false;
        // A virtual block (an image placeholder) below the line pushes the lines after it down.
        if let Some(vb) = p.virt.iter().find(|v| v.after_line == line) {
            if row < rect.h {
                paint_block(cur, row, vb);
                collect_image(
                    out_images,
                    rect,
                    row,
                    vb,
                    vb.path
                        .as_deref()
                        .and_then(|q| p.image_dims.get(q))
                        .copied(),
                );
            }
            row = row.saturating_add(vb.height);
        }
        line += 1;
    }
    // The rows after the buffer's last line show 'fillchars' `eob` in their first column.
    if !cut && line >= count {
        let mut eb = [0u8; 4];
        let eob: &str = win.fill.eob.encode_utf8(&mut eb);
        for r in row..rect.h {
            cur.put(rect.y + r, rect.x, eob, DIM, false);
        }
    }
    // A 'smoothscroll' window scrolled into its top line shows `<<<` over its first cells.
    if win.smoothscroll && win.skip > 0 && win.wrap && win.showbreak.is_empty() {
        for k in 0..3.min(rect.w) {
            cur.put(rect.y, rect.x + k, "<", DIM, false);
        }
    }
}

/// The style of a painted cell: the byte's face plus the frame overlays (selection and match reverse
/// video, diagnostic underline); decoration cells are dim.
fn cell_style(
    p: &PaneInput<'_>,
    byte: Option<usize>,
    kind: crate::ui::line_layout::CellKind,
) -> screen::CellStyle {
    use crate::ui::line_layout::CellKind;
    let Some(i) = byte else {
        return screen::CellStyle {
            fg: DIM,
            ..screen::CellStyle::default()
        };
    };
    let inside = |&(s, e): &(usize, usize)| i >= s && i < e;
    let selected = p.sel.as_ref().is_some_and(inside)
        || p.block.is_some_and(|rows| rows.iter().any(inside))
        || p.hl.iter().any(inside);
    let base = p.byte_style.get(i).copied().unwrap_or_default();
    let mut style = screen::CellStyle {
        reverse: selected,
        underline: base.underline || p.underline.iter().any(inside),
        ..base
    };
    if kind != CellKind::Text {
        style.fg = DIM;
    }
    style
}

/// Paint line `line`'s sign column and number column on screen row `srow` (#616): the sign (or blank),
/// then the number right-aligned with a blank after it — the cursor line's own number left-aligned under
/// `'number'` + `'relativenumber'`, distances that count a closed fold as one line.
fn paint_gutter(
    cur: &mut screen::Screen,
    rect: Rect,
    srow: u16,
    line: usize,
    line_row: usize,
    p: &PaneInput<'_>,
) {
    let win = p.win;
    let mut x = rect.x;
    let end = rect.x + rect.w;
    if win.fold_w > 0 {
        // 'foldcolumn' (#616): on every row of the line, as nvim draws it.
        for ch in crate::ui::window_view::fold_column(p.folds, line, win.fold_w, &win.fill) {
            if x >= end {
                return;
            }
            let mut buf = [0u8; 4];
            x = cur.put(srow, x, ch.encode_utf8(&mut buf), DIM, false);
        }
    }
    if line_row > 0 {
        // A wrapped line's later rows show no sign or number.
        return;
    }
    let sign = p.signs.iter().find(|s| s.0 == line);
    if win.sign_w > 0 {
        if let Some(&(_, ch, color)) = sign {
            let mut buf = [0u8; 4];
            cur.put(srow, x, ch.encode_utf8(&mut buf), color, false);
        }
        x += win.sign_w as u16;
    }
    if win.num_w == 0 || x >= end {
        return;
    }
    let w = win.num_w - 1;
    if let (true, Some(&(_, ch, color))) = (win.signs_in_number, sign) {
        let mut buf = [0u8; 4];
        cur.put(
            srow,
            x + (w - 2) as u16,
            ch.encode_utf8(&mut buf),
            color,
            false,
        );
        return;
    }
    let rel = if line >= p.cursor_line {
        line - p.cursor_line - crate::folds::hidden_between(p.folds, p.cursor_line, line)
    } else {
        p.cursor_line - line - crate::folds::hidden_between(p.folds, line, p.cursor_line)
    };
    let label = match (win.number, win.relativenumber) {
        (true, true) if rel == 0 => format!("{:<w$} ", line + 1),
        (_, true) => format!("{rel:>w$} "),
        _ => format!("{:>w$} ", line + 1),
    };
    let mut c = x;
    for ch in label.chars() {
        if c >= end {
            break;
        }
        let mut buf = [0u8; 4];
        c = cur.put(srow, c, ch.encode_utf8(&mut buf), DIM, false);
    }
}

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

/// Paint one buffer view into its `rect` without wrapping or a gutter — how every window painted before
/// #615 — for the tests that pin the conceal, fold and virtual-block rules.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_pane(
    cur: &mut screen::Screen,
    rect: Rect,
    bytes: &[u8],
    byte_style: &[screen::CellStyle],
    top: usize,
    sel: Option<(usize, usize)>,
    block: Option<&[(usize, usize)]>,
    hl: &[(usize, usize)],
    underline: &[(usize, usize)],
    conceal: &[(usize, usize, Option<&str>)],
    caret_line: usize,
    virt: &[highlight::VirtLine],
    out_images: &mut Vec<(String, graphics::Placement)>,
    graphics_on: bool,
    image_dims: &std::collections::HashMap<String, (u32, u32)>,
    folds: &[crate::folds::Fold],
    tab: u16,
) {
    let mut idx = crate::line_index::LineIndex::default();
    idx.refresh(ruse_core::Revision(0), bytes);
    let win = WinView::plain(usize::from(rect.w), usize::from(tab));
    let p = PaneInput {
        text: PaneText { bytes, idx: &idx },
        win: &win,
        byte_style,
        top,
        sel,
        block,
        hl,
        underline,
        conceal,
        caret_line,
        cursor_line: caret_line,
        virt,
        folds,
        signs: &[],
        visual: false,
        graphics_on,
        image_dims,
    };
    paint_window(cur, rect, &p, out_images);
}

/// The image's `(left_col, cols)` within the pane: sized to the image's natural aspect (from its pixel
/// dimensions) and CENTRED horizontally, rather than stretched to full width. Unknown dimensions fall back
/// to full width. Both painting and `collect_image` go through this so the placeholder cells and the virtual
/// placement agree. F-031 slice 3b-2c.
fn image_layout(rect: Rect, rows: u16, dims: Option<(u32, u32)>) -> (u16, u16) {
    let cols = match dims {
        Some((w, h)) => graphics::fit_cols(w, h, rows, 0.5, rect.w.min(graphics::MAX_PLACEHOLDER)),
        None => rect.w.min(graphics::MAX_PLACEHOLDER),
    };
    let left = rect.x + rect.w.saturating_sub(cols) / 2;
    (left, cols)
}

/// Record an image block's on-screen placement (F-031 slice 3b-2c) for the render loop's graphics pass —
/// only for a block that carries a `path`. Sized to the image's aspect and centred (see [`image_layout`]).
fn collect_image(
    out: &mut Vec<(String, graphics::Placement)>,
    rect: Rect,
    disp_row: u16,
    vb: &highlight::VirtLine,
    dims: Option<(u32, u32)>,
) {
    // No dims ⇒ the image could not be loaded; it renders as a text "not found" band, so record NO
    // graphics placement (a transmit would fail anyway) — keeps the graphics pass in sync with the cells.
    let Some(dims) = dims else { return };
    if let Some(path) = &vb.path {
        let (left, cols) = image_layout(rect, vb.height, Some(dims));
        out.push((
            path.clone(),
            graphics::Placement {
                row: rect.y + disp_row,
                col: left,
                cols,
                rows: vb.height,
            },
        ));
    }
}

/// Paint an image's Unicode PLACEHOLDER cells (F-031 slice 3b-2c): `rows × cols` cells, each holding the
/// placeholder char + row/col diacritics with `fg` = the image id encoded as RGB. The terminal composites
/// the image slice into each cell, so positioning rides the normal cell grid (correct inside a tmux pane).
/// Sized to the image's aspect and centred (see [`image_layout`]).
fn paint_image_cells(
    cur: &mut screen::Screen,
    rect: Rect,
    disp_row: u16,
    rows: u16,
    path: &str,
    dims: Option<(u32, u32)>,
) {
    let id = graphics::image_id(path);
    let (r, g, b) = graphics::id_rgb(id);
    let style = screen::CellStyle {
        fg: crossterm::style::Color::Rgb { r, g, b },
        ..screen::CellStyle::default()
    };
    let (left, cols) = image_layout(rect, rows, dims);
    for ir in 0..rows {
        if disp_row + ir >= rect.h {
            break;
        }
        let srow = rect.y + disp_row + ir;
        for ic in 0..cols {
            cur.put_styled(srow, left + ic, &graphics::placeholder_cell(ir, ic), &style);
        }
    }
}

/// Paint an image PLACEHOLDER block (F-031 slice 3a): `vb.height` rows from display row `disp_row` within
/// `rect` — a `🖼 label` line, then rule lines — clipped to the pane. This is the degrade placeholder rung;
/// a graphics-capable terminal replaces it with real pixels in slice 3b (INV-CAP-DEGRADE).
fn paint_virt_block(cur: &mut screen::Screen, rect: Rect, disp_row: u16, vb: &highlight::VirtLine) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    let (x0, x1) = (rect.x, rect.x + rect.w);
    for r in 0..vb.height {
        let disp = disp_row + r;
        if disp >= rect.h {
            break;
        }
        let srow = rect.y + disp;
        if r == 0 {
            let label = format!("\u{1f5bc} {}", vb.label); // 🖼 alt
            let mut col = x0;
            for gph in label.graphemes(true) {
                if col >= x1 {
                    break;
                }
                col = cur.put(srow, col, gph, Color::DarkGrey, false);
            }
        } else {
            for col in x0..x1 {
                cur.put(srow, col, "\u{2500}", Color::DarkGrey, false); // ─
            }
        }
    }
}

/// Paint the "image could not be loaded" fallback band (F-031): `[image: <path> (not found)]` in red on
/// the first row, blank rows below, filling `vb.height` rows from `disp_row`. Shown on a graphics terminal
/// when the path is missing/unreadable, so a broken link is legible rather than a silent empty band.
fn paint_missing_image(
    cur: &mut screen::Screen,
    rect: Rect,
    disp_row: u16,
    height: u16,
    path: &str,
) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    let (x0, x1) = (rect.x, rect.x + rect.w);
    for r in 0..height {
        let disp = disp_row + r;
        if disp >= rect.h {
            break;
        }
        let srow = rect.y + disp;
        if r == 0 {
            let label = format!("[image: {path} (not found)]");
            let mut col = x0;
            for gph in label.graphemes(true) {
                if col >= x1 {
                    break;
                }
                col = cur.put(srow, col, gph, Color::Red, false);
            }
        }
        // rows below the label stay blank (already cleared by the frame)
    }
}

/// Draw the one-cell dividers between adjacent panes (F-007): a `─` band for a horizontal split, a
/// `│` column for a vertical split. Only interior gaps are drawn (never past the text area).
pub(crate) fn draw_separators(
    cur: &mut screen::Screen,
    rects: &[Rect],
    split: SplitDir,
    cols: u16,
    text_rows: u16,
    wins: &[WinView],
) {
    use crossterm::style::Color;
    for (i, pair) in rects.windows(2).enumerate() {
        // The window before the divider owns it: its 'fillchars' `vert` / `horiz` (#616).
        let fill = wins.get(i).map(|w| &w.fill);
        let mut buf = [0u8; 4];
        match split {
            SplitDir::Horizontal => {
                let ch: &str = fill.map_or('\u{2500}', |f| f.horiz).encode_utf8(&mut buf);
                let y = pair[0].y + pair[0].h;
                if y < text_rows {
                    for x in 0..cols {
                        cur.put(y, x, ch, Color::Reset, false);
                    }
                }
            }
            SplitDir::Vertical => {
                let ch: &str = fill.map_or('\u{2502}', |f| f.vert).encode_utf8(&mut buf);
                let x = pair[0].x + pair[0].w;
                if x < cols {
                    for y in 0..text_rows {
                        cur.put(y, x, ch, Color::Reset, false);
                    }
                }
            }
        }
    }
}

/// What the open command line shows above itself (#613): the wildmenu's popup menu and the matches
/// `c_CTRL-D` listed.
#[derive(Default)]
pub(crate) struct CmdMenu<'a> {
    /// The popup menu: its entries, the selected one, and the char column of the line (after the prompt
    /// glyph) its text aligns with.
    pub pum: Option<(Vec<&'a str>, Option<usize>, usize)>,
    /// The `c_CTRL-D` listing.
    pub listing: Option<&'a [crate::input::complete::Item]>,
}

/// nvim's `'pumwidth'` default: the popup menu's narrowest text column.
const PUM_MIN_WIDTH: usize = 15;

/// Paint the command line's wildmenu as nvim v0.12.6 does with `wildoptions=pum` (#613): a menu just
/// above the command line, as tall as the screen allows, its text column aligned with the completed
/// word's text (one blank cell before it), at least [`PUM_MIN_WIDTH`] wide, the selected entry in
/// reverse video, and a scrollbar on its right when the entries do not fit.
fn paint_cmd_pum(
    cur: &mut screen::Screen,
    entries: &[&str],
    selected: Option<usize>,
    text_col: u16,
    cols: u16,
    text_rows: u16,
) {
    use crossterm::style::Color;
    use unicode_width::UnicodeWidthStr;
    let height = entries.len().min(usize::from(text_rows));
    if height == 0 || cols < 2 {
        return;
    }
    let scroll = entries.len() > height;
    let w = entries
        .iter()
        .map(|e| e.width() + 1)
        .max()
        .unwrap_or(0)
        .max(PUM_MIN_WIDTH);
    let box_w = (w + 1 + usize::from(scroll)).min(usize::from(cols));
    let left = usize::from(text_col.saturating_sub(1)).min(usize::from(cols) - box_w);
    // Keep the selected entry in view: the window ends at it once it passes the bottom.
    let top_item = selected.map_or(0, |s| (s + 1).saturating_sub(height));
    let first_row = text_rows - height as u16;
    let menu = screen::CellStyle {
        bg: Color::DarkGrey,
        ..screen::CellStyle::default()
    };
    let thumb = (
        top_item * height / entries.len(),
        (height * height).div_ceil(entries.len()),
    );
    for r in 0..height {
        let i = top_item + r;
        let style = screen::CellStyle {
            reverse: Some(i) == selected,
            ..menu
        };
        let row = first_row + r as u16;
        let mut text = String::from(" ");
        text.push_str(entries[i]);
        let pad = (w + 1).saturating_sub(text.width());
        text.push_str(&" ".repeat(pad));
        let mut col = left as u16;
        for g in unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true) {
            if usize::from(col) >= left + box_w - usize::from(scroll) {
                break;
            }
            col = cur.put_styled(row, col, g, &style).max(col + 1);
        }
        if scroll {
            let on = r >= thumb.0 && r < thumb.0 + thumb.1;
            let bar = screen::CellStyle {
                bg: if on { Color::Grey } else { Color::DarkGrey },
                ..screen::CellStyle::default()
            };
            cur.put_styled(row, (left + box_w - 1) as u16, " ", &bar);
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

    // Flatten the focused buffer's highlight spans into a per-byte FACE (spans index into it). Panes
    // showing the SAME buffer reuse it; a pane on a different buffer (post-MVP) renders unstyled. Longest
    // spans come first (highlight orders them), so this last-wins flatten lets the shortest/most-specific
    // capture win — the slice-0 stand-in for the decoration model's priority resolution (F-031).
    let focus = ws.focused();
    let focus_doc = focus.view.doc();
    let fbytes = focus.doc.bytes();
    // Size the per-byte face array to the LAST styled byte, not the whole buffer: the highlighter clips
    // `spans` to the viewport, so this is ~O(viewport) instead of an O(buffer) alloc+zero every frame.
    // paint_pane reads any unstyled index (`byte_style.get(i)`) as the default face, so an array that stops
    // at the last span is byte-for-byte identical to a whole-buffer one — just without the below-viewport tail.
    let style_len = spans
        .iter()
        .map(|s| s.end)
        .max()
        .unwrap_or(0)
        .min(fbytes.len());
    let mut byte_style = vec![screen::CellStyle::default(); style_len];
    for s in spans {
        for slot in byte_style
            .iter_mut()
            .take(s.end.min(style_len))
            .skip(s.start)
        {
            *slot = s.style;
        }
    }
    // F-031 conceal: the byte ranges the providers marked hidden (Markdown heading `# ` prefixes today).
    // Off with `render.conceal = false` in the config (`conceal_on`), or `RUSE_CONCEAL=off` for one run.
    // The revealed line is the focused caret's buffer line (reveal-at-point).
    let conceal_on = conceal_on && std::env::var("RUSE_CONCEAL").as_deref() != Ok("off");
    let conceal: Vec<(usize, usize, Option<&'static str>)> = if conceal_on {
        spans
            .iter()
            .filter(|s| s.conceal)
            .map(|s| (s.start, s.end, s.virt))
            .collect()
    } else {
        Vec::new()
    };
    // The focused caret's buffer row — from the revision-cached line index (O(log n)) rather than an
    // O(cursor-offset) newline scan every frame. `line_idx` is refreshed on this same focused buffer.
    let caret_line = line_idx.line_of(focus.view.cursor().0.min(fbytes.len()));
    // #616: the focused buffer's diagnostics as signs, the most severe one on each line.
    let signs: Vec<(usize, char, Color)> = {
        use crate::lsp::model::Severity;
        let mut by_line: std::collections::BTreeMap<usize, Severity> = Default::default();
        let rank = |s: &Severity| match s {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
            Severity::Hint => 3,
        };
        for d in diagnostics {
            let l = line_idx.line_of(d.start.min(fbytes.len()));
            let e = by_line.entry(l).or_insert(d.severity);
            if rank(&d.severity) < rank(e) {
                *e = d.severity;
            }
        }
        by_line
            .into_iter()
            .map(|(l, s)| match s {
                Severity::Error => (l, 'E', Color::Red),
                Severity::Warning => (l, 'W', Color::Yellow),
                Severity::Info => (l, 'I', Color::Blue),
                Severity::Hint => (l, 'H', Color::Cyan),
            })
            .collect()
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

    // Paint every window into its sub-rectangle; the focused view owns the terminal cursor below.
    for (i, &rect) in rects.iter().enumerate().take(ws.window_count()) {
        let p = ws.pane(i);
        // F-011 slice 2: a terminal window paints its VT grid (colors, cursor, styles), not the empty
        // placeholder document.
        #[cfg(unix)]
        if let Some(&grid) = terminals.get(&p.view.doc()) {
            paint_grid(&mut cur, rect, grid);
            continue;
        }
        let pbytes = p.doc.bytes();
        let style: &[screen::CellStyle] = if p.view.doc() == focus_doc {
            &byte_style
        } else {
            &[]
        };
        let sel = p.view.selection_span(pbytes).map(|(s, e)| (s.0, e.0));
        let block: Option<Vec<(usize, usize)>> = p
            .view
            .block_spans(pbytes)
            .map(|rows| rows.into_iter().map(|(s, e)| (s.0, e.0)).collect());
        // The focused pane also paints the `:s///c` confirm match / incsearch+hlsearch matches (F-009
        // #1) in reverse video, on top of any live Visual selection.
        let hl: &[(usize, usize)] = if i == ws.focus() { focus_hl } else { &[] };
        // Conceal applies to the focused pane's view of the focused doc (F-031); other panes render markup.
        let pane_conceal: &[(usize, usize, Option<&'static str>)] =
            if i == ws.focus() && p.view.doc() == focus_doc {
                &conceal
            } else {
                &[]
            };
        // Virtual blocks (image placeholders) likewise paint into the focused pane's view (F-031 slice 3a).
        let pane_virt: &[highlight::VirtLine] = if i == ws.focus() && p.view.doc() == focus_doc {
            virt_lines
        } else {
            &[]
        };
        // Folds are per-view; slice 1 collapses only the FOCUSED pane's folds (no dedicated F-*; F-007 carve-out).
        let pane_folds: &[crate::folds::Fold] = if i == ws.focus() { folds } else { &[] };
        // F-014: underline the focused buffer's diagnostic ranges.
        let underline: Vec<(usize, usize)> = if i == ws.focus() {
            diagnostics.iter().map(|d| (d.start, d.end)).collect()
        } else {
            Vec::new()
        };
        // A pane on another buffer than the focused one reads that buffer's cached line index (#1182),
        // which the frame's viewport pass refreshed.
        let mut own_idx = crate::line_index::LineIndex::default();
        let idx = if p.view.doc() == focus_doc {
            line_idx
        } else if let Some(ix) = pane_idx.get(p.view.doc(), p.doc.revision()) {
            ix
        } else {
            own_idx.refresh_for(p.view.doc(), p.doc.revision(), pbytes);
            &own_idx
        };
        let fallback;
        let win = match wins.get(i) {
            Some(w) => w,
            None => {
                fallback = WinView::resolve(
                    p.view,
                    &crate::ui::window_view::DisplayGlobals::default(),
                    usize::from(rect.w),
                    usize::from(rect.h),
                    1,
                    false,
                );
                &fallback
            }
        };
        let input = PaneInput {
            text: PaneText { bytes: pbytes, idx },
            win,
            byte_style: style,
            top: p.view.top(),
            sel,
            block: block.as_deref(),
            hl,
            underline: &underline,
            conceal: pane_conceal,
            caret_line,
            cursor_line: idx.line_of(p.view.cursor().0.min(pbytes.len())),
            virt: pane_virt,
            folds: pane_folds,
            signs: if i == ws.focus() { &signs } else { &[] },
            visual: matches!(p.view.mode(), Mode::Visual { .. } | Mode::Select { .. }),
            graphics_on,
            image_dims,
        };
        paint_window(&mut cur, rect, &input, &mut images);
    }
    draw_separators(&mut cur, rects, ws.split_dir(), cols, text_rows, wins);

    // The command palette (F-004): paint its match rows just above the status line, newest selection
    // in reverse video. Rows beyond the available height are dropped (the list scrolls with selection
    // in a fuller build; MVP shows the top window).
    if !palette_rows.is_empty() {
        let shown = palette_rows.len().min(text_rows.saturating_sub(1) as usize);
        let top = text_rows.saturating_sub(shown as u16); // first palette row
        for (i, (label, selected)) in palette_rows.iter().take(shown).enumerate() {
            let row = top + i as u16;
            // Clear the row then paint the label (reverse for the selected match).
            for x in 0..cols {
                cur.put(row, x, " ", Color::Reset, *selected);
            }
            cur.put_str(row, 1, label, Color::Reset, *selected);
        }
    }

    // F-014 #5: the completion popup menu (pum) — a bordered box anchored at the cursor, drawn over the
    // buffer into the cell grid (the diff emits it; clearing it next frame repaints the covered cells).
    if let Some((items, selected)) = completion {
        if !items.is_empty() {
            // One display string per item: "label  detail" (detail is the dimmed type/signature).
            let rows: Vec<String> = items
                .iter()
                .map(|it| match &it.detail {
                    Some(d) => format!("{}  {}", it.label, d),
                    None => it.label.clone(),
                })
                .collect();
            let iw = rows
                .iter()
                .map(|r| r.chars().count())
                .max()
                .unwrap_or(10)
                .clamp(10, 40);
            let height = rows.len().min(10) as u16;
            // Anchor at the focused cursor's screen cell (same math the terminal cursor uses below).
            let (crow, ccol) = focus_cursor();
            let frect = rects.get(ws.focus()).copied().unwrap_or(Rect {
                x: 0,
                y: 0,
                w: cols,
                h: text_rows,
            });
            let arow = frect.y + crow;
            let acol = frect.x.saturating_add(ccol); // ccol saturates on a >u16 line (#899)
            let box_w = iw as u16 + 2; // + side borders
            let left = acol.min(cols.saturating_sub(box_w));
            // Prefer below the cursor; flip above if the box (content + 2 borders) would overflow the text.
            let below_top = arow + 1;
            let (top, first) = if below_top + height + 2 <= text_rows {
                (below_top, below_top + 1)
            } else {
                let t = arow.saturating_sub(height + 2);
                (t, t + 1)
            };
            let border = |n: usize| "─".repeat(n);
            cur.put_str(top, left, &format!("┌{}┐", border(iw)), Color::Reset, false);
            for (i, text) in rows.iter().take(height as usize).enumerate() {
                let r = first + i as u16;
                let mut content: String = text.chars().take(iw).collect();
                let pad = iw.saturating_sub(content.chars().count());
                content.push_str(&" ".repeat(pad));
                cur.put(r, left, "│", Color::Reset, false);
                cur.put_str(r, left + 1, &content, Color::Reset, i == selected);
                cur.put(r, left + 1 + iw as u16, "│", Color::Reset, false);
            }
            let bottom = first + height;
            cur.put_str(
                bottom,
                left,
                &format!("└{}┘", border(iw)),
                Color::Reset,
                false,
            );
        }
    }

    // `c_CTRL-D` lists the matches in columns just above the command line (#613).
    if let Some(items) = cmd_menu.listing {
        let lines = crate::input::complete::list_columns(items, usize::from(cols));
        let shown = lines.len().min(usize::from(text_rows));
        let top = text_rows - shown as u16;
        for (i, line) in lines.iter().take(shown).enumerate() {
            let row = top + i as u16;
            for x in 0..cols {
                cur.put(row, x, " ", Color::Reset, false);
            }
            cur.put_str(row, 0, line, Color::Reset, false);
        }
    }
    if let Some((entries, selected, col)) = &cmd_menu.pum {
        // The entries align with the completed word's text, one cell right of the prompt glyph.
        let text_col = cmd_line.map_or(0, |(_, text, _)| {
            let head: String = text.chars().take(*col).collect();
            crate::input::display_with_caret(&head, usize::MAX).1 + 1
        });
        paint_cmd_pum(
            &mut cur,
            entries,
            *selected,
            u16::try_from(text_col).unwrap_or(u16::MAX),
            cols,
            text_rows,
        );
    }

    // The command line shows control chars as `^X` (a literal `<CR>` typed with `c_CTRL-V` is `^M`) and
    // puts the terminal caret on its own caret, not at the end (F-026 line editor).
    let cmd_shown = cmd_line.map(|(prefix, text, caret)| {
        let (shown, col) = crate::input::display_with_caret(text, caret.unwrap_or(usize::MAX));
        (format!("{prefix}{shown}"), col)
    });
    pager.prompt = crate::ui::message::Prompt::None;
    let bar = match &cmd_shown {
        Some((shown, _)) => shown.clone(),
        None => {
            let mode = match focus.view.mode() {
                Mode::Normal => "NORMAL",
                Mode::Insert => "INSERT",
                Mode::Replace => "REPLACE",
                Mode::VirtualReplace => "V-REPLACE",
                Mode::Visual {
                    kind: SelectKind::Charwise,
                } => "VISUAL",
                Mode::Visual {
                    kind: SelectKind::Linewise,
                } => "V-LINE",
                Mode::Visual {
                    kind: SelectKind::Blockwise,
                } => "V-BLOCK",
                Mode::Select {
                    kind: SelectKind::Charwise,
                } => "SELECT",
                Mode::Select {
                    kind: SelectKind::Linewise,
                } => "S-LINE",
                Mode::Select {
                    kind: SelectKind::Blockwise,
                } => "S-BLOCK",
                Mode::Terminal => "TERMINAL",
                Mode::TerminalNormal => "T-NORMAL",
            };
            // Show the FOCUSED buffer's name (multi-buffer, F-007), not the session path — so switching to
            // a scratch buffer reads `[No Name]`, not the file that `path` still points at.
            let name = ws
                .buffer_name(ws.focused_buffer())
                .map_or_else(|| "[No Name]".to_string(), str::to_string);
            // nvim's `%m%r` after the name: `f.txt [+][RO]`; `flags` holds the ones after `[+]`.
            let modified = if focus.doc.is_modified() { "[+]" } else { "" };
            let dirty = if modified.is_empty() && flags.is_empty() {
                String::new()
            } else {
                format!(" {modified}{flags}")
            };
            // Show the window position only once split, so the single-window status line is unchanged.
            let win = if ws.window_count() > 1 {
                format!("  [win {}/{}]", ws.focus() + 1, ws.window_count())
            } else {
                String::new()
            };
            // F-014: a diagnostic summary for the focused buffer, when any (errors/warnings).
            let diag = match crate::lsp::counts(diagnostics) {
                (0, 0) => String::new(),
                (e, w) => format!("  [E:{e} W:{w}]"),
            };
            // `'noshowmode'` leaves the mode out (#1151).
            let prefix = if showmode {
                format!("{mode}  {name}{dirty}{win}{diag}  ")
            } else {
                format!("{name}{dirty}{win}{diag}  ")
            };
            // A message too long for the rest of the row is shown whole above a hit-enter prompt, or a
            // file message cut at its start (#1222). One taller than the screen is paged from its top
            // with `-- More --` (#1235).
            use crate::ui::message::{fit, fit_question, Shown};
            let shown = if question {
                fit_question(status, usize::from(cols))
            } else {
                fit(
                    crate::ui::message::width(&prefix),
                    status,
                    usize::from(cols),
                )
            };
            match shown {
                Shown::Fits => format!("{prefix}{status}"),
                Shown::Cut(tail) => format!("{prefix}{tail}"),
                Shown::CutRow(tail) => tail,
                Shown::More(lines) => {
                    let page = usize::from(text_rows);
                    let (first, prompt) = pager.layout(status_serial, lines.len(), page);
                    let shown = &lines[first..lines.len().min(first + page)];
                    let top = text_rows - shown.len() as u16;
                    for (i, line) in shown.iter().enumerate() {
                        let row = top + i as u16;
                        for x in 0..cols {
                            cur.put(row, x, " ", Color::Reset, false);
                        }
                        cur.put_str(row, 0, line, Color::Reset, false);
                    }
                    pager.prompt_text(prompt)
                }
            }
        }
    };
    // Paint the status / command line into the last row (put_str truncates at the right edge).
    cur.put_str(rows.saturating_sub(1), 0, &bar, Color::Reset, false);

    // Diff the finished grid against the previous frame and emit ONLY the changed runs (F-006 #1),
    // wrapped in synchronized output when supported so a big repaint lands atomically (#3).
    flush_diff(out, &cur, prev, sync)?;
    *prev = cur;

    if pager.prompt != crate::ui::message::Prompt::None {
        // nvim leaves the cursor after the prompt.
        let col = u16::try_from(crate::ui::message::width(&bar))
            .unwrap_or(u16::MAX)
            .min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(col, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else if let Some((_, caret_col)) = cmd_shown {
        let ccol = (u16::try_from(caret_col + 1).unwrap_or(u16::MAX)).min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(ccol, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else {
        // The focused view owns the cursor; its DISPLAY column is grapheme-cluster / width based
        // (F-006 #4), offset into the focused window's rectangle (F-007).
        let frect = rects.get(ws.focus()).copied().unwrap_or(Rect {
            x: 0,
            y: 0,
            w: cols,
            h: text_rows,
        });
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
            out.flush()?;
            return Ok(images);
        }
        let (row, col) = focus_cursor();
        let screen_row = (frect.y + row).min(rows.saturating_sub(1));
        let screen_col = frect.x.saturating_add(col).min(cols.saturating_sub(1)); // #899
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    out.flush()?;
    Ok(images)
}

/// Emit only the cells that changed between `cur` and `prev` (F-006 #1). Each changed run is one
/// `MoveTo` then its cells, printed with lazy SGR (colour/reverse) changes; a continuation cell is
/// skipped (the wide glyph to its left already advanced over it). When `sync` is set the whole batch
/// is fenced in DEC synchronized-output (`?2026h`/`l`) so the terminal shows the frame atomically (#3).
pub(crate) fn flush_diff(
    out: &mut impl Write,
    cur: &screen::Screen,
    prev: &screen::Screen,
    sync: bool,
) -> io::Result<()> {
    use crossterm::style::{
        Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };
    use screen::CellStyle;

    let runs = cur.diff(prev);
    if runs.is_empty() {
        return Ok(()); // nothing changed — no full-screen redraw
    }
    queue!(out, cursor::Hide)?;
    if sync {
        out.write_all(b"\x1b[?2026h")?;
    }
    // Track the currently-emitted style; emit an SGR only when a field changes (lazy, like the original).
    let mut style = CellStyle::default();
    queue!(
        out,
        ResetColor,
        SetAttribute(Attribute::Reset) // clears bold/underline/italic/reverse to a known baseline
    )?;
    for (row, start, cells) in runs {
        queue!(out, cursor::MoveTo(start, row))?;
        for cell in &cells {
            let text: &str = match &cell.content {
                screen::Content::Continuation => continue, // covered by the wide glyph on the left
                screen::Content::Blank => " ",
                screen::Content::Cluster(s) => s,
            };
            let s = &cell.style;
            // Attributes: if any boolean toggled OFF, `Attribute::Reset` is the only portable way back, so
            // reset then re-assert the ones still on (also re-emit colours, which Reset clears).
            let attrs_off = (style.bold && !s.bold)
                || (style.underline && !s.underline)
                || (style.italic && !s.italic)
                || (style.reverse && !s.reverse);
            if attrs_off {
                queue!(out, SetAttribute(Attribute::Reset))?;
                style = CellStyle::default();
            }
            if s.bold && !style.bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            if s.underline && !style.underline {
                queue!(out, SetAttribute(Attribute::Underlined))?;
            }
            if s.italic && !style.italic {
                queue!(out, SetAttribute(Attribute::Italic))?;
            }
            if s.reverse && !style.reverse {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            if s.fg != style.fg {
                queue!(out, SetForegroundColor(s.fg))?;
            }
            if s.bg != style.bg {
                queue!(out, SetBackgroundColor(s.bg))?;
            }
            style = *s;
            queue!(out, Print(text))?;
        }
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    if sync {
        out.write_all(b"\x1b[?2026l")?;
    }
    Ok(())
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

#[cfg(test)]
mod display_tests;

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::ui::layout::window_rects;
    use crossterm::style::Color;
    use proptest::prelude::*;

    /// A `LineIndex` over `bytes` for the `cursor_cell` tests (it now takes the cached index instead of
    /// scanning newlines itself).
    fn li(bytes: &[u8]) -> crate::line_index::LineIndex {
        let mut idx = crate::line_index::LineIndex::default();
        idx.refresh(ruse_core::Revision(0), bytes);
        idx
    }

    // ---- F-031 slice 0: the display-coordinate layout seam (P1/P2/P6) + faces ----

    proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// P1 (round-trip) + P2 (monotonic): the shared column rule [`line_display_col`] and its inverse
        /// [`line_byte_at_col`] agree, and the column never decreases in buffer order — over arbitrary
        /// lines mixing ASCII, tabs, wide CJK (가), and a combining mark (´).
        #[test]
        fn layout_col_roundtrips_and_is_monotonic(line in "[a-z\t \u{ac00}\u{0301}]{0,40}") {
            use unicode_segmentation::UnicodeSegmentation;
            let boundaries: Vec<usize> = line
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain(std::iter::once(line.len()))
                .collect();
            let mut prev = 0u16;
            for &b in &boundaries {
                let c = line_display_col(&line, b, TAB_WIDTH);
                prop_assert!(c >= prev, "P2: column decreased at byte {}", b); // monotonic
                prev = c;
                let back = line_byte_at_col(&line, c, TAB_WIDTH);
                // P1: col -> byte -> col lands on the same column (robust to zero-width clusters).
                prop_assert_eq!(line_display_col(&line, back, TAB_WIDTH), c, "P1: round-trip diverged at byte {}", b);
            }
        }
    }

    /// P6 (identity): the caret column is the documented rule — tabs to the next stop, wide clusters
    /// count 2, a combining mark folds into its base's 1 — pinned by explicit expectations so the slice-0
    /// refactor (paint and caret now share ONE `advance_col` rule) cannot silently drift the layout.
    #[test]
    fn cursor_cell_column_matches_documented_rule() {
        let cases: &[(&str, usize, u16)] = &[
            ("abc", 3, 3),        // plain ASCII
            ("a\tb", 2, 4),       // 'a' -> col 1, tab -> next stop 4
            ("\tx", 1, 4),        // leading tab -> col 4
            ("가나", 3, 2),       // one wide (width-2) cluster before the second
            ("e\u{0301}z", 3, 1), // 'e' + combining acute = one cluster, width 1
        ];
        for &(s, pos, want) in cases {
            let (_row, col) =
                cursor_cell(s.as_bytes(), pos, 0, &[], &[], &li(s.as_bytes()), TAB_WIDTH);
            assert_eq!(col, want, "caret column for {s:?} at byte {pos}");
        }
    }

    /// #601: tabs paint and place the caret at the View's own `'tabstop'`, not a fixed 4 — `:set ts=8`
    /// widens a tab to the next multiple of 8 in both the caret column and the painted cells.
    #[test]
    fn tabs_follow_the_view_tabstop() {
        for (tab, want) in [(8u16, 8u16), (2, 2), (3, 3)] {
            let (_row, col) = cursor_cell(b"\tx", 1, 0, &[], &[], &li(b"\tx"), tab);
            assert_eq!(col, want, "caret after a leading tab at ts={tab}");
        }
        assert_eq!(line_display_col("ab\tc", 3, 8), 8);
        assert_eq!(line_byte_at_col("ab\tc", 8, 8), 3);
        // The painted row: `x` lands in the ninth cell at ts=8.
        let mut scr = screen::Screen::new(12, 1);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 12,
            h: 1,
        };
        let no_dims = std::collections::HashMap::new();
        paint_pane(
            &mut scr,
            rect,
            b"\tx",
            &[],
            0,
            None,
            None,
            &[],
            &[],
            &[],
            0,
            &[],
            &mut Vec::new(),
            false,
            &no_dims,
            &[],
            8,
        );
        assert_eq!(scr.cell(0, 8).content, screen::Content::Cluster("x".into()));
        assert_eq!(scr.cell(0, 4).content, screen::Content::Cluster(" ".into()));
    }

    /// #899: a line wider than `u16::MAX` cells overflowed the column arithmetic (a debug panic, a wrapped
    /// caret column in release). The column now saturates at the ceiling — for plain and tab-expanded text,
    /// at the caret and in the inverse map.
    #[test]
    fn caret_column_saturates_on_a_line_wider_than_u16() {
        let wide = "x".repeat(70_000);
        let (_row, col) = cursor_cell(
            wide.as_bytes(),
            wide.len(),
            0,
            &[],
            &[],
            &li(wide.as_bytes()),
            TAB_WIDTH,
        );
        assert_eq!(col, u16::MAX);
        let tabs = "\t".repeat(20_000); // 80000 cells
        assert_eq!(line_display_col(&tabs, tabs.len(), TAB_WIDTH), u16::MAX);
        assert_eq!(advance_col(u16::MAX, "\t", TAB_WIDTH), u16::MAX);
        assert_eq!(advance_col(u16::MAX - 1, "가", TAB_WIDTH), u16::MAX);
        // The inverse still terminates and lands inside the line.
        assert!(line_byte_at_col(&wide, u16::MAX, TAB_WIDTH) <= wide.len());
    }

    /// The faces channel reaches the cell grid: a byte carrying a bold / italic face paints a bold /
    /// italic cell, and a selection's reverse-video merges ON TOP of the face rather than replacing it
    /// (F-031 slice 0 — the decoration model's first attribute).
    #[test]
    fn paint_pane_carries_bold_and_italic_faces() {
        let bytes = b"ab";
        let styles = [
            screen::CellStyle {
                bold: true,
                ..screen::CellStyle::default()
            },
            screen::CellStyle {
                italic: true,
                ..screen::CellStyle::default()
            },
        ];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 2,
        };
        let mut cur = screen::Screen::new(8, 2);
        paint_pane(
            &mut cur,
            rect,
            bytes,
            &styles,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        assert!(cur.cell(0, 0).style.bold, "byte 0 paints bold");
        assert!(cur.cell(0, 1).style.italic, "byte 1 paints italic");

        let mut sel = screen::Screen::new(8, 2);
        paint_pane(
            &mut sel,
            rect,
            bytes,
            &styles,
            0,
            Some((0, 1)),
            None,
            &[],
            &[],
            &[],
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        let c = sel.cell(0, 0);
        assert!(
            c.style.bold && c.style.reverse,
            "selection reverse merges with the bold face",
        );
    }

    /// F-031 conceal (slice 1): a concealed range is HIDDEN (0 cells, following columns shift) on a
    /// non-caret line, but REVEALED (painted normally) on the caret's own line so its markup stays
    /// editable — the reveal-at-point rule (D-052). Selection/motion are unaffected (core untouched), so
    /// P3/P4/P5 hold by construction.
    #[test]
    fn paint_pane_conceals_off_caret_line_and_reveals_on_it() {
        let bytes = b"abcd\nabcd"; // two identical lines
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 2,
        };
        let conceal: [(usize, usize, Option<&str>); 2] = [(0, 2, None), (5, 7, None)]; // hide "ab" both lines

        // Caret on line 1: line 0's "ab" is concealed so 'c' shifts to col 0; line 1 is revealed so 'a'
        // stays at col 0.
        let mut s = screen::Screen::new(8, 2);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &conceal,
            1,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("c".into()),
            "off-caret line: concealed 'ab' shifts 'c' to col 0",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("a".into()),
            "caret line revealed: 'a' stays at col 0",
        );

        // Caret on line 0: now line 0 is revealed, so 'a' stays at col 0 there too.
        let mut s2 = screen::Screen::new(8, 2);
        paint_pane(
            &mut s2,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &conceal,
            0,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        assert_eq!(
            s2.cell(0, 0).content,
            screen::Content::Cluster("a".into()),
            "caret line revealed: 'a' stays at col 0",
        );
    }

    /// F-031 virt_text (slice 2): a concealed range carrying virt_text paints the GLYPH in the marker's
    /// place on a non-caret line (`- x` → `• x`), and shows the real marker on the revealed caret line.
    #[test]
    fn paint_pane_virt_text_replaces_concealed_marker_off_caret_line() {
        let bytes = b"- x\n- y"; // two list items; conceal "- " [0,2)/[4,6), virt "• "
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 2,
        };
        let conceal: [(usize, usize, Option<&str>); 2] = [(0, 2, Some("• ")), (4, 6, Some("• "))];

        // Caret on line 1: line 0 shows the bullet glyph then the text; line 1 (revealed) shows "- y".
        let mut s = screen::Screen::new(8, 2);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &conceal,
            1,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("•".into()),
            "off-caret line: '- ' concealed, '•' painted in its place",
        );
        assert_eq!(
            s.cell(0, 2).content,
            screen::Content::Cluster("x".into()),
            "text follows the 2-cell bullet glyph",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("-".into()),
            "caret line revealed: raw '-' marker shows",
        );
    }

    /// F-031 slice 3a (row model): virtual blocks above the caret's line shift the caret DOWN by their
    /// height, so the drawn caret stays on its painted row.
    #[test]
    fn cursor_cell_row_accounts_for_virtual_blocks_above() {
        let bytes = b"a\nb\nc"; // buffer lines 0, 1, 2
        let virt = [highlight::VirtLine {
            after_line: 0,
            height: 2,
            label: "img".into(),
            path: None,
        }];
        // Caret on line 2 ('c' at byte 4): row 2 + 2 virtual rows above = screen row 4.
        assert_eq!(
            cursor_cell(bytes, 4, 0, &virt, &[], &li(bytes), TAB_WIDTH).0,
            4
        );
        // Caret on line 0 ('a' at byte 0): nothing above, row 0.
        assert_eq!(
            cursor_cell(bytes, 0, 0, &virt, &[], &li(bytes), TAB_WIDTH).0,
            0
        );
    }

    /// Manual folds: a closed fold collapses its interior lines — its START row shows the summary and the
    /// line below the fold paints on the very next display row.
    #[test]
    fn paint_pane_collapses_a_closed_fold() {
        let bytes = b"a\nb\nc\nd\ne"; // lines 0..=4
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 5,
        };
        // Fold lines 1..=3 closed: line 1 shows the summary, lines 2/3 hidden, line 4 paints on row 2.
        let folds = [crate::folds::Fold {
            start: 1,
            end: 3,
            closed: true,
            level: 1,
        }];
        let mut s = screen::Screen::new(20, 5);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            9,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &folds,
            TAB_WIDTH,
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("a".into()),
            "line 0 on row 0",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("\u{25b8}".into()),
            "fold summary (▸) on the fold's start row",
        );
        assert_eq!(
            s.cell(2, 0).content,
            screen::Content::Cluster("e".into()),
            "line 4 collapses up to row 2 (lines 2,3 hidden)",
        );
    }

    /// End-to-end `foldmethod=indent` (slice 3): the pure pipeline `compute_indent_folds` →
    /// `apply_foldlevel` → `paint_pane` collapses a NESTED indent buffer. At foldlevel=0 the outermost fold
    /// closes and hides its whole subtree behind one summary row (nvim-verified ranges).
    #[test]
    fn paint_pane_collapses_computed_indent_folds() {
        // 0:a0  1:·b  2:··c  3:··d  4:·e  5:f  (sw=4). Folds: outer [1,4] L1, inner [2,3] L2.
        let bytes = b"a0\n    b\n        c\n        d\n    e\nf";
        let mut folds = crate::folds::compute_indent_folds(bytes, 4, 4);
        crate::folds::apply_foldlevel(&mut folds, 0); // foldlevel=0 → close all outermost
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 6,
        };
        let mut s = screen::Screen::new(20, 6);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            9,
            &[],
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &folds,
            TAB_WIDTH,
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("a".into()),
            "the level-0 header line stays visible above the fold",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("\u{25b8}".into()),
            "the outer fold's summary (▸) collapses the whole indented subtree onto its start row",
        );
        assert_eq!(
            s.cell(2, 0).content,
            screen::Content::Cluster("f".into()),
            "the trailing level-0 line pulls up to row 2 (lines 2..=4 hidden inside the fold)",
        );
    }

    /// F-031 slice 3a (row model): a virtual block pushes the buffer lines below it DOWN, and the block's
    /// first row shows the placeholder label.
    #[test]
    fn paint_pane_places_lines_below_a_virtual_block() {
        let bytes = b"a\nb"; // line 0 "a", line 1 "b"
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 8,
            h: 5,
        };
        let virt = [highlight::VirtLine {
            after_line: 0,
            height: 2,
            label: "x".into(),
            path: None,
        }];
        let mut s = screen::Screen::new(8, 5);
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            9,
            &virt,
            &mut Vec::new(),
            false,
            &std::collections::HashMap::new(),
            &[],
            TAB_WIDTH,
        );
        assert_eq!(
            s.cell(0, 0).content,
            screen::Content::Cluster("a".into()),
            "line 0 at row 0",
        );
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("\u{1f5bc}".into()),
            "placeholder label (🖼) on the first block row",
        );
        assert_eq!(
            s.cell(3, 0).content,
            screen::Content::Cluster("b".into()),
            "line 1 pushed below the 2-row block",
        );
    }

    /// F-031: on a graphics terminal, an image whose path could not be loaded (no dims) paints the
    /// `[image: … (not found)]` text band, NOT blank placeholder cells, and records no graphics placement.
    #[test]
    fn paint_pane_shows_text_fallback_for_a_missing_image() {
        let bytes = b"a\nb";
        let no_style: &[screen::CellStyle] = &[];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 40,
            h: 5,
        };
        let virt = [highlight::VirtLine {
            after_line: 0,
            height: 2,
            label: "alt".into(),
            path: Some("/no/such.png".into()),
        }];
        let mut s = screen::Screen::new(40, 5);
        let mut images = Vec::new();
        paint_pane(
            &mut s,
            rect,
            bytes,
            no_style,
            0,
            None,
            None,
            &[],
            &[],
            &[],
            9,
            &virt,
            &mut images,
            true,                              // graphics ON, but the image can't be loaded
            &std::collections::HashMap::new(), // no dims recorded → treated as not-found
            &[],
            TAB_WIDTH,
        );
        // Row 1 (just below line 0) begins the fallback band with the "[image:" text.
        assert_eq!(
            s.cell(1, 0).content,
            screen::Content::Cluster("[".into()),
            "fallback band starts with '['",
        );
        let row: String = (0..7)
            .map(|c| match &s.cell(1, c).content {
                screen::Content::Cluster(g) => g.clone(),
                _ => " ".into(),
            })
            .collect();
        assert_eq!(row, "[image:", "not-found text band, not placeholder cells");
        assert!(
            images.is_empty(),
            "an unloadable image records no graphics placement"
        );
    }

    /// F-009 #1: a search command carries its pattern for hlsearch; other commands do not.
    #[test]
    fn search_pattern_extracts_only_from_search_commands() {
        assert_eq!(
            search_pattern(&ruse_core::Command::SearchNext("foo".into())),
            Some("foo".to_string())
        );
        assert_eq!(search_pattern(&ruse_core::Command::MoveLeft), None);
    }

    /// F-006 #1: an unchanged frame emits ZERO bytes — no full-screen redraw.
    #[test]
    fn an_unchanged_frame_emits_nothing() {
        let mut a = screen::Screen::new(20, 3);
        a.put_str(0, 0, "hello world", Color::Reset, false);
        let mut b = screen::Screen::new(20, 3);
        b.put_str(0, 0, "hello world", Color::Reset, false);
        let mut buf: Vec<u8> = Vec::new();
        flush_diff(&mut buf, &b, &a, false).unwrap();
        assert!(buf.is_empty(), "identical frames must produce no output");
    }

    /// F-006 #1: a one-cell change emits a small, bounded batch containing just the new glyph — not
    /// the whole screen. (A full redraw of 60 cells would be far larger.)
    #[test]
    fn a_one_cell_change_emits_only_that_cell() {
        let mut a = screen::Screen::new(20, 3);
        a.put_str(0, 0, "hello world", Color::Reset, false);
        let mut b = screen::Screen::new(20, 3);
        b.put_str(0, 0, "hello wOrld", Color::Reset, false); // one char differs
        let mut buf: Vec<u8> = Vec::new();
        flush_diff(&mut buf, &b, &a, false).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains('O'), "the changed glyph is emitted");
        assert!(!s.contains("hello"), "the unchanged run is NOT re-emitted");
    }

    /// F-007: a single window fills the whole text area (the single-pane path is unchanged).
    #[test]
    fn one_window_fills_the_area() {
        let r = window_rects(80, 24, 1, SplitDir::Horizontal);
        assert_eq!(
            r,
            vec![Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24
            }]
        );
    }

    /// F-007: `:split` tiles two equal horizontal bands with a one-row divider between them; the rows
    /// partition the area exactly (band + divider + band = text_rows).
    #[test]
    fn horizontal_split_tiles_equal_bands_with_a_divider() {
        let r = window_rects(80, 25, 2, SplitDir::Horizontal);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 12
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 0,
                y: 13,
                w: 80,
                h: 12
            }
        ); // y = 12 (band) + 1 (divider)
        assert_eq!(r[0].h + 1 + r[1].h, 25, "bands + divider fill the height");
    }

    /// F-007: `:vsplit` tiles two columns side by side with a one-column divider; remainder cells go
    /// to the earliest pane so the width is fully used.
    #[test]
    fn vertical_split_tiles_columns_with_a_divider() {
        let r = window_rects(80, 24, 2, SplitDir::Vertical);
        assert_eq!(r.len(), 2);
        // (80 - 1 divider) / 2 = 39 rem 1 → first pane gets the extra column.
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 24
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 41,
                y: 0,
                w: 39,
                h: 24
            }
        ); // x = 40 (col) + 1 (divider)
        assert_eq!(r[0].w + 1 + r[1].w, 80, "columns + divider fill the width");
    }

    /// F-006 #3: with sync support the batch is fenced in DEC synchronized output (?2026h/l).
    #[test]
    fn sync_output_fences_the_batch_when_supported() {
        let a = screen::Screen::new(10, 1);
        let mut b = screen::Screen::new(10, 1);
        b.put(0, 0, "Z", Color::Reset, false);
        let mut on: Vec<u8> = Vec::new();
        flush_diff(&mut on, &b, &a, true).unwrap();
        assert!(
            on.windows(8).any(|w| w == b"\x1b[?2026h"),
            "begins synchronized output"
        );
        assert!(
            on.windows(8).any(|w| w == b"\x1b[?2026l"),
            "ends synchronized output"
        );
        let mut off: Vec<u8> = Vec::new();
        flush_diff(&mut off, &b, &a, false).unwrap();
        assert!(
            !off.windows(8).any(|w| w == b"\x1b[?2026h"),
            "no fence when unsupported"
        );
    }

    /// #909: every case of Unicode's GraphemeBreakTest (17.0.0, `tests/unicode/`) paints one cell group per
    /// extended grapheme cluster, in order, and the caret column of each cluster start is where the painter
    /// put it: the layout never splits a cluster or drifts from the caret. Cases holding a CR are left out:
    /// the painter prints a buffer CR raw and treats a CR LF cluster as no line break (#821).
    #[test]
    fn grapheme_break_test_clusters_paint_whole() {
        use unicode_segmentation::UnicodeSegmentation;
        let data = include_str!("../../../../tests/unicode/GraphemeBreakTest-17.0.0.txt");
        let mut n = 0;
        for (i, raw) in data.lines().enumerate() {
            let body = raw.split('#').next().unwrap_or("").trim();
            if body.is_empty() {
                continue;
            }
            let mut text = String::new();
            let mut want: Vec<String> = Vec::new();
            for tok in body.split_whitespace() {
                match tok {
                    "÷" => want.push(String::new()),
                    "×" => {}
                    hex => {
                        let cp = u32::from_str_radix(hex, 16).expect("hex code point");
                        let c = char::from_u32(cp).expect("scalar value");
                        text.push(c);
                        want.last_mut().expect("a case opens with ÷").push(c);
                    }
                }
            }
            want.pop(); // the closing `÷` opens nothing
            if text.contains('\r') || text.contains('\n') {
                continue;
            }
            // A cluster nvim does not print shows transcribed (`^@`, `<200b>`, #1181), not whole.
            if want.iter().any(|g| screen::transcribe(g, false).is_some()) {
                continue;
            }
            let cols = 64u16;
            let rect = Rect {
                x: 0,
                y: 0,
                w: cols,
                h: 1,
            };
            let mut cur = screen::Screen::new(cols, 1);
            paint_pane(
                &mut cur,
                rect,
                text.as_bytes(),
                &[],
                0,
                None,
                None,
                &[],
                &[],
                &[],
                0,
                &[],
                &mut Vec::new(),
                false,
                &std::collections::HashMap::new(),
                &[],
                TAB_WIDTH,
            );
            let mut painted: Vec<(u16, String)> = Vec::new();
            for col in 0..cols {
                if let screen::Content::Cluster(g) = cur.cell(0, col).content {
                    painted.push((col, g.to_string()));
                }
            }
            let got: Vec<&str> = painted.iter().map(|(_, g)| g.as_str()).collect();
            assert_eq!(got, want, "line {}: {text:?}", i + 1);
            for ((col, _), (b, _)) in painted.iter().zip(text.grapheme_indices(true)) {
                let (_, caret) = cursor_cell(
                    text.as_bytes(),
                    b,
                    0,
                    &[],
                    &[],
                    &li(text.as_bytes()),
                    TAB_WIDTH,
                );
                assert_eq!(caret, *col, "caret at byte {b} of line {}: {text:?}", i + 1);
            }
            n += 1;
        }
        assert!(n > 480, "most cases are checked ({n})");
    }

    // ---- #615 / #616: wrap, gutter, 'list' and the horizontal offset, against nvim v0.12.6 screens ----

    /// The text rows a window paints, right-trimmed, for `lines` from line `top` (0-based), the cursor on
    /// line `cursor`, with the window options `opts` set and `'tabstop'` 8 (nvim's).
    #[allow(clippy::too_many_arguments)]
    fn window_rows(
        lines: &str,
        w: u16,
        h: u16,
        top: usize,
        cursor: usize,
        leftcol: usize,
        opts: impl FnMut(&mut ruse_core::WindowOptions),
        globals: &crate::ui::window_view::DisplayGlobals,
    ) -> Vec<String> {
        let mut ws = Workspace::new(lines);
        ws.set_option_in(
            ruse_core::editor::EditorOption::TabStop(8),
            ruse_core::OptionLevel::Both,
        );
        ws.update_window_options(ruse_core::OptionLevel::Both, opts);
        let bytes = lines.as_bytes();
        let idx = li(bytes);
        let text = PaneText { bytes, idx: &idx };
        let mut win = WinView::resolve(
            ws.focused().view,
            globals,
            usize::from(w),
            usize::from(h),
            text.line_count(),
            false,
        );
        win.leftcol = leftcol;
        let dims = std::collections::HashMap::new();
        let p = PaneInput {
            text,
            win: &win,
            byte_style: &[],
            top,
            sel: None,
            block: None,
            hl: &[],
            underline: &[],
            conceal: &[],
            caret_line: cursor,
            cursor_line: cursor,
            virt: &[],
            folds: &[],
            signs: &[],
            visual: false,
            graphics_on: false,
            image_dims: &dims,
        };
        let mut scr = screen::Screen::new(w, h);
        let rect = Rect { x: 0, y: 0, w, h };
        paint_window(&mut scr, rect, &p, &mut Vec::new());
        (0..h)
            .map(|r| {
                (0..w)
                    .filter_map(|c| match scr.cell(r, c).content {
                        screen::Content::Continuation => None,
                        screen::Content::Blank => Some(" ".to_string()),
                        screen::Content::Cluster(g) => Some(g.to_string()),
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// nvim's probe buffer for the 20-column window.
    const PROBE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccccccccc\nshort\n\tx\u{4e2d}\u{6587}yyyyyyyyyyyyyyyyyyyy\n  indented line that is long enough to wrap around twice ok\nxxxxxxxxxxxxxxxxxx\u{4e2d}zz\ntrail   \n";

    #[test]
    fn number_relativenumber_showbreak_and_lastline_match_nvim() {
        let g = crate::ui::window_view::DisplayGlobals {
            showbreak: ">>".into(),
            ..Default::default()
        };
        let set = |rnu_only: bool| {
            move |o: &mut ruse_core::WindowOptions| {
                o.number = !rnu_only;
                o.relativenumber = true;
                o.linebreak = true;
                o.breakindent = true;
            }
        };
        let rows = window_rows(PROBE_A, 20, 8, 1, 2, 0, set(false), &g);
        assert_eq!(
            rows,
            [
                "  1 short",
                "3           x\u{4e2d}\u{6587}yyy",
                "    >>yyyyyyyyyyyyyy",
                "    >>yyy",
                "  1   indented line",
                "    >>that is long",
                "    >>enough to",
                "    >>wrap around@@@",
            ]
        );
        // 'relativenumber' alone: the cursor line shows 0, right-aligned.
        let rows = window_rows(PROBE_A, 20, 8, 1, 2, 0, set(true), &g);
        assert_eq!(rows[1], "  0         x\u{4e2d}\u{6587}yyy");
    }

    #[test]
    fn listchars_match_nvim() {
        let g = crate::ui::window_view::DisplayGlobals {
            listchars: "tab:>-,trail:~,eol:$,space:.,nbsp:+".into(),
            ..Default::default()
        };
        let rows = window_rows(PROBE_A, 20, 8, 1, 2, 0, |o| o.list = true, &g);
        assert_eq!(
            rows,
            [
                "short$",
                ">-------x\u{4e2d}\u{6587}yyyyyyy",
                "yyyyyyyyyyyyy$",
                "..indented.line.that",
                ".is.long.enough.to.w",
                "rap.around.twice.ok$",
                "xxxxxxxxxxxxxxxxxx\u{4e2d}",
                "zz$",
            ]
        );
    }

    #[test]
    fn nowrap_shows_the_columns_from_leftcol_like_nvim() {
        let g = crate::ui::window_view::DisplayGlobals::default();
        let rows = window_rows(PROBE_A, 20, 8, 0, 0, 6, |o| o.wrap = false, &g);
        assert_eq!(
            rows[..6],
            [
                "aaaaaaaaaaaaaaaaaaaa",
                "",
                "  x\u{4e2d}\u{6587}yyyyyyyyyyyyy",
                "nted line that is lo",
                "xxxxxxxxxxxx\u{4e2d}zz",
                "",
            ]
        );
        // A wide char cut at the left edge shows `<`.
        let rows = window_rows(PROBE_A, 20, 8, 0, 0, 10, |o| o.wrap = false, &g);
        assert_eq!(rows[2], "<\u{6587}yyyyyyyyyyyyyyyyy");
    }

    #[test]
    fn sign_column_and_numberwidth_match_nvim() {
        let text = format!(
            "{}\u{4e2d}yy\nabcdefghijklmnopqrstuvwxyz\tTAB\n    indented words go on and on and on until wrap\n a  b   \nend\n",
            "x".repeat(29)
        );
        let g = crate::ui::window_view::DisplayGlobals::default();
        let yes = |o: &mut ruse_core::WindowOptions| {
            o.number = true;
            o.signcolumn = "yes".into();
        };
        let rows = window_rows(&text, 30, 10, 0, 0, 0, yes, &g);
        assert_eq!(
            rows[..9],
            [
                "    1 xxxxxxxxxxxxxxxxxxxxxxxx",
                "      xxxxx\u{4e2d}yy",
                "    2 abcdefghijklmnopqrstuvwx",
                "      yz      TAB",
                "    3     indented words go on",
                "       and on and on until wra",
                "      p",
                "    4  a  b",
                "    5 end",
            ]
        );
        let narrow = |o: &mut ruse_core::WindowOptions| {
            o.number = true;
            o.numberwidth = 1;
        };
        let rows = window_rows(&text, 30, 10, 0, 0, 0, narrow, &g);
        assert_eq!(
            rows[..4],
            [
                "1 xxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                "  x\u{4e2d}yy",
                "2 abcdefghijklmnopqrstuvwxyz",
                "      TAB",
            ]
        );
    }
}
