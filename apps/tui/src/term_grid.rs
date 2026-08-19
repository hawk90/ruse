//! A VT terminal screen grid (F-011 slice 2), driven by the `vte` parser. A [`Grid`] is a `rows × cols`
//! matrix of styled cells with a cursor; it implements [`vte::Perform`] so `Terminal::drain` can feed raw
//! PTY bytes straight in. This replaces slice 1's line-mode `AnsiStrip` scrollback, so colors, cursor
//! addressing, erases, and the alternate screen render correctly and full-screen TUIs (vim/htop) are usable.
//!
//! Scope (slice 2): printable + wrap + CR/LF/BS/TAB; cursor CUP/CUU/CUD/CUF/CUB/CHA/VPA + save/restore; SGR
//! (16 + bright colors, truecolor, bold/underline/italic/reverse); erase ED/EL; alt-screen (`?1049h/l`);
//! RI/index scrolling; `?25` cursor visibility. Deferred to 2b: scrollback PAGING UI (rows accumulate here),
//! scroll regions, insert/delete char/line, mouse. `vte` decodes UTF-8 itself, so `print` receives `char`.

#![cfg(unix)]

use std::collections::VecDeque;

use crossterm::style::Color;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

use crate::screen::CellStyle;

/// Off-screen rows kept above the visible screen (viewing them is slice 2b). Bounds memory.
const SCROLLBACK_MAX: usize = 1000;

/// One grid cell: the (usually single-char) text, its style, and whether it is the right half of a wide glyph
/// to the left (the painter skips continuations — the wide glyph already covered two columns).
#[derive(Clone)]
struct GridCell {
    text: String,
    style: CellStyle,
    continuation: bool,
}

impl GridCell {
    fn blank(style: CellStyle) -> GridCell {
        GridCell {
            text: String::new(),
            style,
            continuation: false,
        }
    }
}

/// The VT screen. Row-major `cells`; `cx`/`cy` are the cursor column/row; `style` is the active SGR.
pub struct Grid {
    rows: u16,
    cols: u16,
    cells: Vec<GridCell>,
    cx: u16,
    cy: u16,
    style: CellStyle,
    saved: Option<(u16, u16)>,
    /// The stashed MAIN screen while the alternate screen is active (`?1049h`); `None` on the main screen.
    alt: Option<Box<AltState>>,
    scrollback: VecDeque<Vec<GridCell>>,
    /// The vertical scroll region `[top, bottom]` (0-based inclusive), set by `DECSTBM` (CSI `r`). Scrolls
    /// (LF at the bottom margin, RI at the top, IL/DL) stay within it; default = the whole screen (slice 2b).
    scroll_top: u16,
    scroll_bottom: u16,
    /// Deferred wrap: a printable at the last column leaves the cursor there and arms this; the NEXT printable
    /// wraps first (so writing exactly `cols` chars does not scroll until char `cols+1`).
    wrap_next: bool,
    cursor_visible: bool,
}

/// The saved main-screen state during alternate-screen mode.
struct AltState {
    cells: Vec<GridCell>,
    cx: u16,
    cy: u16,
    style: CellStyle,
    saved: Option<(u16, u16)>,
}

