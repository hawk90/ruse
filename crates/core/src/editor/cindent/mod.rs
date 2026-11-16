//! C indent (`'cindent'`, #1163): a port of nvim v0.12.6's `get_c_indent` and its helpers
//! (`indent_c.c`), with the parts of `findmatchlimit` (`search.c`) they run: the backward search for a
//! `/*` comment start or a C++ raw string start, and the bracket search that skips `"strings"`,
//! `'c'` characters and escaped brackets. `'cinoptions'` is parsed into [`CinOptions`] as `parse_cino`
//! does, and [`Ci::in_cinkeys`] reads `'cinkeys'` as `in_cinkeys` does.
//!
//! The port keeps nvim's structure (a cursor that the helpers move, 1-based line numbers, a NUL past the
//! end of each line) so each rule can be checked against the C source line by line. nvim's default
//! `'cpoptions'` is assumed (no `%`, no `M`), and `'lisp'` off: under `'lisp'` the Lisp indent wins.
//!
//! A line's indent depends on the lines above it and, for a function type spec, on the line below, so
//! the callers pass the whole buffer.

use super::range::cluster_cells;

mod indent;
mod keys;
mod lex;
mod options;
mod scan;
mod syntax;

pub(crate) use keys::key_reindents;
pub use keys::CinKey;
pub(crate) use lex::check_linecomment;
pub use options::CinOptions;

use lex::{at, line_marks, skipwhite, strend};

/// `'cinkeys'` at its nvim default.
pub const DEFAULT_CINKEYS: &str = "0{,0},0),0],:,0#,!^F,o,O,e";
/// `'cinscopedecls'` at its nvim default.
pub const DEFAULT_CINSCOPEDECLS: &str = "public,protected,private";

/// What the cindent engine reads besides the buffer: the buffer's indent options.
#[derive(Clone, Copy, Debug)]
pub struct CEnv<'a> {
    /// `'tabstop'` and `'vartabstop'`.
    pub ts: crate::cells::Tabs,
    /// `'cinoptions'`, parsed.
    pub cino: CinOptions,
    /// `'cinwords'`.
    pub cinwords: &'a str,
    /// `'cinscopedecls'`.
    pub cinscopedecls: &'a str,
    /// `'cinkeys'`.
    pub cinkeys: &'a str,
    /// `'comments'`.
    pub comments: &'a str,
}

/// A position: 1-based line number, byte column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
struct Pos {
    lnum: usize,
    col: usize,
}

