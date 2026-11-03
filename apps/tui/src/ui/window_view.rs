//! One window's display for a frame (#615, #616): its options resolved (window-local over global), its
//! gutter (`'signcolumn'`, `'number'` / `'relativenumber'`), the screen rows each line takes, and the
//! screen-line motions (`gj`, `gk`, `g0`, `g^`, `gm`, `g$`) and horizontal scroll (`zh`, `zl`, `zs`, `ze`,
//! `'sidescroll'`) that count in those rows, as nvim v0.12.6 does.
//!
//! Pure over the buffer bytes, a line index and the folds; the session and the renderer share it, so the
//! rows the viewport scrolls over are the rows the painter draws.

use ruse_core::View;

use crate::folds::Fold;
pub(crate) use crate::input::display_opts::{
    parse_colorcolumn, parse_culopt, parse_fillchars, parse_foldcolumn, parse_signcolumn,
    ColorColumn, CursorLineOpt, FillChars, FoldColumn, SignColumn,
};
pub(crate) use crate::input::ScreenMotion;
use crate::line_index::LineIndex;
use crate::ui::line_layout::{self, BreakAt, BriOpt, LineLayout, ListChars};
use crate::viewport::Lines;

/// The global copies of the display options that are global or global-local in nvim (#615, #616).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayGlobals {
    pub showbreak: String,
    pub listchars: String,
    pub fillchars: String,
    pub breakat: String,
    /// `'display'`, as typed.
    pub display: String,
    pub sidescroll: i64,
    pub sidescrolloff: i64,
    pub scrolljump: i64,
}

impl Default for DisplayGlobals {
    fn default() -> DisplayGlobals {
        DisplayGlobals {
            showbreak: String::new(),
            listchars: line_layout::LISTCHARS_DEFAULT.into(),
            fillchars: String::new(),
            breakat: line_layout::BREAKAT_DEFAULT.into(),
            display: "lastline".into(),
            sidescroll: 1,
            sidescrolloff: 0,
            scrolljump: 1,
        }
    }
}

/// How the last line shows when it does not fit (`'display'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LastLine {
    /// `@` in the first column of each row it would take (`display=`).
    AtRows,
    /// As much as fits, `@@@` in the last row's last cells (`lastline`).
    Lastline,
    /// As much as fits, the last row `@@@` (`truncate`).
    Truncate,
}

/// One window's display for a frame: its options resolved and its geometry.
#[derive(Clone, Debug)]
pub(crate) struct WinView {
    pub wrap: bool,
    pub linebreak: bool,
    pub breakat: BreakAt,
    pub showbreak: String,
    pub breakindent: Option<BriOpt>,
    pub list: Option<ListChars>,
    /// `'display'` `uhex` (#1181).
    pub uhex: bool,
    pub tabstop: usize,
    pub lastline: LastLine,
    pub number: bool,
    pub relativenumber: bool,
    /// The sign column's cells (two per sign), 0 when it does not show.
    pub sign_w: usize,
    /// The number column's cells, its trailing blank included; 0 without `'number'`/`'relativenumber'`.
    pub num_w: usize,
    /// Whether signs show in the number column (`signcolumn=number`).
    pub signs_in_number: bool,
    /// The text area's width (the window's width less the gutter), at least 1.
    pub width: usize,
    pub sidescroll: usize,
    pub sidescrolloff: usize,
    pub leftcol: usize,
    /// Screen rows of the first line left out at the top: a cursor line taller than the window shows
    /// from the row that keeps the cursor in view.
    pub skip: usize,
    /// The cursor's buffer byte and virtual column as the viewport pass found them, so the frame's
    /// render does not lay the cursor's line out again.
    pub cursor: Option<(usize, usize)>,
    /// `'fillchars'` (#616).
    pub fill: FillChars,
    /// `'cursorlineopt'` when `'cursorline'` is on.
    pub cursorline: Option<CursorLineOpt>,
    /// `'cursorcolumn'`.
    pub cursorcolumn: bool,
    /// `'colorcolumn'`, as 0-based virtual columns (`'textwidth'` applied).
    pub colorcolumns: Vec<usize>,
    /// `'foldcolumn'` as set, and its cells this frame ([`WinView::with_folds`]).
    pub foldcolumn: FoldColumn,
    pub fold_w: usize,
    /// `'smoothscroll'`.
    pub smoothscroll: bool,
}

