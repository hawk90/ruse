//! The one display-width model (F-002, #1251): how many screen cells a piece of a line takes.
//!
//! The renderer paints a line and the core counts virtual columns (`{count}|`, `gM`, the `j`/`k` wanted
//! column, blockwise Visual, `g CTRL-G`, `'textwidth'`), and both must agree on every cell. They do by calling
//! the functions here, which follow nvim v0.12.6 (`charsize.c`, `transchar`, `transchar_hex`):
//!
//! - a byte that belongs to no valid UTF-8 char is drawn `<xx>` (lowercase hex) and is
//!   [`INVALID_BYTE_CELLS`] wide;
//! - a C0 control and DEL are `^X` (two cells, `<xx>` under `'display'` `uhex`), a C1 control `<xx>`, and the
//!   format characters nvim does not print `<xxxx>` ([`unprintable`]);
//! - any other grapheme cluster takes its base char's terminal width, at least one cell
//!   ([`cluster_width`]); a zero-width cluster right after an invalid byte composes onto it and adds nothing.
//!
//! A `<Tab>` is the caller's: its width depends on the column it starts at and on `'list'`.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// The cells a byte that is not UTF-8 takes: nvim draws it as `<xx>`.
pub const INVALID_BYTE_CELLS: usize = 4;

/// The characters nvim v0.12.6's `utf_printable` refuses above U+00FF (`mbyte.c` `nonprint`): format,
/// bidi-control and other invisible characters, shown as `<xxxx>`.
const NONPRINT: &[(u32, u32)] = &[
    (0x070f, 0x070f),
    (0x180b, 0x180e),
    (0x200b, 0x200f),
    (0x202a, 0x202e),
    (0x2060, 0x206f),
    (0xd800, 0xdfff),
    (0xfeff, 0xfeff),
    (0xfff9, 0xfffb),
    (0xfffe, 0xffff),
];

/// Whether `c` is a C0 control, DEL or a C1 control: a byte the terminal would act on (#1181).
#[must_use]
pub fn is_control(c: char) -> bool {
    let n = u32::from(c);
    n < 0x20 || (0x7f..0xa0).contains(&n)
}

/// How nvim v0.12.6 shows a char it does not print (`transchar`, #1181): `^X` for a C0 control and
/// `^?` for DEL (`<xx>` under `'display'` `uhex`), `<xx>` for a C1 control, `<xxxx>` for the format
/// characters nvim does not print. `None` for a printable char; a tab is the layout's.
#[must_use]
pub fn unprintable(c: char, uhex: bool) -> Option<String> {
    let n = u32::from(c);
    if n < 0x20 || n == 0x7f {
        return Some(if uhex {
            format!("<{n:02x}>")
        } else {
            format!("^{}", char::from_u32(n ^ 0x40).unwrap_or('?'))
        });
    }
    if (0x80..0xa0).contains(&n) {
        return Some(format!("<{n:02x}>"));
    }
    if n < NONPRINT[0].0 {
        return None;
    }
    NONPRINT
        .iter()
        .any(|&(a, b)| (a..=b).contains(&n))
        .then(|| format!("<{n:04x}>"))
}

/// The cells grapheme cluster `g` paints as when nvim would not print it raw: its unprintable chars
/// transcribed ([`unprintable`]), printable ones kept. `None` when `g` paints as itself. Only a cluster
/// that starts with an unprintable char, or holds a control anywhere, is transcribed: a zero-width joiner
/// inside an emoji sequence stays.
#[must_use]
pub fn transcribe(g: &str, uhex: bool) -> Option<String> {
    // The common case first: one printable ASCII byte (a layout asks for every grapheme of a line).
    if let [b] = g.as_bytes() {
        if (0x20..0x7f).contains(b) {
            return None;
        }
    }
    let first = g.chars().next()?;
    let head = unprintable(first, uhex);
    if head.is_none() && !g.chars().any(is_control) {
        return None;
    }
    let mut out = String::new();
    for (i, c) in g.chars().enumerate() {
        match unprintable(c, uhex) {
            Some(r) if i == 0 || is_control(c) => out.push_str(&r),
            _ => out.push(c),
        }
    }
    Some(out)
}

/// How nvim shows a byte that is not UTF-8: `<xx>`, lowercase hex (`transchar_hex`).
#[must_use]
pub fn invalid_byte_text(b: u8) -> String {
    format!("<{b:02x}>")
}

/// The display width, in terminal columns, of the grapheme cluster `g` as the terminal prints it: its base
/// char's width (1 for normal, 2 for wide/CJK/emoji), floored at 1 for any non-empty cluster so a cluster
/// always occupies at least one cell. 0 for an empty one.
#[must_use]
pub fn cluster_width(g: &str) -> usize {
    match g.chars().next() {
        None => 0,
        Some(base) => UnicodeWidthChar::width(base).unwrap_or(0).max(1),
    }
}