/// The C indent engine over the lines of a buffer, with nvim's moving cursor.
pub(crate) struct Ci<'a, L: AsRef<[u8]>> {
    lines: &'a [L],
    e: &'a CEnv<'a>,
    cur: Pos,
    /// nvim's `State & MODE_INSERT` while indenting.
    insert: bool,
    /// Per line, [`line_marks`]: which comment or raw-string marks it holds, so the backward searches
    /// for them step over a line without any in one go. `None`: worked out per line.
    marks: Option<&'a [u8]>,
}

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    pub(crate) fn new(lines: &'a [L], e: &'a CEnv<'a>) -> Self {
        Ci {
            lines,
            e,
            cur: Pos { lnum: 1, col: 0 },
            insert: false,
            marks: None,
        }
    }

    /// Whether line `lnum` holds the mark `bit`.
    fn has_mark(&self, lnum: usize, bit: u8) -> bool {
        match self.marks.and_then(|m| m.get(lnum.max(1) - 1)) {
            Some(&m) => m & bit != 0,
            None => line_marks(self.line(lnum)) & bit != 0,
        }
    }

    fn o(&self) -> &CinOptions {
        &self.e.cino
    }

    fn count(&self) -> usize {
        self.lines.len().max(1)
    }

    /// `ml_get`: line `lnum` (line 0 reads as line 1, past the end as `???`).
    fn line(&self, lnum: usize) -> &'a [u8] {
        if lnum > self.lines.len() {
            return b"???";
        }
        match self.lines.get(lnum.max(1) - 1) {
            Some(l) => l.as_ref(),
            None => b"",
        }
    }

    fn cur_line(&self) -> &'a [u8] {
        self.line(self.cur.lnum)
    }

    /// The virtual column where the char at byte `col` of line `lnum` starts (`getvcol`).
    fn vcol(&self, lnum: usize, col: usize) -> usize {
        let l = self.line(lnum);
        let end = strend(l, 0);
        let mut v = 0;
        let mut i = 0;
        while i < col.min(end) {
            let (next, w) = cluster_cells(l, i, end, v, self.e.ts);
            v += w;
            i = next;
        }
        v
    }

    /// `get_indent_lnum`: the indent of line `lnum` in columns.
    fn indent_of(&self, lnum: usize) -> i32 {
        let l = self.line(lnum);
        let ts = self.e.ts;
        let mut v = 0usize;
        for &c in l {
            match c {
                b' ' => v += 1,
                b'\t' => v += ts.padding(v),
                _ => break,
            }
        }
        i32::try_from(v).unwrap_or(i32::MAX)
    }

    fn get_indent(&self) -> i32 {
        self.indent_of(self.cur.lnum)
    }

    fn linewhite(&self, lnum: usize) -> bool {
        let l = self.line(lnum);
        at(l, skipwhite(l, 0)) == 0
    }

    // --- findmatchlimit (search.c), the parts cindent runs ---
}

/// The C indent of line `row` (0-based) of `lines`, as `get_c_indent` gives it with the cursor at byte
/// `col` (in Insert mode a `)` there is cut off first). `None` keeps the line (inside a raw string).
pub(crate) fn line_indent<L: AsRef<[u8]>>(
    lines: &[L],
    row: usize,
    col: usize,
    insert: bool,
    e: &CEnv<'_>,
) -> Option<usize> {
    let marks: Vec<u8> = lines.iter().map(|l| line_marks(l.as_ref())).collect();
    marked_line_indent(lines, &marks, row, col, insert, e)
}

/// [`line_indent`] with the lines' [`line_marks`] worked out already.
fn marked_line_indent<L: AsRef<[u8]>>(
    lines: &[L],
    marks: &[u8],
    row: usize,
    col: usize,
    insert: bool,
    e: &CEnv<'_>,
) -> Option<usize> {
    let mut ci = Ci::new(lines, e);
    ci.marks = Some(marks);
    let n = ci.get_c_indent(row + 1, col, insert);
    usize::try_from(n).ok()
}

/// `=` over lines `[first, last]` (0-based) of `b`, as nvim's `op_reindent` runs `get_c_indent`: top
/// down, each line seeing the lines above it already re-indented. A blank line is emptied; `None` keeps
/// a line. (The engine reads indents as columns, so the re-indented lines above are kept as spaces.)
pub(crate) fn reindent(b: &[u8], first: usize, last: usize, e: &CEnv<'_>) -> Vec<Option<usize>> {
    let mut lines: Vec<Vec<u8>> = b.split(|&c| c == b'\n').map(<[u8]>::to_vec).collect();
    let last = last.min(lines.len().saturating_sub(1));
    let mut out = Vec::with_capacity(last + 1 - first.min(last + 1));
    // A re-indent changes only blanks, so the marks hold for the whole run.
    let marks: Vec<u8> = lines.iter().map(|l| line_marks(l)).collect();
    for l in first..=last {
        let fnb = skipwhite(&lines[l], 0);
        let want = if fnb >= lines[l].len() {
            Some(0)
        } else {
            marked_line_indent(&lines, &marks, l, 0, false, e)
        };
        if let Some(c) = want {
            let mut line = vec![b' '; c];
            line.extend_from_slice(&lines[l][fnb..]);
            lines[l] = line;
        }
        out.push(want);
    }
    out
}

#[cfg(test)]
mod tests;
