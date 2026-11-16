//! Screen layout of one buffer line (#615, #616): soft wrap, `'linebreak'`, `'showbreak'`,
//! `'breakindent'`, `'list'` / `'listchars'` and the horizontal offset of an unwrapped window.
//!
//! The model is nvim v0.12.6's `charsize_regular` (`charsize.c`). Every grapheme gets a SIZE in virtual
//! columns that includes what the screen inserts for it: the `>` filler in the last cell when a wide
//! glyph does not fit, the `'showbreak'` / `'breakindent'` head of each continuation row it starts or
//! crosses, and the `'linebreak'` padding that pushes the next word to the next row. With wrapping the
//! virtual column of a cell is then `row * width + col`, which is what `gj`, `g$` and `gm` count in
//! (nvim's `nv_screengo`), and the inverse ([`LineLayout::char_at_vcol`]) is nvim's `coladvance`.
//!
//! Without `'wrap'` the sizes are the plain cell widths (a tab to the next stop) and the window shows
//! the columns from its `leftcol`; [`LineLayout::cells`] still holds every cell and the painter clips.
//!
//! Pure: no terminal and no buffer, unit-tested against nvim screens.

use ruse_core::cells::{self, Glyph};
use unicode_segmentation::UnicodeSegmentation;

use crate::screen::cluster_width;

pub(crate) use crate::input::display_opts::{
    parse_briopt, parse_listchars, BriOpt, ListChars, BREAKAT_DEFAULT, LISTCHARS_DEFAULT,
};

/// The bytes of `'breakat'` (nvim's `breakat_flags`: only single-byte characters break).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BreakAt([bool; 128]);

impl BreakAt {
    pub(crate) fn new(s: &str) -> BreakAt {
        let mut f = [false; 128];
        for b in s.bytes().filter(u8::is_ascii) {
            f[usize::from(b)] = true;
        }
        BreakAt(f)
    }

    fn has(&self, b: u8) -> bool {
        b.is_ascii() && self.0[usize::from(b)]
    }
}

impl Default for BreakAt {
    fn default() -> BreakAt {
        BreakAt::new(BREAKAT_DEFAULT)
    }
}

/// How one line is laid out.
#[derive(Clone, Debug)]
pub(crate) struct Layout<'a> {
    /// The text area's width in cells (the window less its gutter), at least 1.
    pub width: usize,
    pub tabstop: ruse_core::Tabs,
    pub wrap: bool,
    pub linebreak: bool,
    pub breakat: &'a BreakAt,
    /// `'showbreak'`, painted at the start of each continuation row.
    pub showbreak: &'a str,
    /// `'breakindentopt'` when `'breakindent'` is on.
    pub breakindent: Option<BriOpt>,
    /// `'listchars'` when `'list'` is on.
    pub list: Option<&'a ListChars>,
    /// `'display'` `uhex`: a control char shows as `<xx>`, not `^X` (#1181).
    pub uhex: bool,
}

/// What a painted cell shows, for its highlight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CellKind {
    /// Buffer text.
    Text,
    /// nvim's `NonText`: `'showbreak'`, the wide-char `>` filler, `eol`, `extends` / `precedes`, `@@@`.
    NonText,
    /// nvim's `Whitespace` / `SpecialKey`: the `'listchars'` tab, space and nbsp characters.
    Whitespace,
}

/// One painted cell group: a glyph of `width` cells at virtual column `vcol`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cell<'t> {
    pub vcol: usize,
    pub text: CellText<'t>,
    pub width: usize,
    /// The byte (offset in the line) the cell belongs to; `None` for a head or concealed virt text.
    pub byte: Option<usize>,
    pub kind: CellKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CellText<'t> {
    Str(&'t str),
    Char(char),
    Blank,
}

/// One grapheme's place in the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CharSpan {
    /// Byte offset in the line and length.
    pub byte: usize,
    pub len: usize,
    /// First virtual column (where the char's size starts).
    pub vcol: usize,
    /// Cells before the glyph inside the size (filler, heads).
    pub head: usize,
    /// The whole size in virtual columns.
    pub size: usize,
    pub is_tab: bool,
}

