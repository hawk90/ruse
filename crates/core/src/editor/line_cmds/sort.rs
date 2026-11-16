//! `:sort` as nvim's `ex_sort` does it: the command ([`EditorState::sort_lines`]), the per-line key
//! ([`sort_key`], [`SortKey`]) and the number readers behind `n`/`x`/`o`/`b` ([`first_number`]) and `f`
//! ([`first_float`], a `strtod` prefix reader).

use super::{buffer_lines, line_start_of};
use crate::edit::{Edit, EditList};
use crate::editor::{line_spans, resolve_line_range, EditorState, SortKind, SortOptions, SubRange};
use crate::motion;
use crate::pattern::{Regex, RegexError};
use crate::pos::BytePos;

impl EditorState {
    /// `:[range]sort[!] [i][n][r][u] [/pattern/]` — sort the range's lines (whole file when the caller
    /// passes `WholeFile`) as one undo group, the way nvim's `ex_sort` does (v0.12.6). The sort KEY per
    /// line is the text after the first `pattern` match (the matched text itself under `r`), or the whole
    /// line; a line the pattern does not match has an empty key. Text keys compare bytewise, ignoring
    /// ASCII case under `i`. Under `n` the key is the first decimal number in it (a `-` just before it
    /// counts, and a number too large saturates); a key with no number sorts before every number. The
    /// sort is stable; `reverse` (`!`) reverses the sorted order, ties included. `unique` (`u`) then drops
    /// a line equal to the one kept before it, comparing WHOLE lines (case-folded under `i`), not keys.
    /// The pattern is matched with the `'ignorecase'` setting (no smartcase). The cursor lands on the
    /// first non-blank of the range's first line. Returns the lines removed by `u`.
    ///
    /// # Errors
    /// [`RegexError`] if `pattern` does not compile; the buffer is left as it was. A one-line range is a
    /// no-op that never compiles the pattern (Vim returns before reading the arguments).
    pub fn sort_lines(&mut self, range: SubRange, opts: &SortOptions) -> Result<usize, RegexError> {
        let bytes = self.doc.bytes();
        let hay: &[u8] = bytes;
        let had_trailing_nl = hay.ends_with(b"\n");
        let mut lines = buffer_lines(hay);
        if lines.is_empty() {
            return Ok(0);
        }
        let spans = line_spans(hay);
        let cursor_line = crate::pos::raw::line_of(hay, self.view.cursor);
        let (first, last) = resolve_line_range(range, &spans, cursor_line);
        let last = last.min(lines.len() - 1);
        let first = first.min(last);
        if first == last {
            return Ok(0);
        }

        let search = self.view.search_options();
        let compiled = match opts.pattern.as_deref() {
            Some(p) => Some(Regex::compile(
                p,
                crate::pattern::Options {
                    magic: search.magic,
                    ignore_case: search.ignore_case,
                    smart_case: false,
                },
            )?),
            None => None,
        };
        let mut keyed: Vec<(SortKey, &[u8])> = lines[first..=last]
            .iter()
            .map(|&l| (sort_key(l, compiled.as_ref(), opts), l))
            .collect();
        keyed.sort_by(|a, b| a.0.compare(&b.0)); // stable (Vim breaks ties by line number)
        if opts.reverse {
            keyed.reverse();
        }
        let mut seg: Vec<&[u8]> = keyed.into_iter().map(|(_, l)| l).collect();
        if opts.unique {
            if opts.ignore_case {
                seg.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            } else {
                seg.dedup();
            }
        }
        let removed = (last - first + 1) - seg.len();
        lines.splice(first..=last, seg);

        let mut new_bytes = lines.join(&b'\n');
        // One empty line is still a line: `\n`, not the emptied buffer (RFC-0024).
        if had_trailing_nl && !lines.is_empty() {
            new_bytes.push(b'\n');
        }
        let line_start = line_start_of(&new_bytes, first).min(new_bytes.len());
        let list = EditList::new(vec![Edit::lines_changed(self.doc.bytes(), new_bytes)])
            .expect("a single edit is well-formed");
        self.apply_ex_edit(list, self.origin, None)
            .expect("sort applies cleanly");
        self.view.last_was_edit = true;
        let nb = self.doc.bytes();
        self.view.cursor = motion::first_non_blank(nb, line_start.min(nb.len()));
        self.settle_offsets();
        self.view.curswant = self.wanted_col(); // Vim's `beginline` resets the wanted column (#1104)
        Ok(removed)
    }
}

