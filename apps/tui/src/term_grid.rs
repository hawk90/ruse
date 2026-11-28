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

/// Combining marks kept per cell; further marks are dropped (#780). xterm keeps 2 by default (up to 10 with
/// `combiningChars`); 8 covers stacked diacritics and Hebrew points with cantillation and still bounds a cell.
const COMBINING_MAX: usize = 8;

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
    /// The child turned bracketed paste on (`CSI ? 2004 h`): a paste forwarded to it is wrapped in
    /// `CSI 200 ~` / `CSI 201 ~` (#600).
    bracketed_paste: bool,
    /// [`ChunkParser`] is feeding a lone 0x80..=0x9F byte (not a UTF-8 encoded U+0080..=U+009F). vte
    /// executes such a byte as an 8-bit C1 control; xterm in UTF-8 mode does not take 8-bit C1 and prints
    /// U+FFFD for the malformed byte, so [`Grid::execute`] prints that while this is set (#1750).
    lone_c1: bool,
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
            bracketed_paste: false,
            lone_c1: false,
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

    /// Whether the child enabled bracketed paste (DEC private mode 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
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

    /// Before cell `(row, col)` is overwritten: if it is one half of a wide glyph, blank the other half, so the
    /// glyph does not survive as a stray half (#782).
    fn split_wide(&mut self, row: u16, col: u16) {
        let i = self.idx(row, col);
        if self.cells[i].continuation && col > 0 {
            let left = i - 1;
            self.cells[left] = GridCell::blank(self.cells[left].style);
        }
        if col + 1 < self.cols && self.cells[i + 1].continuation {
            self.cells[i + 1] = GridCell::blank(self.cells[i + 1].style);
        }
    }

    fn write_char(&mut self, c: char, w: u16) {
        if self.cx >= self.cols {
            return;
        }
        self.split_wide(self.cy, self.cx);
        if w == 2 && self.cx + 1 < self.cols {
            self.split_wide(self.cy, self.cx + 1);
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

    /// Join a zero-width (combining) char to the last written glyph (#780). With a wrap pending the cursor
    /// still sits ON that glyph (the last column); otherwise it is one left of the cursor. A continuation
    /// cell means the glyph is wide, and the painter skips continuations, so the mark goes on its left half.
    /// No glyph to the left (column 0, no wrap pending) drops the mark. At most [`COMBINING_MAX`] marks are
    /// kept per cell, so a flood of marks cannot grow one cell without bound.
    fn combine(&mut self, c: char) {
        let mut col = if self.wrap_next {
            self.cx
        } else if self.cx > 0 {
            self.cx - 1
        } else {
            return;
        };
        if self.at(self.cy, col).is_some_and(|cell| cell.continuation) && col > 0 {
            col -= 1;
        }
        let i = self.idx(self.cy, col);
        let cell = &mut self.cells[i];
        if cell.text.chars().count() <= COMBINING_MAX {
            cell.text.push(c);
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
            2 => {
                for r in 0..self.rows {
                    self.erase_span(r, 0, self.cols);
                }
            }
            3 => self.scrollback.clear(), // xterm "erase saved lines": the screen stays (#781)
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
        self.bracketed_paste = false;
        for cell in &mut self.cells {
            *cell = GridCell::blank(CellStyle::default());
        }
    }

    /// Apply an SGR. `groups` holds one slice per `;`-separated parameter; a slice longer than one carries
    /// `:` subparameters (ITU T.416). A colon group is one self-contained attribute (`38:2::R:G:B`, `38:5:N`,
    /// `4:3`), so its subparameters must never be read as SGR codes of their own (#779); the legacy `;` form
    /// of 38/48 instead spends the parameters that follow it.
    fn apply_sgr(&mut self, groups: &[&[u16]]) {
        if groups.is_empty() {
            self.style = CellStyle::default();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let code = group.first().copied().unwrap_or(0);
            if group.len() > 1 {
                match code {
                    // `4:0` is "no underline"; `4:1`..`4:5` (single/double/curly/dotted/dashed) all draw as the
                    // cell model's one underline.
                    4 => self.style.underline = group[1] != 0,
                    38 => self.style.fg = colon_color(group).unwrap_or(self.style.fg),
                    48 => self.style.bg = colon_color(group).unwrap_or(self.style.bg),
                    _ => {} // 58 (underline colour) has no cell field; other subparameters are ignored
                }
                i += 1;
                continue;
            }
            match code {
                0 => self.style = CellStyle::default(),
                1 => self.style.bold = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.reverse = true,
                22 => self.style.bold = false,
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                30..=37 => self.style.fg = Color::AnsiValue((code - 30) as u8),
                39 => self.style.fg = Color::Reset,
                40..=47 => self.style.bg = Color::AnsiValue((code - 40) as u8),
                49 => self.style.bg = Color::Reset,
                90..=97 => self.style.fg = Color::AnsiValue((code - 90 + 8) as u8),
                100..=107 => self.style.bg = Color::AnsiValue((code - 100 + 8) as u8),
                // 58 is the underline colour (VTE, kitty): it takes its parameters as 38/48 do, so they are
                // not read as SGR codes of their own (#1742); the cell model has no field for the colour.
                38 | 48 | 58 => {
                    let ps: Vec<u16> = groups[i..]
                        .iter()
                        .map(|g| g.first().copied().unwrap_or(0))
                        .collect();
                    let (color, adv) = ext_color(&ps);
                    match code {
                        38 => self.style.fg = color,
                        48 => self.style.bg = color,
                        _ => {}
                    }
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

/// Parse a legacy `;`-form extended-colour SGR at `ps[0] == 38|48|58`: `;5;n` → 256-colour, `;2;r;g;b` →
/// truecolor. Returns the colour and how many params it consumed (so the SGR loop can skip them), tolerating
/// short/garbled sequences. The `:` form is parsed by [`colon_color`].
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

/// Parse a `:`-form extended colour: `group == [38|48, 5, n]` or `[38|48, 2, (colorspace,) r, g, b]`. The
/// colour-space id is optional in practice (`38:2::R:G:B` per T.416, `38:2:R:G:B` as many programs emit it);
/// six or more elements mean it is present. `None` for an unknown or truncated form (the colour is kept).
fn colon_color(group: &[u16]) -> Option<Color> {
    match group.get(1).copied()? {
        5 => Some(Color::AnsiValue(*group.get(2)? as u8)),
        2 => {
            let rgb = if group.len() >= 6 {
                &group[3..6]
            } else {
                group.get(2..5)?
            };
            Some(Color::Rgb {
                r: rgb[0] as u8,
                g: rgb[1] as u8,
                b: rgb[2] as u8,
            })
        }
        _ => None,
    }
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        let w = UnicodeWidthChar::width(c).unwrap_or(0) as u16;
        if w == 0 {
            self.combine(c);
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
        if self.lone_c1 {
            self.print('\u{fffd}');
            return;
        }
        match byte {
            0x0a..=0x0c => self.line_feed(), // LF / VT / FF
            0x0d => self.carriage_return(),
            0x09 => self.tab(),
            0x08 => {
                self.cx = self.cx.saturating_sub(1);
                self.wrap_next = false;
            }
            _ => {} // BEL and other C0: ignore
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // The parser collects a private marker (`?`, `>`, `=`, `<`) and intermediate bytes (` `, `!`, `$`, …)
        // into `intermediates`. The only such form handled is `CSI ? … h|l` (DECSET/DECRST); any other is a
        // different control and must not run as the plain CSI — `CSI > 4 ; 2 m` (XTMODKEYS) is not SGR
        // underline, `CSI > 1 u` (kitty keyboard) is not restore-cursor (#776).
        if !intermediates.is_empty() && !(intermediates == b"?" && matches!(action, 'h' | 'l')) {
            return;
        }
        let ps: Vec<u16> = params.iter().flatten().copied().collect();
        let first = ps.first().copied().unwrap_or(0);
        let n = first.max(1);
        let private = intermediates.first() == Some(&b'?');
        // Mode 2004 may share a DECSET/DECRST with other modes (`CSI ? 1049 ; 2004 h`).
        if private && matches!(action, 'h' | 'l') && ps.contains(&2004) {
            self.bracketed_paste = action == 'h';
        }
        // Any cursor movement cancels a pending wrap (xterm, VT100): `abcd` at the right margin then `CHA 2`
        // must overwrite column 2, not wrap first (#775). DECSTBM homes the cursor, so it counts too.
        if matches!(action, 'A' | 'B' | 'C' | 'D' | 'G' | 'd' | 'r' | 'u') {
            self.wrap_next = false;
        }
        match action {
            'H' | 'f' => {
                let row = ps.first().copied().unwrap_or(1).max(1);
                let col = ps.get(1).copied().unwrap_or(1).max(1);
                self.cy = (row - 1).min(self.rows - 1);
                self.cx = (col - 1).min(self.cols - 1);
                self.wrap_next = false;
            }
            'A' => self.cy = self.cy.saturating_sub(n),
            'B' => self.cy = self.cy.saturating_add(n).min(self.rows - 1),
            'C' => self.cx = self.cx.saturating_add(n).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(n),
            'G' => self.cx = (n - 1).min(self.cols - 1),
            'd' => self.cy = (n - 1).min(self.rows - 1),
            'J' => self.erase_in_display(first),
            'K' => self.erase_in_line(first),
            'L' => self.insert_lines(n),
            'M' => self.delete_lines(n),
            '@' => self.insert_chars(n),
            'P' => self.delete_chars(n),
            'X' => self.erase_span(self.cy, self.cx, self.cx.saturating_add(n)), // ECH: erase n cells in place
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
            'm' => self.apply_sgr(&params.iter().collect::<Vec<_>>()),
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
                self.wrap_next = false;
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

/// The `vte` parser fed so that a PTY read boundary cannot change the grid (#1765). Feeding a stream in
/// pieces must give the grid one feed gives, since a read can end anywhere.
///
/// vte 0.15 breaks that when a piece ends inside a UTF-8 sequence. It carries the partial sequence into the
/// next `advance` and completes it from a 4-byte window. Two things go wrong there. When the window does
/// not end on a char boundary, every byte in it after the char is skipped (`é` cut after `C3`, then
/// `A9 0A E4 B8 AD`, loses the LF before `中`). A completed U+0080..=U+009F is also printed, where one feed
/// executes it as C1.
///
/// So this parser never ends an `advance` call inside a UTF-8 sequence. It keeps an incomplete trailing
/// sequence (a lead byte and fewer continuation bytes than it needs, at most 3 bytes) and puts it in front
/// of the next piece. vte's partial path is then never taken. Holding bytes back is safe in any parser
/// state: vte sees the same bytes in the same order. A stream that ends inside a sequence prints nothing
/// for it either way.
///
/// It also splits each piece around a lone 0x80..=0x9F byte and marks the grid while vte takes that byte
/// alone, so the grid can tell it from an encoded U+0080..=U+009F and print U+FFFD for it (#1750). vte
/// executes either as a C1 control only in its ground state; in any other state the byte reaches no
/// `execute`, so the mark is inert there. Splitting at a UTF-8 chunk boundary does not change what vte does.
pub struct ChunkParser {
    parser: vte::Parser,
    carry: [u8; 3],
    carry_len: usize,
}

impl Default for ChunkParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkParser {
    pub fn new() -> ChunkParser {
        ChunkParser {
            parser: vte::Parser::new(),
            carry: [0; 3],
            carry_len: 0,
        }
    }

    /// Feed one piece of child output to `grid`.
    pub fn advance(&mut self, grid: &mut Grid, bytes: &[u8]) {
        if self.carry_len == 0 {
            self.advance_whole_chars(grid, bytes);
            return;
        }
        let mut joined = Vec::with_capacity(self.carry_len + bytes.len());
        joined.extend_from_slice(&self.carry[..self.carry_len]);
        joined.extend_from_slice(bytes);
        self.carry_len = 0;
        self.advance_whole_chars(grid, &joined);
    }

    /// Feed `bytes` without an incomplete trailing UTF-8 sequence, which is kept for the next call.
    fn advance_whole_chars(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let keep = incomplete_utf8_tail(bytes);
        let (now, later) = bytes.split_at(bytes.len() - keep);
        self.advance_marking_lone_c1(grid, now);
        self.carry[..keep].copy_from_slice(later);
        self.carry_len = keep;
    }

    /// Feed `bytes`, giving each lone 0x80..=0x9F byte to vte in an `advance` of its own with
    /// [`Grid::lone_c1`] set.
    fn advance_marking_lone_c1(&mut self, grid: &mut Grid, bytes: &[u8]) {
        let mut start = 0;
        let mut pos = 0;
        for chunk in bytes.utf8_chunks() {
            pos += chunk.valid().len();
            let bad = chunk.invalid();
            if let [b @ 0x80..=0x9F] = bad {
                if start < pos {
                    self.parser.advance(grid, &bytes[start..pos]);
                }
                grid.lone_c1 = true;
                self.parser.advance(grid, std::slice::from_ref(b));
                grid.lone_c1 = false;
                start = pos + 1;
            }
            pos += bad.len();
        }
        if start < bytes.len() {
            self.parser.advance(grid, &bytes[start..]);
        }
    }
}

/// How many trailing bytes of `bytes` are an incomplete UTF-8 sequence: a lead byte followed only by
/// continuation bytes, still a valid prefix and short of its length. 0 when the tail is complete or invalid.
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for k in 1..=bytes.len().min(3) {
        let tail = &bytes[bytes.len() - k..];
        match tail[0] {
            0x80..=0xBF => continue, // a continuation byte: keep looking for the lead
            0xC0..=0xFF => {
                return match std::str::from_utf8(tail) {
                    Err(e) if e.valid_up_to() == 0 && e.error_len().is_none() => k,
                    _ => 0,
                };
            }
            _ => return 0,
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Parser;

    fn feed(grid: &mut Grid, bytes: &[u8]) {
        let mut parser = Parser::new();
        parser.advance(grid, bytes);
    }

    #[test]
    fn incomplete_utf8_tail_counts_only_a_valid_unfinished_sequence() {
        assert_eq!(incomplete_utf8_tail(b""), 0);
        assert_eq!(incomplete_utf8_tail(b"ab"), 0);
        assert_eq!(incomplete_utf8_tail("a\u{e9}".as_bytes()), 0);
        assert_eq!(incomplete_utf8_tail(b"a\xc3"), 1);
        assert_eq!(incomplete_utf8_tail(b"a\xe4\xb8"), 2);
        assert_eq!(incomplete_utf8_tail(b"\xf0\x9f\x98"), 3);
        assert_eq!(incomplete_utf8_tail(b"\x80"), 0, "a lone continuation");
        assert_eq!(incomplete_utf8_tail(b"\xff"), 0, "never a lead byte");
        assert_eq!(incomplete_utf8_tail(b"\xf0\x80"), 0, "not a valid prefix");
        assert_eq!(incomplete_utf8_tail(b"\xc3\x1b"), 0);
    }

    /// The vte defect `ChunkParser` works around (#1765): a char completed across `advance` calls swallows
    /// the bytes after it when its 4-byte lookahead does not end on a char boundary (here the LF before
    /// `中`). When this fails, a vte update fixed it and the carry in `ChunkParser` can go.
    #[test]
    fn raw_vte_drops_the_byte_after_a_split_char() {
        let bytes = "\u{e9}\n\u{4e2d}".as_bytes();
        let (mut whole, mut split) = (Grid::new(2, 4), Grid::new(2, 4));
        feed(&mut whole, bytes);
        let mut parser = Parser::new();
        parser.advance(&mut split, &bytes[..1]);
        parser.advance(&mut split, &bytes[1..]);
        assert_eq!(whole.cursor(), (1, 3));
        assert_eq!(split.cursor(), (0, 3), "vte skipped the LF");

        let mut fixed = Grid::new(2, 4);
        let mut parser = ChunkParser::new();
        parser.advance(&mut fixed, &bytes[..1]);
        parser.advance(&mut fixed, &bytes[1..]);
        assert_eq!(fixed.cursor(), whole.cursor());
        assert_eq!(row_text(&fixed, 1), row_text(&whole, 1));
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

    /// #779: colon-form SGR subparameters belong to their group, not to the SGR code list.
    #[test]
    fn sgr_colon_subparameters_stay_in_their_group() {
        let rgb = Color::Rgb {
            r: 10,
            g: 20,
            b: 30,
        };
        let style = |bytes: &str| {
            let mut g = Grid::new(1, 4);
            feed(&mut g, bytes.as_bytes());
            *g.cell(0, 0).unwrap().1
        };
        let s = style("\x1b[4:3mX"); // curly underline: not italic
        assert!(s.underline && !s.italic);
        let s = style("\x1b[1;4m\x1b[4:0mX"); // underline off: not a full reset
        assert!(s.bold && !s.underline);
        assert_eq!(style("\x1b[38:2::10:20:30mX").fg, rgb);
        assert_eq!(style("\x1b[38:2:10:20:30mX").fg, rgb); // colour-space id omitted
        assert_eq!(style("\x1b[48:2::10:20:30mX").bg, rgb);
        let s = style("\x1b[38:5:1mX"); // 256-colour index 1, not bold
        assert!(s.fg == Color::AnsiValue(1) && !s.bold);
        let s = style("\x1b[58:2::1:3:4mX"); // underline colour: ignored, no bold/italic/underline
        assert!(!s.bold && !s.italic && !s.underline);
        // The `;` form still spends the following parameters.
        let s = style("\x1b[38;5;1;1mX");
        assert!(s.fg == Color::AnsiValue(1) && s.bold);
        assert_eq!(style("\x1b[48;2;10;20;30mX").bg, rgb);
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

    /// #907 (found by `hostile_output_never_panics`): a maximal CUF/CUD/ECH count overflowed `u16` — a panic in
    /// debug builds and a wrapped (wrong, leftward) cursor in release. It clamps to the edge instead.
    #[test]
    fn huge_cursor_counts_clamp_to_the_edge() {
        let mut g = Grid::new(3, 5);
        feed(&mut g, b"ab\x1b[65535C");
        assert_eq!(g.cursor(), (0, 4));
        feed(&mut g, b"\x1b[65535B");
        assert_eq!(g.cursor(), (2, 4));
        feed(&mut g, b"\x1b[1;2Hxyz\x1b[1;2H\x1b[65535X");
        assert_eq!(row_text(&g, 0), "a    ");
    }

    /// #775: a pending wrap at the right margin is cancelled by any cursor movement, so the next printable
    /// lands where the cursor was moved to instead of wrapping onto the next row first.
    #[test]
    fn cursor_movement_cancels_the_pending_wrap() {
        for (moves, want) in [
            (&b"\x1b[2G"[..], "aXcd"),                   // CHA
            (b"\x08", "abXd"),                           // BS
            (b"\x1b[D", "abXd"),                         // CUB
            (b"\x1b[C", "abcX"),                         // CUF (already at the margin)
            (b"\x1b[1;1H\x1b7\x1b[1;4H\x1b8", "Xbcd"),   // DECRC
            (b"\x1b[1;1H\x1b[s\x1b[1;4H\x1b[u", "Xbcd"), // SCORC
            (b"\x1b[r", "Xbcd"),                         // DECSTBM homes the cursor
        ] {
            let mut g = Grid::new(2, 4);
            feed(&mut g, b"abcd");
            feed(&mut g, moves);
            feed(&mut g, b"X");
            assert_eq!(row_text(&g, 0), want, "after {moves:?}");
            assert_eq!(row_text(&g, 1), "    ", "after {moves:?}: no wrap");
        }
        // CUU / CUD / VPA on the last column: the next printable overwrites that column on the new row.
        for (moves, row) in [(&b"\x1b[A"[..], 0), (b"\x1b[B", 2), (b"\x1b[3d", 2)] {
            let mut g = Grid::new(3, 4);
            feed(&mut g, b"\x1b[2;1Habcd");
            feed(&mut g, moves);
            feed(&mut g, b"X");
            assert_eq!(g.cursor(), (row, 3), "after {moves:?}");
            assert_eq!(g.cell(row, 3).map(|(t, _)| t), Some("X"), "after {moves:?}");
        }
    }

    /// #776: a CSI with a private marker or intermediate the grid does not handle is ignored, instead of
    /// running as the plain CSI with the same final byte.
    #[test]
    fn unhandled_private_and_intermediate_csi_are_ignored() {
        let mut g = Grid::new(3, 6);
        feed(&mut g, b"\x1b[2;2H\x1b[s\x1b[1;1H"); // saved cursor (1,1), cursor home
        for seq in [
            &b"\x1b[>4;2m"[..], // XTMODKEYS, not SGR 4 (underline)
            b"\x1b[>1u",        // kitty keyboard push, not SCORC
            b"\x1b[=1u",        // kitty keyboard set, not SCORC
            b"\x1b[<u",         // kitty keyboard pop, not SCORC
            b"\x1b[?1s",        // XTSAVE, not SCOSC
            b"\x1b[?1r",        // XTRESTORE, not DECSTBM
            b"\x1b[?2J",        // DECSED, not ED
            b"\x1b[?2K",        // DECSEL, not EL
            b"\x1b[2 q",        // DECSCUSR, not a cursor move
            b"\x1b[!p",         // DECSTR, not handled
            b"\x1b[>c",         // secondary DA
        ] {
            feed(&mut g, b"\x1b[1;1Hab");
            feed(&mut g, seq);
            feed(&mut g, b"c");
            assert_eq!(row_text(&g, 0), "abc   ", "after {seq:?}");
            let (_, style) = g.cell(0, 2).unwrap();
            assert!(!style.underline, "after {seq:?}");
        }
        feed(&mut g, b"\x1b[u"); // the saved cursor is still (1,1): no marker form overwrote it
        assert_eq!(g.cursor(), (1, 1));
        feed(&mut g, b"\x1b[?25l\x1b[?2004h"); // DECRST / DECSET still apply
        assert!(!g.cursor_visible() && g.bracketed_paste());
    }

    /// #781: `CSI 3 J` erases the scrollback only; `CSI 2 J` erases the screen only.
    #[test]
    fn ed3_clears_only_the_scrollback() {
        let mut g = Grid::new(2, 3);
        feed(&mut g, b"aaa\r\nbbb\r\nccc"); // one row scrolled off
        assert_eq!(g.scrollback.len(), 1);
        feed(&mut g, b"\x1b[3J");
        assert!(g.scrollback.is_empty());
        assert_eq!(row_text(&g, 0), "bbb");
        assert_eq!(row_text(&g, 1), "ccc");
        feed(&mut g, b"\r\nddd\x1b[2J"); // scroll one more off, then erase the screen
        assert_eq!(g.scrollback.len(), 1);
        assert_eq!(row_text(&g, 0), "   ");
        assert_eq!(row_text(&g, 1), "   ");
    }

    /// #782: overwriting either half of a wide glyph blanks the other half.
    #[test]
    fn overwriting_half_a_wide_glyph_blanks_the_other_half() {
        for (bytes, want) in [
            ("한\x1b[1;2HX", " X   "),   // narrow onto the right half
            ("한\x1b[1;1HX", "X    "),   // narrow onto the left half
            ("a한\x1b[1;1H中", "中   "), // wide whose right half lands on a left half
            ("한\x1b[1;2H中", " 中  "),  // wide whose left half lands on a right half
        ] {
            let mut g = Grid::new(1, 5);
            feed(&mut g, bytes.as_bytes());
            assert_eq!(row_text(&g, 0), want, "after {bytes:?}");
        }
    }

    /// #780: a combining mark joins the glyph it follows: the last cell while a wrap is pending, the left
    /// half of a wide glyph (never its skipped continuation), and at most `COMBINING_MAX` marks per cell.
    #[test]
    fn combining_marks_join_the_glyph_they_follow() {
        let text = |g: &Grid, col: u16| {
            g.cell(0, col)
                .map_or("<cont>".into(), |(t, _)| t.to_string())
        };
        let mut g = Grid::new(2, 3);
        feed(&mut g, "abc\u{301}".as_bytes()); // `c` is at the last column, wrap pending
        assert_eq!(text(&g, 1), "b");
        assert_eq!(text(&g, 2), "c\u{301}");
        feed(&mut g, b"d"); // the pending wrap is untouched by the mark
        assert_eq!(row_text(&g, 1).trim_end(), "d");

        let mut g = Grid::new(1, 5);
        feed(&mut g, "한\u{302}".as_bytes()); // after a wide glyph mid-line
        assert_eq!(text(&g, 0), "한\u{302}");
        assert_eq!(text(&g, 1), "<cont>");

        let mut g = Grid::new(2, 4);
        feed(&mut g, "ab한\u{302}".as_bytes()); // a wide glyph ending at the last column
        assert_eq!(text(&g, 2), "한\u{302}");

        let mut g = Grid::new(1, 4);
        feed(&mut g, "\u{301}a".as_bytes()); // nothing to the left: dropped
        assert_eq!(text(&g, 0), "a");

        let mut g = Grid::new(1, 4);
        feed(&mut g, format!("e{}", "\u{301}".repeat(1000)).as_bytes());
        assert_eq!(text(&g, 0).chars().count(), 1 + COMBINING_MAX);
    }

    /// One piece of hostile terminal output: a CSI/ESC sequence with arbitrary (often huge) parameters, a
    /// wide or combining character, a C0 control, or raw bytes.
    fn chunk() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
        use proptest::prelude::*;
        let param = prop_oneof![
            Just(String::new()),
            (0u32..12).prop_map(|n| n.to_string()),
            Just("65535".to_string()),
            Just("99999".to_string()),
            any::<u16>().prop_map(|n| n.to_string()),
        ];
        let csi = (
            prop_oneof![Just(""), Just("?")],
            proptest::collection::vec(param, 0..6),
            proptest::sample::select(b"HfABCDGdJKLM@PXrmsuhlST".to_vec()),
        )
            .prop_map(|(lead, ps, act)| {
                let mut b = format!("\x1b[{lead}{}", ps.join(";")).into_bytes();
                b.push(act);
                b
            });
        prop_oneof![
            csi,
            proptest::sample::select(b"M78cDE".to_vec()).prop_map(|c| vec![0x1b, c]),
            Just(b"\x1b[?1049h".to_vec()),
            Just(b"\x1b[?1049l".to_vec()),
            proptest::sample::select(vec![
                "\u{4e2d}",
                "\u{301}",
                "\u{1f600}",
                "a",
                "\t",
                "\r\n",
                "\x08"
            ])
            .prop_map(|s| s.as_bytes().to_vec()),
            proptest::collection::vec(any::<u8>(), 0..24),
        ]
    }

    proptest::proptest! {
        #![proptest_config(crate::test_cases(512))]

        /// Arbitrary child-process output never panics the grid (a PTY child is untrusted), across resizes,
        /// and the cursor always stays inside the screen.
        #[test]
        fn hostile_output_never_panics(
            size in (1u16..12, 1u16..12),
            parts in proptest::collection::vec(
                (chunk(), proptest::option::of((0u16..14, 0u16..14))), 0..24),
        ) {
            let mut g = Grid::new(size.0, size.1);
            let mut parser = ChunkParser::new();
            for (bytes, resize) in parts {
                parser.advance(&mut g, &bytes);
                if let Some((r, c)) = resize {
                    g.resize(r, c);
                }
                let (rows, cols) = g.size();
                let (cy, cx) = g.cursor();
                proptest::prop_assert!(cy < rows && cx < cols, "cursor ({cy},{cx}) outside {rows}x{cols}");
                for r in 0..rows {
                    for c in 0..cols {
                        let _ = g.cell(r, c);
                    }
                }
            }
        }
    }
}