/// A laid-out line.
#[derive(Clone, Debug)]
pub(crate) struct LineLayout<'t> {
    pub chars: Vec<CharSpan>,
    pub cells: Vec<Cell<'t>>,
    /// The virtual columns the text takes (nvim's `linetabsize`, without the `eol` cell).
    pub total: usize,
    /// Screen rows with wrapping (at least 1); 1 without.
    pub rows: usize,
    width: usize,
    wrap: bool,
    /// Whether [`LineLayout::cells`] is filled (not for a layout that only places the cursor).
    with_cells: bool,
}

/// The `'breakindent'` indent of `line` (nvim's `get_breakindent_win`), less the `'showbreak'` width under
/// `briopt` `sbr`, kept so that `briopt` `min` cells stay.
fn breakindent(
    line: &[u8],
    o: &BriOpt,
    tabstop: ruse_core::Tabs,
    width: usize,
    sbr_w: usize,
) -> usize {
    let mut indent = 0usize;
    for &b in line {
        match b {
            b' ' => indent += 1,
            b'\t' => indent += tabstop.padding(indent),
            _ => break,
        }
    }
    let base = if o.column > 0 {
        o.column
    } else {
        i64::try_from(indent).unwrap_or(i64::MAX)
    };
    let mut bri = base.saturating_add(o.shift);
    if o.sbr {
        bri -= i64::try_from(sbr_w).unwrap_or(0);
    }
    let w = i64::try_from(width).unwrap_or(i64::MAX);
    if bri < 0 {
        bri = 0;
    } else if bri > w - o.min {
        bri = (w - o.min).max(0);
    }
    usize::try_from(bri).unwrap_or(0)
}

pub(crate) fn str_width(s: &str) -> usize {
    s.graphemes(true)
        .map(|g| usize::from(cluster_width(g)))
        .sum()
}

/// The size nvim gives a char at virtual column `at` without wrapping (`win_chartabsize`): a tab to the
/// next stop (`^I`, two cells, under `'list'` without `tab:`), else the core's cells for the glyph
/// ([`cells::glyph_cells`]: `<ff>` for a byte that is not UTF-8, `^A`, `<85>`, `<200b>` for what nvim does
/// not print, #1181/#1251), so the cells painted are the virtual columns the core counts. `prev_invalid`:
/// the glyph before is a byte that is not UTF-8.
fn plain_size(
    g: Glyph<'_>,
    at: usize,
    ts: ruse_core::Tabs,
    list_tab: bool,
    uhex: bool,
    prev_invalid: bool,
) -> usize {
    if g == Glyph::Text("\t") {
        if list_tab {
            ts.padding(at)
        } else {
            2
        }
    } else {
        cells::glyph_cells(g, prev_invalid, uhex)
    }
}

/// The cells glyph `g` paints as when it does not paint as itself: `<ff>` for a byte that is not UTF-8,
/// the transcription of a char nvim does not print ([`crate::screen::transcribe`]).
fn shown_as(g: Glyph<'_>, uhex: bool) -> Option<String> {
    match g {
        Glyph::Byte(b) => Some(cells::invalid_byte_text(b)),
        Glyph::Text(s) => crate::screen::transcribe(s, uhex),
    }
}

/// A line to lay out: its bytes, which need not be UTF-8 (#1251).
pub(crate) trait LineBytes<'t> {
    fn line_bytes(self) -> &'t [u8];
}

impl<'t> LineBytes<'t> for &'t str {
    fn line_bytes(self) -> &'t [u8] {
        self.as_bytes()
    }
}

impl<'t> LineBytes<'t> for &'t [u8] {
    fn line_bytes(self) -> &'t [u8] {
        self
    }
}