/// The deepest fold nesting in `folds` (nvim's `getDeepestNesting`), for `'foldcolumn'` `auto`.
pub(crate) fn deepest_fold(folds: &[Fold]) -> usize {
    folds
        .iter()
        .map(|f| {
            folds
                .iter()
                .filter(|o| o.start <= f.start && f.end <= o.end)
                .count()
        })
        .max()
        .unwrap_or(0)
}

/// What the fold column shows for line `l` (nvim's `fold_info`): its fold level, the first line of the
/// fold it is in, the lowest level of a fold starting on it, and the lines of the closed fold it is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FoldInfo {
    pub level: usize,
    pub first: usize,
    pub low_level: usize,
    pub closed: bool,
}

/// nvim's `fold_info` over ruse's fold list: walk the folds holding `l` from the outermost; the first
/// closed one ends the walk.
pub(crate) fn fold_info(folds: &[Fold], l: usize) -> FoldInfo {
    let mut holding: Vec<&Fold> = folds.iter().filter(|f| f.contains(l)).collect();
    holding.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let (mut level, mut low, mut first) = (0, 0, l);
    for f in holding {
        if f.start == l && low == 0 {
            low = level + 1;
        }
        if f.closed {
            return FoldInfo {
                level: level + 1,
                first: f.start,
                low_level: if low == 0 { level + 1 } else { low },
                closed: true,
            };
        }
        first = f.start;
        level += 1;
    }
    FoldInfo {
        level,
        first,
        low_level: if low == 0 { level } else { low },
        closed: false,
    }
}

/// nvim's `fill_foldcolumn`: the `fdc` cells of the fold column for line `l`.
pub(crate) fn fold_column(folds: &[Fold], l: usize, fdc: usize, fill: &FillChars) -> Vec<char> {
    let fi = fold_info(folds, l);
    let closed = usize::from(fi.closed);
    let first_level = (fi.level as isize - fdc as isize - closed as isize + 1).max(1) as usize;
    let closedcol = fdc.min(fi.level);
    (0..fdc)
        .map(|i| {
            if i >= fi.level {
                ' '
            } else if fi.closed && i + 1 == closedcol {
                fill.foldclose
            } else if fi.first == l && first_level + i >= fi.low_level {
                fill.foldopen
            } else if first_level == 1 {
                fill.foldsep
            } else if let Some(c) = fill.foldinner {
                c
            } else if first_level + i <= 9 {
                char::from(b'0' + (first_level + i) as u8)
            } else {
                '>'
            }
        })
        .collect()
}

/// nvim's `number_width`: the digits of the largest number shown, at least `'numberwidth'` - 1.
fn number_digits(view: &ruse_core::WindowOptions, line_count: usize, height: usize) -> usize {
    let n = if view.relativenumber && !view.number {
        height
    } else {
        line_count
    };
    let digits = n.max(1).ilog10() as usize + 1;
    digits.max(usize::try_from(view.numberwidth - 1).unwrap_or(0))
}

