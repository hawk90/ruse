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

/// What [`paint_window`] works out once for the whole window: the text area's columns, the layout options
/// and the `'fillchars'` `lastline` text.
struct WinGeom<'a> {
    /// The text area's first column and the window's right edge.
    tx: u16,
    x1: u16,
    /// The text area's width in cells.
    tw: usize,
    /// The first virtual column shown (0 with `'wrap'`).
    leftcol: usize,
    lay_opts: crate::ui::line_layout::Layout<'a>,
    lastline: String,
}

/// What the row loop of [`paint_window`] carries from one line to the next.
struct RowCursor {
    /// The window row the next line paints at.
    row: u16,
    /// The buffer line painted next.
    line: usize,
    /// No line painted yet: the first one starts `win.skip` rows in and always paints.
    first: bool,
    /// A last line that did not fit ended the loop (no `eob` filler then).
    cut: bool,
}

/// One buffer line as the row loop hands it to a row painter.
struct LineAt<'a> {
    line: usize,
    /// The screen row it starts at.
    srow: u16,
    /// The byte where it starts.
    ls: usize,
    /// Its own rows above the window (`win.skip` on the first line).
    skip: usize,
    /// The virtual column past which nothing shows.
    max_vcol: usize,
    /// Its bytes, up to what the window can show.
    text: &'a [u8],
}

