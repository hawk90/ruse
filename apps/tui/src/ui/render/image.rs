//! Image blocks and virtual lines (F-031). Split from `render.rs` (#1386).

use super::*;

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
pub(super) fn collect_image(
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
pub(super) fn paint_image_cells(
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
pub(super) fn paint_virt_block(
    cur: &mut screen::Screen,
    rect: Rect,
    disp_row: u16,
    vb: &highlight::VirtLine,
) {
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
pub(super) fn paint_missing_image(
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