impl WinView {
    /// Resolve `view`'s display for a window `rect_w` x `height` over a buffer of `line_count` lines;
    /// `has_signs` says whether any line has a sign (a diagnostic).
    pub(crate) fn resolve(
        view: &View,
        g: &DisplayGlobals,
        rect_w: usize,
        height: usize,
        line_count: usize,
        has_signs: bool,
    ) -> WinView {
        let o = view.window_options();
        let sbr = o.showbreak.clone().unwrap_or_else(|| g.showbreak.clone());
        let lcs = o.listchars.as_deref().unwrap_or(&g.listchars);
        let numbered = o.number || o.relativenumber;
        let scl = parse_signcolumn(&o.signcolumn).unwrap_or(SignColumn::Auto { min: 0, max: 1 });
        let signs = usize::from(has_signs);
        let (sign_cols, signs_in_number) = match scl {
            SignColumn::No => (0, false),
            SignColumn::Yes(n) => (n, false),
            SignColumn::Auto { min, max } => (signs.min(max).max(min), false),
            SignColumn::Number if numbered => (0, true),
            SignColumn::Number => (signs, false),
        };
        let mut num_w = if numbered {
            number_digits(o, line_count, height) + 1
        } else {
            0
        };
        if signs_in_number && has_signs && num_w < 3 {
            num_w = 3; // a sign needs two cells plus the blank
        }
        let sign_w = sign_cols * 2;
        let fcs = o.fillchars.as_deref().unwrap_or(&g.fillchars);
        let textwidth = view
            .editing_options()
            .iter()
            .find_map(|e| match e {
                ruse_core::editor::EditorOption::TextWidth(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(0);
        let colorcolumns = parse_colorcolumn(&o.colorcolumn)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| {
                let col = match c {
                    ColorColumn::At(n) => n,
                    // Relative to 'textwidth', and only with one.
                    ColorColumn::FromTextWidth(_) if textwidth == 0 => return None,
                    ColorColumn::FromTextWidth(n) => i64::try_from(textwidth).ok()? + n,
                };
                usize::try_from(col - 1).ok()
            })
            .collect();
        let foldcolumn = parse_foldcolumn(&o.foldcolumn).unwrap_or(FoldColumn::Fixed(0));
        let fold_w = match foldcolumn {
            FoldColumn::Fixed(n) => n,
            FoldColumn::Auto(_) => 0,
        };
        let display = &g.display;
        let has = |f: &str| display.split(',').any(|x| x == f);
        WinView {
            wrap: o.wrap,
            linebreak: o.linebreak,
            breakat: BreakAt::new(&g.breakat),
            showbreak: sbr,
            breakindent: o
                .breakindent
                .then(|| line_layout::parse_briopt(&o.breakindentopt).unwrap_or_default()),
            list: o.list.then(|| {
                line_layout::parse_listchars(lcs)
                    .or_else(|_| line_layout::parse_listchars(line_layout::LISTCHARS_DEFAULT))
                    .unwrap_or_default()
            }),
            uhex: has("uhex"),
            tabstop: view.tab_width().max(1),
            lastline: if has("truncate") {
                LastLine::Truncate
            } else if has("lastline") {
                LastLine::Lastline
            } else {
                LastLine::AtRows
            },
            number: o.number,
            relativenumber: o.relativenumber,
            sign_w,
            num_w,
            signs_in_number,
            width: rect_w.saturating_sub(fold_w + sign_w + num_w).max(1),
            sidescroll: usize::try_from(g.sidescroll).unwrap_or(0),
            sidescrolloff: usize::try_from(
                o.sidescrolloff
                    .filter(|&n| n >= 0)
                    .unwrap_or(g.sidescrolloff),
            )
            .unwrap_or(0),
            leftcol: view.leftcol(),
            skip: 0,
            cursor: None,
            fill: parse_fillchars(fcs).unwrap_or_default(),
            cursorline: o
                .cursorline
                .then(|| parse_culopt(&o.cursorlineopt).ok())
                .flatten(),
            cursorcolumn: o.cursorcolumn,
            colorcolumns,
            foldcolumn,
            fold_w,
            smoothscroll: o.smoothscroll,
        }
    }

    /// Size an `auto` fold column for the window's `folds` (as deep as the deepest fold, at most its
    /// maximum) and narrow the text area by it.
    pub(crate) fn with_folds(mut self, folds: &[Fold]) -> WinView {
        if let FoldColumn::Auto(max) = self.foldcolumn {
            let w = deepest_fold(folds).min(max);
            self.width = (self.width + self.fold_w).saturating_sub(w).max(1);
            self.fold_w = w;
        }
        self
    }

    /// A window `width` x `height` with no gutter that does not wrap: how the renderer drew every window
    /// before #615 (its tests paint through it).
    #[cfg(test)]
    pub(crate) fn plain(width: usize, tabstop: usize) -> WinView {
        WinView {
            wrap: false,
            linebreak: false,
            breakat: BreakAt::default(),
            showbreak: String::new(),
            breakindent: None,
            list: None,
            uhex: false,
            tabstop,
            lastline: LastLine::Lastline,
            number: false,
            relativenumber: false,
            sign_w: 0,
            num_w: 0,
            signs_in_number: false,
            width: width.max(1),
            sidescroll: 1,
            sidescrolloff: 0,
            leftcol: 0,
            skip: 0,
            cursor: None,
            fill: FillChars::default(),
            cursorline: None,
            cursorcolumn: false,
            colorcolumns: Vec::new(),
            foldcolumn: FoldColumn::Fixed(0),
            fold_w: 0,
            smoothscroll: false,
        }
    }

    /// The gutter's width: the fold column, the sign column and the number column.
    pub(crate) fn gutter(&self) -> usize {
        self.fold_w + self.sign_w + self.num_w
    }

    /// The line layout options.
    pub(crate) fn layout(&self) -> line_layout::Layout<'_> {
        line_layout::Layout {
            width: self.width,
            tabstop: self.tabstop,
            wrap: self.wrap,
            linebreak: self.linebreak,
            breakat: &self.breakat,
            showbreak: &self.showbreak,
            breakindent: self.breakindent,
            list: self.list.as_ref(),
            uhex: self.uhex,
        }
    }

    /// Lay out `line`.
    pub(crate) fn lay<'t>(&'t self, line: &'t str) -> LineLayout<'t> {
        line_layout::layout(line, &self.layout(), &[])
    }

    /// Whether the cursor on a tab shows on its last cell (Normal mode without `'list'`).
    pub(crate) fn tab_end(&self, mode: ruse_core::Mode) -> bool {
        self.list.is_none() && mode == ruse_core::Mode::Normal
    }
}

/// A buffer's text by line, over its line index.
#[derive(Clone, Copy)]
pub(crate) struct PaneText<'a> {
    pub bytes: &'a [u8],
    pub idx: &'a LineIndex,
}

