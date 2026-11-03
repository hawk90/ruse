//! The text the Ex print commands show (#609): `:p`, `:l`, `:nu`/`:#`, `:z`, and the `p`/`#`/`l` flags of
//! `:s` — each line the way nvim's `print_line` writes it into the message area.

use unicode_width::UnicodeWidthChar;

use crate::input::PrintFlags;

/// The width of the line-number column: at least 3 digits, wider for a longer buffer (nvim's
/// `number_width` with the default `'numberwidth'` of 4, less the separating blank).
pub(crate) fn number_width(nlines: usize) -> usize {
    nlines.to_string().len().max(3)
}

/// One line as a print command shows it: the line number first under `#`, then the text with tabs
/// expanded to `tabstop` and control characters as `^X`. Under `l` (`:list`) a tab shows as `>` and
/// blanks, a no-break space as `+`, and trailing spaces as `-` (nvim's default `'listchars'`).
pub(crate) fn print_line(
    text: &str,
    lnum: usize,
    flags: PrintFlags,
    width: usize,
    ts: usize,
) -> String {
    let mut out = String::new();
    if flags.number {
        out.push_str(&format!("{lnum:>width$} "));
    }
    let ts = ts.max(1);
    let trail = text.trim_end_matches([' ', '\t']).len();
    let mut vcol = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '\t' => {
                let w = ts - vcol % ts;
                if flags.list {
                    out.push('>');
                    out.extend(std::iter::repeat_n(' ', w - 1));
                } else {
                    out.extend(std::iter::repeat_n(' ', w));
                }
                vcol += w;
            }
            ' ' if flags.list && i >= trail => {
                out.push('-');
                vcol += 1;
            }
            '\u{a0}' if flags.list => {
                out.push('+');
                vcol += 1;
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push('^');
                out.push(if c == '\u{7f}' {
                    '?'
                } else {
                    char::from(c as u8 + b'@')
                });
                vcol += 2;
            }
            c => {
                out.push(c);
                vcol += c.width().unwrap_or(0);
            }
        }
    }
    out
}

/// The 1-based lines `first..=last` of `bytes` as print rows, skipping those `keep` rejects (`:filter`).
pub(crate) fn rows(
    bytes: &[u8],
    first: usize,
    last: usize,
    flags: PrintFlags,
    ts: usize,
    keep: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    let width = number_width(lines.len().max(1));
    (first.max(1)..=last.min(lines.len().max(1)))
        .filter_map(|n| {
            let line = lines.get(n - 1).copied().unwrap_or("");
            keep(line).then(|| print_line(line, n, flags, width, ts))
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
        assert_eq!(print_line("ab\tc", 1, plain, 3, 8), "ab      c");
        assert_eq!(print_line("ab\tc", 1, number, 3, 8), "  1 ab      c");
        assert_eq!(print_line("a\tb  ", 1, list, 3, 8), "a>      b--");
        assert_eq!(print_line("a\u{a0}b \t", 1, list, 3, 8), "a+b->   ");
        assert_eq!(print_line("a\u{7f}b\u{1b}", 1, plain, 3, 8), "a^?b^[");
        assert_eq!(print_line("j", 10, number, 3, 8), " 10 j");
        assert_eq!(number_width(1000), 4);
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