/// The cells grapheme cluster `g` (not a tab) takes on the screen: its transcription's when nvim does not
/// print it raw ([`transcribe`]: `^A`, `<85>`, `<200b>`), else [`cluster_width`]. `after_invalid` is whether
/// a byte that is not UTF-8 comes right before it: a zero-width cluster there (a combining mark) composes
/// onto that byte's `<xx>`, as nvim counts it, and takes no cell.
#[must_use]
pub fn text_cells(g: &str, after_invalid: bool, uhex: bool) -> usize {
    if let [b] = g.as_bytes() {
        if (0x20..0x7f).contains(b) {
            return 1;
        }
    }
    if let Some(t) = transcribe(g, uhex) {
        return t.graphemes(true).map(cluster_width).sum();
    }
    if after_invalid
        && g.chars()
            .next()
            .is_some_and(|c| UnicodeWidthChar::width(c) == Some(0))
    {
        return 0;
    }
    cluster_width(g)
}

/// One display unit of a line: a grapheme cluster of a valid UTF-8 run, or one byte that belongs to no valid
/// char.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph<'a> {
    Text(&'a str),
    Byte(u8),
}

impl Glyph<'_> {
    /// Its length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Glyph::Text(s) => s.len(),
            Glyph::Byte(_) => 1,
        }
    }

    /// Whether it is empty (never, for a glyph of [`glyphs`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The glyphs of `line` with their byte offsets: the grapheme clusters of each valid UTF-8 run, segmented as
/// if the run stood alone, and each byte that belongs to no valid char on its own, as the core's motions
/// count cells (#1232).
pub fn glyphs(line: &[u8]) -> impl Iterator<Item = (usize, Glyph<'_>)> {
    let base = line.as_ptr() as usize;
    line.utf8_chunks().flat_map(move |chunk| {
        let valid = chunk.valid();
        let at = valid.as_ptr() as usize - base;
        let bad = chunk.invalid();
        let bad_at = bad.as_ptr() as usize - base;
        valid
            .grapheme_indices(true)
            .map(move |(i, g)| (at + i, Glyph::Text(g)))
            .chain(
                bad.iter()
                    .enumerate()
                    .map(move |(i, &b)| (bad_at + i, Glyph::Byte(b))),
            )
    })
}

/// Where the last glyph of `line` (a line without its newline) starts, as [`glyphs`] splits it; `None` when
/// the line is empty. Reads only the end of the line.
#[must_use]
pub fn last_glyph(line: &[u8]) -> Option<usize> {
    (!line.is_empty()).then(|| crate::motion::prev_grapheme(line, line.len()))
}

/// The cells glyph `g` (not a tab) takes, `prev_invalid` being whether the glyph before it is a
/// [`Glyph::Byte`] (see [`text_cells`]).
#[must_use]
pub fn glyph_cells(g: Glyph<'_>, prev_invalid: bool, uhex: bool) -> usize {
    match g {
        Glyph::Text(s) => text_cells(s, prev_invalid, uhex),
        Glyph::Byte(_) => INVALID_BYTE_CELLS,
    }
}

/// The most stops a `'vartabstop'` / `'varsofttabstop'` list holds in ruse (nvim sets no limit).
pub const MAX_VAR_STOPS: usize = 64;
/// The widest tab stop nvim accepts (`TABSTOP_MAX`).
pub const MAX_STOP: usize = 9999;

/// A `'vartabstop'` / `'varsofttabstop'` list (#1163): the widths of the successive tab stops, the last
/// one repeating; empty when the option is unset. Parsed as nvim's `tabstop_set` does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VarStops {
    n: u8,
    stops: [u16; MAX_VAR_STOPS],
}

impl Default for VarStops {
    fn default() -> Self {
        VarStops::NONE
    }
}

impl VarStops {
    /// No stops (the option unset).
    pub const NONE: VarStops = VarStops {
        n: 0,
        stops: [0; MAX_VAR_STOPS],
    };

    /// Parse an option value: comma-separated widths `1..=`[`MAX_STOP`] (`""` and `"0"` mean unset).
    /// `None` for anything else, which nvim refuses with `E474`.
    #[must_use]
    pub fn parse(s: &str) -> Option<VarStops> {
        if s.is_empty() || s == "0" {
            return Some(VarStops::NONE);
        }
        let mut out = VarStops::NONE;
        for part in s.split(',') {
            if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let n: usize = part.parse().ok()?;
            if n == 0 || n > MAX_STOP || usize::from(out.n) == MAX_VAR_STOPS {
                return None;
            }
            out.stops[usize::from(out.n)] = u16::try_from(n).ok()?;
            out.n += 1;
        }
        Some(out)
    }

