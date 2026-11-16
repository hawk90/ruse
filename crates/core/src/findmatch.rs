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

/// The state the `findmatchlimit` scan loop carries from one position to the next.
struct MatchScan<'l, 'a> {
    lines: &'l Lines<'a>,
    lisp: bool,
    t: Target,
    /// The current search position (line, column).
    pos: (usize, usize),
    /// The text of line `pos.0`.
    linep: &'a [u8],
    /// The parity of the quotes on the line, -1 until counted.
    do_quotes: i32,
    /// Whether the search started inside quotes, `None` until known.
    start_in_quotes: Option<bool>,
    /// True when inside quotes.
    inquote: bool,
    /// The cumulative number of unmatched `initc`s (or `/*`s).
    count: i32,
    /// Where the last `/*` was found.
    match_pos: (usize, usize),
    /// The start of the line's `//` (or `;`) comment, if any.
    comment_col: Option<usize>,
    /// Inside a `'lisp'` comment: the match is searched in it only.
    lispcomm: bool,
}

/// What a phase of one scan-loop iteration decides.
enum MatchStep {
    /// Go on to the next phase of this iteration.
    Go,
    /// The iteration is over: go to the next position (nvim's `continue`).
    Next,
    /// Leave the loop (nvim's `break`).
    Break,
    /// Return this result from `find` at once.
    Return(Option<(usize, usize)>),
}