impl<'a> PaneText<'a> {
    /// The number of lines (the slot after a final newline is no line, #677).
    pub(crate) fn line_count(&self) -> usize {
        let end = ruse_core::motion::off_phantom(self.bytes, self.bytes.len());
        self.idx.line_of(end) + 1
    }

    /// Line `l`'s text, without its newline.
    pub(crate) fn line(&self, l: usize) -> &'a str {
        let s = self.idx.nth_line_start(l).min(self.bytes.len());
        let e = self.idx.nth_line_start(l + 1).min(self.bytes.len());
        let mut e = e.max(s);
        if e > s && self.bytes[e - 1] == b'\n' {
            e -= 1;
        }
        std::str::from_utf8(&self.bytes[s..e]).unwrap_or("")
    }

    /// The first `max` bytes of line `l` (cut back to a char boundary): what a layout limited to the
    /// window's cells reads, without checking the UTF-8 of a huge line's rest every frame.
    pub(crate) fn line_upto(&self, l: usize, max: usize) -> &'a str {
        let s = self.idx.nth_line_start(l).min(self.bytes.len());
        let mut e = self.idx.nth_line_start(l + 1).min(self.bytes.len()).max(s);
        if e > s && self.bytes[e - 1] == b'\n' {
            e -= 1;
        }
        let e = e.min(s.saturating_add(max));
        match std::str::from_utf8(&self.bytes[s..e]) {
            Ok(t) => t,
            Err(err) if err.error_len().is_none() => {
                std::str::from_utf8(&self.bytes[s..s + err.valid_up_to()]).unwrap_or("")
            }
            Err(_) => "",
        }
    }

    /// The bytes a layout of `vcols` virtual columns reads at most, give or take long clusters.
    pub(crate) fn bytes_for(vcols: usize) -> usize {
        vcols.saturating_mul(4).saturating_add(4096)
    }

    /// The byte where line `l` starts.
    pub(crate) fn start(&self, l: usize) -> usize {
        self.idx.nth_line_start(l)
    }
}

