//! Display-column math: the column of a byte in a line and its inverse. Split from `render.rs` (#1386).

use super::screen;

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