    /// The number of stops (`tabstop_count`).
    #[must_use]
    pub fn count(&self) -> usize {
        usize::from(self.n)
    }

    /// Whether the list is empty (the option unset).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The stop widths.
    #[must_use]
    pub fn stops(&self) -> &[u16] {
        &self.stops[..self.count()]
    }

    /// The sum of the first `k` stop widths (`get_vts_sum`).
    #[must_use]
    pub fn sum(&self, k: usize) -> usize {
        let s = self.stops();
        let inside: usize = s.iter().take(k).map(|&w| usize::from(w)).sum();
        inside + k.saturating_sub(s.len()) * usize::from(*s.last().unwrap_or(&0))
    }
}

/// The tab stops a line is laid out with (#1163): `'tabstop'` and, when set, `'vartabstop'`, which then
/// wins. Every width and column computation that meets a `<Tab>` goes through this, as nvim's goes
/// through `tabstop_padding` and friends (`indent.c`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Tabs {
    ts: usize,
    vts: VarStops,
}

impl From<usize> for Tabs {
    fn from(ts: usize) -> Tabs {
        Tabs::uniform(ts)
    }
}

impl Tabs {
    /// A stop every `ts` columns.
    #[must_use]
    pub const fn uniform(ts: usize) -> Tabs {
        Tabs {
            ts: if ts == 0 { 1 } else { ts },
            vts: VarStops::NONE,
        }
    }

    /// `'tabstop'` `ts` with `'vartabstop'` `vts`.
    #[must_use]
    pub fn new(ts: usize, vts: VarStops) -> Tabs {
        Tabs { ts: ts.max(1), vts }
    }

    /// `'tabstop'`.
    #[must_use]
    pub fn tabstop(&self) -> usize {
        self.ts
    }

    /// `'vartabstop'`.
    #[must_use]
    pub fn var(&self) -> &VarStops {
        &self.vts
    }

    /// The cells a `<Tab>` starting at virtual column `col` takes (`tabstop_padding`).
    #[must_use]
    pub fn padding(&self, col: usize) -> usize {
        padding(col, self.ts, &self.vts)
    }

    /// The width of the tab interval holding `col`, or (`left`) the one before it (`tabstop_at`).
    #[must_use]
    pub fn interval_width(&self, col: usize, left: bool) -> usize {
        let s = self.vts.stops();
        if s.is_empty() {
            return self.ts;
        }
        let mut tabcol = 0usize;
        for (t, &w) in s.iter().enumerate() {
            tabcol += usize::from(w);
            if tabcol > col {
                return if left && t == 0 {
                    col
                } else if left {
                    usize::from(s[t - 1])
                } else {
                    usize::from(w)
                };
            }
        }
        usize::from(s[s.len() - 1])
    }

    /// The column the tab interval holding `col` starts at (`tabstop_start`).
    #[must_use]
    pub fn start_col(&self, col: usize) -> usize {
        start_col(col, self.ts, &self.vts)
    }

    /// The tabs and spaces that fill the columns `[from, to)` (`tabstop_fromto`).
    #[must_use]
    pub fn fromto(&self, from_col: usize, to_col: usize) -> (usize, usize) {
        let (from, to) = (from_col, to_col);
        let spaces = to.saturating_sub(from);
        let s = self.vts.stops();
        if s.is_empty() {
            let ts = self.ts;
            let initspc = ts - from % ts;
            let (mut tabs, mut sp) = (0, spaces);
            if sp >= initspc {
                sp -= initspc;
                tabs += 1;
            }
            return (tabs + sp / ts, sp % ts);
        }
        let mut tabcol = 0usize;
        let mut t = 0usize;
        let mut pad = None;
        while t < s.len() {
            tabcol += usize::from(s[t]);
            if tabcol > from {
                pad = Some(tabcol - from);
                break;
            }
            t += 1;
        }
        let last = usize::from(s[s.len() - 1]);
        let pad = pad.unwrap_or_else(|| last - (from - tabcol) % last);
        if spaces < pad {
            return (0, spaces);
        }
        let (mut tabs, mut sp) = (1usize, spaces - pad);
        while sp != 0 {
            t += 1;
            if t >= s.len() {
                break;
            }
            let w = usize::from(s[t]);
            if sp < w {
                return (tabs, sp);
            }
            tabs += 1;
            sp -= w;
        }
        (tabs + sp / last, sp % last)
    }

    /// The leading whitespace that spans `cols` columns from column 0: tabs then spaces (`noexpandtab`).
    #[must_use]
    pub fn fill(&self, cols: usize) -> Vec<u8> {
        let (t, s) = self.fromto(0, cols);
        let mut v = vec![b'\t'; t];
        v.extend(std::iter::repeat_n(b' ', s));
        v
    }
}

