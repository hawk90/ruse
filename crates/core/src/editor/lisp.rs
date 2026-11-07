//! Lisp indent (`'lisp'`, #1163): a port of nvim v0.12.6's `get_lisp_indent` (`indent.c`) and the
//! backward bracket search it runs, `findmatchlimit` with its Lisp rules (`search.c`: `;` comments,
//! `#\(` characters, `"` strings by the line's quote count). Without `p` in `'cpoptions'` (nvim's
//! default, the only `'cpoptions'` ruse reads).
//!
//! A line's indent depends only on the lines above it, so the callers pass those lines.

use super::range::cluster_cells;
use crate::cells::Tabs;

/// `'lispwords'` at its nvim default.
pub const DEFAULT_LISPWORDS: &str = "defun,define,defmacro,set!,lambda,if,case,let,flet,let*,\
letrec,do,do*,define-syntax,let-syntax,letrec-syntax,destructuring-bind,defpackage,defparameter,\
defstruct,deftype,defvar,do-all-symbols,do-external-symbols,do-symbols,dolist,dotimes,ecase,etypecase,\
eval-when,labels,macrolet,multiple-value-bind,multiple-value-call,multiple-value-prog1,\
multiple-value-setq,prog1,progv,typecase,unless,unwind-protect,when,with-input-from-string,\
with-open-file,with-open-stream,with-output-to-string,with-package-iterator,define-condition,\
handler-bind,handler-case,restart-bind,restart-case,with-simple-restart,store-value,use-value,\
muffle-warning,abort,continue,with-slots,with-slots*,with-accessors,with-accessors*,defclass,\
defmethod,print-unreadable-object";

fn is_white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// The byte at `i`, or NUL past the end.
fn at(line: &[u8], i: usize) -> u8 {
    line.get(i).copied().unwrap_or(0)
}

/// `skip_string` (`indent_c.c`): past a `"string"` or `'c'` character at `p` (strings may run on:
/// `"a""b"`), or `p` itself.
fn skip_string(line: &[u8], mut p: usize) -> usize {
    loop {
        if at(line, p) == b'\'' {
            if at(line, p + 1) == 0 {
                break;
            }
            let mut i = 2;
            if at(line, p + 1) == b'\\' && at(line, p + 2) != 0 {
                i += 1;
                while at(line, p + i - 1).is_ascii_digit() {
                    i += 1;
                }
            }
            if at(line, p + i - 1) != 0 && at(line, p + i) == b'\'' {
                p += i + 1;
                continue;
            }
        } else if at(line, p) == b'"' {
            p += 1;
            while at(line, p) != 0 {
                if at(line, p) == b'\\' && at(line, p + 1) != 0 {
                    p += 1;
                } else if at(line, p) == b'"' {
                    break;
                }
                p += 1;
            }
            if at(line, p) == b'"' {
                p += 1;
                continue;
            }
        } else if at(line, p) == b'R' && at(line, p + 1) == b'"' {
            let delim = p + 2;
            if let Some(paren) = line[delim.min(line.len())..]
                .iter()
                .position(|&c| c == b'(')
            {
                let delim_end = delim + paren;
                let d = &line[delim..delim_end];
                p += 3;
                while at(line, p) != 0 {
                    if at(line, p) == b')'
                        && line[p + 1..].starts_with(d)
                        && at(line, p + 1 + d.len()) == b'"'
                    {
                        p += d.len() + 1;
                        break;
                    }
                    p += 1;
                }
                if at(line, p) == b'"' {
                    p += 1;
                    continue;
                }
            }
        }
        break;
    }
    if at(line, p) == 0 && p > 0 {
        p -= 1;
    }
    p
}

/// `is_pos_in_string`: whether `line[col]` is inside a C-style string.
fn is_pos_in_string(line: &[u8], col: usize) -> bool {
    let mut p = 0;
    while at(line, p) != 0 && p < col {
        p = skip_string(line, p) + 1;
    }
    p > col
}