/// How many leading bytes of `line` [`layout_from`] counts instead of walking, and the virtual column after
/// them: printable ASCII chars, each one cell and its own cluster, up to the char that starts the row
/// `min_vcol` is in (with `wrap`; without it, the char at `min_vcol`), never past `max_byte`. At least one
/// char of the run is left to walk, so the cut falls between two ASCII bytes.
///
/// With `wrap` and a continuation-row head `head_w` cells wide, the first row holds `width` chars and each
/// later one `width - head_w`, the head being in the size of the char that starts the row (nvim's
/// `charsize_regular`): row `r > 0` starts at char `width + (r - 1) * (width - head_w)`, at virtual column
/// `r * width`.
fn ascii_skip(
    line: &[u8],
    wrap: bool,
    width: usize,
    head_w: usize,
    min_vcol: usize,
    max_byte: usize,
) -> (usize, usize) {
    if min_vcol == 0 {
        return (0, 0);
    }
    let run = line
        .iter()
        .position(|b| !(0x20..0x7f).contains(b))
        .unwrap_or(line.len());
    let most = run.saturating_sub(1).min(max_byte);
    if !wrap {
        let n = most.min(min_vcol);
        return (n, n);
    }
    let per_row = (width - head_w).max(1);
    let by_chars = if most < width {
        0
    } else {
        1 + (most - width) / per_row
    };
    let r = by_chars.min(min_vcol / width);
    if r == 0 {
        (0, 0)
    } else {
        (width + (r - 1) * per_row, r * width)
    }
}

/// The per-line facts the layout walk reads.
struct Walk<'t, 'a> {
    lay: &'a Layout<'t>,
    width: usize,
    ts: ruse_core::Tabs,
    list_tab: bool,
    sbr: &'t str,
    bri: usize,
    head_w: usize,
    bytes: &'t [u8],
    graphemes: Vec<(usize, Glyph<'t>)>,
    /// The byte 'linebreak' starts at (after the leading break characters), when it applies.
    lbr_from: Option<usize>,
    lead_end: usize,
    trail_start: usize,
}

impl<'t, 'a> Walk<'t, 'a> {
    /// The facts the walk of `line` reads, and the virtual columns of the plain ASCII run it skips before
    /// `min_vcol` ([`layout_from`]; only when `skippable`, i.e. nothing is concealed).
    fn new(
        line: &'t [u8],
        lay: &'a Layout<'t>,
        skippable: bool,
        min_vcol: usize,
        max_vcol: usize,
        max_byte: usize,
    ) -> (Walk<'t, 'a>, usize) {
        // Every char takes a cell (but concealed ones), so the graphemes past the limits plus a margin for
        // the 'linebreak' look-ahead are never read.
        let slack = 1024;
        let width = lay.width.max(1);
        let ts = lay.tabstop;
        let sbr = if lay.wrap { lay.showbreak } else { "" };
        let sbr_w = str_width(sbr);
        let bri = match (lay.wrap, &lay.breakindent) {
            (true, Some(o)) => breakindent(line, o, ts, width, sbr_w),
            _ => 0,
        };
        let bytes = line;
        let blank = |b: &&u8| matches!(**b, b' ' | b'\t');
        let head_w = (bri + sbr_w).min(width.saturating_sub(1));
        let (skip_bytes, skip_vcol) = if skippable && !(lay.wrap && lay.linebreak) {
            ascii_skip(
                line,
                lay.wrap,
                width,
                head_w,
                min_vcol.min(max_vcol),
                max_byte,
            )
        } else {
            (0, 0)
        };
        let w = Walk {
            lay,
            width,
            ts,
            list_tab: lay.list.is_none_or(|l| l.tab.is_some()),
            sbr,
            bri,
            // A continuation row's head never takes the whole row.
            head_w,
            bytes,
            // The run skipped ends between two ASCII bytes, a cluster boundary with no context reaching back
            // across it, so the rest segments as it does in the whole line.
            graphemes: cells::glyphs(&line[skip_bytes..])
                .map(|(i, g)| (i + skip_bytes, g))
                .take_while(|&(i, _)| i <= max_byte.saturating_add(slack))
                .take(max_vcol.saturating_sub(skip_vcol).saturating_add(slack))
                .collect(),
            lbr_from: (lay.wrap && lay.linebreak)
                .then(|| bytes.iter().take_while(|&&b| lay.breakat.has(b)).count()),
            lead_end: bytes.iter().take_while(blank).count(),
            trail_start: bytes.len() - bytes.iter().rev().take_while(blank).count(),
        };
        (w, skip_vcol)
    }
}

impl<'t> Walk<'t, '_> {
    fn is_break(&self, b: usize, g: Glyph<'_>) -> bool {
        matches!(g, Glyph::Text(s) if s.len() == 1) && self.lay.breakat.has(self.bytes[b])
    }

