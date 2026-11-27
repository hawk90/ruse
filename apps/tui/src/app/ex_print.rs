//! The text the Ex print commands show (#609): `:p`, `:l`, `:nu`/`:#`, `:z`, and the `p`/`#`/`l` flags of
//! `:s` — each line the way nvim's `print_line` writes it into the message area.

use ruse_core::cells::{self, Glyph};

use crate::input::PrintFlags;

/// The width of the line-number column: at least 3 digits, wider for a longer buffer (nvim's
/// `number_width` with the default `'numberwidth'` of 4, less the separating blank).
pub(crate) fn number_width(nlines: usize) -> usize {
    nlines.to_string().len().max(3)
}

/// One line as a print command shows it: the line number first under `#`, then the text with tabs
/// expanded to `tabstop` and C0 controls and DEL as `^X` (`<xx>` under `'display'` `uhex`). Under `l`
/// (`:list`) a tab shows as `>` and blanks, a no-break space as `+`, and trailing spaces as `-` (nvim's
/// default `'listchars'`).
///
/// Columns count as nvim's `msg_prt_line` counts them (#1269): a cluster takes its base char's cells from
/// [`cells::text_cells`] (`<85>` 4, `<200b>` 6, which the screen draws them as), and a byte that is not
/// UTF-8 is written as the Latin-1 char of its value (`ÿ` for 0xFF) and takes that char's cells.
pub(crate) fn print_line(
    text: &[u8],
    lnum: usize,
    flags: PrintFlags,
    width: usize,
    ts: impl Into<ruse_core::Tabs>,
    uhex: bool,
) -> String {
    let ts: ruse_core::Tabs = ts.into();
    let mut out = String::new();
    if flags.number {
        out.push_str(&format!("{lnum:>width$} "));
    }
    let trail = text.len()
        - text
            .iter()
            .rev()
            .take_while(|&&c| matches!(c, b' ' | b'\t'))
            .count();
    let mut vcol = 0usize;
    let mut latin1 = [0u8; 4];
    for (i, g) in cells::glyphs(text) {
        let g: &str = match g {
            Glyph::Text(s) => s,
            Glyph::Byte(b) => char::from(b).encode_utf8(&mut latin1),
        };
        match g {
            "\t" => {
                let w = ts.padding(vcol);
                if flags.list {
                    out.push('>');
                    out.extend(std::iter::repeat_n(' ', w - 1));
                } else {
                    out.extend(std::iter::repeat_n(' ', w));
                }
                vcol += w;
            }
            " " if flags.list && i >= trail => {
                out.push('-');
                vcol += 1;
            }
            _ if flags.list && g.starts_with(['\u{a0}', '\u{202f}']) => {
                out.push('+');
                vcol += 1;
            }
            _ => match g.as_bytes() {
                &[b] if b < 0x20 || b == 0x7f => {
                    let shown = cells::unprintable(char::from(b), uhex).unwrap_or_default();
                    vcol += shown.len();
                    out.push_str(&shown);
                }
                _ => {
                    out.push_str(g);
                    vcol += cells::text_cells(g, false, uhex);
                }
            },
        }
    }
    out
}

/// The 1-based lines `first..=last` of `bytes` as print rows, skipping those `keep` rejects (`:filter`).
/// `uhex` is `'display'` `uhex`.
pub(crate) fn rows(
    bytes: &[u8],
    first: usize,
    last: usize,
    flags: PrintFlags,
    ts: impl Into<ruse_core::Tabs>,
    uhex: bool,
    keep: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let ts: ruse_core::Tabs = ts.into();
    let mut lines: Vec<&[u8]> = bytes.split(|&c| c == b'\n').collect();
    if bytes.ends_with(b"\n") {
        lines.pop();
    }
    let width = number_width(lines.len().max(1));
    (first.max(1)..=last.min(lines.len().max(1)))
        .filter_map(|n| {
            let line = lines.get(n - 1).copied().unwrap_or_default();
            keep(&String::from_utf8_lossy(line))
                .then(|| print_line(line, n, flags, width, ts, uhex))
        })
        .collect()
}

