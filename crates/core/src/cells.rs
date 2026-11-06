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

#[cfg(test)]
mod tests {
    use super::*;

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
