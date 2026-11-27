//! Window layout geometry: tile the text area into per-pane rectangles (F-007 MVP flat layout), and the
//! screen's bottom rows below it: the bar and the `'cmdheight'` message rows (RFC-0027 §1.1).

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

/// The bottom of the screen (RFC-0027 §1.1): the windows' rows, the bar (ruse's one status line) and the
/// `'cmdheight'` message rows below it. At `'cmdheight'` 0 the bar is the last row and also shows the
/// message, and the command line opens over it. At N ≥ 1 the bar has its own row and the last N rows hold
/// the messages and the command line, which opens on the first of them (nvim v0.12.6 with
/// `'laststatus'` 2, probed in a PTY at 80x24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BottomLayout {
    /// The windows' rows, from the top of the screen.
    pub(crate) text_rows: u16,
    /// The bar's row.
    pub(crate) bar_row: u16,
    /// The message rows below the bar: `'cmdheight'` as the screen allows it.
    pub(crate) msg_rows: u16,
}

impl BottomLayout {
    /// The layout of a screen `rows` tall for `cmdheight`. A height that would leave the windows no row is
    /// clamped to `rows − 2`, as nvim clamps `:set ch=30` (to 22 at 24 rows); a screen of fewer than 3
    /// rows has no room for a message row and falls back to 0.
    pub(crate) fn new(rows: u16, cmdheight: i64) -> BottomLayout {
        let room = i64::from(rows.saturating_sub(2));
        let msg_rows = if rows < 3 {
            0
        } else {
            u16::try_from(cmdheight.clamp(0, room)).unwrap_or(0)
        };
        let bar_row = rows.saturating_sub(1 + msg_rows);
        BottomLayout {
            text_rows: bar_row,
            bar_row,
            msg_rows,
        }
    }

    /// The row the command line opens on: over the bar at 0, else the first message row.
    pub(crate) fn cmd_row(&self) -> u16 {
        if self.msg_rows == 0 {
            self.bar_row
        } else {
            self.bar_row + 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BottomLayout;

    fn layout(rows: u16, ch: i64) -> (u16, u16, u16, u16) {
        let l = BottomLayout::new(rows, ch);
        (l.text_rows, l.bar_row, l.msg_rows, l.cmd_row())
    }

    /// nvim v0.12.6 at 80x24: `winheight(0)` is 23 at ch=0, 22 at ch=1, 21 at ch=2, 20 at ch=3; the status
    /// line is just above the message rows, and `:` opens on the first of them.
    #[test]
    fn every_height_leaves_the_windows_what_nvim_leaves_them() {
        assert_eq!(layout(24, 0), (23, 23, 0, 23));
        assert_eq!(layout(24, 1), (22, 22, 1, 23));
        assert_eq!(layout(24, 2), (21, 21, 2, 22));
        assert_eq!(layout(24, 3), (20, 20, 3, 21));
    }

    /// `:set ch=30` at 24 rows leaves one window row (nvim's clamp to `rows − 2`); a smaller screen narrows
    /// only the layout, and one too small for a message row falls back to 0.
    #[test]
    fn a_height_too_tall_for_the_screen_is_clamped() {
        assert_eq!(layout(24, 30), (1, 1, 22, 2));
        assert_eq!(layout(24, 22), (1, 1, 22, 2));
        assert_eq!(layout(10, 3), (6, 6, 3, 7));
        assert_eq!(layout(3, 5), (1, 1, 1, 2));
        assert_eq!(layout(2, 1), (1, 1, 0, 1));
        assert_eq!(layout(1, 1), (0, 0, 0, 0));
        assert_eq!(layout(0, 1), (0, 0, 0, 0));
        assert_eq!(layout(24, -1), (23, 23, 0, 23));
    }
}