/// The lines one window scrolls over: their wrapped rows, the virtual rows below them and the closed
/// folds (the viewport's [`Lines`]).
pub(crate) struct PaneLines<'a> {
    pub text: PaneText<'a>,
    pub win: &'a WinView,
    pub folds: &'a [Fold],
    /// `(after_line, rows)` of the virtual blocks (images) below lines.
    pub virt: &'a [(usize, usize)],
    /// The window's height: a line's rows are counted up to it.
    pub height: usize,
}

impl PaneLines<'_> {
    /// The rows of line `l`'s text alone (no virtual rows), counted up to the window's height (nvim's
    /// `plines_win` limit), which is all the viewport reads.
    pub(crate) fn text_rows(&self, l: usize) -> usize {
        if self.win.wrap {
            let limit = self.height.saturating_add(1).saturating_mul(self.win.width);
            let line = self.text.line_upto(l, PaneText::bytes_for(limit));
            line_layout::layout_upto(line, &self.win.layout(), &[], limit, usize::MAX).rows
        } else {
            1
        }
    }
}

impl Lines for PaneLines<'_> {
    fn count(&self) -> usize {
        self.text.line_count()
    }

    fn height(&self, l: usize) -> usize {
        let virt: usize = self
            .virt
            .iter()
            .filter(|&&(a, _)| a == l)
            .map(|&(_, h)| h)
            .sum();
        self.text_rows(l) + virt
    }

    fn fold(&self, l: usize) -> Option<(usize, usize)> {
        crate::folds::closed_range(self.folds, l)
    }

    fn any_fold(&self) -> bool {
        self.folds.iter().any(|f| f.closed)
    }
}

/// nvim's `curs_columns` horizontal part for an unwrapped window: the `leftcol` that keeps the cursor's
/// cells `'sidescrolloff'` inside the window, scrolling by at least `'sidescroll'` columns, or putting the
/// cursor in the middle when it is far off (or `'sidescroll'` is 0).
pub(crate) fn follow_leftcol(win: &WinView, start: usize, end: usize, cursor_vcol: usize) -> usize {
    let w = win.width as isize;
    let siso = win.sidescrolloff.min(win.width.saturating_sub(1) / 2) as isize;
    let leftcol = win.leftcol as isize;
    let off_left = start as isize - leftcol - siso;
    let off_right = end as isize - (leftcol + w - siso) + 1;
    if off_left >= 0 && off_right <= 0 {
        return win.leftcol;
    }
    let mut diff = if off_left < 0 { -off_left } else { off_right };
    let new = if win.sidescroll == 0 || diff >= w / 2 || off_right >= off_left {
        cursor_vcol as isize - w / 2
    } else {
        diff = diff.max(win.sidescroll as isize);
        if off_left < 0 {
            leftcol - diff
        } else {
            leftcol + diff
        }
    };
    usize::try_from(new.max(0)).unwrap_or(0)
}

/// The cursor's place on a laid-out line: its virtual column, and the first and last cells of the char
/// it is on (nvim's `getvvcol`).
pub(crate) fn cursor_cols(
    l: &LineLayout<'_>,
    line_byte: usize,
    tab_end: bool,
) -> (usize, usize, usize) {
    let v = l.cursor_vcol(line_byte, tab_end);
    match l.char_index(line_byte) {
        Some(i) => {
            let c = l.chars[i];
            (c.vcol + c.head, c.vcol + c.size.max(1) - 1, v)
        }
        None => (v, v, v),
    }
}