    /// Whether the glyph before glyph `gi` is a byte that is not UTF-8.
    fn prev_invalid(&self, gi: usize) -> bool {
        gi > 0 && matches!(self.graphemes[gi - 1].1, Glyph::Byte(_))
    }

    /// The size and head of the char `gi` at `vcol` (nvim's `charsize_regular`).
    fn size(&self, gi: usize, vcol: usize) -> (usize, usize, usize) {
        let (b, g) = self.graphemes[gi];
        let width = self.width;
        let is_tab = g == Glyph::Text("\t");
        let mut size = plain_size(
            g,
            vcol,
            self.ts,
            self.list_tab,
            self.lay.uhex,
            self.prev_invalid(gi),
        );
        let mut mb_added = 0;
        if self.lay.wrap && size == 2 && !is_tab && g.len() > 1 && vcol % width == width - 1 {
            // A wide glyph that does not fit in the row's last cell: a `>` there, the glyph below.
            size += 1;
            mb_added = 1;
        }
        let mut head = mb_added;
        if self.lay.wrap && self.head_w > 0 {
            let mut added = 0;
            let mut col = vcol % width;
            if vcol >= width && col == 0 {
                // The char starts a continuation row: the row's head comes first.
                added += self.head_w;
                col += self.head_w;
            }
            if col + size > width {
                // The char runs into the next rows: each starts with the head too.
                let prev_rem = width - col;
                let w = (width - self.head_w).max(1);
                added += (size - prev_rem).div_ceil(w) * self.head_w;
            }
            head += added;
            size += added;
        }
        if let Some(from) = self.lbr_from {
            let next_break = self
                .graphemes
                .get(gi + 1)
                .is_some_and(|&(nb, ng)| self.is_break(nb, ng));
            if b >= from && self.is_break(b, g) && !next_break {
                size = self.linebreak_size(gi, vcol, size);
            }
        }
        (size, head, mb_added)
    }

    /// nvim's `need_lbr` block: a break char before a word that does not fit in the row is padded to the
    /// row's end. The column arithmetic is nvim's, kept as is.
    fn linebreak_size(&self, gi: usize, vcol: usize, size: usize) -> usize {
        let as_i = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        let (w, v) = (as_i(self.width), as_i(vcol));
        let col_adj = as_i(size) - 1;
        let mut colmax = w - col_adj;
        if v >= colmax {
            colmax += col_adj;
            let n = colmax;
            if n > 0 {
                colmax += ((v - colmax) / n + 1) * n - col_adj;
            }
        }
        let mut vcol2 = v;
        let mut prev_break = true;
        for (k, &(nb, ng)) in self.graphemes[gi + 1..].iter().enumerate() {
            let is_break = self.is_break(nb, ng);
            if !(is_break || vcol2 == v || !prev_break) {
                break;
            }
            let at = usize::try_from(vcol2).unwrap_or(0);
            let prev_invalid = self.prev_invalid(gi + 1 + k);
            vcol2 += as_i(plain_size(
                ng,
                at,
                self.ts,
                self.list_tab,
                self.lay.uhex,
                prev_invalid,
            ));
            if vcol2 >= colmax {
                // The padding only ever grows the char: a tab in the last cell keeps its width
                // (nvim draws it whole there).
                return usize::try_from(colmax - v + col_adj)
                    .unwrap_or(size)
                    .max(size);
            }
            prev_break = is_break;
        }
        size
    }

