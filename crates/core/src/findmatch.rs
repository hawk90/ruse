//! `%` as nvim v0.12.6 runs it: `findmatchlimit(NULL, NUL, 0, 0)` (`search.c`, #1163). The bracket under
//! or after the cursor on its line is matched the 'matchpairs' way (`(:),{:},[:]`, nvim's default), with
//! nvim's default `'cpoptions'` (no `%`, no `M`):
//!
//! - a bracket inside `"…"` is skipped when the line has an even number of quotes (or the line, or the
//!   one above, ends in a backslash), and a `'x'` / `'\x'` character literal is skipped;
//! - a bracket after an odd number of backslashes only matches another escaped one;
//! - on `/*` or `*/` the match is the other end of the comment;
//! - on a `#if`/`#else`/`#elif`/`#endif` line (or a line starting with `#` and holding no bracket) the match
//!   is the next or previous directive of the chain;
//! - under `'lisp'`: a `;` comment is not searched (unless the cursor is in it, then only it is), and
//!   `#\(` and the like are characters, not brackets.

use std::cell::Cell;

use crate::pos::raw::line_end;

thread_local! {
    static LISP: Cell<bool> = const { Cell::new(false) };
}

/// While the returned guard lives, `%` runs with the `'lisp'` rules when `on` (the core sets it around a
/// command from the focused View's `'lisp'`).
pub(crate) fn lisp_scope(on: bool) -> LispScope {
    let prev = LISP.with(|c| c.replace(on));
    LispScope { prev }
}

/// Restores the previous `'lisp'` flag on drop ([`lisp_scope`]).
pub(crate) struct LispScope {
    prev: bool,
}

impl Drop for LispScope {
    fn drop(&mut self) {
        LISP.with(|c| c.set(self.prev));
    }
}

/// The byte at `i` of `s`, NUL past the end.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn skipwhite(s: &[u8], mut i: usize) -> usize {
    while matches!(at(s, i), b' ' | b'\t') {
        i += 1;
    }
    i
}

/// The lines of `b` as `(start, end)` byte spans (no newline), and the line holding byte `pos`.
struct Lines<'a> {
    b: &'a [u8],
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(b: &'a [u8]) -> Self {
        let mut starts = vec![0];
        starts.extend(
            b.iter()
                .enumerate()
                .filter(|&(_, &c)| c == b'\n')
                .map(|(i, _)| i + 1),
        );
        // A final newline ends the last line; it starts no new one.
        if starts.len() > 1 && *starts.last().unwrap_or(&0) == b.len() {
            starts.pop();
        }
        Lines { b, starts }
    }

    fn count(&self) -> usize {
        self.starts.len()
    }

    fn line(&self, l: usize) -> &'a [u8] {
        let s = self.starts[l];
        &self.b[s..line_end(self.b, s)]
    }
}

/// `check_linecomment`: where the line's `//` comment (or, under `'lisp'`, `;` comment) starts.
fn check_linecomment(line: &[u8], lisp: bool) -> Option<usize> {
    if lisp {
        crate::editor::lisp_line_comment(line)
    } else {
        crate::editor::c_line_comment(line)
    }
}

/// Whether the character before `line[col]` is `ch` (`check_prevcol`); `prevcol` takes its column.
fn check_prevcol(line: &[u8], col: usize, ch: u8, prevcol: &mut usize) -> bool {
    if col == 0 {
        return false;
    }
    let mut c = col - 1;
    while c > 0 && line[c] & 0xC0 == 0x80 {
        c -= 1;
    }
    *prevcol = c;
    line[c] == ch
}

/// The number of backslashes right before `line[col]`.
fn backslashes(line: &[u8], col: usize) -> usize {
    let mut n = 0;
    let mut c = col;
    let mut p = 0;
    while check_prevcol(line, c, b'\\', &mut p) {
        n += 1;
        c = p;
    }
    n
}

const PAIRS: [(u8, u8); 3] = [(b'(', b')'), (b'{', b'}'), (b'[', b']')];