/// [`cursor_cols`] for the cursor on line byte `byte` of `text`'s line `l`, without laying the line
/// out when it is plain printable ASCII up to the cursor (one cell per byte, which no wrap rule changes
/// without 'showbreak' / 'breakindent' / 'linebreak'): a cursor at the end of a huge line costs a byte
/// scan, not a layout.
pub(crate) fn cursor_place(
    win: &WinView,
    text: PaneText<'_>,
    l: usize,
    byte: usize,
    tab_end: bool,
) -> (usize, usize, usize) {
    let s = text.start(l);
    let plain = |b: &u8| (0x20..0x7f).contains(b);
    let simple = !win.linebreak && win.showbreak.is_empty() && win.breakindent.is_none();
    if simple
        && text
            .bytes
            .get(s..s + byte)
            .is_some_and(|p| p.iter().all(plain))
        && text
            .bytes
            .get(s + byte)
            .is_none_or(|b| *b == b'\n' || plain(b))
    {
        return (byte, byte, byte);
    }
    let lay = line_layout::cursor_layout(text.line_upto(l, byte + 64), &win.layout(), byte);
    cursor_cols(&lay, byte, tab_end)
}

/// Where a screen-line motion lands: the line, the byte in it, whether the motion is inclusive, and the
/// wanted column nvim keeps after it (`gj` / `gk` keep theirs, `usize::MAX` sticks to the row's end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Landing {
    pub line: usize,
    pub byte: usize,
    pub inclusive: bool,
    pub want: Option<usize>,
}

/// A resolved screen-line motion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Without `'wrap'` `gj` / `gk` are `j` / `k` (linewise under an operator).
    Lines { down: bool },
    /// The motion lands here; `complete` is false when it stopped early at the buffer's end (nvim moves
    /// that far and beeps, and an operator is cancelled).
    At { landing: Landing, complete: bool },
    /// Nothing to move to (nvim beeps).
    Fail,
}

/// nvim's `coladvance(wcol)` on `layout`: the line byte of the char holding virtual column `wcol` (the
/// last char past the end; 0 on an empty line).
pub(crate) fn coladvance(l: &LineLayout<'_>, wcol: usize, one_more: bool) -> usize {
    if one_more && wcol >= l.total {
        // Visual mode may stop past the last char, on the line's end.
        return l.chars.last().map_or(0, |c| c.byte + c.len);
    }
    l.char_at_vcol(wcol).map_or(0, |i| l.chars[i].byte)
}