/// `tabstop_padding` for `'tabstop'` (or `'softtabstop'`) `ts` and the stops `vts`.
#[must_use]
pub fn padding(col: usize, ts: usize, vts: &VarStops) -> usize {
    let s = vts.stops();
    if s.is_empty() {
        let ts = if ts == 0 { 8 } else { ts };
        return ts - col % ts;
    }
    let mut tabcol = 0usize;
    for &w in s {
        tabcol += usize::from(w);
        if tabcol > col {
            return tabcol - col;
        }
    }
    let last = usize::from(s[s.len() - 1]);
    last - (col - tabcol) % last
}

/// `tabstop_start` for `ts` (or `'softtabstop'`) and the stops `vts`.
#[must_use]
pub fn start_col(col: usize, ts: usize, vts: &VarStops) -> usize {
    let s = vts.stops();
    if s.is_empty() {
        let ts = ts.max(1);
        return col - col % ts;
    }
    let mut tabcol = 0usize;
    for &w in s {
        tabcol += usize::from(w);
        if tabcol > col {
            return tabcol - usize::from(w);
        }
    }
    let last = usize::from(s[s.len() - 1]);
    let excess = tabcol % last;
    col - (col - excess) % last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(s: &str) -> Tabs {
        Tabs::new(8, VarStops::parse(s).expect("valid"))
    }

    /// #1163: nvim's `tabstop_padding`, `tabstop_at`, `tabstop_start`, `tabstop_fromto`.
    #[test]
    fn var_tab_stops_follow_nvim() {
        let t = tabs("4,8");
        assert_eq!(
            (t.padding(0), t.padding(3), t.padding(4), t.padding(12)),
            (4, 1, 8, 8)
        );
        assert_eq!((t.padding(13), t.padding(20)), (7, 8));
        assert_eq!(
            (
                t.interval_width(0, false),
                t.interval_width(4, false),
                t.interval_width(30, false)
            ),
            (4, 8, 8)
        );
        assert_eq!(
            (t.interval_width(4, true), t.interval_width(2, true)),
            (4, 2)
        );
        assert_eq!(
            (t.start_col(3), t.start_col(5), t.start_col(13)),
            (0, 4, 12)
        );
        assert_eq!(t.fromto(0, 8), (1, 4));
        assert_eq!(t.fromto(0, 12), (2, 0));
        assert_eq!(t.fromto(5, 21), (2, 1));
        assert_eq!(tabs("3,8").fill(12), b"\t\t ".to_vec());
        assert_eq!(Tabs::uniform(4).fromto(2, 9), (2, 1));
        let v = VarStops::parse("2,6").expect("valid");
        assert_eq!((v.sum(0), v.sum(2), v.sum(4)), (0, 8, 20));
        assert_eq!(VarStops::parse(""), Some(VarStops::NONE));
        assert_eq!(VarStops::parse("0"), Some(VarStops::NONE));
        for bad in ["4,0", "a", "4,", ",4", "4,,8", "-1", "10000"] {
            assert_eq!(VarStops::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn widths_match_nvim() {
        // Probed in nvim 0.12.6 with `virtcol()` on each byte.
        assert_eq!(glyph_cells(Glyph::Byte(0xff), false, false), 4);
        assert_eq!(text_cells("\u{1}", false, false), 2, "^A");
        assert_eq!(text_cells("\u{1}", false, true), 4, "<01> under uhex");
        assert_eq!(text_cells("\u{7f}", false, false), 2, "^?");
        assert_eq!(text_cells("\u{85}", false, false), 4, "<85>");
        assert_eq!(text_cells("\u{feff}", false, false), 6, "<feff>");
        assert_eq!(text_cells("\u{a0}", false, false), 1);
        assert_eq!(text_cells("中", false, false), 2);
        assert_eq!(
            text_cells("\u{301}", false, false),
            1,
            "a lone mark shows on a cell"
        );
        assert_eq!(
            text_cells("\u{301}", true, false),
            0,
            "a mark after <ff> composes onto it"
        );
        assert_eq!(text_cells("e\u{301}", true, false), 1);
    }

    #[test]
    fn glyphs_split_invalid_bytes_and_keep_clusters() {
        let line = b"e\xcc\x81\xff\xea\xb0a";
        let got: Vec<_> = glyphs(line).collect();
        assert_eq!(
            got,
            vec![
                (0, Glyph::Text("e\u{301}")),
                (3, Glyph::Byte(0xff)),
                (4, Glyph::Byte(0xea)),
                (5, Glyph::Byte(0xb0)),
                (6, Glyph::Text("a")),
            ]
        );
        assert_eq!(invalid_byte_text(0xff), "<ff>");
    }
}
