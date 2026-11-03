//! The scroll commands over screen rows (#615), ported from nvim v0.12.6's `move.c`: `CTRL-E` / `CTRL-Y`
//! (`scroll_redraw`, `scrollup`, `scrolldown`), `CTRL-D` / `CTRL-U` / `CTRL-F` / `CTRL-B` (`pagescroll`,
//! `scroll_with_sms`, `get_scroll_overlap`), their `'scrolloff'` correction (`cursor_correct`,
//! `cursor_correct_sms`), and the top line's skipped columns a frame keeps (`curs_columns`, nvim's
//! `w_skipcol`): `'smoothscroll'` scrolls a wrapped line by rows, and a cursor line taller than the window
//! shows the rows around the cursor.
//!
//! Without `'smoothscroll'` nvim scrolls the page commands by ROWS too, with `'smoothscroll'` on for the
//! scroll and the top line then put back on a line boundary; that is why `CTRL-D` over a wrapped line
//! moves the view and the cursor by different amounts. The port keeps that.
//!
//! Pure over a window's laid-out lines ([`PaneLines`]); the session applies the result.

use crate::ui::window_view::{self, PaneLines, Resolved};
use crate::viewport::Lines;

/// Where a window's view and cursor are: the top line and its skipped virtual columns, and the cursor's
/// line, its byte in that line and the wanted column (`usize::MAX` after `$`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewPos {
    pub top: usize,
    pub skipcol: usize,
    pub line: usize,
    pub byte: usize,
    pub curswant: usize,
}

/// One window for a scroll command: its lines, height, `'scrolloff'` and `'smoothscroll'`.
pub(crate) struct Scroll<'a> {
    pub lines: &'a PaneLines<'a>,
    pub height: usize,
    pub so: usize,
    pub sms: bool,
    /// The cursor on a tab shows on its last cell (Normal mode without 'list').
    pub tab_end: bool,
    /// The last `(line, byte, virtual column)` asked for: the frame asks for the cursor's several times,
    /// and on a huge line each answer is a scan.
    pub memo: std::cell::Cell<Option<(usize, usize, usize)>>,
}

/// The result of a scroll command: the new position, whether it failed (nvim beeps), and whether nvim
/// left its top line marked valid (`VALID_TOPLINE`), so the next redraw keeps it as it is instead of
/// pulling it back to the cursor's `'scrolloff'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scrolled {
    pub pos: ViewPos,
    pub beep: bool,
    pub keep_top: bool,
}