/// `check_linecomment` under `'lisp'`: where the line's `;` comment starts.
pub(crate) fn line_comment(line: &[u8]) -> Option<usize> {
    if !line.contains(&b';') {
        return None;
    }
    let mut in_str = false;
    let mut p = 0;
    while let Some(off) = line[p..].iter().position(|&c| c == b'"' || c == b';') {
        let i = p + off;
        if line[i] == b'"' {
            if in_str {
                if line[i - 1] != b'\\' {
                    in_str = false;
                }
            } else if i == 0 || (i >= 2 && line[i - 1] != b'\\' && line[i - 2] != b'#') {
                in_str = true;
            }
        } else if !in_str
            && (i < 2 || (line[i - 1] != b'\\' && line[i - 2] != b'#'))
            && !is_pos_in_string(line, i)
        {
            return Some(i);
        }
        p = i + 1;
    }
    None
}

/// The number of backslashes right before `line[col]`.
fn backslashes_before(line: &[u8], col: usize) -> usize {
    line[..col]
        .iter()
        .rev()
        .take_while(|&&c| c == b'\\')
        .count()
}

/// `findmatch(NULL, open)` with the cursor at the start of line `l`: the `open` bracket left open before
/// it, as (line, byte column).
fn find_open<T: AsRef<[u8]>>(lines: &[T], l: usize, open: u8, close: u8) -> Option<(usize, usize)> {
    let mut lnum = l;
    let mut col = 0usize;
    let mut count = 0usize;
    let mut inquote = false;
    let mut start_in_quotes: Option<bool> = None;
    let mut do_quotes: i32 = -1;
    loop {
        // One character back; at a line start, to the end of the line above (or its `;` comment).
        if col == 0 {
            if lnum == 0 {
                return None;
            }
            lnum -= 1;
            col = lines[lnum].as_ref().len();
            do_quotes = -1;
            if let Some(c) = line_comment(lines[lnum].as_ref()) {
                col = c;
            }
        } else {
            col -= 1;
            while col > 0 && (lines[lnum].as_ref()[col] & 0xC0) == 0x80 {
                col -= 1;
            }
        }
        let line = lines[lnum].as_ref();
        // With an even number of quotes on the line, brackets inside them are skipped.
        if do_quotes == -1 {
            let mut n: i32 = -1;
            let mut at_start: i32 = -1;
            let mut i = 0;
            while i < line.len() {
                if i == col + 1 {
                    at_start = n & 1;
                }
                if line[i] == b'"' && (i == 0 || line[i - 1] != b'\'' || at(line, i + 1) != b'\'') {
                    n += 1;
                }
                if line[i] == b'\\' && i + 1 < line.len() {
                    i += 1;
                }
                i += 1;
            }
            do_quotes = n & 1;
            if do_quotes == 0 {
                inquote = false;
                if line.last() == Some(&b'\\') {
                    do_quotes = 1;
                    if start_in_quotes.is_none() {
                        start_in_quotes = Some(true);
                    }
                    inquote = true;
                }
                if lnum > 0 && lines[lnum - 1].as_ref().last() == Some(&b'\\') {
                    do_quotes = 1;
                    if start_in_quotes.is_none() {
                        inquote = at_start != 0;
                        if inquote {
                            start_in_quotes = Some(true);
                        }
                    }
                }
            }
        }
        if start_in_quotes.is_none() {
            start_in_quotes = Some(false);
        }
        let c = at(line, col);
        match c {
            0 => {
                if col == 0 || line[col - 1] != b'\\' {
                    inquote = false;
                    start_in_quotes = Some(false);
                }
            }
            b'"' => {
                if do_quotes != 0 && backslashes_before(line, col).is_multiple_of(2) {
                    inquote = !inquote;
                    start_in_quotes = Some(false);
                }
            }
            _ => {
                // A `'x'` or `'\x'` character literal is skipped.
                if c == b'\'' && col > 1 {
                    if line[col - 2] == b'\'' {
                        col -= 2;
                        continue;
                    }
                    if line[col - 2] == b'\\' && col > 2 && line[col - 3] == b'\'' {
                        col -= 3;
                        continue;
                    }
                }
                // `#\(` and the like are characters, not brackets.
                if b"(){}[]".contains(&c)
                    && col > 1
                    && line[col - 1] == b'\\'
                    && line[col - 2] == b'#'
                {
                    continue;
                }
                if (!inquote || start_in_quotes == Some(true))
                    && (c == open || c == close)
                    && backslashes_before(line, col).is_multiple_of(2)
                {
                    if c == close {
                        count += 1;
                    } else if count == 0 {
                        return Some((lnum, col));
                    } else {
                        count -= 1;
                    }
                }
            }
        }
    }
}