/// Resolve a screen-line motion from the cursor at `(line, line_byte)` with `curswant` (the core's
/// wanted column; `usize::MAX` after `$`), `count` times, in a window `win` with closed `folds`. `None`
/// when it fails (nvim beeps): `gj` on the last screen row of the buffer, `gk` on the first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn screen_motion(
    text: PaneText<'_>,
    win: &WinView,
    folds: &[Fold],
    line: usize,
    line_byte: usize,
    curswant: usize,
    motion: ScreenMotion,
    count: usize,
    one_more: bool,
) -> Resolved {
    let count = count.max(1);
    let cur_line = text.line(line);
    let lay = win.lay(cur_line);
    let virtcol = lay.cursor_vcol(line_byte, false);
    let width1 = win.width;
    let width2 = win.width;
    match motion {
        ScreenMotion::Down | ScreenMotion::Up if !win.wrap => Resolved::Lines {
            down: motion == ScreenMotion::Down,
        },
        ScreenMotion::Down | ScreenMotion::Up => {
            let down = motion == ScreenMotion::Down;
            screengo(
                text, win, folds, line, virtcol, curswant, down, count, one_more,
            )
        }
        ScreenMotion::Start | ScreenMotion::FirstNonBlank | ScreenMotion::Middle => {
            let mut i = if win.wrap {
                if virtcol >= width1 {
                    (virtcol - width1) / width2 * width2 + width1
                } else {
                    0
                }
            } else {
                win.leftcol
            };
            if motion == ScreenMotion::Middle {
                i += win.width / 2;
            }
            let mut byte = coladvance(&lay, i, false);
            if motion == ScreenMotion::FirstNonBlank {
                let b = cur_line.as_bytes();
                while matches!(b.get(byte), Some(b' ' | b'\t')) && byte + 1 < b.len() {
                    byte += 1;
                }
            }
            at(Landing {
                line,
                byte,
                inclusive: false,
                want: None,
            })
        }
        ScreenMotion::End => {
            if win.wrap {
                if count > 1 {
                    let dist = count - 1;
                    return match screengo(
                        text,
                        win,
                        folds,
                        line,
                        virtcol,
                        usize::MAX,
                        true,
                        dist,
                        false,
                    ) {
                        Resolved::At {
                            mut landing,
                            complete: true,
                        } => {
                            landing.inclusive = true;
                            at(landing)
                        }
                        _ => Resolved::Fail,
                    };
                }
                let mut i = width1 - 1;
                if virtcol >= width1 {
                    i += ((virtcol - width1) / width2 + 1) * width2;
                }
                let mut byte = coladvance(&lay, i, false);
                // A char split at the row's end belongs to the next row: stay on this one.
                if let Some(ci) = lay.char_index(byte) {
                    let c = lay.chars[ci];
                    if byte > 0 && c.vcol + c.head > i && ci > 0 {
                        byte = lay.chars[ci - 1].byte;
                    }
                }
                at(Landing {
                    line,
                    byte,
                    inclusive: true,
                    want: Some(usize::MAX),
                })
            } else {
                let line = if count > 1 {
                    crate::folds::line_down(folds, line, count - 1, text.line_count() - 1)
                        .unwrap_or(line)
                } else {
                    line
                };
                let lay = win.lay(text.line(line));
                let i = win.leftcol + win.width - 1;
                let mut byte = coladvance(&lay, i, false);
                if let Some(ci) = lay.char_index(byte) {
                    let c = lay.chars[ci];
                    if ci > 0
                        && c.size > 1
                        && !c.is_tab
                        && c.vcol + c.size > win.leftcol + win.width
                    {
                        byte = lay.chars[ci - 1].byte;
                    }
                }
                at(Landing {
                    line,
                    byte,
                    inclusive: true,
                    want: None,
                })
            }
        }
    }
}

fn at(landing: Landing) -> Resolved {
    Resolved::At {
        landing,
        complete: true,
    }
}

/// The line's virtual width rounded up to its last screen row (nvim's `n` in `nv_screengo`).
fn row_span(linelen: usize, width1: usize, width2: usize) -> usize {
    if linelen > width1 {
        ((linelen - width1 - 1) / width2 + 1) * width2 + width1
    } else {
        width1
    }
}

/// nvim's `nv_screengo`: `gj` / `gk` `dist` screen rows, keeping the wanted column within the row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn screengo(
    text: PaneText<'_>,
    win: &WinView,
    folds: &[Fold],
    line: usize,
    virtcol: usize,
    curswant: usize,
    down: bool,
    dist: usize,
    one_more: bool,
) -> Resolved {
    let last = text.line_count() - 1;
    let (width1, width2) = (win.width, win.width);
    let linetabsize = |l: usize| win.lay(text.line(l)).total;
    let mut line = line;
    let mut linelen = linetabsize(line);
    let atend = curswant == usize::MAX;
    let mut want = if atend {
        let mut w = width1 - 1;
        if virtcol > w {
            w += ((virtcol - w - 1) / width2 + 1) * width2;
        }
        w
    } else {
        curswant.min(row_span(linelen, width1, width2) - 1)
    };
    let folded = |l: usize| crate::folds::closed_range(folds, l).is_some();
    let mut moved = false;
    let mut complete = true;
    for _ in 0..dist {
        if down {
            let n = row_span(linelen, width1, width2);
            if want + width2 < n && !folded(line) {
                want += width2;
            } else {
                let Some(next) =
                    crate::folds::line_down(folds, line, 1, last).filter(|&n| n != line)
                else {
                    complete = false;
                    break;
                };
                line = next;
                want %= width2;
                linelen = linetabsize(line);
            }
        } else if want >= width1 && !folded(line) {
            want -= width2;
        } else {
            let Some(prev) = crate::folds::line_up(folds, line, 1) else {
                complete = false;
                break;
            };
            line = prev;
            linelen = linetabsize(line);
            if linelen > width1 {
                want += ((linelen - width1 - 1) / width2 + 1) * width2;
            }
        }
        moved = true;
    }
    if !moved {
        return Resolved::Fail;
    }
    let lay = win.lay(text.line(line));
    let mut byte = coladvance(&lay, want, one_more);
    // Landing on a char split at the end of the row above: nvim steps back one when the wanted column
    // is past the middle of that row.
    if let Some(ci) = lay.char_index(byte).filter(|_| byte > 0) {
        let c = lay.chars[ci];
        let mut vc = c.vcol + c.head;
        if vc > width1 && !win.showbreak.is_empty() {
            vc -= line_layout::str_width(&win.showbreak);
        }
        let past_half = if want < width1 {
            want > width1 / 2
        } else {
            (want - width1) % width2 > width2 / 2
        };
        if vc > want && past_half && ci > 0 {
            byte = lay.chars[ci - 1].byte;
        }
    }
    Resolved::At {
        landing: Landing {
            line,
            byte,
            inclusive: atend,
            want: Some(if atend { usize::MAX } else { want }),
        },
        complete,
    }
}