    /// The run of spaces around byte `b`, for `multispace` / `leadmultispace`.
    fn space_run(&self, b: usize) -> (usize, usize) {
        let mut s = b;
        while s > 0 && self.bytes[s - 1] == b' ' {
            s -= 1;
        }
        let mut e = b + 1;
        while e < self.bytes.len() && self.bytes[e] == b' ' {
            e += 1;
        }
        (s, e)
    }

    /// The text and highlight a non-tab grapheme paints, with `'list'` applied to spaces and nbsp.
    fn glyph(&self, b: usize, g: &'t str) -> (CellText<'t>, CellKind) {
        let text = (CellText::Str(g), CellKind::Text);
        let Some(lc) = self.lay.list else {
            return text;
        };
        let ws = |c: char| (CellText::Char(c), CellKind::Whitespace);
        if g == "\u{a0}" || g == "\u{202f}" {
            return lc.nbsp.map_or(text, ws);
        }
        if g != " " {
            return text;
        }
        if b >= self.trail_start {
            if let Some(c) = lc.trail {
                return ws(c);
            }
        }
        let (s, e) = self.space_run(b);
        if b < self.lead_end {
            if e - s > 1 && !lc.leadmultispace.is_empty() {
                return ws(lc.leadmultispace[(b - s) % lc.leadmultispace.len()]);
            }
            if let Some(c) = lc.lead {
                return ws(c);
            }
        }
        if e - s > 1 && !lc.multispace.is_empty() {
            return ws(lc.multispace[(b - s) % lc.multispace.len()]);
        }
        lc.space.map_or(text, ws)
    }

    /// The `k`th content cell of a tab `content` cells wide.
    fn tab_cell(&self, k: usize, content: usize) -> CellText<'t> {
        match self.lay.list {
            // `tab:xy[z]`: x first, then y, z last (z alone on a one-cell tab).
            Some(ListChars {
                tab: Some((x, y, z)),
                ..
            }) => CellText::Char(match (k, z) {
                (_, Some(z)) if k + 1 == content => *z,
                (0, _) => *x,
                _ => *y,
            }),
            // 'list' without `tab:` shows `^I`.
            Some(_) => CellText::Char(if k == 0 { '^' } else { 'I' }),
            None => CellText::Blank,
        }
    }
}

/// Lay out `line` (no trailing newline). `conceal` holds the line-relative byte ranges hidden from the
/// layout, each with the text painted in its place (F-031; empty on the caret line).
pub(crate) fn layout<'t>(
    line: impl LineBytes<'t>,
    lay: &Layout<'t>,
    conceal: &[(usize, usize, Option<&'t str>)],
) -> LineLayout<'t> {
    layout_upto(line, lay, conceal, usize::MAX, usize::MAX)
}

/// The layout of `line` up to byte `byte`, without its cells: where the cursor on `byte` is.
pub(crate) fn cursor_layout<'t>(
    line: impl LineBytes<'t>,
    lay: &Layout<'t>,
    byte: usize,
) -> LineLayout<'t> {
    layout_upto(line, lay, &[], usize::MAX, byte)
}

/// [`layout`] of the start of `line` only: the chars before virtual column `max_vcol` and up to byte
/// `max_byte`, so a very long line (a minified file, a huge count insert) costs what the window shows,
/// not its length. `rows` and `total` then count the part laid out.
pub(crate) fn layout_upto<'t>(
    line: impl LineBytes<'t>,
    lay: &Layout<'t>,
    conceal: &[(usize, usize, Option<&'t str>)],
    max_vcol: usize,
    max_byte: usize,
) -> LineLayout<'t> {
    layout_from(line, lay, conceal, 0, max_vcol, max_byte)
}

/// The rows [`layout_upto`] gives `line` up to virtual column `max_vcol`, without laying out the plain
/// ASCII run it starts with ([`layout_from`]): what the viewport and the cursor-row count read of a line.
pub(crate) fn rows_upto<'t>(line: impl LineBytes<'t>, lay: &Layout<'t>, max_vcol: usize) -> usize {
    layout_from(line, lay, &[], max_vcol, max_vcol, usize::MAX).rows
}