/// `lisp_match`: whether `p` starts with a word of `'lispwords'` followed by a blank or the line end.
fn lisp_match(p: &[u8], words: &str) -> bool {
    words.split(',').any(|w| {
        let w = w.as_bytes();
        p.starts_with(w) && p.get(w.len()).is_none_or(|&c| is_white(c))
    })
}

/// The width of the character at `i` when it starts at virtual column `v`.
fn width(line: &[u8], i: usize, v: usize, ts: Tabs) -> (usize, usize) {
    cluster_cells(line, i, line.len(), v, ts)
}

/// `get_lisp_indent` for line `l` of `lines` (only the lines above it are read).
pub(crate) fn lisp_indent<T: AsRef<[u8]>>(lines: &[T], l: usize, ts: Tabs, words: &str) -> usize {
    let paren = find_open(lines, l, b'(', b')');
    let bracket = find_open(lines, l, b'[', b']');
    let pos = match (paren, bracket) {
        (Some(p), Some(b)) if b < p => Some(p),
        (Some(_), Some(b)) => Some(b),
        (p, b) => p.or(b),
    };
    let Some((plnum, pcol)) = pos else {
        return 0; // no open bracket: column zero
    };
    // Take the indent of the first line above at the same bracket level.
    let mut parencount: i64 = 0;
    for lnum in (plnum..l).rev() {
        let line = lines[lnum].as_ref();
        if line.iter().all(|&c| is_white(c)) {
            continue;
        }
        let mut i = 0;
        while i < line.len() {
            if line[i] == b';' {
                break;
            }
            if line[i] == b'\\' {
                i += if i + 1 < line.len() { 2 } else { 1 };
                continue;
            }
            if line[i] == b'"' && i + 1 < line.len() {
                i += 1;
                while i < line.len() && line[i] != b'"' {
                    if line[i] == b'\\' {
                        i += 1;
                        if i >= line.len() {
                            break;
                        }
                        if i + 1 >= line.len() {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
                if i >= line.len() {
                    break;
                }
            }
            match line[i] {
                b'(' | b'[' => parencount += 1,
                b')' | b']' => parencount -= 1,
                _ => {}
            }
            i += 1;
        }
        if parencount == 0 {
            let ind = line.iter().take_while(|&&c| is_white(c));
            return ind.fold(
                0,
                |v, &c| {
                    if c == b'\t' {
                        v + ts.padding(v)
                    } else {
                        v + 1
                    }
                },
            );
        }
    }
    // Else from the open bracket: after it, lined up with the first argument, or two in for a body.
    let line = lines[plnum].as_ref();
    let mut amount = 0usize;
    let mut that = 0usize;
    let mut col = pcol;
    while that < line.len() && col > 0 {
        let (next, w) = width(line, that, amount, ts);
        amount += w;
        that = next;
        col -= 1;
    }
    if matches!(at(line, that), b'(' | b'[') && lisp_match(&line[that + 1..], words) {
        return amount + 2;
    }
    if that < line.len() {
        that = width(line, that, amount, ts).0;
        amount += 1;
    }
    let mut firsttry = amount;
    while that < line.len() && is_white(line[that]) {
        amount += width(line, that, amount, ts).1;
        that += 1;
    }
    if that < line.len() && line[that] != b';' {
        if !matches!(line[that], b'(' | b'[') {
            firsttry += 1;
        }
        let ci = line[that];
        if ci != b'"' && ci != b'\'' && ci != b'#' && !ci.is_ascii_digit() {
            let mut quotecount = false;
            let mut parencount: i64 = 0;
            while that < line.len() && (!is_white(line[that]) || quotecount || parencount != 0) {
                let ci = line[that];
                if ci == b'"' {
                    quotecount = !quotecount;
                }
                if matches!(ci, b'(' | b'[') && !quotecount {
                    parencount += 1;
                }
                if matches!(ci, b')' | b']') && !quotecount {
                    parencount -= 1;
                }
                if ci == b'\\' && that + 1 < line.len() {
                    let (next, w) = width(line, that, amount, ts);
                    amount += w;
                    that = next;
                }
                if that >= line.len() {
                    break;
                }
                let (next, w) = width(line, that, amount, ts);
                amount += w;
                that = next;
            }
        }
        while that < line.len() && is_white(line[that]) {
            amount += width(line, that, amount, ts).1;
            that += 1;
        }
        if that >= line.len() || line[that] == b';' {
            amount = firsttry;
        }
    }
    amount
}

/// `=` under `'lisp'` over lines `[first, last]` of `b` (nvim's `op_reindent` with `get_lisp_indent`):
/// each line's new indent, top down with the lines above already re-indented (`ws` builds an indent's
/// blanks). The first line keeps its indent unless it is the only one (Vi-compatible); a blank line is
/// emptied.
pub(crate) fn reindent(
    b: &[u8],
    first: usize,
    last: usize,
    ts: Tabs,
    words: &str,
    ws: impl Fn(usize) -> Vec<u8>,
) -> Vec<Option<usize>> {
    let mut lines: Vec<Vec<u8>> = b.split(|&c| c == b'\n').map(<[u8]>::to_vec).collect();
    let last = last.min(lines.len().saturating_sub(1));
    let mut out = Vec::new();
    for l in first..=last {
        let fnb = lines[l].iter().take_while(|&&c| is_white(c)).count();
        let want = if l == first && last > first {
            None
        } else if fnb == lines[l].len() {
            Some(0)
        } else {
            Some(lisp_indent(&lines, l, ts, words))
        };
        if let Some(c) = want {
            let mut line = ws(c);
            line.extend_from_slice(&lines[l][fnb..]);
            lines[l] = line;
        }
        out.push(want);
    }
    out
}

/// The Lisp indent of line `l` of `b`.
pub(crate) fn line_indent(b: &[u8], l: usize, ts: Tabs, words: &str) -> usize {
    let lines: Vec<&[u8]> = b.split(|&c| c == b'\n').take(l + 1).collect();
    lisp_indent(&lines, l.min(lines.len().saturating_sub(1)), ts, words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ind(src: &str, l: usize) -> usize {
        let lines: Vec<&[u8]> = src.split('\n').map(str::as_bytes).collect();
        lisp_indent(&lines, l, Tabs::uniform(8), DEFAULT_LISPWORDS)
    }

    #[test]
    fn lisp_indent_rules() {
        // A body form: two in; arguments line up with the first one; none: two in (one past `(`+1).
        assert_eq!(ind("(defun f (x)\n", 1), 2);
        assert_eq!(ind("(foo a\n", 1), 5);
        assert_eq!(ind("(foo\n", 1), 2);
        assert_eq!(ind("((lambda (x)\n   x)\n", 2), 1);
        // Strings, comments and `#\(` open nothing.
        assert_eq!(ind("(foo \"a (b\"\n", 1), 5);
        assert_eq!(ind("(foo a ; (x\n", 1), 5);
        assert_eq!(ind("(foo #\\( a\n", 1), 5);
        // A line above at the same level gives its indent; no open bracket gives zero.
        assert_eq!(ind("(foo a\n     b\n  c\n", 3), 2);
        assert_eq!(ind("(a)\n", 1), 0);
    }
}