/// `findmatchlimit` with no `initc`, from (`lnum`, `col`): choose the target, then search the `#if`
/// chain or scan for the match.
fn find(lines: &Lines<'_>, lnum: usize, col: usize, lisp: bool) -> Option<(usize, usize)> {
    let linep = lines.line(lnum);
    match match_target(linep, col)? {
        Start::Directive => find_directive(lines, lnum),
        Start::Match(t) => MatchScan::new(lines, lnum, linep, t, lisp).run(),
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

impl<'l, 'a> MatchScan<'l, 'a> {
    /// `findmatchlimit`, "backward search: Check if this line contains a single-line comment": the scan
    /// state at the target, with the first line's comment column (and, under `'lisp'`, whether the
    /// cursor is in that comment).
    fn new(lines: &'l Lines<'a>, lnum: usize, linep: &'a [u8], t: Target, lisp: bool) -> Self {
        let mut comment_col = None;
        if (t.backwards && t.comment_dir != 0) || lisp {
            comment_col = check_linecomment(linep, lisp);
        }
        let lispcomm = lisp && comment_col.is_some_and(|cc| t.col > cc);
        MatchScan {
            lines,
            lisp,
            pos: (lnum, t.col),
            t,
            linep,
            do_quotes: -1,
            start_in_quotes: None,
            inquote: false,
            count: 0,
            match_pos: (0, 0),
            comment_col,
            lispcomm,
        }
    }

    /// The `findmatchlimit` scan loop: each iteration runs the phases in nvim's order until one
    /// breaks or returns; a backward comment search that ran out returns the last `/*` it found.
    fn run(mut self) -> Option<(usize, usize)> {
        loop {
            match self.iteration() {
                MatchStep::Go | MatchStep::Next => {}
                MatchStep::Break => break,
                MatchStep::Return(r) => return r,
            }
        }
        if self.t.comment_dir < 0 && self.count > 0 {
            return Some(self.match_pos);
        }
        None
    }

    /// One position of the scan: move, then the comment search or the quote count and the char.
    fn iteration(&mut self) -> MatchStep {
        let step = self.next_pos();
        if !matches!(step, MatchStep::Go) {
            return step;
        }
        let step = self.comment_search();
        if !matches!(step, MatchStep::Go) {
            return step;
        }
        self.count_quotes();
        self.match_char()
    }

    /// `findmatchlimit`, "Go to the next position, forward or backward": one char on, crossing to the
    /// previous or next line (and its comment column) at a line end; breaks at the buffer's ends and
    /// at the edge of a `'lisp'` comment the match is searched in.
    fn next_pos(&mut self) -> MatchStep {
        let lisp = self.lisp;
        let maxcol = usize::MAX;
        if self.t.backwards {
            if self.lispcomm && self.pos.1 < self.comment_col.unwrap_or(maxcol) {
                return MatchStep::Break;
            }
            if self.pos.1 == 0 {
                if self.pos.0 == 0 {
                    return MatchStep::Break;
                }
                self.pos.0 -= 1;
                self.linep = self.lines.line(self.pos.0);
                self.pos.1 = self.linep.len();
                self.do_quotes = -1;
                if self.t.comment_dir != 0 || lisp {
                    self.comment_col = check_linecomment(self.linep, lisp);
                }
                if lisp {
                    if let Some(cc) = self.comment_col {
                        self.pos.1 = cc;
                    }
                }
            } else {
                self.pos.1 -= 1;
                while self.pos.1 > 0 && self.linep[self.pos.1] & 0xC0 == 0x80 {
                    self.pos.1 -= 1;
                }
            }
        } else if at(self.linep, self.pos.1) == 0 || (lisp && self.comment_col == Some(self.pos.1))
        {
            if self.pos.0 + 1 >= self.lines.count() || self.lispcomm {
                return MatchStep::Break;
            }
            self.pos.0 += 1;
            self.linep = self.lines.line(self.pos.0);
            self.pos.1 = 0;
            self.do_quotes = -1;
            if lisp {
                self.comment_col = check_linecomment(self.linep, lisp);
            }
        } else {
            self.pos.1 += char_len(self.linep, self.pos.1);
        }
        MatchStep::Go
    }

    /// `findmatchlimit`, "Note: comments do not nest, and we ignore quotes in them": on a comment
    /// search, the `*/` ahead or, backward, the `/*` (counted) and the `*/` that ends the search.
    fn comment_search(&mut self) -> MatchStep {
        if self.t.comment_dir == 0 {
            return MatchStep::Go;
        }
        let (linep, pos) = (self.linep, self.pos);
        if self.t.comment_dir > 0 {
            if at(linep, pos.1) == b'*' && at(linep, pos.1 + 1) == b'/' {
                return MatchStep::Return(Some((pos.0, pos.1 + 1)));
            }
        } else if pos.1 == 0 {
            return MatchStep::Next;
        } else if at(linep, pos.1 - 1) == b'/'
            && at(linep, pos.1) == b'*'
            && (pos.1 == 1 || at(linep, pos.1 - 2) != b'*')
            && self.comment_col.is_none_or(|cc| pos.1 < cc)
        {
            self.count += 1;
            self.match_pos = (pos.0, pos.1 - 1);
        } else if at(linep, pos.1 - 1) == b'*' && at(linep, pos.1) == b'/' {
            if self.count > 0 {
                return MatchStep::Return(Some(self.match_pos));
            } else if pos.1 > 1
                && at(linep, pos.1 - 2) == b'/'
                && self.comment_col.is_none_or(|cc| pos.1 <= cc)
            {
                return MatchStep::Return(Some((pos.0, pos.1 - 2)));
            }
            return MatchStep::Return(None);
        }
        MatchStep::Next
    }

    /// `findmatchlimit`, "If smart matching ('cpoptions' does not contain '%'), braces inside of
    /// quotes are ignored, but only if there is an even number of quotes in the line": on a new line,
    /// the parity of its quotes, and "If we find an uneven count, check current line and previous one
    /// for a '\' at the end".
    fn count_quotes(&mut self) {
        let (linep, pos, backwards) = (self.linep, self.pos, self.t.backwards);
        if self.do_quotes == -1 {
            let mut at_start = self.do_quotes;
            let mut i = 0;
            while i < linep.len() {
                if i == pos.1 + usize::from(backwards) {
                    at_start = self.do_quotes & 1;
                }
                if linep[i] == b'"'
                    && (i == 0 || linep[i - 1] != b'\'' || at(linep, i + 1) != b'\'')
                {
                    self.do_quotes += 1;
                }
                if linep[i] == b'\\' && i + 1 < linep.len() {
                    i += 1;
                }
                i += 1;
            }
            self.do_quotes &= 1;
            if self.do_quotes == 0 {
                self.inquote = false;
                if linep.last() == Some(&b'\\') {
                    self.do_quotes = 1;
                    if self.start_in_quotes.is_none() {
                        self.inquote = true;
                        self.start_in_quotes = Some(true);
                    } else if backwards {
                        self.inquote = true;
                    }
                }
                if pos.0 > 0 && self.lines.line(pos.0 - 1).last() == Some(&b'\\') {
                    self.do_quotes = 1;
                    if self.start_in_quotes.is_none() {
                        self.inquote = at_start != 0;
                        if self.inquote {
                            self.start_in_quotes = Some(true);
                        }
                    } else if !backwards {
                        self.inquote = true;
                    }
                }
            }
        }
        if self.start_in_quotes.is_none() {
            self.start_in_quotes = Some(false);
        }
    }

    /// `findmatchlimit`, "If 'smartmatch' is set: Things inside quotes are ignored by setting
    /// 'inquote'": a line end without a trailing `\` resets it, an unescaped `"` flips it; any other
    /// char is skipped as a char literal or `'lisp'` `#\(`, else counted ([`Self::count_match`]).
    fn match_char(&mut self) -> MatchStep {
        let (linep, pos) = (self.linep, self.pos);
        let c = at(linep, pos.1);
        match c {
            0 => {
                if pos.1 == 0 || linep[pos.1 - 1] != b'\\' {
                    self.inquote = false;
                    self.start_in_quotes = Some(false);
                }
            }
            b'"' => {
                if self.do_quotes != 0 && backslashes(linep, pos.1).is_multiple_of(2) {
                    self.inquote = !self.inquote;
                    self.start_in_quotes = Some(false);
                }
            }
            _ => {
                if self.skip_char_literal(c) || self.lisp_char(c) {
                    return MatchStep::Next;
                }
                return self.count_match(c);
            }
        }
        MatchStep::Next
    }

    /// `findmatchlimit`, "Skip things in single quotes: 'x' or '\x'": whether `c` opens (or, backward,
    /// closes) one; the scan then stands on its other quote.
    fn skip_char_literal(&mut self, c: u8) -> bool {
        if c != b'\'' || self.t.initc == b'\'' || self.t.findc == b'\'' {
            return false;
        }
        let linep = self.linep;
        let col = &mut self.pos.1;
        if self.t.backwards {
            if *col > 1 {
                if linep[*col - 2] == b'\'' {
                    *col -= 2;
                    return true;
                } else if linep[*col - 2] == b'\\' && *col > 2 && linep[*col - 3] == b'\'' {
                    *col -= 3;
                    return true;
                }
            }
        } else if at(linep, *col + 1) != 0 {
            if at(linep, *col + 1) == b'\\'
                && at(linep, *col + 2) != 0
                && at(linep, *col + 3) == b'\''
            {
                *col += 3;
                return true;
            } else if at(linep, *col + 2) == b'\'' {
                *col += 2;
                return true;
            }
        }
        false
    }

    /// `findmatchlimit`, "For Lisp skip over backslashed (), {} and []. (actually, we skip #\( et
    /// al)": whether `c` is such a character under `'lisp'`.
    fn lisp_char(&self, c: u8) -> bool {
        let (linep, col) = (self.linep, self.pos.1);
        let mut p = 0;
        self.lisp
            && b"(){}[]".contains(&c)
            && col > 1
            && check_prevcol(linep, col, b'\\', &mut p)
            && check_prevcol(linep, col - 1, b'#', &mut p)
    }

    /// `findmatchlimit`, "Check for match outside of quotes, and inside of quotes when the start is
    /// also inside of quotes": `c` counts up as `initc`, down as `findc`, and at zero is the match;
    /// "Only accept a match when ... escaping is what we expect".
    fn count_match(&mut self, c: u8) -> MatchStep {
        if (!self.inquote || self.start_in_quotes == Some(true))
            && (c == self.t.initc || c == self.t.findc)
            && backslashes(self.linep, self.pos.1) & 1 == self.t.match_escaped
        {
            if c == self.t.initc {
                self.count += 1;
            } else {
                if self.count == 0 {
                    return MatchStep::Return(Some(self.pos));
                }
                self.count -= 1;
            }
        }
        MatchStep::Next
    }
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
