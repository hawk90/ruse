//! The screen cell grid and its render diff (F-006 #1/#4).
//!
//! Rendering paints the frame into a [`Screen`] — a `cols × rows` grid of [`Cell`]s — instead of
//! writing escape sequences straight to the terminal. [`Screen::diff`] compares the new frame
//! against the previous one and yields only the CHANGED runs, so the frontend emits only changed
//! cells, never a full-screen redraw (acceptance #1).
//!
//! A cell holds a whole **grapheme cluster**, not a `char` (acceptance #4): a base letter plus its
//! combining marks, or a ZWJ emoji sequence, is ONE user-perceived character occupying `1` or `2`
//! display columns — the same unit the cursor moves by (F-002). Painting per-cluster is what keeps
//! the rendered columns aligned with the logical cursor; painting per-`char` would split `👨‍👩‍👧`
//! into three wide glyphs and drop combining accents. A cluster's width is taken from its base
//! (first) char, which is how terminals actually advance for these sequences.
//!
//! Pure and terminal-free: painting and diffing are just data, unit-tested without a tty. The thin
//! flush that turns a diff into `MoveTo`+`Print` bytes lives in `main.rs` (which owns all IO).

use crossterm::style::Color;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// What occupies one display cell.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Content {
    /// A blank cell (a space).
    Blank,
    /// The right half of a wide glyph in the cell to the left — printed by that glyph, not here.
    Continuation,
    /// A grapheme cluster (base + combining marks, or a ZWJ emoji): printed verbatim.
    Cluster(Box<str>),
}

/// One display cell: its content plus the attributes to draw it with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    pub content: Content,
    pub fg: Color,
    pub reverse: bool,
}

impl Cell {
    fn blank() -> Cell {
        Cell {
            content: Content::Blank,
            fg: Color::Reset,
            reverse: false,
        }
    }
}

/// The display width, in terminal columns, of the grapheme cluster `g`: its base char's width
/// (0 for a lone combining mark, 1 for normal, 2 for wide/CJK/emoji), floored at 1 for any
/// non-empty cluster so a cluster always occupies at least one cell.
#[must_use]
pub fn cluster_width(g: &str) -> u16 {
    match g.chars().next() {
        None => 0,
        Some(base) => (UnicodeWidthChar::width(base).unwrap_or(0) as u16).max(1),
    }
}

/// A `cols × rows` grid of cells, row-major. Repainted each frame, then diffed against the prior one.
pub struct Screen {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
}

impl Screen {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Screen {
        Screen {
            cols,
            rows,
            cells: vec![Cell::blank(); cols as usize * rows as usize],
        }
    }

    fn idx(&self, row: u16, col: u16) -> Option<usize> {
        (row < self.rows && col < self.cols)
            .then(|| row as usize * self.cols as usize + col as usize)
    }

    /// Place grapheme cluster `g` at `(row, col)` and return the column AFTER it. A width-2 cluster
    /// also marks the next cell as a continuation; a width-1 cluster fills one cell. A wide cluster
    /// with no room for its second half is drawn as a blank so nothing is clipped mid-glyph.
    pub fn put(&mut self, row: u16, col: u16, g: &str, fg: Color, reverse: bool) -> u16 {
        let w = cluster_width(g);
        if w == 0 {
            return col;
        }
        let Some(i) = self.idx(row, col) else {
            return col.saturating_add(w);
        };
        if w == 2 && col + 1 >= self.cols {
            self.cells[i] = Cell {
                content: Content::Blank,
                fg,
                reverse,
            };
            return self.cols;
        }
        self.cells[i] = Cell {
            content: Content::Cluster(g.into()),
            fg,
            reverse,
        };
        if w == 2 {
            if let Some(j) = self.idx(row, col + 1) {
                self.cells[j] = Cell {
                    content: Content::Continuation,
                    fg,
                    reverse,
                };
            }
        }
        col + w
    }

    /// Paint a plain ASCII/short string left-to-right from `col`, one cluster per call. Convenience
    /// for the status line. Stops at the right edge.
    pub fn put_str(&mut self, row: u16, mut col: u16, s: &str, fg: Color, reverse: bool) -> u16 {
        for g in s.graphemes(true) {
            if col >= self.cols {
                break;
            }
            col = self.put(row, col, g, fg, reverse);
        }
        col
    }