/// A horizontal scroll command (`:help scroll-horizontal`), for a window that does not wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HScroll {
    /// `zh` / `z<Left>`: the view `n` columns right (the text moves right).
    Left(usize),
    /// `zl` / `z<Right>`: the view `n` columns left.
    Right(usize),
    /// `zs`: the cursor's column first.
    CursorFirst,
    /// `ze`: the cursor's column last.
    CursorLast,
}

/// nvim's `nv_zet` horizontal commands and `leftcol_changed` on the cursor's laid-out line: the new
/// `leftcol`, and the line byte the cursor moves to when the scroll left it outside the window
/// (`'sidescrolloff'` inside). `None` with `'wrap'` on, where they do nothing.
pub(crate) fn hscroll(
    win: &WinView,
    lay: &LineLayout<'_>,
    line_byte: usize,
    tab_end: bool,
    folded: bool,
    cmd: HScroll,
) -> Option<(usize, Option<usize>)> {
    if win.wrap {
        return None;
    }
    let siso = win.sidescrolloff;
    let (start, end, _) = cursor_cols(lay, line_byte, tab_end);
    let leftcol = match cmd {
        HScroll::Left(n) => win.leftcol.saturating_sub(n),
        HScroll::Right(n) => win.leftcol.saturating_add(n),
        HScroll::CursorFirst => {
            let col = if folded { 0 } else { start };
            return Some((col.saturating_sub(siso), None));
        }
        HScroll::CursorLast => {
            let col = if folded { 0 } else { end };
            let n = win.width;
            return Some((
                if col + siso < n {
                    0
                } else {
                    col + siso - n + 1
                },
                None,
            ));
        }
    };
    // `leftcol_changed`: bring the cursor back inside the window, 'sidescrolloff' from its edges.
    let lastcol = (leftcol + win.width).saturating_sub(1);
    let virtcol = lay.cursor_vcol(line_byte, tab_end);
    let coladv = |wcol: usize| lay.char_at_vcol(wcol).map_or(0, |i| lay.chars[i].byte);
    let mut byte = line_byte;
    let mut moved = false;
    if virtcol > lastcol.saturating_sub(siso) {
        byte = coladv(lastcol.saturating_sub(siso));
        moved = true;
    } else if virtcol < leftcol + siso {
        byte = coladv(leftcol + siso);
        moved = true;
    }
    let (s, e, _) = cursor_cols(lay, byte, tab_end);
    let mut leftcol = leftcol;
    if e > lastcol {
        byte = coladv(s.saturating_sub(1));
        moved = true;
    } else if s < leftcol {
        let next = coladv(e + 1);
        if next == byte {
            leftcol = s; // no char after it: scroll back to show it instead
        } else {
            byte = next;
            moved = true;
        }
    }
    Some((leftcol, moved.then_some(byte)))
}
