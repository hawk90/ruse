//! Window tiling and its dividers. Split from `render.rs` (#1386).

use super::*;
use crate::ui::layout::window_rects;

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