    /// The changed runs versus `prev`, as `(row, start_col, cells)` — the minimal set the flush must
    /// repaint. A run that would begin on a wide glyph's continuation is extended left to that glyph
    /// so the flush never starts mid-glyph.
    #[must_use]
    pub fn diff(&self, prev: &Screen) -> Vec<(u16, u16, Vec<Cell>)> {
        let mut out = Vec::new();
        let same_size = self.cols == prev.cols && self.rows == prev.rows;
        for row in 0..self.rows {
            let mut col = 0;
            while col < self.cols {
                if same_size && self.cell(row, col) == prev.cell(row, col) {
                    col += 1;
                    continue;
                }
                let mut start = col;
                if start > 0 && self.cell(row, start).content == Content::Continuation {
                    start -= 1;
                }
                let mut end = col + 1;
                while end < self.cols && !(same_size && self.cell(row, end) == prev.cell(row, end))
                {
                    end += 1;
                }
                let run: Vec<Cell> = (start..end).map(|c| self.cell(row, c)).collect();
                out.push((row, start, run));
                col = end;
            }
        }
        out
    }

    fn cell(&self, row: u16, col: u16) -> Cell {
        self.idx(row, col)
            .map(|i| self.cells[i].clone())
            .unwrap_or_else(Cell::blank)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(cell: &Cell) -> Option<&str> {
        match &cell.content {
            Content::Cluster(s) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn a_zwj_emoji_is_one_cell_pair_not_three_glyphs() {
        let fam = "👨\u{200D}👩\u{200D}👧";
        let mut s = Screen::new(6, 1);
        let next = s.put(0, 0, fam, Color::Reset, false);
        assert_eq!(
            next, 2,
            "the whole family emoji is ONE width-2 cluster, not 3 wide glyphs"
        );
        assert_eq!(
            cluster(&s.cell(0, 0)),
            Some(fam),
            "the cell holds the entire cluster"
        );
        assert_eq!(s.cell(0, 1).content, Content::Continuation);
    }

    #[test]
    fn a_combining_sequence_stays_in_one_cell_with_its_accent() {
        let mut s = Screen::new(4, 1);
        let next = s.put(0, 0, "e\u{0301}", Color::Reset, false);
        assert_eq!(next, 1, "base + combining acute is one 1-wide cluster");
        assert_eq!(
            cluster(&s.cell(0, 0)),
            Some("e\u{0301}"),
            "the accent is kept, not dropped"
        );
    }

    #[test]
    fn wide_cjk_takes_two_cells() {
        let mut s = Screen::new(6, 1);
        assert_eq!(s.put(0, 0, "宽", Color::Reset, false), 2);
        assert_eq!(s.cell(0, 1).content, Content::Continuation);
    }

    #[test]
    fn diff_emits_only_changed_runs_and_nothing_when_equal() {
        let prev = Screen::new(10, 1);
        let mut cur = Screen::new(10, 1);
        cur.put(0, 3, "X", Color::Reset, false);
        let d = cur.diff(&prev);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].0, d[0].1), (0, 3));
        assert_eq!(cluster(&d[0].2[0]), Some("X"));
        // Repainting the same frame yields no output — no full redraw.
        let mut same = Screen::new(10, 1);
        same.put(0, 3, "X", Color::Reset, false);
        assert!(same.diff(&cur).is_empty());
    }

    #[test]
    fn a_changed_wide_glyph_run_never_starts_on_the_continuation() {
        let mut prev = Screen::new(6, 1);
        prev.put(0, 0, "a", Color::Reset, false);
        prev.put(0, 1, "宽", Color::Reset, false);
        let mut cur = Screen::new(6, 1);
        cur.put(0, 0, "a", Color::Reset, false);
        cur.put(0, 1, "广", Color::Reset, false);
        let d = cur.diff(&prev);
        assert_eq!(
            d[0].1, 1,
            "run starts at the glyph, not its continuation at col 2"
        );
        assert_eq!(cluster(&d[0].2[0]), Some("广"));
    }

    #[test]
    fn a_size_change_repaints_everything() {
        let prev = Screen::new(4, 1);
        let mut cur = Screen::new(6, 1);
        cur.put(0, 0, "h", Color::Reset, false);
        assert!(
            !cur.diff(&prev).is_empty(),
            "a resize forces a full repaint"
        );
    }
}
