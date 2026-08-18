//! Window layout geometry: tile the text area into per-pane rectangles (F-007 MVP flat layout).

use ruse_core::SplitDir;

/// A window's on-screen sub-rectangle in cells: origin `(x, y)` and size `w × h` (F-007 layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) w: u16,
    pub(crate) h: u16,
}

/// Tile `count` windows into the text area (`cols` × `text_rows`) as equal bands/columns separated by
/// a one-cell divider (F-007 MVP flat layout — no recursive tree). `Horizontal` stacks panes top to
/// bottom, `Vertical` places them side by side. The `count-1` dividers are subtracted first, then the
/// remaining cells split evenly with any remainder handed to the earliest panes (so the area is fully
/// used). Always returns `count.max(1)` rects.
pub(crate) fn window_rects(cols: u16, text_rows: u16, count: usize, split: SplitDir) -> Vec<Rect> {
    let n = count.max(1) as u16;
    let seps = n.saturating_sub(1);
    let mut rects = Vec::with_capacity(n as usize);
    match split {
        SplitDir::Horizontal => {
            let avail = text_rows.saturating_sub(seps);
            let (base, extra) = (avail / n, avail % n);
            let mut y = 0u16;
            for i in 0..n {
                let h = base + u16::from(i < extra);
                rects.push(Rect {
                    x: 0,
                    y,
                    w: cols,
                    h,
                });
                y = y.saturating_add(h + 1); // + the divider row
            }
        }
        SplitDir::Vertical => {
            let avail = cols.saturating_sub(seps);
            let (base, extra) = (avail / n, avail % n);
            let mut x = 0u16;
            for i in 0..n {
                let w = base + u16::from(i < extra);
                rects.push(Rect {
                    x,
                    y: 0,
                    w,
                    h: text_rows,
                });
                x = x.saturating_add(w + 1); // + the divider column
            }
        }
    }
    rects
}
