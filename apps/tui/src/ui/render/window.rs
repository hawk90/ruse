//! Painting one window: its text, the gutter, and the cursorline and colorcolumn overlays. Split from `render.rs` (#1386).

use super::*;

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
pub(super) const CURSORLINE_BG: crossterm::style::Color = crossterm::style::Color::AnsiValue(236);
pub(super) const COLORCOLUMN_BG: crossterm::style::Color = crossterm::style::Color::AnsiValue(239);

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