/// Whether the row loop goes on after a line.
enum Step {
    Next,
    /// A last line that did not fit was painted as `'display'` says: the window is full.
    Stop,
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
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let win = p.win;
    let gutter = u16::try_from(win.gutter()).unwrap_or(u16::MAX).min(rect.w);
    let mut buf = [0u8; 4];
    let lastline: &str = win.fill.lastline.encode_utf8(&mut buf);
    let g = WinGeom {
        tx: rect.x + gutter,
        x1: rect.x + rect.w,
        tw: usize::from(rect.w - gutter),
        leftcol: if win.wrap { 0 } else { win.leftcol },
        lay_opts: win.layout(),
        lastline: lastline.to_string(),
    };
    let count = p.text.line_count();
    let mut at = RowCursor {
        row: 0,
        line: p.top,
        first: true,
        cut: false,
    };
    while at.row < rect.h && at.line < count {
        if crate::folds::hidden(p.folds, at.line) {
            at.line += 1;
            continue;
        }
        // Only what the window can show is laid out (a huge line costs its visible rows).
        let skip = if at.first { win.skip } else { 0 };
        let max_vcol = if win.wrap {
            (skip + usize::from(rect.h - at.row) + 1).saturating_mul(g.tw.max(1))
        } else {
            g.leftcol + g.tw + 1
        };
        let ln = LineAt {
            line: at.line,
            srow: rect.y + at.row,
            ls: p.text.start(at.line),
            skip,
            max_vcol,
            text: p.text.line_upto(at.line, PaneText::bytes_for(max_vcol)),
        };
        if let Some(fold) = crate::folds::closed_starting_at(p.folds, at.line) {
            paint_fold_row(cur, rect, p, &g, fold, &ln);
            at.row += 1;
        } else if let Step::Stop = paint_text_line(cur, rect, p, &g, &mut at, &ln) {
            break;
        }
        at.first = false;
        paint_virt_below(cur, rect, p, &mut at, out_images);
        at.line += 1;
    }
    // The rows after the buffer's last line show 'fillchars' `eob` in their first column.
    if !at.cut && at.line >= count {
        let mut eb = [0u8; 4];
        let eob: &str = win.fill.eob.encode_utf8(&mut eb);
        for r in at.row..rect.h {
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

/// A closed fold's start row: its one-line summary in place of the text, then 'fillchars' `fold`.
fn paint_fold_row(
    cur: &mut screen::Screen,
    rect: Rect,
    p: &PaneInput<'_>,
    g: &WinGeom<'_>,
    fold: &crate::folds::Fold,
    ln: &LineAt<'_>,
) {
    use unicode_segmentation::UnicodeSegmentation;
    let (srow, x1) = (ln.srow, g.x1);
    paint_gutter(cur, rect, srow, ln.line, 0, p);
    let mut c = g.tx;
    for gph in crate::folds::summary(fold, &String::from_utf8_lossy(ln.text)).graphemes(true) {
        if c >= x1 {
            break;
        }
        c = cur.put(srow, c, gph, DIM, false);
    }
    // 'fillchars' `fold` fills the rest of the row.
    let mut fb = [0u8; 4];
    let fill: &str = p.win.fill.fold.encode_utf8(&mut fb);
    while c < x1 {
        c = cur.put(srow, c, fill, DIM, false);
    }
    paint_line_overlays(
        cur,
        rect,
        p,
        &LineRows {
            line: ln.line,
            srow,
            shown: 1,
            skip: 0,
            rows: 1,
            fold: true,
        },
    );
}

/// A text line: laid out (concealed but on the caret's line), its gutter, cells, `'listchars'` edges and
/// overlays, then `at.row` moved past its rows. A last line that does not fit shows as `'display'` says,
/// sets `at.cut` and stops the loop.
fn paint_text_line(
    cur: &mut screen::Screen,
    rect: Rect,
    p: &PaneInput<'_>,
    g: &WinGeom<'_>,
    at: &mut RowCursor,
    ln: &LineAt<'_>,
) -> Step {
    use crate::ui::window_view::LastLine;
    let win = p.win;
    let (srow, ls, skip, text, tw) = (ln.srow, ln.ls, ln.skip, ln.text, g.tw);
    // F-031 conceal: hidden on every line but the caret's, which is revealed so its markup stays
    // editable.
    let conceal: Vec<(usize, usize, Option<&str>)> = if ln.line == p.caret_line {
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
        g.leftcol
    };
    let lay = crate::ui::line_layout::layout_from(
        text,
        &g.lay_opts,
        &conceal,
        min_vcol,
        ln.max_vcol,
        usize::MAX,
    );
    let rows = lay.rows.saturating_sub(skip).max(1);
    let avail = usize::from(rect.h - at.row);
    // The first line always paints; a later one that does not fit shows as 'display' says.
    let partial = rows > avail && !at.first;
    if partial && win.lastline == LastLine::AtRows {
        for r in 0..avail {
            cur.put(srow + r as u16, g.tx, &g.lastline, DIM, false);
        }
        at.cut = true;
        return Step::Stop;
    }
    let shown = rows.min(avail);
    for r in 0..shown {
        paint_gutter(cur, rect, srow + r as u16, ln.line, r + skip, p);
    }
    let truncate_row = (partial && win.lastline == LastLine::Truncate).then_some(shown - 1);
    paint_cells(cur, p, g, &lay, ln, shown, truncate_row);
    if !win.wrap {
        paint_list_edges(cur, p, g, &lay, srow);
    }
    paint_line_overlays(
        cur,
        rect,
        p,
        &LineRows {
            line: ln.line,
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
                    cur.put(last, g.x1 - 1 - k as u16, &g.lastline, DIM, false);
                }
            }
            _ => {
                for k in 0..3.min(tw) {
                    cur.put(last, g.tx + k as u16, &g.lastline, DIM, false);
                }
            }
        }
        at.cut = true;
        return Step::Stop;
    }
    at.row += shown as u16;
    Step::Next
}

/// The laid-out cells of a text line on its `shown` rows from its row `skip` (none on `truncate_row`).
fn paint_cells(
    cur: &mut screen::Screen,
    p: &PaneInput<'_>,
    g: &WinGeom<'_>,
    lay: &crate::ui::line_layout::LineLayout<'_>,
    ln: &LineAt<'_>,
    shown: usize,
    truncate_row: Option<usize>,
) {
    use crate::ui::line_layout::{CellKind, CellText};
    let win = p.win;
    let (skip, leftcol, tw) = (ln.skip, g.leftcol, g.tw);
    for c in &lay.cells {
        let (r, col) = if win.wrap {
            lay.place(c.vcol)
        } else {
            (0, c.vcol)
        };
        if r < skip || r - skip >= shown || Some(r - skip) == truncate_row {
            continue;
        }
        let srow = ln.srow + (r - skip) as u16;
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
        let x = g.tx + col as u16;
        let byte = c.byte.map(|b| ln.ls + b);
        let style = cell_style(p, byte, kind);
        let mut buf = [0u8; 4];
        let glyph: &str = match &text {
            CellText::Str(s) => s,
            CellText::Char(ch) => ch.encode_utf8(&mut buf),
            CellText::Blank => " ",
        };
        if col + width > tw {
            // A wide glyph with no room for its second half (a window narrower than it).
            cur.put_styled(srow, x, " ", &style);
        } else {
            cur.put_styled(srow, x, glyph, &style);
        }
    }
}

/// 'listchars' `extends` / `precedes` on a line without 'wrap': it goes on past the right / left edge.
fn paint_list_edges(
    cur: &mut screen::Screen,
    p: &PaneInput<'_>,
    g: &WinGeom<'_>,
    lay: &crate::ui::line_layout::LineLayout<'_>,
    srow: u16,
) {
    let Some(lc) = p.win.list.as_ref() else {
        return;
    };
    let (tx, tw, leftcol) = (g.tx, g.tw, g.leftcol);
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

/// A virtual block (an image placeholder) below line `at.line` pushes the lines after it down: painted
/// (and its image collected) while it starts inside the window, `at.row` moved past it either way.
fn paint_virt_below(
    cur: &mut screen::Screen,
    rect: Rect,
    p: &PaneInput<'_>,
    at: &mut RowCursor,
    out_images: &mut Vec<(String, graphics::Placement)>,
) {
    let Some(vb) = p.virt.iter().find(|v| v.after_line == at.line) else {
        return;
    };
    let row = at.row;
    if row < rect.h {
        match (p.graphics_on, &vb.path) {
            // A graphics terminal shows the real image ONLY when its pixel dimensions were read; a missing
            // or unreadable path paints a text "not found" band instead of a blank placeholder grid.
            (true, Some(path)) => match p.image_dims.get(path).copied() {
                Some(dims) => paint_image_cells(cur, rect, row, vb.height, path, Some(dims)),
                None => paint_missing_image(cur, rect, row, vb.height, path),
            },
            _ => paint_virt_block(cur, rect, row, vb),
        }
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
    at.row = row.saturating_add(vb.height);
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

/// What the frame paints into every window from the focused buffer: its faces, conceal, signs, folds and
/// virtual blocks, which only the focused pane shows, and the per-window layout.
pub(super) struct Focused<'a> {
    pub(super) ws: &'a Workspace,
    /// The focused buffer.
    pub(super) doc: ruse_core::DocumentId,
    pub(super) byte_style: Vec<screen::CellStyle>,
    pub(super) conceal: Vec<(usize, usize, Option<&'static str>)>,
    /// The focused caret's buffer line (reveal-at-point).
    pub(super) caret_line: usize,
    pub(super) signs: Vec<(usize, char, crossterm::style::Color)>,
    pub(super) focus_hl: &'a [(usize, usize)],
    pub(super) virt_lines: &'a [highlight::VirtLine],
    pub(super) folds: &'a [crate::folds::Fold],
    pub(super) diagnostics: &'a [crate::lsp::Diag],
    pub(super) line_idx: &'a crate::line_index::LineIndex,
    pub(super) pane_idx: &'a crate::line_index::PaneIndexes,
    pub(super) wins: &'a [WinView],
    pub(super) terminals: &'a TermViews<'a>,
    pub(super) graphics_on: bool,
    pub(super) image_dims: &'a std::collections::HashMap<String, (u32, u32)>,
}

/// Flatten the focused buffer's highlight spans into a per-byte FACE (spans index into it). Panes
/// showing the SAME buffer reuse it; a pane on a different buffer (post-MVP) renders unstyled. Longest
/// spans come first (highlight orders them), so this last-wins flatten lets the shortest/most-specific
/// capture win — the slice-0 stand-in for the decoration model's priority resolution (F-031).
pub(super) fn face_map(spans: &[highlight::Span], len: usize) -> Vec<screen::CellStyle> {
    // Size the per-byte face array to the LAST styled byte, not the whole buffer: the highlighter clips
    // `spans` to the viewport, so this is ~O(viewport) instead of an O(buffer) alloc+zero every frame.
    // paint_pane reads any unstyled index (`byte_style.get(i)`) as the default face, so an array that stops
    // at the last span is byte-for-byte identical to a whole-buffer one — just without the below-viewport tail.
    let style_len = spans.iter().map(|s| s.end).max().unwrap_or(0).min(len);
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
    byte_style
}

/// F-031 conceal: the byte ranges the providers marked hidden (Markdown heading `# ` prefixes today).
/// Off with `render.conceal = false` in the config (`conceal_on`), or `RUSE_CONCEAL=off` for one run.
/// The revealed line is the focused caret's buffer line (reveal-at-point).
pub(super) fn concealed(
    spans: &[highlight::Span],
    conceal_on: bool,
) -> Vec<(usize, usize, Option<&'static str>)> {
    let conceal_on = conceal_on && std::env::var("RUSE_CONCEAL").as_deref() != Ok("off");
    if conceal_on {
        spans
            .iter()
            .filter(|s| s.conceal)
            .map(|s| (s.start, s.end, s.virt))
            .collect()
    } else {
        Vec::new()
    }
}

/// #616: the focused buffer's diagnostics as signs, the most severe one on each line.
pub(super) fn diagnostic_signs(
    diagnostics: &[crate::lsp::Diag],
    line_idx: &crate::line_index::LineIndex,
    len: usize,
) -> Vec<(usize, char, crossterm::style::Color)> {
    use crate::lsp::model::Severity;
    use crossterm::style::Color;
    let mut by_line: std::collections::BTreeMap<usize, Severity> = Default::default();
    let rank = |s: &Severity| match s {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
        Severity::Hint => 3,
    };
    for d in diagnostics {
        let l = line_idx.line_of(d.start.min(len));
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
}

/// Paint every window into its sub-rectangle; the focused view owns the terminal cursor below.
pub(super) fn paint_windows(
    cur: &mut screen::Screen,
    rects: &[Rect],
    f: &Focused<'_>,
    images: &mut Vec<(String, graphics::Placement)>,
) {
    for (i, &rect) in rects.iter().enumerate().take(f.ws.window_count()) {
        let p = f.ws.pane(i);
        // F-011 slice 2: a terminal window paints its VT grid (colors, cursor, styles), not the empty
        // placeholder document.
        #[cfg(unix)]
        if let Some(&grid) = f.terminals.get(&p.view.doc()) {
            paint_grid(cur, rect, grid);
            continue;
        }
        let pbytes = p.doc.bytes();
        let style: &[screen::CellStyle] = if p.view.doc() == f.doc {
            &f.byte_style
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
        let hl: &[(usize, usize)] = if i == f.ws.focus() { f.focus_hl } else { &[] };
        // Conceal applies to the focused pane's view of the focused doc (F-031); other panes render markup.
        let pane_conceal: &[(usize, usize, Option<&'static str>)] =
            if i == f.ws.focus() && p.view.doc() == f.doc {
                &f.conceal
            } else {
                &[]
            };
        // Virtual blocks (image placeholders) likewise paint into the focused pane's view (F-031 slice 3a).
        let pane_virt: &[highlight::VirtLine] = if i == f.ws.focus() && p.view.doc() == f.doc {
            f.virt_lines
        } else {
            &[]
        };
        // Folds are per-view; slice 1 collapses only the FOCUSED pane's folds (no dedicated F-*; F-007 carve-out).
        let pane_folds: &[crate::folds::Fold] = if i == f.ws.focus() { f.folds } else { &[] };
        // F-014: underline the focused buffer's diagnostic ranges.
        let underline: Vec<(usize, usize)> = if i == f.ws.focus() {
            f.diagnostics.iter().map(|d| (d.start, d.end)).collect()
        } else {
            Vec::new()
        };
        // A pane on another buffer than the focused one reads that buffer's cached line index (#1182),
        // which the frame's viewport pass refreshed.
        let mut own_idx = crate::line_index::LineIndex::default();
        let idx = if p.view.doc() == f.doc {
            f.line_idx
        } else if let Some(ix) = f.pane_idx.get(p.view.doc(), p.doc.revision()) {
            ix
        } else {
            own_idx.refresh_for(p.view.doc(), p.doc.revision(), pbytes);
            &own_idx
        };
        let fallback;
        let win = match f.wins.get(i) {
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
            caret_line: f.caret_line,
            cursor_line: idx.line_of(p.view.cursor().0.min(pbytes.len())),
            virt: pane_virt,
            folds: pane_folds,
            signs: if i == f.ws.focus() { &f.signs } else { &[] },
            visual: matches!(p.view.mode(), Mode::Visual { .. } | Mode::Select { .. }),
            graphics_on: f.graphics_on,
            image_dims: f.image_dims,
        };
        paint_window(cur, rect, &input, images);
    }
}