impl Scroll<'_> {
    fn width(&self) -> usize {
        self.lines.win.width.max(1)
    }

    fn wrap(&self) -> bool {
        self.lines.win.wrap
    }

    fn count(&self) -> usize {
        self.lines.count()
    }

    fn last(&self) -> usize {
        self.count().saturating_sub(1)
    }

    fn fold(&self, l: usize) -> Option<(usize, usize)> {
        self.lines.fold(l)
    }

    fn fold_start(&self, l: usize) -> usize {
        self.fold(l).map_or(l, |(s, _)| s)
    }

    fn fold_end(&self, l: usize) -> usize {
        self.fold(l).map_or(l, |(_, e)| e)
    }

    /// nvim's `plines_win(.., true)`: the line's rows up to the window height, 1 for a closed fold.
    fn plines(&self, l: usize) -> usize {
        if self.fold(l).is_some() {
            1
        } else {
            self.lines.text_rows(l).clamp(1, self.height.max(1))
        }
    }

    /// The cursor line's rows (up to `cap`) from the cursor's virtual column when the rest of the line
    /// is printable ASCII under options that add nothing to a row: only the text after the cursor is
    /// read, so a frame on a huge line (a macro typing into it, one frame per key) scans no more than
    /// the window shows.
    fn rows_past_cursor(&self, p: &ViewPos, vcol: usize, cap: usize) -> Option<usize> {
        let win = self.lines.win;
        let simple = win.wrap
            && !win.linebreak
            && win.showbreak.is_empty()
            && win.breakindent.is_none()
            && win.list.is_none()
            && self.fold(p.line).is_none();
        if !simple {
            return None;
        }
        let text = self.lines.text;
        let s = text.start(p.line);
        let e = text.start(p.line + 1).max(s);
        let bytes = &text.bytes[s..e];
        let len = bytes.len() - usize::from(bytes.last() == Some(&b'\n'));
        let plain = |b: &u8| (0x20..0x7f).contains(b);
        // The cursor's own char, one cell, then the rest of the line.
        let from = if p.byte < len {
            plain(&bytes[p.byte]).then_some(p.byte + 1)?
        } else {
            len
        };
        let w = self.width();
        let crow = vcol / w;
        let more = cap.saturating_sub(crow);
        let probe = len.min(from + more.saturating_mul(w));
        if !bytes[from..probe].iter().all(plain) {
            return None;
        }
        if probe < len {
            return Some(cap);
        }
        let total = vcol + usize::from(p.byte < len) + (len - from);
        Some(total.div_ceil(w).clamp(1, cap.max(1)))
    }

    /// [`Scroll::rows_upto`] without laying out a long plain line (a huge count insert is one line the
    /// frame would otherwise lay out to the cursor every time): printable ASCII under options that add
    /// nothing to a row is `width` bytes a row.
    fn rows_capped(&self, l: usize, cap: usize) -> usize {
        let win = self.lines.win;
        let simple = win.wrap
            && !win.linebreak
            && win.showbreak.is_empty()
            && win.breakindent.is_none()
            && win.list.is_none()
            && self.fold(l).is_none();
        if simple {
            let text = self.lines.text;
            let s = text.start(l);
            let e = text.start(l + 1).max(s);
            let bytes = &text.bytes[s..e];
            let len = bytes.len() - usize::from(bytes.last() == Some(&b'\n'));
            let w = self.width();
            let probe = len.min(cap.saturating_mul(w));
            if bytes[..probe].iter().all(|b| (0x20..0x7f).contains(b)) {
                return if probe < len {
                    cap
                } else {
                    len.div_ceil(w).clamp(1, cap.max(1))
                };
            }
        }
        self.rows_upto(l, cap)
    }

    /// The line's rows up to `max` (nvim's `plines_win_full` without the height limit).
    fn rows_upto(&self, l: usize, max: usize) -> usize {
        if self.fold(l).is_some() {
            return 1;
        }
        if !self.wrap() {
            return 1;
        }
        let win = self.lines.win;
        let limit = max.saturating_add(1).saturating_mul(win.width);
        let text = self
            .lines
            .text
            .line_upto(l, window_view::PaneText::bytes_for(limit));
        crate::ui::line_layout::layout_upto(text, &win.layout(), &[], limit, usize::MAX)
            .rows
            .clamp(1, max.max(1))
    }

    /// nvim's `linetabsize_eol`: the line's virtual columns, and one for a `'listchars'` `eol`.
    fn size(&self, l: usize) -> usize {
        let win = self.lines.win;
        let total = win.lay(self.lines.text.line(l)).total;
        total + usize::from(win.list.as_ref().is_some_and(|lc| lc.eol.is_some()))
    }

    /// The cursor's virtual column on line `l` at line byte `byte` (nvim's `w_virtcol`).
    fn vcol(&self, l: usize, byte: usize) -> usize {
        if let Some((ml, mb, v)) = self.memo.get() {
            if (ml, mb) == (l, byte) {
                return v;
            }
        }
        let v = window_view::cursor_place(self.lines.win, self.lines.text, l, byte, self.tab_end).2;
        self.memo.set(Some((l, byte, v)));
        v
    }

    /// nvim's `coladvance(wcol)`: the line byte of the char holding virtual column `wcol`.
    fn coladvance(&self, l: usize, wcol: usize) -> usize {
        window_view::coladvance(&self.lines.win.lay(self.lines.text.line(l)), wcol, false)
    }

    /// Rows of the top line scrolled out (nvim's `adjust_plines_for_skipcol`).
    fn skip_rows(&self, skipcol: usize) -> usize {
        let w = self.width();
        if skipcol >= w {
            (skipcol - w) / w + 1
        } else {
            0
        }
    }

    /// nvim's `w_botline`: the first line below the last one that shows whole.
    fn botline(&self, p: &ViewPos) -> usize {
        let mut done = 0;
        let mut l = p.top;
        while l < self.count() {
            let n = if l == p.top {
                self.plines(l)
                    .saturating_sub(self.skip_rows(p.skipcol))
                    .max(1)
            } else {
                self.plines(l)
            };
            if done + n > self.height {
                break;
            }
            done += n;
            l = self.fold_end(l) + 1;
        }
        l
    }

    /// The cursor's screen row in the window (nvim's `w_wrow`).
    fn wrow(&self, p: &ViewPos) -> usize {
        let mut row = 0;
        let mut l = p.top;
        while l < p.line && l < self.count() {
            row += if l == p.top {
                self.plines(l).saturating_sub(self.skip_rows(p.skipcol))
            } else {
                self.plines(l)
            };
            l = self.fold_end(l) + 1;
        }
        if self.wrap() && self.fold(p.line).is_none() {
            let crow = self.vcol(p.line, p.byte) / self.width();
            row += if p.line == p.top {
                crow.saturating_sub(self.skip_rows(p.skipcol))
            } else {
                crow
            };
        }
        row
    }

    /// The cursor moved to another line: nvim's `coladvance(curswant)`.
    fn advance(&self, p: &mut ViewPos) {
        p.byte = self.coladvance(p.line, p.curswant);
    }

    /// nvim's `sms_marker_overlap`: the cells of the top row the `<<<` marker covers.
    fn marker_overlap(&self) -> usize {
        let win = self.lines.win;
        if !win.showbreak.is_empty() {
            return 0;
        }
        if win.list.as_ref().is_some_and(|lc| lc.precedes.is_some()) {
            return 1;
        }
        3usize.saturating_sub(win.gutter())
    }

    /// nvim's `scrollup` (`CTRL-E`): the view `count` lines down, by rows with `'smoothscroll'`.
    fn scrollup(&self, p: &mut ViewPos, count: usize, sms: bool) -> bool {
        let (top0, skip0) = (p.top, p.skipcol);
        let do_sms = self.wrap() && sms;
        let w = self.width();
        if do_sms || self.lines.any_fold() {
            let mut size = if do_sms { self.size(p.top) } else { 0 };
            let mut todo = count;
            while todo > 0 {
                let mut lnum = self.fold_end(p.top);
                if lnum == p.top && do_sms {
                    p.skipcol += w;
                    if p.skipcol >= size {
                        if lnum == self.last() {
                            p.skipcol -= w; // the last screen line: no further
                            break;
                        }
                        lnum += 1;
                    }
                } else {
                    if lnum >= self.last() {
                        break;
                    }
                    lnum += 1;
                }
                if lnum > p.top {
                    p.top = lnum;
                    p.skipcol = 0;
                    if todo > 1 && do_sms {
                        size = self.size(p.top);
                    }
                }
                todo -= 1;
            }
        } else {
            p.top += count;
        }
        p.top = self.fold_start(p.top.min(self.last()));
        if p.line < p.top {
            p.line = p.top;
            self.advance(p);
        }
        p.top != top0 || p.skipcol != skip0
    }

    /// nvim's `scrolldown` (`CTRL-Y`): the view `count` lines up, by rows with `'smoothscroll'`; the
    /// cursor moves up when its line would leave the window's bottom.
    fn scrolldown(&self, p: &mut ViewPos, count: usize, sms: bool) -> bool {
        let do_sms = self.wrap() && sms;
        let w = self.width();
        p.top = self.fold_start(p.top);
        let mut wrow = self.wrow(p);
        let mut done = 0;
        let mut todo = count;
        while todo > 0 {
            if p.top == 0 && (!do_sms || p.skipcol < w) {
                break;
            }
            if do_sms && p.skipcol >= w {
                p.skipcol -= w; // a screen row down
                done += 1;
            } else {
                p.top -= 1;
                p.skipcol = 0;
                if let Some((first, _)) = self.fold(p.top) {
                    done += 1;
                    p.top = first;
                } else if do_sms {
                    let mut size = self.size(p.top);
                    if size > w {
                        p.skipcol = w;
                        size -= w;
                    }
                    while size > w {
                        p.skipcol += w;
                        size -= w;
                    }
                    done += 1;
                } else {
                    done += self.plines(p.top);
                }
            }
            todo -= 1;
        }
        wrow += done;
        // The last row of the cursor line, kept inside the window (nvim divides by the window's whole
        // width here, gutter included).
        let mut wrow = wrow as isize;
        if self.wrap() {
            let cline_height = self.plines(p.line) as isize;
            let full = w + self.lines.win.gutter();
            let crow = (self.vcol(p.line, p.byte) / full) as isize;
            wrow += cline_height - 1 - crow;
        }
        let mut moved = false;
        while wrow >= self.height as isize && p.line > 0 {
            if let Some((first, _)) = self.fold(p.line) {
                wrow -= 1;
                p.line = first.saturating_sub(1);
            } else {
                wrow -= self.plines(p.line) as isize;
                p.line -= 1;
            }
            moved = true;
        }
        if moved {
            p.line = self.fold_start(p.line);
            self.advance(p);
        }
        if p.line < p.top {
            p.line = p.top;
            self.advance(p);
        }
        moved
    }

    /// nvim's `cursor_correct`: the cursor at least `'scrolloff'` rows from the window's edges, unless an
    /// edge is the buffer's. Returns whether it got past its early return (it then marks the top line
    /// valid).
    fn cursor_correct(&self, p: &mut ViewPos) -> bool {
        let mut above_wanted = self.so;
        let mut below_wanted = self.so;
        if p.top == 0 {
            above_wanted = 0;
            below_wanted = below_wanted.min(self.height / 2);
        }
        let botline = self.botline(p);
        if botline >= self.count() {
            below_wanted = 0;
            above_wanted = above_wanted.min(self.height.saturating_sub(1) / 2);
        }
        let cln = p.line;
        if cln >= p.top + above_wanted && cln + below_wanted < botline {
            return false;
        }
        let mut topline = p.top as isize;
        let mut bot = botline as isize - 1;
        let (mut above, mut below) = (0, 0);
        while (above < above_wanted || below < below_wanted) && topline < bot {
            if below < below_wanted && (below <= above || above >= above_wanted) {
                below += self.plines(bot as usize);
                bot = self.fold_start(bot as usize) as isize - 1;
            }
            if above < above_wanted && (above < below || below >= below_wanted) {
                above += self.plines(topline as usize);
                topline = self.fold_end(topline as usize) as isize + 1;
            }
        }
        let new = if topline == bot || bot < 0 {
            topline
        } else if topline > bot {
            bot
        } else {
            let mut l = cln as isize;
            if l < topline && p.top > 0 {
                l = topline;
            }
            if l > bot && botline < self.count() {
                l = bot;
            }
            l
        };
        p.line = usize::try_from(new).unwrap_or(0).min(self.last());
        true
    }

    /// nvim's `cursor_correct_sms`: with `'smoothscroll'`, the cursor in the shown part of the top line.
    fn cursor_correct_sms(&self, p: &mut ViewPos, sms: bool) {
        if !sms || !self.wrap() || p.line != p.top {
            return;
        }
        let w = self.width() as isize;
        let so = self.so as isize;
        let mut so_cols = if so == 0 { 0 } else { w + (so - 1) * w };
        let space_cols = (self.height as isize - 1) * w;
        let size = if so == 0 {
            0
        } else {
            self.size(p.top) as isize
        };
        if p.top == 0 && p.skipcol == 0 {
            so_cols = 0;
        } else if so_cols > space_cols / 2 {
            so_cols = space_cols / 2;
        }
        while so_cols > size && so_cols - w >= w && w > 0 {
            so_cols -= w;
        }
        if so_cols >= w && so_cols > size {
            so_cols -= w;
        }
        let overlap = if p.skipcol == 0 {
            0
        } else {
            self.marker_overlap() as isize
        };
        let skip = p.skipcol as isize;
        let top = skip + if so_cols != 0 { so_cols } else { overlap };
        let bot = skip + w + (self.height as isize - 1) * w - so_cols;
        let vcol = self.vcol(p.line, p.byte) as isize;
        let mut col = vcol;
        if col < top {
            if col < w {
                col += w;
            }
            while col < top {
                col += w;
            }
        } else {
            while col >= bot {
                col -= w;
            }
        }
        if col != vcol {
            p.curswant = usize::try_from(col).unwrap_or(0);
            self.advance(p);
            let reached = self.vcol(p.line, p.byte) as isize;
            if reached < col && p.skipcol > 0 && p.line < self.last() && reached < skip + overlap {
                // Still not visible: the next line instead.
                p.line += 1;
                p.byte = 0;
                p.curswant = 0;
            }
        }
    }

    fn cursor_down(&self, p: &mut ViewPos) -> bool {
        if self.fold_end(p.line) >= self.last() {
            return false;
        }
        p.line = self.fold_end(p.line) + 1;
        self.advance(p);
        true
    }

    fn cursor_up(&self, p: &mut ViewPos) -> bool {
        if p.line == 0 {
            return false;
        }
        p.line = self.fold_start(self.fold_start(p.line).saturating_sub(1));
        self.advance(p);
        true
    }

    /// nvim's `scroll_redraw`: scroll `count` lines, then keep `'scrolloff'` (moving the cursor at least
    /// once when the view could not move) and the `'smoothscroll'` top row.
    fn scroll_redraw(&self, p: &mut ViewPos, up: bool, count: usize, sms: bool) {
        let (top0, skip0, line0) = (p.top, p.skipcol, p.line);
        if up {
            self.scrollup(p, count, sms);
        } else {
            self.scrolldown(p, count, sms);
        }
        if self.so > 0 {
            let before = p.line;
            self.cursor_correct(p);
            if p.line != before {
                self.advance(p);
            }
            while p.top == top0 && p.skipcol == skip0 {
                let ok = if up {
                    p.line <= line0 && self.cursor_down(p)
                } else {
                    p.line >= line0 && top0 != 0 && self.cursor_up(p)
                };
                if !ok {
                    break;
                }
            }
        }
        self.cursor_correct_sms(p, sms);
        if p.line != line0 {
            self.advance(p);
        }
    }

    /// `{count}CTRL-E` / `{count}CTRL-Y`.
    pub(crate) fn scroll_lines(&self, p: ViewPos, up: bool, count: usize) -> Scrolled {
        let mut q = p;
        self.scroll_redraw(&mut q, up, count.max(1), self.sms);
        Scrolled {
            pos: q,
            beep: false,
            keep_top: self.so > 0,
        }
    }

    /// nvim's `get_scroll_overlap`: the rows a page scroll moves (the window less the overlap kept).
    fn scroll_overlap(&self, p: &ViewPos, forward: bool) -> usize {
        let min_height = self.height.saturating_sub(2);
        let botline = self.botline(p);
        if (!forward && p.top == 0) || (forward && botline >= self.count()) {
            return min_height + 2;
        }
        let rows = |l: isize| -> Option<usize> {
            (l >= 0 && (l as usize) < self.count()).then(|| self.plines(l as usize))
        };
        let mut l: isize = if forward {
            botline as isize
        } else {
            p.top as isize - 1
        };
        let step = |l: &mut isize| {
            if forward {
                *l -= 1;
                if *l >= 0 {
                    *l = self.fold_start(*l as usize) as isize;
                }
            } else {
                *l += 1;
                if (*l as usize) < self.count() {
                    *l = self.fold_end(*l as usize) as isize;
                }
            }
        };
        let h1 = rows(l).unwrap_or(usize::MAX);
        if h1 > min_height {
            return min_height + 2;
        }
        step(&mut l);
        let Some(h2) = rows(l).filter(|h2| h2 + h1 <= min_height) else {
            return min_height + 2;
        };
        step(&mut l);
        let Some(h3) = rows(l).filter(|h3| h3 + h2 <= min_height) else {
            return min_height + 2;
        };
        step(&mut l);
        match rows(l) {
            Some(h4) if h4 + h3 + h2 <= min_height && h3 + h2 + h1 <= min_height => min_height,
            _ => min_height + 1,
        }
    }

    /// nvim's `scroll_with_sms`: scroll `count` rows with `'smoothscroll'` on; without it, finish on a line
    /// boundary and adjust `curscount` by the rows that took.
    fn scroll_with_sms(
        &self,
        p: &mut ViewPos,
        forward: bool,
        count: usize,
        curscount: &mut isize,
    ) -> bool {
        let (top0, skip0) = (p.top, p.skipcol);
        self.scroll_redraw(p, forward, count, true);
        if !self.sms && p.skipcol > 0 {
            let w = self.width() as isize;
            let moved = p.top.abs_diff(top0);
            let fix_forward = if moved > usize::from(!forward) {
                !forward
            } else {
                forward
            };
            let skip = p.skipcol as isize;
            let count = if fix_forward {
                1 + (self.size(p.top) as isize - skip - w + w - 1) / w
            } else {
                1 + (skip - w - 1) / w
            };
            let count = count.max(0);
            self.scroll_redraw(p, fix_forward, count as usize, true);
            *curscount += if fix_forward == forward {
                count
            } else {
                -count
            };
        }
        p.top != top0 || p.skipcol != skip0
    }

    /// nvim's `pagescroll`: `CTRL-F` / `CTRL-B` (`half` false: `count` pages) and `CTRL-D` / `CTRL-U`
    /// (`half`: `amount` rows, the window's `'scroll'`).
    pub(crate) fn page(&self, p: ViewPos, forward: bool, count: usize, half: bool) -> Scrolled {
        let mut q = p;
        let mut did_move = false;
        // `scroll_redraw` marks the top line valid under 'scrolloff'; moving the cursor to another line
        // afterwards takes that back (nvim's `check_cursor_moved`).
        let mut keep_top = false;
        if half {
            let mut count = count.min(self.height) as isize;
            let mut curscount = count;
            let h = self.height as isize;
            if forward && q.top + self.height + count as usize > self.last() {
                let max = (self.height + count as usize) * 2;
                let mut n = self
                    .rows_upto(q.top, max)
                    .saturating_sub(self.skip_rows(q.skipcol)) as isize;
                if n - count < h && q.top < self.last() {
                    let mut l = self.fold_end(q.top) + 1;
                    while l <= self.last() && n < h + count {
                        n += self.plines(l) as isize;
                        l = self.fold_end(l) + 1;
                    }
                }
                if n < h + count {
                    count = n - h;
                }
            }
            if count > 0 {
                did_move = self.scroll_with_sms(&mut q, forward, count as usize, &mut curscount);
                keep_top = self.so > 0 && q.line == p.line;
                q.line = p.line;
                q.byte = p.byte;
                q.curswant = p.curswant;
            }
            let before = q.line;
            let dist = usize::try_from(curscount).unwrap_or(0);
            if self.wrap() {
                if dist > 0 {
                    let virtcol = self
                        .lines
                        .win
                        .lay(self.lines.text.line(q.line))
                        .cursor_vcol(q.byte, false);
                    if let Resolved::At { landing, .. } = window_view::screengo(
                        self.lines.text,
                        self.lines.win,
                        self.lines.folds,
                        q.line,
                        virtcol,
                        q.curswant,
                        forward,
                        dist,
                        false,
                    ) {
                        q.line = landing.line;
                        q.byte = landing.byte;
                        if let Some(w) = landing.want {
                            q.curswant = w;
                        }
                    }
                }
            } else {
                let last = self.last();
                q.line = if forward {
                    crate::folds::line_down(self.lines.folds, q.line, dist, last).unwrap_or(last)
                } else {
                    crate::folds::line_up(self.lines.folds, q.line, dist).unwrap_or(0)
                };
            }
            keep_top &= q.line == before;
        } else {
            let rows = count.max(1) * self.scroll_overlap(&q, forward);
            let mut c = rows as isize;
            did_move = self.scroll_with_sms(&mut q, forward, rows, &mut c);
            keep_top = self.so > 0;
            if did_move {
                let before = q.line;
                let bot = self.botline(&q);
                q.line = if forward {
                    q.top
                } else {
                    bot.saturating_sub(1)
                }
                .min(self.last());
                keep_top &= q.line == before;
            }
        }
        if self.so > 0 && self.cursor_correct(&mut q) {
            keep_top = true;
        }
        q.line = self.fold_start(q.line);
        let did_move = did_move || q.line != p.line || q.byte != p.byte;
        if did_move && !self.sms {
            // `beginline(BL_SOL | BL_FIX)` under 'nostartofline': the wanted column.
            self.advance(&mut q);
        } else {
            // With 'smoothscroll' nvim keeps the byte column, inside the line.
            let line = self.lines.text.line(q.line);
            if q.byte >= line.len() {
                q.byte = unicode_segmentation::UnicodeSegmentation::grapheme_indices(line, true)
                    .next_back()
                    .map_or(0, |(i, _)| i);
            }
        }
        Scrolled {
            pos: q,
            beep: !did_move,
            keep_top,
        }
    }

    /// The skipped columns after the cursor moved up into the top line's hidden part (nvim's
    /// `adjust_skipcol`): with `'smoothscroll'` the rows above it come back, else none stay hidden.
    pub(crate) fn uncover_cursor(&self, p: &ViewPos) -> usize {
        if p.line != p.top || p.skipcol == 0 || !self.wrap() {
            return p.skipcol;
        }
        let w = self.width();
        let vcol = self.vcol(p.line, p.byte);
        let so_cols = if self.so == 0 {
            0
        } else {
            w + (self.so - 1) * w
        };
        let overlap = self.marker_overlap();
        let mut skipcol = p.skipcol;
        while skipcol > 0 && vcol < skipcol + overlap + so_cols {
            skipcol = skipcol.saturating_sub(w);
        }
        skipcol
    }

    /// The rows of the top line left out at the top of the window.
    pub(crate) fn skipped_rows(&self, skipcol: usize) -> usize {
        if self.wrap() {
            self.skip_rows(skipcol)
        } else {
            0
        }
    }

    /// nvim's `curs_columns` skipcol rule, for the frame: when the cursor's line is the top line and does
    /// not fit (or the view had skipped columns), the virtual columns to skip so the cursor shows with
    /// `'scrolloff'` rows around it; else 0 without `'smoothscroll'`.
    pub(crate) fn frame_skipcol(&self, p: &ViewPos) -> usize {
        let w = self.width();
        if !self.wrap() {
            return 0;
        }
        let so = self.so;
        let height = self.height;
        let vcol = self.vcol(p.line, p.byte);
        let prev = p.skipcol;
        let wrow = self.wrow(p);
        let cap = vcol / w + so + height.saturating_mul(2) + 2;
        let rows = std::cell::OnceCell::new();
        let plines = || {
            *rows.get_or_init(|| {
                self.rows_past_cursor(p, vcol, cap)
                    .unwrap_or_else(|| self.rows_capped(p.line, cap))
            })
        };
        let tall = p.line == p.top
            && height > 0
            && (wrow >= height || ((prev > 0 || wrow + so >= height) && plines() > height));
        if !tall {
            return if self.sms { prev } else { 0 };
        }
        let mut skipcol = prev;
        let mut extra = 0;
        if skipcol + so * w > vcol {
            extra = 1;
        }
        let plines = plines().saturating_sub(1);
        let n = if plines > wrow + so {
            wrow + so
        } else {
            plines
        };
        if n + so >= height + skipcol / w {
            extra += 2;
        }
        if extra == 3 || height <= so * 2 {
            let mut n = vcol / w;
            n = n
                .saturating_sub(height / 2)
                .min((plines + 1).saturating_sub(height));
            skipcol = if n > 0 { w + (n - 1) * w } else { 0 };
        } else if extra == 1 {
            let e = (skipcol + so * w).saturating_sub(vcol).div_ceil(w);
            if e > 0 {
                skipcol -= (e * w).min(skipcol / w * w);
            }
        } else if extra == 2 {
            let mut endcol = (n + 1).saturating_sub(height) * w;
            while endcol > vcol {
                endcol -= w;
            }
            skipcol = skipcol.max(endcol);
        }
        // A small window: the cursor row inside it.
        let row = vcol / w - self.skip_rows(skipcol).min(vcol / w);
        if row >= height {
            skipcol += (row - height + 1) * w;
        }
        skipcol
    }
}

