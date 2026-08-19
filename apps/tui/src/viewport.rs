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

/// Where a recenter command (`z`) places the cursor's line in the window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecenterTo {
    /// `zz` / `z.` — the cursor's line at the vertical center.
    Center,
    /// `zt` / `z<CR>` — the cursor's line at the top.
    Top,
    /// `zb` / `z-` — the cursor's line at the bottom.
    Bottom,
}

/// The scroll offset (`top`) that places `cursor_row` at the requested position in a `height`-row window
/// (Vim `z`). Pure and saturating. The per-frame [`scroll_top`] pass then applies `scrolloff`, so `Top`/
/// `Bottom` end up `scrolloff` rows inside the edge exactly as Vim's `zt`/`zb` do; `Center` is unaffected.
pub fn recenter(cursor_row: usize, height: usize, to: RecenterTo) -> usize {
    match to {
        RecenterTo::Top => cursor_row,
        RecenterTo::Center => cursor_row.saturating_sub(height / 2),
        RecenterTo::Bottom => cursor_row.saturating_sub(height.saturating_sub(1)),
    }
}

/// Which visible line `H`/`M`/`L` target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenTo {
    /// `H` — top of the window (`{count}H` = count lines below the top), scrolloff-aware.
    High,
    /// `M` — the middle visible line.
    Middle,
    /// `L` — bottom of the window (`{count}L` = count lines above the bottom), scrolloff-aware.
    Low,
}

/// The buffer row `H`/`M`/`L` move the cursor to, given the window's `top` / `height` and `scrolloff`,
/// a `count` (0 = none), and the buffer's `last_line`. Pure; the row is clamped into the visible range.
/// `H`/`L` keep a `scrolloff` margin from the edge unless the edge is the buffer start/end (as in Vim).
pub fn screen_line(
    top: usize,
    height: usize,
    scrolloff: usize,
    to: ScreenTo,
    count: u32,
    last_line: usize,
) -> usize {
    let bottom = (top + height.saturating_sub(1)).min(last_line);
    let n = (count.max(1) - 1) as usize; // extra lines from a count
    let row = match to {
        ScreenTo::High => {
            let floor = if top > 0 { scrolloff } else { 0 };
            top + floor.max(n)
        }
        ScreenTo::Middle => top + (bottom - top) / 2,
        ScreenTo::Low => {
            let floor = if bottom < last_line { scrolloff } else { 0 };
            bottom.saturating_sub(floor.max(n))
        }
    };
    row.clamp(top, bottom)
}

/// `C-e` (down) / `C-y` (up) scroll the view `count` lines while keeping the cursor inside the `scrolloff`
/// band. Returns `(new_top, new_cursor_row)`; the caller sets the top and, if the cursor row changed,
/// moves the cursor to keep it on screen. Pure and saturating.
pub fn scroll_lines(
    top: usize,
    cursor_row: usize,
    height: usize,
    scrolloff: usize,
    count: usize,
    down: bool,
    last_line: usize,
) -> (usize, usize) {
    let margin = scrolloff.min(height.saturating_sub(1) / 2);
    let new_top = if down {
        (top + count).min(last_line)
    } else {
        top.saturating_sub(count)
    };
    let lo = new_top + margin;
    let hi = (new_top + height.saturating_sub(1))
        .saturating_sub(margin)
        .min(last_line);
    let new_cursor = cursor_row.clamp(lo, hi.max(lo)).min(last_line);
    (new_top, new_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_line_targets_are_scrolloff_aware() {
        // Window rows 10..=29 (top=10, height=20), scrolloff 3, big buffer.
        assert_eq!(screen_line(10, 20, 3, ScreenTo::High, 1, 999), 13); // top + scrolloff
        assert_eq!(screen_line(10, 20, 3, ScreenTo::Low, 1, 999), 26); // bottom(29) - scrolloff
        assert_eq!(screen_line(10, 20, 3, ScreenTo::Middle, 1, 999), 19); // top + 19/2
        assert_eq!(screen_line(10, 20, 3, ScreenTo::High, 5, 999), 14); // 5H = 4 below top (> scrolloff)
                                                                        // At the buffer top, H has no scrolloff floor.
        assert_eq!(screen_line(0, 20, 3, ScreenTo::High, 1, 999), 0);
        // A short buffer clamps the bottom to the last line.
        assert_eq!(screen_line(0, 20, 3, ScreenTo::Low, 1, 5), 5);
    }

    #[test]
    fn scroll_lines_moves_view_and_keeps_cursor_in_band() {
        // C-e by 1 from top=0, cursor at row 0 (top edge): view moves to 1, cursor pulled to margin.
        let (nt, nr) = scroll_lines(0, 0, 20, 3, 1, true, 999);
        assert_eq!(nt, 1);
        assert_eq!(nr, 1 + 3); // new_top + scrolloff
                               // Cursor comfortably mid-screen doesn't move.
        let (nt, nr) = scroll_lines(10, 20, 20, 3, 1, true, 999);
        assert_eq!((nt, nr), (11, 20));
        // C-y up saturates at 0.
        let (nt, _) = scroll_lines(2, 10, 20, 3, 5, false, 999);
        assert_eq!(nt, 0);
    }

    #[test]
    fn recenter_positions_the_cursor_line() {
        // A 20-row window, cursor at row 50.
        assert_eq!(recenter(50, 20, RecenterTo::Top), 50);
        assert_eq!(recenter(50, 20, RecenterTo::Center), 40); // 50 - 20/2
        assert_eq!(recenter(50, 20, RecenterTo::Bottom), 31); // 50 - (20-1)
                                                              // Near the top of the buffer, everything saturates at 0 (no negative scroll).
        assert_eq!(recenter(2, 20, RecenterTo::Center), 0);
        assert_eq!(recenter(0, 20, RecenterTo::Bottom), 0);
    }

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