impl Grid {
    pub fn new(rows: u16, cols: u16) -> Grid {
        let (rows, cols) = (rows.max(1), cols.max(1));
        Grid {
            rows,
            cols,
            cells: vec![GridCell::blank(CellStyle::default()); rows as usize * cols as usize],
            cx: 0,
            cy: 0,
            style: CellStyle::default(),
            saved: None,
            alt: None,
            scrollback: VecDeque::new(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            wrap_next: false,
            cursor_visible: true,
        }
    }

    // --- read side (the renderer) ---

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    pub fn cursor(&self) -> (u16, u16) {
        (self.cy, self.cx)
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// The cell at `(row, col)` as `(text, style)`, or `None` for a continuation cell or out of bounds — the
    /// painter skips those. Blank cells return an empty string (paint a space with the cell's background).
    pub fn cell(&self, row: u16, col: u16) -> Option<(&str, &CellStyle)> {
        let c = self.at(row, col)?;
        if c.continuation {
            return None;
        }
        Some((c.text.as_str(), &c.style))
    }

    fn at(&self, row: u16, col: u16) -> Option<&GridCell> {
        (row < self.rows && col < self.cols).then(|| &self.cells[self.idx(row, col)])
    }

    fn idx(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    // --- resize (window geometry / PTY winsize change) ---

    /// Reallocate to `rows × cols`, preserving the top-left overlap and clamping the cursor. Content that no
    /// longer fits is dropped (no reflow — a common simplification; slice 2b can reflow).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.cells = remap(&self.cells, self.rows, self.cols, rows, cols);
        if let Some(alt) = self.alt.as_mut() {
            alt.cells = remap(&alt.cells, self.rows, self.cols, rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.wrap_next = false;
    }

    // --- write side (VT actions) ---

    fn write_char(&mut self, c: char, w: u16) {
        if self.cx >= self.cols {
            return;
        }
        let i = self.idx(self.cy, self.cx);
        self.cells[i] = GridCell {
            text: c.to_string(),
            style: self.style,
            continuation: false,
        };
        if w == 2 && self.cx + 1 < self.cols {
            let j = self.idx(self.cy, self.cx + 1);
            self.cells[j] = GridCell {
                text: String::new(),
                style: self.style,
                continuation: true,
            };
        }
    }

    fn carriage_return(&mut self) {
        self.cx = 0;
        self.wrap_next = false;
    }

    fn line_feed(&mut self) {
        if self.cy == self.scroll_bottom {
            self.scroll_region_up(1); // at the bottom margin: scroll the region
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    /// Set all cells of row `r` to a blank carrying the current style.
    fn blank_row(&mut self, r: u16) {
        for c in 0..self.cols {
            let i = self.idx(r, c);
            self.cells[i] = GridCell::blank(self.style);
        }
    }

    /// Copy row `src` onto row `dst` (used by the region-scroll / insert-line shifts).
    fn copy_row(&mut self, src: u16, dst: u16) {
        for c in 0..self.cols {
            let (s, d) = (self.idx(src, c), self.idx(dst, c));
            self.cells[d] = self.cells[s].clone();
        }
    }

    /// Scroll the scroll region up by `n` rows: rows move toward the top margin, blank rows fill the bottom.
    /// When the region is the full screen (main buffer), the departing top rows go to scrollback.
    fn scroll_region_up(&mut self, n: u16) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        let full = top == 0 && bottom == self.rows - 1 && self.alt.is_none();
        for _ in 0..n {
            if full {
                let row: Vec<GridCell> = (0..self.cols)
                    .map(|c| self.cells[self.idx(top, c)].clone())
                    .collect();
                self.scrollback.push_back(row);
                while self.scrollback.len() > SCROLLBACK_MAX {
                    self.scrollback.pop_front();
                }
            }
            for r in top..bottom {
                self.copy_row(r + 1, r);
            }
            self.blank_row(bottom);
        }
    }

    /// Scroll the scroll region down by `n` rows: rows move toward the bottom margin, blank rows fill the top.
    fn scroll_region_down(&mut self, n: u16) {
        let (top, bottom) = (self.scroll_top, self.scroll_bottom);
        for _ in 0..n {
            let mut r = bottom;
            while r > top {
                self.copy_row(r - 1, r);
                r -= 1;
            }
            self.blank_row(top);
        }
    }

    fn reverse_index(&mut self) {
        if self.cy == self.scroll_top {
            self.scroll_region_down(1); // at the top margin: scroll the region down
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    fn tab(&mut self) {
        let next = (self.cx / 8 + 1) * 8;
        self.cx = next.min(self.cols - 1);
    }

    /// Erase the cells `[from, to)` on row `row` to a blank carrying the current style.
    fn erase_span(&mut self, row: u16, from: u16, to: u16) {
        for col in from..to.min(self.cols) {
            let i = self.idx(row, col);
            self.cells[i] = GridCell::blank(self.style);
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        match mode {
            0 => self.erase_span(self.cy, self.cx, self.cols), // cursor → EOL
            1 => self.erase_span(self.cy, 0, self.cx + 1),     // BOL → cursor
            2 => self.erase_span(self.cy, 0, self.cols),       // whole line
            _ => {}
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.erase_span(self.cy, self.cx, self.cols);
                for r in self.cy + 1..self.rows {
                    self.erase_span(r, 0, self.cols);
                }
            }
            1 => {
                for r in 0..self.cy {
                    self.erase_span(r, 0, self.cols);
                }
                self.erase_span(self.cy, 0, self.cx + 1);
            }
            2 | 3 => {
                for r in 0..self.rows {
                    self.erase_span(r, 0, self.cols);
                }
            }
            _ => {}
        }
    }

    /// `IL` — insert `n` blank lines at the cursor row, pushing the rows below down within the scroll region
    /// (rows shoved past the bottom margin are lost). A no-op when the cursor is outside the region.
    fn insert_lines(&mut self, n: u16) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - self.cy + 1);
        for _ in 0..n {
            let mut r = self.scroll_bottom;
            while r > self.cy {
                self.copy_row(r - 1, r);
                r -= 1;
            }
            self.blank_row(self.cy);
        }
    }

    /// `DL` — delete `n` lines at the cursor row, pulling the rows below up within the scroll region; blank
    /// rows fill the bottom of the region.
    fn delete_lines(&mut self, n: u16) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bottom {
            return;
        }
        let n = n.min(self.scroll_bottom - self.cy + 1);
        for _ in 0..n {
            for r in self.cy..self.scroll_bottom {
                self.copy_row(r + 1, r);
            }
            self.blank_row(self.scroll_bottom);
        }
    }

    /// `ICH` — insert `n` blank cells at the cursor, shifting the rest of the line right (cells past the right
    /// edge are lost).
    fn insert_chars(&mut self, n: u16) {
        let n = n.min(self.cols - self.cx);
        let row = self.cy;
        let mut c = self.cols - 1;
        while c >= self.cx + n {
            let (src, dst) = (self.idx(row, c - n), self.idx(row, c));
            self.cells[dst] = self.cells[src].clone();
            c -= 1;
        }
        for c in self.cx..self.cx + n {
            let i = self.idx(row, c);
            self.cells[i] = GridCell::blank(self.style);
        }
    }

    /// `DCH` — delete `n` cells at the cursor, shifting the rest of the line left; blanks fill the right edge.
    fn delete_chars(&mut self, n: u16) {
        let n = n.min(self.cols - self.cx);
        let row = self.cy;
        for c in self.cx..self.cols - n {
            let (src, dst) = (self.idx(row, c + n), self.idx(row, c));
            self.cells[dst] = self.cells[src].clone();
        }
        self.erase_span(row, self.cols - n, self.cols);
    }

    fn enter_alt_screen(&mut self) {
        if self.alt.is_some() {
            return;
        }
        let blank = GridCell::blank(CellStyle::default());
        let cells = std::mem::replace(
            &mut self.cells,
            vec![blank; self.rows as usize * self.cols as usize],
        );
        self.alt = Some(Box::new(AltState {
            cells,
            cx: self.cx,
            cy: self.cy,
            style: self.style,
            saved: self.saved,
        }));
        self.cx = 0;
        self.cy = 0;
        self.style = CellStyle::default();
    }

    fn leave_alt_screen(&mut self) {
        if let Some(alt) = self.alt.take() {
            self.cells = alt.cells;
            self.cx = alt.cx.min(self.cols - 1);
            self.cy = alt.cy.min(self.rows - 1);
            self.style = alt.style;
            self.saved = alt.saved;
        }
    }

    fn full_reset(&mut self) {
        self.alt = None;
        self.style = CellStyle::default();
        self.saved = None;
        self.cx = 0;
        self.cy = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.wrap_next = false;
        self.cursor_visible = true;
        for cell in &mut self.cells {
            *cell = GridCell::blank(CellStyle::default());
        }
    }

    fn apply_sgr(&mut self, ps: &[u16]) {
        if ps.is_empty() {
            self.style = CellStyle::default();
            return;
        }
        let mut i = 0;
        while i < ps.len() {
            match ps[i] {
                0 => self.style = CellStyle::default(),
                1 => self.style.bold = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.reverse = true,
                22 => self.style.bold = false,
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                30..=37 => self.style.fg = Color::AnsiValue((ps[i] - 30) as u8),
                39 => self.style.fg = Color::Reset,
                40..=47 => self.style.bg = Color::AnsiValue((ps[i] - 40) as u8),
                49 => self.style.bg = Color::Reset,
                90..=97 => self.style.fg = Color::AnsiValue((ps[i] - 90 + 8) as u8),
                100..=107 => self.style.bg = Color::AnsiValue((ps[i] - 100 + 8) as u8),
                38 => {
                    let (color, adv) = ext_color(&ps[i..]);
                    self.style.fg = color;
                    i += adv;
                    continue;
                }
                48 => {
                    let (color, adv) = ext_color(&ps[i..]);
                    self.style.bg = color;
                    i += adv;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Copy the top-left overlap of an `old_rows × old_cols` grid into a fresh `rows × cols` one (blanks fill
/// the rest). No reflow — good enough for slice 2.
fn remap(old: &[GridCell], old_rows: u16, old_cols: u16, rows: u16, cols: u16) -> Vec<GridCell> {
    let mut out = vec![GridCell::blank(CellStyle::default()); rows as usize * cols as usize];
    for r in 0..rows.min(old_rows) {
        for c in 0..cols.min(old_cols) {
            out[r as usize * cols as usize + c as usize] =
                old[r as usize * old_cols as usize + c as usize].clone();
        }
    }
    out
}

/// Parse an extended-colour SGR at `ps[0] == 38|48`: `;5;n` → 256-colour, `;2;r;g;b` → truecolor. Returns the
/// colour and how many params it consumed (so the SGR loop can skip them). Handles both `;` and `:` forms
/// (the parser flattens subparams), tolerating short/garbled sequences.
fn ext_color(ps: &[u16]) -> (Color, usize) {
    match ps.get(1).copied() {
        Some(5) => (
            Color::AnsiValue(ps.get(2).copied().unwrap_or(0) as u8),
            3.min(ps.len()),
        ),
        Some(2) => (
            Color::Rgb {
                r: ps.get(2).copied().unwrap_or(0) as u8,
                g: ps.get(3).copied().unwrap_or(0) as u8,
                b: ps.get(4).copied().unwrap_or(0) as u8,
            },
            5.min(ps.len()),
        ),
        _ => (Color::Reset, 1),
    }
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        let w = UnicodeWidthChar::width(c).unwrap_or(0) as u16;
        if w == 0 {
            // A combining mark: append to the last written cell if there is one.
            if self.cx > 0 {
                let i = self.idx(self.cy, self.cx - 1);
                self.cells[i].text.push(c);
            }
            return;
        }
        if self.wrap_next {
            self.carriage_return();
            self.line_feed();
            self.wrap_next = false;
        }
        if w == 2 && self.cx + 1 >= self.cols {
            self.carriage_return();
            self.line_feed();
        }
        self.write_char(c, w);
        self.cx += w;
        if self.cx >= self.cols {
            self.cx = self.cols - 1;
            self.wrap_next = true;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0a..=0x0c => self.line_feed(), // LF / VT / FF
            0x0d => self.carriage_return(),
            0x09 => self.tab(),
            0x08 => self.cx = self.cx.saturating_sub(1),
            _ => {} // BEL and other C0: ignore
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let ps: Vec<u16> = params.iter().flatten().copied().collect();
        let first = ps.first().copied().unwrap_or(0);
        let n = first.max(1);
        let private = intermediates.first() == Some(&b'?');
        match action {
            'H' | 'f' => {
                let row = ps.first().copied().unwrap_or(1).max(1);
                let col = ps.get(1).copied().unwrap_or(1).max(1);
                self.cy = (row - 1).min(self.rows - 1);
                self.cx = (col - 1).min(self.cols - 1);
                self.wrap_next = false;
            }
            'A' => self.cy = self.cy.saturating_sub(n),
            'B' => self.cy = (self.cy + n).min(self.rows - 1),
            'C' => self.cx = (self.cx + n).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(n),
            'G' => self.cx = (n - 1).min(self.cols - 1),
            'd' => self.cy = (n - 1).min(self.rows - 1),
            'J' => self.erase_in_display(first),
            'K' => self.erase_in_line(first),
            'L' => self.insert_lines(n),
            'M' => self.delete_lines(n),
            '@' => self.insert_chars(n),
            'P' => self.delete_chars(n),
            'X' => self.erase_span(self.cy, self.cx, self.cx + n), // ECH: erase n cells in place
            'r' => {
                // DECSTBM: set the scroll region `[top, bottom]` (1-based, inclusive) and home the cursor.
                let top = ps.first().copied().unwrap_or(1).max(1) - 1;
                let bottom = ps.get(1).copied().filter(|&b| b != 0).unwrap_or(self.rows) - 1;
                if top < bottom && bottom < self.rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.cx = 0;
                    self.cy = 0;
                }
            }
            'm' => self.apply_sgr(&ps),
            's' => self.saved = Some((self.cy, self.cx)),
            'u' => {
                if let Some((r, c)) = self.saved {
                    self.cy = r.min(self.rows - 1);
                    self.cx = c.min(self.cols - 1);
                }
            }
            'h' if private => match first {
                1049 | 1047 | 47 => self.enter_alt_screen(),
                25 => self.cursor_visible = true,
                _ => {}
            },
            'l' if private => match first {
                1049 | 1047 | 47 => self.leave_alt_screen(),
                25 => self.cursor_visible = false,
                _ => {}
            },
            _ => {} // mouse modes, DA, etc. — later
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'M' => self.reverse_index(),
            b'7' => self.saved = Some((self.cy, self.cx)),
            b'8' => {
                if let Some((r, c)) = self.saved {
                    self.cy = r.min(self.rows - 1);
                    self.cx = c.min(self.cols - 1);
                }
            }
            b'c' => self.full_reset(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;

    fn feed(grid: &mut Grid, bytes: &[u8]) {
        let mut parser = Parser::new();
        parser.advance(grid, bytes);
    }

    fn row_text(grid: &Grid, row: u16) -> String {
        (0..grid.cols)
            .filter_map(|c| {
                grid.cell(row, c)
                    .map(|(t, _)| if t.is_empty() { " " } else { t })
            })
            .collect()
    }

    #[test]
    fn prints_and_wraps_and_line_feeds() {
        let mut g = Grid::new(3, 4);
        feed(&mut g, b"abcdef"); // 4 cols: "abcd" then wrap "ef"
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!(row_text(&g, 1).trim_end(), "ef");
        feed(&mut g, b"\r\nX"); // CR+LF to row 2, then X
        assert_eq!(row_text(&g, 2).chars().next(), Some('X'));
    }

    #[test]
    fn cup_positions_the_cursor() {
        let mut g = Grid::new(5, 10);
        feed(&mut g, b"\x1b[3;5Hhi"); // row 3, col 5 (1-based) → (2,4)
        assert_eq!(g.cursor(), (2, 6)); // after writing "hi"
        assert_eq!(&row_text(&g, 2)[4..6], "hi");
    }

    #[test]
    fn sgr_sets_color_and_bold() {
        let mut g = Grid::new(2, 8);
        feed(&mut g, b"\x1b[1;31mR\x1b[0mn");
        let (_, style) = g.cell(0, 0).unwrap();
        assert!(style.bold && style.fg == Color::AnsiValue(1));
        let (_, plain) = g.cell(0, 1).unwrap();
        assert!(!plain.bold && plain.fg == Color::Reset);
    }

    #[test]
    fn truecolor_fg() {
        let mut g = Grid::new(2, 4);
        feed(&mut g, b"\x1b[38;2;10;20;30mX");
        let (_, style) = g.cell(0, 0).unwrap();
        assert_eq!(
            style.fg,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        );
    }

    #[test]
    fn erase_line_clears_to_end() {
        let mut g = Grid::new(2, 6);
        feed(&mut g, b"abcdef\x1b[1;3H\x1b[K"); // home-ish to col 3, erase to EOL
        assert_eq!(row_text(&g, 0), "ab    ");
    }

    #[test]
    fn alt_screen_isolates_then_restores() {
        let mut g = Grid::new(2, 5);
        feed(&mut g, b"main");
        feed(&mut g, b"\x1b[?1049h"); // enter alt: cleared
        assert_eq!(row_text(&g, 0).trim_end(), "");
        feed(&mut g, b"alt");
        feed(&mut g, b"\x1b[?1049l"); // leave alt: main restored
        assert_eq!(row_text(&g, 0).trim_end(), "main");
    }

    #[test]
    fn scroll_pushes_top_row_to_scrollback() {
        let mut g = Grid::new(2, 3);
        feed(&mut g, b"aaa\r\nbbb\r\nccc"); // 3rd line forces a scroll
        assert_eq!(g.scrollback.len(), 1);
        assert_eq!(row_text(&g, 0), "bbb");
        assert_eq!(row_text(&g, 1), "ccc");
    }

    #[test]
    fn scroll_region_confines_the_scroll() {
        let mut g = Grid::new(4, 4);
        feed(&mut g, b"\x1b[2;3r"); // region = rows 2..3 (1-based) → 0-based [1,2]
        feed(&mut g, b"\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc\x1b[4;1Hd");
        feed(&mut g, b"\x1b[3;1H\n"); // cursor to the bottom margin, LF scrolls only the region
        assert_eq!(row_text(&g, 0).trim_end(), "a"); // above the region: untouched
        assert_eq!(row_text(&g, 1).trim_end(), "c"); // region scrolled up
        assert_eq!(row_text(&g, 2).trim_end(), ""); // fresh blank at the region bottom
        assert_eq!(row_text(&g, 3).trim_end(), "d"); // below the region: untouched
    }

    #[test]
    fn insert_and_delete_lines() {
        let mut g = Grid::new(4, 4);
        feed(&mut g, b"\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc\x1b[4;1Hd");
        feed(&mut g, b"\x1b[2;1H\x1b[L"); // cursor row 2, insert a blank line
        assert_eq!(row_text(&g, 1).trim_end(), "");
        assert_eq!(row_text(&g, 2).trim_end(), "b");
        feed(&mut g, b"\x1b[2;1H\x1b[M"); // delete it → b pulls back up
        assert_eq!(row_text(&g, 1).trim_end(), "b");
    }

    #[test]
    fn insert_and_delete_chars() {
        let mut g = Grid::new(1, 6);
        feed(&mut g, b"abcdef\x1b[1;3H\x1b[@"); // cursor col 3, insert a blank
        assert_eq!(row_text(&g, 0), "ab cde");
        feed(&mut g, b"\x1b[1;3H\x1b[P"); // delete a char
        assert_eq!(row_text(&g, 0), "abcde ");
    }

    #[test]
    fn erase_chars_in_place() {
        let mut g = Grid::new(1, 6);
        feed(&mut g, b"abcdef\x1b[1;2H\x1b[3X"); // erase 3 cells from col 2
        assert_eq!(row_text(&g, 0), "a   ef");
    }

    #[test]
    fn resize_clamps_cursor_and_keeps_overlap() {
        let mut g = Grid::new(4, 8);
        feed(&mut g, b"\x1b[4;8Hhi"); // cursor near the far corner
        g.resize(2, 4);
        let (r, c) = g.cursor();
        assert!(r < 2 && c < 4, "cursor clamped into the new size");
    }
}