/// The comparable key for one `line` of [`EditorState::sort_lines`], honoring `/pattern/` (`compiled`),
/// `r`, `i`, and `n`. A byte that is not UTF-8 is one char to the pattern (#1243), and text keys compare
/// the line's bytes.
fn sort_key(line: &[u8], compiled: Option<&Regex>, opts: &SortOptions) -> SortKey {
    let base: &[u8] = match compiled {
        Some(re) => {
            let lh = crate::pattern::Hay::new(line);
            match re
                .find_at(lh.as_str(), BytePos(0))
                .map(|m| lh.to_buf_match(m))
            {
                Some(m) if opts.use_match => &line[m.start.0..m.end.0],
                Some(m) => &line[m.end.0..],
                None => b"",
            }
        }
        None => line,
    };
    match opts.key {
        SortKind::Decimal | SortKind::Hex | SortKind::Octal | SortKind::Binary => {
            SortKey::Num(first_number(&crate::pattern::spell(base), opts.key))
        }
        SortKind::Float => SortKey::Float(first_float(&crate::pattern::spell(base))),
        SortKind::Locale => SortKey::Locale(crate::pattern::spell(base).into_owned()),
        SortKind::Text if opts.ignore_case => SortKey::Text(base.to_ascii_lowercase()),
        SortKind::Text => SortKey::Text(base.to_vec()),
    }
}

/// A line's comparable sort key. All keys in one sort share a variant. `Num(None)` (no number in the key)
/// orders before every number, as in Vim.
#[derive(Clone, PartialEq)]
enum SortKey {
    Num(Option<i64>),
    Float(f64),
    Text(Vec<u8>),
    Locale(String),
}

impl SortKey {
    fn compare(&self, other: &SortKey) -> std::cmp::Ordering {
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.cmp(b),
            // nvim compares with `==`/`>`, which a NaN defeats; a total order keeps the sort well-defined.
            (SortKey::Float(a), SortKey::Float(b)) => a.total_cmp(b),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            (SortKey::Locale(a), SortKey::Locale(b)) => locale_cmp(a, b),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// The `:sort l` order (see [`SortKind::Locale`]): case-insensitive first, then lowercase before
/// uppercase, then bytewise.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
    fold(a).cmp(&fold(b)).then_with(|| {
        let case = |s: &str| s.chars().map(char::is_uppercase).collect::<Vec<_>>();
        case(a).cmp(&case(b)).then_with(|| a.cmp(b))
    })
}

/// The first number in `s` for `:sort n`/`x`/`o`/`b` (nvim's `ex_sort` + `vim_str2nr`): skip to the
/// first digit of the base (for `x` any hex digit, for `b` a `0` or `1`), take a `-` just before it, then
/// read the number, after an optional `0x`/`0b` prefix for `x`/`b`. `None` when `s` has no such digit. A
/// number past the `i64` range saturates to its bound.
fn first_number(s: &str, kind: SortKind) -> Option<i64> {
    let b = s.as_bytes();
    let starts = |c: &u8| match kind {
        SortKind::Hex => c.is_ascii_hexdigit(),
        SortKind::Binary => matches!(c, b'0' | b'1'),
        _ => c.is_ascii_digit(),
    };
    let i = b.iter().position(starts)?;
    let negative = i > 0 && b[i - 1] == b'-';
    let (radix, prefix) = match kind {
        SortKind::Hex => (16, b'x'),
        SortKind::Octal => (8, 0),
        SortKind::Binary => (2, b'b'),
        _ => (10, 0),
    };
    let digit = |c: u8| char::from(c).to_digit(radix);
    let mut at = i;
    if prefix != 0
        && b[at] == b'0'
        && b.get(at + 1).map(u8::to_ascii_lowercase) == Some(prefix)
        && b.get(at + 2).is_some_and(|&c| digit(c).is_some())
    {
        at += 2;
    }
    let magnitude = b[at..].iter().map_while(|&c| digit(c)).fold(0u64, |n, d| {
        n.saturating_mul(u64::from(radix))
            .saturating_add(u64::from(d))
    });
    Some(if negative {
        0i64.checked_sub_unsigned(magnitude).unwrap_or(i64::MIN)
    } else {
        i64::try_from(magnitude).unwrap_or(i64::MAX)
    })
}

/// The number at the start of `s` for `:sort f` (nvim's `ex_sort`): blanks and one `+` are skipped, an
/// empty rest sorts before every number (`-DBL_MAX`), and otherwise the value is what C's `strtod` reads
/// (0 when nothing parses).
fn first_float(s: &str) -> f64 {
    let s = s.trim_start_matches([' ', '\t']);
    let s = s
        .strip_prefix('+')
        .map_or(s, |r| r.trim_start_matches([' ', '\t']));
    if s.is_empty() {
        return -f64::MAX;
    }
    strtod_prefix(s)
}

/// Advance `*i` over the (hex, with `hex`) digits of `b` from it; returns how many there were.
fn digits_from(b: &[u8], i: &mut usize, hex: bool) -> usize {
    let start = *i;
    while b.get(*i).is_some_and(|c| {
        if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        }
    }) {
        *i += 1;
    }
    *i - start
}

