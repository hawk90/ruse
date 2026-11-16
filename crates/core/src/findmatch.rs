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

pub(crate) mod scan;

use scan::{check_prevcol, Backslashes, MatchScan, MatchStep};

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

/// What `%` searches for, as the first phase of `findmatchlimit` chooses it.
enum Start {
    /// The next or previous `#if`/`#else`/`#elif`/`#endif` of the chain.
    Directive,
    /// The other end of a comment, or the match of a bracket.
    Match(Target),
}

/// The bracket or comment end `findmatchlimit` matches, chosen once and read by the scan.
struct Target {
    /// The column the scan starts from.
    col: usize,
    /// The char whose match is wanted (counted up), 0 for a comment.
    initc: u8,
    /// The matching char (counted down), 0 for a comment.
    findc: u8,
    backwards: bool,
    /// 1 forward, -1 backward, 0 none.
    comment_dir: i32,
    /// 1 when the match wanted is escaped (an odd number of backslashes before it).
    match_escaped: usize,
}

/// `findmatchlimit` with no `initc`, from (`lnum`, `col`): choose the target, then search the `#if`
/// chain or scan for the match.
fn find(lines: &Lines<'_>, lnum: usize, col: usize, lisp: bool) -> Option<(usize, usize)> {
    let linep = lines.line(lnum);
    match match_target(linep, col)? {
        Start::Directive => find_directive(lines, lnum),
        Start::Match(t) => scan_start(lnum, linep, t, lisp).run(|s| iteration(lines, s)),
    }
}

/// `findmatchlimit`, "initc was not given, must look for something to match under or near the
/// cursor": a `#if`-like line the cursor is before or at, or a `/*` / `*/` under the cursor; else the
/// bracket ([`brace_target`]).
fn match_target(linep: &[u8], col: usize) -> Option<Start> {
    let mut t = Target {
        col,
        initc: 0,
        findc: 0,
        backwards: false,
        comment_dir: 0,
        match_escaped: 0,
    };
    // Before or at `#if`, `#else`, `#endif`?
    let p = skipwhite(linep, 0);
    if at(linep, p) == b'#' && t.col <= p {
        let q = skipwhite(linep, p + 1);
        if linep[q..].starts_with(b"if")
            || linep[q..].starts_with(b"endif")
            || linep[q..].starts_with(b"el")
        {
            return Some(Start::Directive);
        }
    } else if at(linep, t.col) == b'/' {
        if at(linep, t.col + 1) == b'*' {
            t.comment_dir = 1;
            t.backwards = false;
            t.col += 1;
        } else if t.col > 0 && at(linep, t.col - 1) == b'*' {
            t.comment_dir = -1;
            t.backwards = true;
            t.col -= 1;
        }
    } else if at(linep, t.col) == b'*' {
        if at(linep, t.col + 1) == b'/' {
            t.comment_dir = -1;
            t.backwards = true;
        } else if t.col > 0 && at(linep, t.col - 1) == b'/' {
            t.comment_dir = 1;
            t.backwards = false;
        }
    }
    if t.comment_dir != 0 {
        return Some(Start::Match(t));
    }
    brace_target(linep, t)
}

/// `findmatchlimit`, "If we are not on a comment or the # at the start of a line, then look for brace
/// anywhere on this line after the cursor": the bracket under or after `t.col`, its escaping, or
/// ("no brace in the line, maybe use "  #if" then") the `#` line's directive chain.
fn brace_target(linep: &[u8], mut t: Target) -> Option<Start> {
    if at(linep, t.col) == 0 && t.col > 0 {
        t.col -= 1;
    }
    loop {
        let ch = at(linep, t.col);
        if ch == 0 {
            break;
        }
        if let Some(&(o, cl)) = PAIRS.iter().find(|&&(o, cl)| ch == o || ch == cl) {
            t.initc = ch;
            if ch == o {
                t.findc = cl;
                t.backwards = false;
            } else {
                t.findc = o;
                t.backwards = true;
            }
            break;
        }
        t.col += char_len(linep, t.col);
    }
    if t.findc == 0 {
        // No bracket on the line: maybe a `#if` line then.
        return (at(linep, skipwhite(linep, 0)) == b'#').then_some(Start::Directive);
    }
    t.match_escaped = backslashes(linep, t.col) & 1;
    Some(Start::Match(t))
}