/// The lines `:z` shows and where it leaves the cursor, as nvim's `ex_z` computes them. `lnum` is the
/// addressed line (the cursor's when `addressed` is false), `window` the default size (twice `'scroll'`,
/// or the screen height under `!`). Returns `(start, end, cursor)`, 1-based; `start > end` shows nothing.
pub(crate) fn z_window(
    spec: &crate::input::ZSpec,
    lnum: usize,
    addressed: bool,
    window: usize,
    nlines: usize,
) -> (usize, usize, usize) {
    let lnum = lnum as i64;
    let mut big = window.max(1) as i64;
    if let Some(n) = spec.count {
        big = (n as i64).min(2 * nlines as i64);
        if spec.kind == Some('=') {
            big += 2;
        }
    }
    let run = spec.run as i64;
    let (start, end, curs) = match spec.kind {
        Some('-') => {
            let start = lnum - big * run + 1;
            (start, start + big - 1, start + big - 1)
        }
        Some('=') | Some('.') => {
            let start = lnum - (big + 1) / 2 + 1;
            let end = lnum + (big + 1) / 2 - 1;
            (start, end, if spec.kind == Some('=') { lnum } else { end })
        }
        Some('^') => (lnum - big * 2, lnum - big, lnum - big),
        kind => {
            let mut start = lnum;
            if kind == Some('+') {
                start += big * (run - 1) + 1;
            } else if !addressed {
                start += 1;
            }
            (start, start + big - 1, start + big - 1)
        }
    };
    let n = nlines as i64;
    let start = start.max(1);
    let end = end.min(n);
    let curs = curs.clamp(1, n.max(1));
    (start as usize, end.max(0) as usize, curs as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::ZSpec;

    #[test]
    fn lines_render_like_nvim_print_line() {
        let plain = PrintFlags::default();
        let list = PrintFlags {
            list: true,
            ..PrintFlags::default()
        };
        let number = PrintFlags {
            number: true,
            ..PrintFlags::default()
        };
        assert_eq!(
            print_line("ab\tc".as_bytes(), 1, plain, 3, 8, false),
            "ab      c"
        );
        assert_eq!(
            print_line("ab\tc".as_bytes(), 1, number, 3, 8, false),
            "  1 ab      c"
        );
        assert_eq!(
            print_line("a\tb  ".as_bytes(), 1, list, 3, 8, false),
            "a>      b--"
        );
        assert_eq!(
            print_line("a\u{a0}b \t".as_bytes(), 1, list, 3, 8, false),
            "a+b->   "
        );
        assert_eq!(
            print_line("a\u{7f}b\u{1b}".as_bytes(), 1, plain, 3, 8, false),
            "a^?b^["
        );
        assert_eq!(print_line("j".as_bytes(), 10, number, 3, 8, false), " 10 j");
        assert_eq!(number_width(1000), 4);
    }

    #[test]
    fn lines_count_cells_like_nvim_msg_prt_line() {
        // #1269, checked against nvim v0.12.6 (`execute('%print')` / `'%list'`).
        let plain = PrintFlags::default();
        let list = PrintFlags {
            list: true,
            ..PrintFlags::default()
        };
        let p = |t: &[u8], f, uhex| print_line(t, 1, f, 3, 8, uhex);
        // A byte that is not UTF-8 is written as its Latin-1 char and takes that char's cells: 1 for 0xFF,
        // 4 for 0x85 (`<85>`).
        assert_eq!(p(b"a\xff\tb", plain, false), "a\u{ff}      b");
        assert_eq!(p(b"\xff\xff\tb", list, false), "\u{ff}\u{ff}>     b");
        assert_eq!(p(b"a\x85\tb", plain, false), "a\u{85}   b");
        // A char nvim does not print takes the cells of its transcription; a wide char 2.
        assert_eq!(p("a\u{85}\tb".as_bytes(), plain, false), "a\u{85}   b");
        assert_eq!(p("a\u{200b}\tb".as_bytes(), list, false), "a\u{200b}>b");
        assert_eq!(p("中\tb".as_bytes(), plain, false), "中      b");
        // A combining mark after a byte that is not UTF-8 is a char of its own, one cell.
        assert_eq!(p(b"\xff\xcc\x81\tb", plain, false), "\u{ff}\u{301}      b");
        assert_eq!(
            p("e\u{301}\tb".as_bytes(), plain, false),
            "e\u{301}       b"
        );
        // 'display' uhex: `<01>`, `<7f>`.
        assert_eq!(p(b"a\x01\tb", plain, true), "a<01>   b");
        assert_eq!(p(b"a\x7f\tb", list, true), "a<7f>>  b");
        // U+202F is a no-break space for 'listchars' too.
        assert_eq!(p("a\u{202f}b".as_bytes(), list, false), "a+b");
    }

    #[test]
    fn z_windows_match_ex_z() {
        let z = |kind, run, count| ZSpec {
            line: None,
            bang: false,
            flags: PrintFlags::default(),
            kind,
            run,
            count,
        };
        // Checked against nvim v0.12.6 on a 4-line buffer with a 22-line window.
        assert_eq!(z_window(&z(None, 0, None), 2, true, 22, 4), (2, 4, 4));
        assert_eq!(z_window(&z(None, 0, None), 1, false, 22, 4), (2, 4, 4));
        assert_eq!(z_window(&z(Some('-'), 1, None), 3, true, 22, 4), (1, 3, 3));
        assert_eq!(z_window(&z(Some('.'), 1, None), 2, true, 22, 4), (1, 4, 4));
        assert_eq!(z_window(&z(Some('^'), 1, None), 2, true, 22, 4), (1, 0, 1));
        assert_eq!(z_window(&z(Some('+'), 1, None), 1, false, 22, 4), (2, 4, 4));
        assert_eq!(z_window(&z(None, 0, Some(2)), 1, false, 22, 4), (2, 3, 3));
        assert_eq!(
            z_window(&z(Some('-'), 1, Some(2)), 3, true, 22, 4),
            (2, 3, 3)
        );
        assert_eq!(
            z_window(&z(Some('='), 1, Some(1)), 4, true, 22, 7),
            (3, 5, 4)
        );
        assert_eq!(z_window(&z(None, 0, Some(0)), 1, false, 22, 3), (2, 1, 1));
    }
}