/// The value of the longest prefix of `s` that C's `strtod` accepts (after leading whitespace): an
/// optional sign, then `inf`/`infinity`, `nan`, a hex number (`0x1.8p3`, [`strtod_hex`]) or a decimal one
/// (`1.5e-3`).
fn strtod_prefix(s: &str) -> f64 {
    let s = s.trim_start();
    let b = s.as_bytes();
    let (sign, mut i) = match b.first() {
        Some(b'-') => (-1.0, 1),
        Some(b'+') => (1.0, 1),
        _ => (1.0, 0),
    };
    let rest = &s[i..];
    let starts_ci = |w: &str| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
    if starts_ci("inf") {
        return sign * f64::INFINITY;
    }
    if starts_ci("nan") {
        return f64::NAN;
    }
    if b.get(i) == Some(&b'0') && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        return sign * strtod_hex(s, i + 2);
    }
    // Decimal: digits [`.` digits] [`e` exponent], with at least one digit.
    let start = i;
    let int_len = digits_from(b, &mut i, false);
    let mut frac_len = 0;
    if b.get(i) == Some(&b'.') {
        let mut k = i + 1;
        frac_len = digits_from(b, &mut k, false);
        if int_len + frac_len > 0 {
            i = k;
        }
    }
    if int_len + frac_len == 0 {
        return 0.0;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut k = i + 1;
        if matches!(b.get(k), Some(b'-' | b'+')) {
            k += 1;
        }
        if digits_from(b, &mut k, false) > 0 {
            i = k;
        }
    }
    sign * s[start..i].parse::<f64>().unwrap_or(0.0)
}

/// The unsigned value of the hex number of [`strtod_prefix`] whose digits start at `j` of `s` (just after
/// its `0x`): hexdigits [`.` hexdigits] [`p` exponent]. With no digit only the `0` parses: 0.
fn strtod_hex(s: &str, mut j: usize) -> f64 {
    let b = s.as_bytes();
    let mut mantissa = 0f64;
    let int_start = j;
    let int_len = digits_from(b, &mut j, true);
    for &c in &b[int_start..int_start + int_len] {
        mantissa = mantissa * 16.0 + f64::from(char::from(c).to_digit(16).unwrap_or(0));
    }
    let mut frac_len = 0;
    if b.get(j) == Some(&b'.') {
        let mut k = j + 1;
        let fs = k;
        frac_len = digits_from(b, &mut k, true);
        let mut scale = 1.0 / 16.0;
        for &c in &b[fs..fs + frac_len] {
            mantissa += f64::from(char::from(c).to_digit(16).unwrap_or(0)) * scale;
            scale /= 16.0;
        }
        if int_len + frac_len > 0 {
            j = k;
        }
    }
    if int_len + frac_len == 0 {
        // Only the `0` parses.
        return 0.0;
    }
    let mut exp = 0i32;
    if matches!(b.get(j), Some(b'p' | b'P')) {
        let mut k = j + 1;
        let neg = match b.get(k) {
            Some(b'-') => {
                k += 1;
                true
            }
            Some(b'+') => {
                k += 1;
                false
            }
            _ => false,
        };
        let es = k;
        if digits_from(b, &mut k, false) > 0 {
            let e: i32 = s[es..k].parse().unwrap_or(i32::MAX);
            exp = if neg { -e } else { e };
        }
    }
    mantissa * 2f64.powi(exp)
}