/// `findmatchlimit`, "Look for matching #if, #else, #elif, or #endif": from line `lnum`, the next
/// `#else`/`#elif`/`#endif` (or the previous `#if` from an `#endif`) at the same nesting.
fn find_directive(lines: &Lines<'_>, lnum: usize) -> Option<(usize, usize)> {
    let linep = lines.line(lnum);
    let q = skipwhite(linep, skipwhite(linep, 0) + 1);
    let rest = &linep[q.min(linep.len())..];
    let forward = if rest.starts_with(b"if") || rest.starts_with(b"el") {
        true
    } else if rest.starts_with(b"endif") {
        false
    } else {
        return None;
    };
    let mut count = 0i32;
    let mut l = lnum;
    loop {
        if forward {
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
        if forward {
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

/// `%` counts the backslashes before a `"` and before a bracket alike, with its [`check_prevcol`].
const PERCENT_BACKSLASHES: Backslashes = Backslashes {
    quote: backslashes,
    escape: backslashes,
};

/// `findmatchlimit`, "backward search: Check if this line contains a single-line comment": the scan
/// state at the target, with the first line's comment column (and, under `'lisp'`, whether the
/// cursor is in that comment).
fn scan_start(lnum: usize, linep: &[u8], t: Target, lisp: bool) -> MatchScan<'_> {
    let mut s = MatchScan::new((lnum, t.col), linep, PERCENT_BACKSLASHES);
    s.initc = t.initc;
    s.findc = t.findc;
    s.backwards = t.backwards;
    s.comment_dir = t.comment_dir;
    s.match_escaped = t.match_escaped;
    s.lisp = lisp;
    if (t.backwards && t.comment_dir != 0) || lisp {
        s.comment_col = check_linecomment(linep, lisp);
    }
    s.lispcomm = lisp && s.comment_col.is_some_and(|cc| t.col > cc);
    s
}

/// One position of the `%` scan: move, then the comment search or the quote count and the char.
fn iteration<'a>(lines: &Lines<'a>, s: &mut MatchScan<'a>) -> MatchStep {
    let step = next_pos(lines, s);
    if !matches!(step, MatchStep::Go) {
        return step;
    }
    if s.comment_dir != 0 {
        return comment_search(s);
    }
    let lnum = s.pos.0;
    s.count_quotes(s.linep.len(), || {
        lnum > 0 && lines.line(lnum - 1).last() == Some(&b'\\')
    });
    s.match_char()
}

/// `findmatchlimit`, "Go to the next position, forward or backward": one char on, crossing to the
/// previous or next line (and its comment column) at a line end; breaks at the buffer's ends and
/// at the edge of a `'lisp'` comment the match is searched in.
fn next_pos<'a>(lines: &Lines<'a>, s: &mut MatchScan<'a>) -> MatchStep {
    let lisp = s.lisp;
    let maxcol = usize::MAX;
    if s.backwards {
        if s.lispcomm && s.pos.1 < s.comment_col.unwrap_or(maxcol) {
            return MatchStep::Break;
        }
        if s.pos.1 == 0 {
            if s.pos.0 == 0 {
                return MatchStep::Break;
            }
            s.pos.0 -= 1;
            s.linep = lines.line(s.pos.0);
            s.pos.1 = s.linep.len();
            s.do_quotes = -1;
            if s.comment_dir != 0 || lisp {
                s.comment_col = check_linecomment(s.linep, lisp);
            }
            if lisp {
                if let Some(cc) = s.comment_col {
                    s.pos.1 = cc;
                }
            }
        } else {
            s.pos.1 -= 1;
            while s.pos.1 > 0 && s.linep[s.pos.1] & 0xC0 == 0x80 {
                s.pos.1 -= 1;
            }
        }
    } else if at(s.linep, s.pos.1) == 0 || (lisp && s.comment_col == Some(s.pos.1)) {
        if s.pos.0 + 1 >= lines.count() || s.lispcomm {
            return MatchStep::Break;
        }
        s.pos.0 += 1;
        s.linep = lines.line(s.pos.0);
        s.pos.1 = 0;
        s.do_quotes = -1;
        if lisp {
            s.comment_col = check_linecomment(s.linep, lisp);
        }
    } else {
        s.pos.1 += char_len(s.linep, s.pos.1);
    }
    MatchStep::Go
}

/// `findmatchlimit`, "Note: comments do not nest, and we ignore quotes in them": forward, the `*/`
/// ahead; backward, the shared `/*` search ([`MatchScan::comment_backward`]).
fn comment_search(s: &mut MatchScan<'_>) -> MatchStep {
    if s.comment_dir < 0 {
        return s.comment_backward();
    }
    let (linep, pos) = (s.linep, s.pos);
    if at(linep, pos.1) == b'*' && at(linep, pos.1 + 1) == b'/' {
        return MatchStep::Return(Some((pos.0, pos.1 + 1)));
    }
    MatchStep::Next
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