/// The UTF-8 length of the char at `i` (1 for an invalid byte).
fn char_len(s: &[u8], i: usize) -> usize {
    crate::motion::char_len_at(s, i).unwrap_or(1).max(1)
}

/// `%` from byte `cursor` of `b` (nvim's `findmatchlimit` with no `initc`): the byte the match is at.
pub(crate) fn percent(b: &[u8], cursor: usize) -> Option<usize> {
    let lisp = LISP.with(Cell::get);
    let lines = Lines::new(b);
    let cur = cursor.min(b.len());
    let lnum0 = lines
        .starts
        .partition_point(|&s| s <= cur)
        .saturating_sub(1);
    let col0 = cur - lines.starts[lnum0];
    let (l, c) = find(&lines, lnum0, col0, lisp)?;
    Some(lines.starts[l] + c)
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn find(lines: &Lines<'_>, lnum: usize, col: usize, lisp: bool) -> Option<(usize, usize)> {
    let mut pos = (lnum, col);
    let mut linep = lines.line(pos.0);
    let mut initc: u8 = 0;
    let mut findc: u8 = 0;
    let mut backwards = false;
    let mut hash_dir: i32 = 0;
    // 1 forward, -1 backward, 0 none.
    let mut comment_dir: i32 = 0;
    let mut match_escaped = 0usize;

    // Before or at `#if`, `#else`, `#endif`?
    let p = skipwhite(linep, 0);
    if at(linep, p) == b'#' && pos.1 <= p {
        let q = skipwhite(linep, p + 1);
        if linep[q..].starts_with(b"if")
            || linep[q..].starts_with(b"endif")
            || linep[q..].starts_with(b"el")
        {
            hash_dir = 1;
        }
    } else if at(linep, pos.1) == b'/' {
        if at(linep, pos.1 + 1) == b'*' {
            comment_dir = 1;
            backwards = false;
            pos.1 += 1;
        } else if pos.1 > 0 && at(linep, pos.1 - 1) == b'*' {
            comment_dir = -1;
            backwards = true;
            pos.1 -= 1;
        }
    } else if at(linep, pos.1) == b'*' {
        if at(linep, pos.1 + 1) == b'/' {
            comment_dir = -1;
            backwards = true;
        } else if pos.1 > 0 && at(linep, pos.1 - 1) == b'/' {
            comment_dir = 1;
            backwards = false;
        }
    }
    if hash_dir == 0 && comment_dir == 0 {
        // The bracket under or after the cursor.
        if at(linep, pos.1) == 0 && pos.1 > 0 {
            pos.1 -= 1;
        }
        loop {
            let ch = at(linep, pos.1);
            if ch == 0 {
                break;
            }
            if let Some(&(o, cl)) = PAIRS.iter().find(|&&(o, cl)| ch == o || ch == cl) {
                initc = ch;
                if ch == o {
                    findc = cl;
                    backwards = false;
                } else {
                    findc = o;
                    backwards = true;
                }
                break;
            }
            pos.1 += char_len(linep, pos.1);
        }
        if findc == 0 {
            // No bracket on the line: maybe a `#if` line then.
            if at(linep, skipwhite(linep, 0)) == b'#' {
                hash_dir = 1;
            } else {
                return None;
            }
        } else {
            match_escaped = backslashes(linep, pos.1) & 1;
        }
    }
    if hash_dir != 0 {
        let q = skipwhite(linep, skipwhite(linep, 0) + 1);
        let rest = &linep[q.min(linep.len())..];
        hash_dir = if rest.starts_with(b"if") || rest.starts_with(b"el") {
            1
        } else if rest.starts_with(b"endif") {
            -1
        } else {
            return None;
        };
        let mut count = 0i32;
        let mut l = pos.0;
        loop {
            if hash_dir > 0 {
                if l + 1 >= lines.count() {
                    return None;
                }
                l += 1;
            } else {
                if l == 0 {
                    return None;
                }
                l -= 1;
            }
            let line = lines.line(l);
            let p = skipwhite(line, 0);
            if at(line, p) != b'#' {
                continue;
            }
            let q = skipwhite(line, p + 1);
            let r = &line[q.min(line.len())..];
            if hash_dir > 0 {
                if r.starts_with(b"if") {
                    count += 1;
                } else if r.starts_with(b"el") {
                    if count == 0 {
                        return Some((l, p));
                    }
                } else if r.starts_with(b"endif") {
                    if count == 0 {
                        return Some((l, p));
                    }
                    count -= 1;
                }
            } else if r.starts_with(b"if") {
                if count == 0 {
                    return Some((l, p));
                }
                count -= 1;
            } else if r.starts_with(b"endif") {
                count += 1;
            }
        }
    }

    let mut do_quotes: i32 = -1;
    let mut at_start: i32;
    let mut start_in_quotes: Option<bool> = None;
    let mut inquote = false;
    let mut count = 0i32;
    let mut match_pos = (0usize, 0usize);
    let mut comment_col: Option<usize> = None;
    let mut lispcomm = false;
    let maxcol = usize::MAX;
    if (backwards && comment_dir != 0) || lisp {
        comment_col = check_linecomment(linep, lisp);
    }
    if lisp && comment_col.is_some_and(|cc| pos.1 > cc) {
        lispcomm = true;
    }
    loop {
        if backwards {
            if lispcomm && pos.1 < comment_col.unwrap_or(maxcol) {
                break;
            }
            if pos.1 == 0 {
                if pos.0 == 0 {
                    break;
                }
                pos.0 -= 1;
                linep = lines.line(pos.0);
                pos.1 = linep.len();
                do_quotes = -1;
                if comment_dir != 0 || lisp {
                    comment_col = check_linecomment(linep, lisp);
                }
                if lisp {
                    if let Some(cc) = comment_col {
                        pos.1 = cc;
                    }
                }
            } else {
                pos.1 -= 1;
                while pos.1 > 0 && linep[pos.1] & 0xC0 == 0x80 {
                    pos.1 -= 1;
                }
            }
        } else if at(linep, pos.1) == 0 || (lisp && comment_col == Some(pos.1)) {
            if pos.0 + 1 >= lines.count() || lispcomm {
                break;
            }
            pos.0 += 1;
            linep = lines.line(pos.0);
            pos.1 = 0;
            do_quotes = -1;
            if lisp {
                comment_col = check_linecomment(linep, lisp);
            }
        } else {
            pos.1 += char_len(linep, pos.1);
        }

        if comment_dir != 0 {
            if comment_dir > 0 {
                if at(linep, pos.1) == b'*' && at(linep, pos.1 + 1) == b'/' {
                    return Some((pos.0, pos.1 + 1));
                }
            } else if pos.1 == 0 {
                continue;
            } else if at(linep, pos.1 - 1) == b'/'
                && at(linep, pos.1) == b'*'
                && (pos.1 == 1 || at(linep, pos.1 - 2) != b'*')
                && comment_col.is_none_or(|cc| pos.1 < cc)
            {
                count += 1;
                match_pos = (pos.0, pos.1 - 1);
            } else if at(linep, pos.1 - 1) == b'*' && at(linep, pos.1) == b'/' {
                if count > 0 {
                    return Some(match_pos);
                } else if pos.1 > 1
                    && at(linep, pos.1 - 2) == b'/'
                    && comment_col.is_none_or(|cc| pos.1 <= cc)
                {
                    return Some((pos.0, pos.1 - 2));
                }
                return None;
            }
            continue;
        }

        if do_quotes == -1 {
            at_start = do_quotes;
            let mut i = 0;
            while i < linep.len() {
                if i == pos.1 + usize::from(backwards) {
                    at_start = do_quotes & 1;
                }
                if linep[i] == b'"'
                    && (i == 0 || linep[i - 1] != b'\'' || at(linep, i + 1) != b'\'')
                {
                    do_quotes += 1;
                }
                if linep[i] == b'\\' && i + 1 < linep.len() {
                    i += 1;
                }
                i += 1;
            }
            do_quotes &= 1;
            if do_quotes == 0 {
                inquote = false;
                if linep.last() == Some(&b'\\') {
                    do_quotes = 1;
                    if start_in_quotes.is_none() {
                        inquote = true;
                        start_in_quotes = Some(true);
                    } else if backwards {
                        inquote = true;
                    }
                }
                if pos.0 > 0 && lines.line(pos.0 - 1).last() == Some(&b'\\') {
                    do_quotes = 1;
                    if start_in_quotes.is_none() {
                        inquote = at_start != 0;
                        if inquote {
                            start_in_quotes = Some(true);
                        }
                    } else if !backwards {
                        inquote = true;
                    }
                }
            }
        }
        if start_in_quotes.is_none() {
            start_in_quotes = Some(false);
        }

        let c = at(linep, pos.1);
        match c {
            0 => {
                if pos.1 == 0 || linep[pos.1 - 1] != b'\\' {
                    inquote = false;
                    start_in_quotes = Some(false);
                }
            }
            b'"' => {
                if do_quotes != 0 && backslashes(linep, pos.1).is_multiple_of(2) {
                    inquote = !inquote;
                    start_in_quotes = Some(false);
                }
            }
            _ => {
                // Skip a 'x' or '\x' character literal.
                if c == b'\'' && initc != b'\'' && findc != b'\'' {
                    if backwards {
                        if pos.1 > 1 {
                            if linep[pos.1 - 2] == b'\'' {
                                pos.1 -= 2;
                                continue;
                            } else if linep[pos.1 - 2] == b'\\'
                                && pos.1 > 2
                                && linep[pos.1 - 3] == b'\''
                            {
                                pos.1 -= 3;
                                continue;
                            }
                        }
                    } else if at(linep, pos.1 + 1) != 0 {
                        if at(linep, pos.1 + 1) == b'\\'
                            && at(linep, pos.1 + 2) != 0
                            && at(linep, pos.1 + 3) == b'\''
                        {
                            pos.1 += 3;
                            continue;
                        } else if at(linep, pos.1 + 2) == b'\'' {
                            pos.1 += 2;
                            continue;
                        }
                    }
                }
                // 'lisp': `#\(` and the like are characters.
                let mut p = 0;
                if lisp
                    && b"(){}[]".contains(&c)
                    && pos.1 > 1
                    && check_prevcol(linep, pos.1, b'\\', &mut p)
                    && check_prevcol(linep, pos.1 - 1, b'#', &mut p)
                {
                    continue;
                }
                if (!inquote || start_in_quotes == Some(true))
                    && (c == initc || c == findc)
                    && backslashes(linep, pos.1) & 1 == match_escaped
                {
                    if c == initc {
                        count += 1;
                    } else {
                        if count == 0 {
                            return Some(pos);
                        }
                        count -= 1;
                    }
                }
            }
        }
    }
    if comment_dir < 0 && count > 0 {
        return Some(match_pos);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(src: &str, cur: usize) -> Option<usize> {
        percent(src.as_bytes(), cur)
    }

    #[test]
    fn percent_follows_findmatchlimit() {
        assert_eq!(pct("(a \")\" b)", 0), Some(8));
        assert_eq!(pct("(a ')' b)", 0), Some(8));
        assert_eq!(pct("(a ; )\nb)", 0), Some(5));
        assert_eq!(pct("x = f(a, b);", 0), Some(10));
        assert_eq!(pct("#if A\nx\n#endif", 0), Some(8));
        assert_eq!(pct("/* a\nb */ x", 0), Some(8));
        assert_eq!(pct("abc", 0), None);
        {
            let _l = lisp_scope(true);
            assert_eq!(pct("(a ; )\nb)", 0), Some(8));
            assert_eq!(pct("(a #\\) b)", 0), Some(8));
        }
        // Without 'lisp' the `\)` is an escaped bracket, skipped all the same.
        assert_eq!(pct("(a #\\) b)", 0), Some(8));
    }
}