/// [`layout_upto`] that need not lay out what lies before virtual column `min_vcol` (#1294). The plain ASCII
/// run a line starts with (printable bytes, one cell and one cluster each) is counted in closed form, up to
/// the start of the row `min_vcol` is in (the column itself without `'wrap'`), and only the rest is walked.
/// So a window scrolled far into a huge line (a count insert, a minified file) costs the rows it shows, not
/// the line's length. [`LineLayout::chars`] and [`LineLayout::cells`] then start at the first char walked;
/// `total` and `rows` are those of the whole layout. With `conceal` or `'linebreak'` nothing is skipped.
pub(crate) fn layout_from<'t>(
    line: impl LineBytes<'t>,
    lay: &Layout<'t>,
    conceal: &[(usize, usize, Option<&'t str>)],
    min_vcol: usize,
    max_vcol: usize,
    max_byte: usize,
) -> LineLayout<'t> {
    let (w, skip_vcol) = Walk::new(
        line.line_bytes(),
        lay,
        conceal.is_empty(),
        min_vcol,
        max_vcol,
        max_byte,
    );
    let mut out = LineLayout {
        chars: Vec::with_capacity(w.graphemes.len()),
        cells: Vec::with_capacity(w.graphemes.len()),
        total: 0,
        rows: 1,
        width: w.width,
        wrap: lay.wrap,
        with_cells: max_vcol != usize::MAX || max_byte == usize::MAX,
    };
    let mut vcol = skip_vcol;
    let mut truncated = false;
    for gi in 0..w.graphemes.len() {
        let (b, g) = w.graphemes[gi];
        if vcol >= max_vcol || b > max_byte {
            truncated = true;
            break;
        }
        if let Some(&(s, _, virt)) = conceal.iter().find(|&&(s, e, _)| b >= s && b < e) {
            vcol = out.push_concealed(vcol, b, g.len(), (s == b).then_some(virt));
            continue;
        }
        vcol += out.push_char(&w, gi, vcol);
    }
    out.finish(&w, vcol, truncated);
    out
}

impl<'t> LineLayout<'t> {
    /// The screen `(row, col)` of virtual column `vcol` (row 0 without wrapping).
    pub(crate) fn place(&self, vcol: usize) -> (usize, usize) {
        if self.wrap {
            (vcol / self.width, vcol % self.width)
        } else {
            (0, vcol)
        }
    }

    /// The index in [`LineLayout::chars`] of the char holding byte offset `byte`, or `None` past the end.
    pub(crate) fn char_index(&self, byte: usize) -> Option<usize> {
        match self.chars.binary_search_by(|c| c.byte.cmp(&byte)) {
            Ok(i) => Some(i),
            Err(i) if i < self.chars.len() => i.checked_sub(1),
            Err(i) => i
                .checked_sub(1)
                .filter(|&j| byte < self.chars[j].byte + self.chars[j].len),
        }
    }

    /// The virtual column the cursor on byte `byte` shows at (nvim's `getvvcol` cursor column): the
    /// glyph's cell, or a tab's LAST cell when `tab_end` (Normal mode without `'list'`). Past the end of
    /// the line it is the cell after the text (after the head when that starts a row).
    pub(crate) fn cursor_vcol(&self, byte: usize, tab_end: bool) -> usize {
        match self.char_index(byte) {
            Some(i) => {
                let c = self.chars[i];
                if c.is_tab && tab_end {
                    c.vcol + c.size.max(1) - 1
                } else {
                    c.vcol + c.head
                }
            }
            None => self.total,
        }
    }

    /// The char whose span holds virtual column `wcol` (nvim's `coladvance`), clamped to the last char;
    /// `None` on an empty (or wholly concealed) line.
    pub(crate) fn char_at_vcol(&self, wcol: usize) -> Option<usize> {
        let mut last = None;
        for (i, c) in self.chars.iter().enumerate() {
            if c.size == 0 {
                continue;
            }
            if wcol < c.vcol + c.size {
                return Some(i);
            }
            last = Some(i);
        }
        last
    }
}

mod cell_steps;

#[cfg(test)]
mod tests;