/// A window's lines with the top line's skipped rows taken off it (nvim's `plines_correct_topline`), for
/// the frame's [`crate::viewport::update_topline`].
pub(crate) struct TopSkipped<'a> {
    pub lines: &'a PaneLines<'a>,
    pub top: usize,
    pub rows: usize,
}

impl Lines for TopSkipped<'_> {
    fn count(&self) -> usize {
        self.lines.count()
    }

    fn height(&self, l: usize) -> usize {
        let h = self.lines.height(l);
        if l == self.top {
            h.saturating_sub(self.rows).max(1)
        } else {
            h
        }
    }

    fn fold(&self, l: usize) -> Option<(usize, usize)> {
        self.lines.fold(l)
    }

    fn any_fold(&self) -> bool {
        self.lines.any_fold()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::window_view::{DisplayGlobals, PaneText, WinView};

    /// `lines` one-row lines in an 80-column window `height` rows high, `'scrolloff'` `so`: the page
    /// command's landing `(top, cursor line)` from `top` with the cursor on line `cursor`, or `None` when
    /// nvim beeps.
    fn page(
        lines: usize,
        height: usize,
        so: usize,
        (top, cursor): (usize, usize),
        forward: bool,
        count: usize,
        half: bool,
    ) -> Option<(usize, usize)> {
        let text: String = (1..=lines).map(|i| format!("l{i}\n")).collect();
        let ws = ruse_core::Workspace::new(text.as_str());
        let mut idx = crate::line_index::LineIndex::default();
        idx.refresh(ruse_core::Revision(0), text.as_bytes());
        let win = WinView::resolve(
            ws.focused().view,
            &DisplayGlobals::default(),
            80,
            height,
            lines,
            false,
        );
        let pl = PaneLines {
            text: PaneText {
                bytes: text.as_bytes(),
                idx: &idx,
            },
            win: &win,
            folds: &[],
            virt: &[],
            height,
        };
        let sc = Scroll {
            lines: &pl,
            height,
            so,
            sms: false,
            tab_end: true,
            memo: Default::default(),
        };
        let p = ViewPos {
            top,
            skipcol: 0,
            line: cursor,
            byte: 0,
            curswant: 0,
        };
        let r = sc.page(p, forward, count, half);
        (!r.beep).then_some((r.pos.top, r.pos.line))
    }

    /// nvim v0.12.6 (headless, a 23-row window over 100 lines, `scrolloff=3`; 0-based rows here), as
    /// the line-counting `page_scroll` before #615's row port pinned them.
    #[test]
    fn page_keys_over_one_row_lines_match_nvim() {
        let ps = |top: usize, count, fwd| page(100, 23, 3, (top, top + 3), fwd, count, false);
        // `<C-f>` from the top: top 1 -> 22, cursor 25. `2<C-f>`: top 43, cursor 46.
        assert_eq!(ps(0, 1, true), Some((21, 24)));
        assert_eq!(ps(0, 2, true), Some((42, 45)));
        // Top 77 -> 98 (bottom 99 not yet the last line); top 78 shows the last line -> top 100.
        assert_eq!(ps(76, 1, true), Some((97, 99)));
        assert_eq!(ps(77, 1, true), Some((99, 99)));
        // `<C-b>`: top 39 -> 18, cursor 37 (bottom 40 minus scrolloff). `3<C-b>` from top 78: top 15, cur 34.
        assert_eq!(ps(38, 1, false), Some((17, 36)));
        assert_eq!(ps(77, 3, false), Some((14, 33)));
        // From top 10 back to the start: cursor 20. At the start: no scroll, a beep.
        assert_eq!(ps(9, 1, false), Some((0, 19)));
        assert_eq!(page(100, 23, 3, (0, 0), false, 1, false), None);
        // From the last line at the top: no overlap left, top 100 -> 77 (scrolloff 0: cursor 99).
        assert_eq!(page(100, 23, 0, (99, 99), false, 1, false), Some((76, 98)));
        // A buffer shorter than the window: `<C-f>` puts the last line at the top.
        assert_eq!(page(10, 23, 3, (0, 0), true, 1, false), Some((9, 9)));
    }

    /// nvim v0.12.6 (23-row window, 100 lines, `scrolloff=3`): `CTRL-D` / `CTRL-U` by the 'scroll' rows.
    #[test]
    fn half_page_keys_over_one_row_lines_match_nvim() {
        let hp = |top, cur, amount, fwd| page(100, 23, 3, (top, cur), fwd, amount, true);
        // `<C-d>` (scroll=11) from the top: top 12, cursor 15. `2<C-d>`: top 3, cursor 6.
        assert_eq!(hp(0, 0, 11, true), Some((11, 14)));
        assert_eq!(hp(0, 0, 2, true), Some((2, 5)));
        // `<C-u>` from top 39 / line 50: top 28, cursor 39.
        assert_eq!(hp(38, 49, 11, false), Some((27, 38)));
        // Near the end the view stops with the last line on the bottom row; the cursor still moves.
        assert_eq!(hp(86, 89, 11, true), Some((86, 99)));
        assert_eq!(hp(77, 80, 11, true), Some((77, 91)));
        // At the start `<C-u>` only moves the cursor.
        assert_eq!(hp(8, 11, 11, false), Some((0, 0)));
        assert_eq!(hp(0, 4, 11, false), Some((0, 0)));
    }
}
