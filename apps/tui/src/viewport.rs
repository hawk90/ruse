//! Vertical viewport for the v0 TUI — a frontend-only view concern (the core stays view-free,
//! INV-DOC-VIEW). The whole viewport is one `top` offset (first visible buffer line); this module owns
//! the single rule that keeps the cursor on screen. See `docs/design/render-and-frontends.md` (v0 section).

/// Recompute the scroll offset so the cursor row stays visible with a `scrolloff` margin of context above
/// and below (Vim's `scrolloff`). Pure and total: all arithmetic saturates at 0.
///
/// - `cursor_row` — the cursor's buffer row (0-based).
/// - `height`     — visible text rows (terminal rows minus the status line).
/// - `scrolloff`  — desired context margin; clamped to `(height-1)/2` so it always fits.
/// - `top`        — the current first-visible row.
///
/// Returns the new `top`. Only ever moves `top` enough to bring the cursor (plus its margin) into view;
/// a cursor already comfortably inside the window leaves `top` unchanged (no scroll on horizontal motion).
pub fn scroll_top(cursor_row: usize, height: usize, scrolloff: usize, top: usize) -> usize {
    if height == 0 {
        return cursor_row; // degenerate window: pin to the cursor so it is at least addressable.
    }
    // A window of `height` rows can reserve at most `(height-1)/2` rows of margin on each side.
    let margin = scrolloff.min((height - 1) / 2);
    if cursor_row < top + margin {
        // Cursor is within `margin` of (or above) the top edge → scroll up to give it room.
        cursor_row.saturating_sub(margin)
    } else if cursor_row + margin >= top + height {
        // Cursor is within `margin` of (or below) the bottom edge → scroll down.
        (cursor_row + margin + 1).saturating_sub(height)
    } else {
        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_buffer_never_scrolls() {
        // Cursor anywhere in a buffer that fits leaves the top pinned at 0.
        for row in 0..24 {
            assert_eq!(
                scroll_top(row, 24, 0, 0),
                0,
                "row {row} fits without scrolling"
            );
        }
    }

    #[test]
    fn scrolls_down_to_reveal_cursor_below_the_fold() {
        // height 24, no margin: cursor at row 30 → top so that row 30 is the last visible (30-23=7).
        assert_eq!(scroll_top(30, 24, 0, 0), 7);
        // Bottom visible row = top + height - 1 = 7 + 23 = 30. Cursor exactly on the last row.
    }

    #[test]
    fn scrolls_up_to_reveal_cursor_above_the_top() {
        // Already scrolled to top=10; moving the cursor to row 3 pulls the view back up.
        assert_eq!(scroll_top(3, 24, 0, 10), 3);
    }

    #[test]
    fn keeps_scrolloff_margin_below() {
        // margin 3: cursor at row 30 must leave 3 rows of context beneath it.
        let top = scroll_top(30, 24, 3, 0);
        let bottom = top + 24 - 1;
        assert_eq!(bottom, 30 + 3, "3 rows of context remain below the cursor");
    }

    #[test]
    fn keeps_scrolloff_margin_above() {
        // Scrolled down (top=20); cursor at row 21 with margin 3 must pull the view up to keep 3 above.
        let top = scroll_top(21, 24, 3, 20);
        assert_eq!(top, 21 - 3, "3 rows of context remain above the cursor");
    }

    #[test]
    fn margin_is_clamped_to_the_window() {
        // A 3-row window cannot honor a margin of 5; clamped margin is (3-1)/2 = 1, so it stays total.
        // Cursor at row 100 → top = 100 + 1 + 1 - 3 = 99; bottom = 99 + 2 = 101 ≥ cursor. Never panics.
        let top = scroll_top(100, 3, 5, 0);
        assert!(
            top <= 100 && top + 3 > 100,
            "cursor stays inside a tiny window"
        );
    }

    #[test]
    fn zero_height_is_total() {
        assert_eq!(scroll_top(42, 0, 3, 0), 42);
    }
}
