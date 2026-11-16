//! Byte-level C lexing for the C indent: the NUL-terminated line reads, word and white tests,
//! string and comment skips, and the per-line comment / raw-string marks.

/// The byte at `i`, or NUL past the end (a NUL byte in the line also ends it, as in nvim).
pub(super) fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `strlen` from `i`: where the line ends (its first NUL byte, or its length).
pub(super) fn strend(s: &[u8], i: usize) -> usize {
    match s.get(i..) {
        Some(t) => i + t.iter().position(|&c| c == 0).unwrap_or(t.len()),
        None => i,
    }
}

/// The line's last byte (`line[strlen(line) - 1]`), NUL when it is empty.
pub(super) fn last_byte(s: &[u8]) -> u8 {
    let n = strend(s, 0);
    if n == 0 {
        0
    } else {
        s[n - 1]
    }
}

pub(super) fn is_white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

pub(super) fn skipwhite(s: &[u8], mut i: usize) -> usize {
    while is_white(at(s, i)) {
        i += 1;
    }
    i
}

pub(super) fn skiptowhite(s: &[u8], mut i: usize) -> usize {
    while at(s, i) != 0 && !is_white(at(s, i)) {
        i += 1;
    }
    i
}

/// `vim_isIDc` with nvim's default `'isident'` (`@,48-57,_,192-255`), byte-wise.
pub(super) fn is_idc(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 192
}

/// `vim_iswordc` with nvim's default `'iskeyword'` (`@,48-57,_,192-255`), byte-wise.
pub(super) fn is_wordc(c: u8) -> bool {
    is_idc(c)
}

/// `strncmp(s + i, w, w.len()) == 0`.
pub(super) fn starts(s: &[u8], i: usize, w: &[u8]) -> bool {
    s.get(i..).is_some_and(|t| t.starts_with(w))
}

/// The byte length of the UTF-8 char at `i` (1 for an invalid byte or past the end).
pub(super) fn char_len(s: &[u8], i: usize) -> usize {
    let c = at(s, i);
    let n = match c {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    };
    if n > 1 && (1..n).all(|k| at(s, i + k) & 0xC0 == 0x80) {
        n
    } else {
        1
    }
}

/// `utf_head_off`: how far `i` is past the start of the char it is in.
pub(super) fn head_off(s: &[u8], i: usize) -> usize {
    let mut k = 0;
    while k < 3 && i > k && at(s, i - k) & 0xC0 == 0x80 {
        k += 1;
    }
    if k > 0 && char_len(s, i - k) > k {
        k
    } else {
        0
    }
}

/// `skip_string`: past a `"string"`, `'c'` character or C++ raw string at `p` (they may run on:
/// `"a""b"`), else `p`; a string that runs to the end of the line leaves `p` on its last byte.
pub(super) fn skip_string(s: &[u8], mut p: usize) -> usize {
    loop {
        if at(s, p) == b'\'' {
            if at(s, p + 1) == 0 {
                break;
            }
            let mut i = 2;
            if at(s, p + 1) == b'\\' && at(s, p + 2) != 0 {
                i += 1;
                while at(s, p + i - 1).is_ascii_digit() {
                    i += 1;
                }
            }
            if at(s, p + i - 1) != 0 && at(s, p + i) == b'\'' {
                p += i + 1;
                continue;
            }
        } else if at(s, p) == b'"' {
            p += 1;
            while at(s, p) != 0 {
                if at(s, p) == b'\\' && at(s, p + 1) != 0 {
                    p += 1;
                } else if at(s, p) == b'"' {
                    break;
                }
                p += 1;
            }
            if at(s, p) == b'"' {
                p += 1;
                continue;
            }
        } else if at(s, p) == b'R' && at(s, p + 1) == b'"' {
            let delim = p + 2;
            let end = strend(s, delim);
            if let Some(off) = s[delim.min(end)..end].iter().position(|&c| c == b'(') {
                let d = &s[delim..delim + off];
                p += 3;
                while at(s, p) != 0 {
                    if at(s, p) == b')' && starts(s, p + 1, d) && at(s, p + d.len() + 1) == b'"' {
                        p += d.len() + 1;
                        break;
                    }
                    p += 1;
                }
                if at(s, p) == b'"' {
                    p += 1;
                    continue;
                }
            }
        }
        break;
    }
    if at(s, p) == 0 {
        p = p.saturating_sub(1);
    }
    p
}

/// `is_pos_in_string`: whether `line[col]` is inside a C string.
pub(super) fn is_pos_in_string(line: &[u8], col: usize) -> bool {
    let mut p = 0;
    while at(line, p) != 0 && p < col {
        p = skip_string(line, p) + 1;
    }
    p > col
}

/// `check_linecomment` (not `'lisp'`): the column of the line's `//` comment, if any.
pub(crate) fn check_linecomment(line: &[u8]) -> Option<usize> {
    let end = strend(line, 0);
    let mut p = 0;
    while let Some(off) = line[p.min(end)..end].iter().position(|&c| c == b'/') {
        let i = p + off;
        if at(line, i + 1) == b'/'
            && (i == 0 || line[i - 1] != b'*' || at(line, i + 2) != b'*')
            && !is_pos_in_string(line, i)
        {
            return Some(i);
        }
        p = i + 1;
    }
    None
}

/// `strncmp(a, b, n) == 0` over NUL-terminated strings.
pub(super) fn ncmp(a: &[u8], b: &[u8], n: usize) -> bool {
    for i in 0..n {
        let (x, y) = (at(a, i), at(b, i));
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

pub(super) fn cin_iscomment(s: &[u8], p: usize) -> bool {
    at(s, p) == b'/' && (at(s, p + 1) == b'*' || at(s, p + 1) == b'/')
}

pub(super) fn cin_islinecomment(s: &[u8], p: usize) -> bool {
    at(s, p) == b'/' && at(s, p + 1) == b'/'
}

pub(super) fn cin_ispreproc(s: &[u8]) -> bool {
    at(s, skipwhite(s, 0)) == b'#'
}

pub(super) fn cin_isif(s: &[u8], p: usize) -> bool {
    cin_starts_with(s, p, b"if")
}

pub(super) fn cin_isdo(s: &[u8], p: usize) -> bool {
    cin_starts_with(s, p, b"do")
}

pub(super) fn cin_isbreak(s: &[u8], p: usize) -> bool {
    cin_starts_with(s, p, b"break")
}

pub(super) fn cin_starts_with(s: &[u8], p: usize, word: &[u8]) -> bool {
    starts(s, p, word) && !is_idc(at(s, p + word.len()))
}

/// The comma-separated parts of an option value.
pub(super) fn parts(opt: &str) -> impl Iterator<Item = &[u8]> {
    opt.as_bytes()
        .split(|&c| c == b',')
        .filter(|p| !p.is_empty())
}

/// A line holds `/*` or `*/`.
pub(super) const MARK_COMMENT: u8 = 1;

/// A line holds `R"`.
pub(super) const MARK_RAW: u8 = 2;

/// The [`MARK_COMMENT`] / [`MARK_RAW`] bits of `line`.
pub(super) fn line_marks(line: &[u8]) -> u8 {
    let mut m = 0;
    for w in line.windows(2) {
        match w {
            b"/*" | b"*/" => m |= MARK_COMMENT,
            b"R\"" => m |= MARK_RAW,
            _ => {}
        }
    }
    m
}
