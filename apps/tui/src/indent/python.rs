//! Python indent (#1205, #1206): a port of nvim v0.12.6's `python#GetIndent` (`autoload/python.vim`,
//! `indent/python.vim`) with its `g:python_indent` defaults, where the node-depth model of the parent
//! module stops matching: the line after `return`/`pass`/`break`/`continue`/`raise` dedents, a line in an
//! open bracket aligns with it (or takes `shiftwidth() * 2` after a bracket ending its line), an explicit
//! `\` join adds `shiftwidth() * 2`, and `elif`/`else`/`except`/`finally` line up with their statement.
//!
//! The script reads strings and comments from the syntax highlighting; here they come from the tree, as
//! for nvim with `syntax on` (a bracket in a string or comment never opens one, a `#` in a string starts
//! no comment, and a line starting inside a string keeps its indent). Without a usable tree the
//! script's own fallback applies: a comment starts at the first `#`.
//!
//! The answer is a column count, `None` for the script's "keep the current indent" (`-1`, or any
//! negative amount, which nvim's `=` and `fixthisline` leave alone).

use tree_sitter::Tree;

/// `s:maxoff`: how many lines a bracket search looks back.
const MAX_OFF: usize = 50;

/// The indent settings the script reads.
#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub shiftwidth: usize,
    pub tabstop: usize,
    pub expandtab: bool,
}

/// One buffer line as the script sees it: its indent (columns and the bytes `set_indent` would write)
/// and the text after it, with where that text sits in the parsed bytes (for the string/comment lookup).
#[derive(Clone)]
struct Line {
    cols: usize,
    ws_len: usize,
    /// Where the line starts in the parsed bytes.
    start: usize,
    text: std::ops::Range<usize>,
}

/// A Python buffer the script runs over: the bytes, their parse tree, and each line's current indent,
/// which `=` updates line by line as nvim's `op_reindent` does.
pub struct Buf<'a> {
    bytes: &'a [u8],
    tree: Option<&'a Tree>,
    lines: Vec<Line>,
    opts: Opts,
}

/// The columns and byte length of the leading blanks in `ws`.
fn blank_cols(ws: &[u8], ts: usize) -> usize {
    ws.iter().fold(0, |c, &b| {
        if b == b'\t' {
            c + ts.max(1) - c % ts.max(1)
        } else {
            c + 1
        }
    })
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Whether `text` starts with one of `words` as a whole word (`^\s*\(w1\|w2\)\>`, the blanks already
/// stripped).
fn starts_with_word(text: &[u8], words: &[&str]) -> bool {
    words
        .iter()
        .any(|w| text.starts_with(w.as_bytes()) && text.get(w.len()).is_none_or(|&b| !is_word(b)))
}

impl<'a> Buf<'a> {
    /// The buffer `bytes` with its parse `tree` (lined up with these bytes; see
    /// [`crate::highlight::CachedHighlight::tree_matching`]).
    pub fn new(bytes: &'a [u8], tree: Option<&'a Tree>, opts: Opts) -> Buf<'a> {
        let mut lines = Vec::new();
        let mut ls = 0usize;
        loop {
            let le = bytes[ls..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |i| ls + i);
            let mut fnb = ls;
            while fnb < le && matches!(bytes[fnb], b' ' | b'\t') {
                fnb += 1;
            }
            lines.push(Line {
                cols: blank_cols(&bytes[ls..fnb], opts.tabstop),
                ws_len: fnb - ls,
                start: ls,
                text: fnb..le,
            });
            if le == bytes.len() {
                break;
            }
            ls = le + 1;
        }
        // The slot after a final newline is kept as an empty line: a line just opened at the end is one.
        Buf {
            bytes,
            tree,
            lines,
            opts,
        }
    }

    /// The number of lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    fn text(&self, l: usize) -> &[u8] {
        &self.bytes[self.lines[l].text.clone()]
    }

    fn indent(&self, l: usize) -> usize {
        self.lines[l].cols
    }

    fn blank(&self, l: usize) -> bool {
        self.lines[l].text.is_empty()
    }

    /// The line `l`'s indent becomes `cols` (what `set_indent` writes: spaces, or tabs then spaces).
    pub fn set_indent(&mut self, l: usize, cols: usize) {
        let ts = self.opts.tabstop.max(1);
        let ws_len = if self.opts.expandtab {
            cols
        } else {
            cols / ts + cols % ts
        };
        self.lines[l].cols = cols;
        self.lines[l].ws_len = ws_len;
    }

    /// The string or comment node the byte at `pos` (in the parsed bytes) sits in.
    fn string_or_comment(&self, pos: usize) -> Option<tree_sitter::Node<'a>> {
        let tree = self.tree?;
        let mut node = tree.root_node().descendant_for_byte_range(pos, pos + 1);
        while let Some(n) = node {
            if n.start_byte() <= pos
                && pos < n.end_byte()
                && matches!(n.kind(), "comment" | "string")
            {
                return Some(n);
            }
            node = n.parent();
        }
        None
    }

    /// Whether the byte at `pos` is in a string or comment (`comment_only`: in a comment).
    fn in_string_or_comment(&self, pos: usize, comment_only: bool) -> bool {
        self.string_or_comment(pos)
            .is_some_and(|n| !comment_only || n.kind() == "comment")
    }

    /// Where line `l`'s byte column `col` sits in the parsed bytes (only its indent may have changed).
    fn pos(&self, l: usize, col: usize) -> Option<usize> {
        let line = &self.lines[l];
        col.checked_sub(line.ws_len).map(|i| line.text.start + i)
    }

    /// `s:SearchBracket(from, …)` with the cursor on line `l`'s byte column `col`: the innermost bracket
    /// left open before the cursor, looking back to line `from - 50`, skipping brackets in strings and
    /// comments. Its line and byte column.
    fn search_bracket(&self, from: usize, l: usize, col: usize) -> Option<(usize, usize)> {
        // `[0, a:fromlnum - s:maxoff]->max()` on 1-based lines: line `from - 50` is the last one searched.
        let stop = from.saturating_sub(MAX_OFF);
        let mut nest = 0usize;
        let mut l = l;
        let mut end = col; // the columns before it are searched
        loop {
            let line = &self.lines[l];
            let text = self.text(l);
            for c in (line.ws_len..end).rev() {
                let ch = text[c - line.ws_len];
                if !matches!(ch, b'(' | b'[' | b'{' | b')' | b']' | b'}') {
                    continue;
                }
                if self
                    .pos(l, c)
                    .is_some_and(|p| self.in_string_or_comment(p, false))
                {
                    continue;
                }
                if matches!(ch, b')' | b']' | b'}') {
                    nest += 1;
                } else if nest == 0 {
                    return Some((l, c));
                } else {
                    nest -= 1;
                }
            }
            if l == 0 || l <= stop {
                return None;
            }
            l -= 1;
            end = self.lines[l].ws_len + self.lines[l].text.len();
        }
    }

    /// The line's text with a trailing comment removed.
    fn without_comment(&self, l: usize) -> &[u8] {
        let text = self.text(l);
        if self.tree.is_some() {
            // Is the last character in a comment? Then cut at the comment's start.
            let Some(last) = text.len().checked_sub(1) else {
                return text;
            };
            let start = self.lines[l].text.start;
            if !self.in_string_or_comment(start + last, true) {
                return text;
            }
            let (mut lo, mut hi) = (0usize, last);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if self.in_string_or_comment(start + mid, true) {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            &text[..lo]
        } else {
            match text.iter().position(|&b| b == b'#') {
                Some(i) => &text[..i],
                None => text,
            }
        }
    }

    /// `python#GetIndent(lnum)` for the 0-based line `l`: the columns to indent it to, or `None` to keep
    /// its indent.
    pub fn get_indent(&self, l: usize) -> Option<usize> {
        let sw = self.opts.shiftwidth;
        let dedented = |l: usize, expected: usize| self.indent(l) + sw <= expected;
        // An explicitly joined line: line up with a joined line above, else two shiftwidths in.
        let joined = |l: usize| self.text(l).last() == Some(&b'\\');
        if l >= 1 && joined(l - 1) {
            if l >= 2 && joined(l - 2) {
                return Some(self.indent(l - 1));
            }
            return Some(self.indent(l - 1) + 2 * sw);
        }
        // A line starting in a string keeps its indent (the syntax name at its first column ends in
        // `String`: inside a string opened on an earlier line).
        let ls = self.lines[l].start;
        if self
            .string_or_comment(ls)
            .is_some_and(|n| n.kind() == "string" && n.start_byte() < ls)
        {
            return None;
        }
        // The first non-blank line is at column zero.
        let Some(plnum) = (0..l).rev().find(|&p| !self.blank(p)) else {
            return Some(0);
        };
        // In a bracket: align with it, unless it ends its line.
        if let Some((pl, pc)) = self.search_bracket(l, l, 0) {
            let len = self.lines[pl].ws_len + self.lines[pl].text.len();
            if pc + 1 != len {
                return Some(pc + 1);
            }
        }
        // The previous line in brackets counts from the line they open on.
        let pl_paren = self.search_bracket(plnum, plnum, 0).map(|(pl, _)| pl);
        let (plindent, plnumstart) = match pl_paren {
            Some(pl) => (self.indent(pl), pl),
            None => (self.indent(plnum), plnum),
        };
        // In brackets: two shiftwidths in on the first line below them (one inside other brackets), else
        // as the previous line.
        if let Some((p, pc)) = self.search_bracket(l, l, 0) {
            if p == plnum {
                if self.search_bracket(l, p, pc).is_some() {
                    return Some(self.indent(plnum) + sw);
                }
                return Some(self.indent(plnum) + 2 * sw);
            }
            if plnumstart == p {
                return Some(self.indent(plnum));
            }
            return Some(plindent);
        }
        let pline = self.without_comment(plnum);
        // After a line ending in a colon, one in.
        let mut end = pline.len();
        while end > 0 && matches!(pline[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        if end > 0 && pline[end - 1] == b':' {
            return Some(plindent + sw);
        }
        // After a stop-execution statement, one out unless the user already dedented.
        if starts_with_word(
            self.text(plnum),
            &["break", "continue", "raise", "return", "pass"],
        ) {
            if dedented(l, self.indent(plnum)) {
                return None;
            }
            return self.indent(plnum).checked_sub(sw);
        }
        let text = self.text(l);
        // `except`/`finally` line up with the `try` or `except` above.
        if starts_with_word(text, &["except", "finally"]) {
            for p in (0..l).rev() {
                if starts_with_word(self.text(p), &["try", "except"]) {
                    let ind = self.indent(p);
                    if ind >= self.indent(l) {
                        return None;
                    }
                    return Some(ind);
                }
            }
            return None;
        }
        // `elif`/`else` dedent, unless the line above was a one-liner or the user already dedented.
        if starts_with_word(text, &["elif", "else"]) {
            if starts_with_word(self.text(plnumstart), &["for", "if", "elif", "try"]) {
                return Some(plindent);
            }
            if dedented(l, plindent) {
                return None;
            }
            return plindent.checked_sub(sw);
        }
        // After a bracketed construct, back to its start line.
        if pl_paren.is_some() {
            if dedented(l, plindent) {
                return None;
            }
            return Some(plindent);
        }
        None
    }

    /// `=` over lines `[first, last]`: each line's new indent (`None` keeps it), computed top down with the
    /// lines above already re-indented, as nvim's `op_reindent` runs the expression line by line. A blank
    /// line is emptied.
    pub fn reindent(&mut self, first: usize, last: usize) -> Vec<Option<usize>> {
        let last = last.min(self.len().saturating_sub(1));
        let mut out = Vec::new();
        for l in first..=last {
            let want = if self.blank(l) {
                Some(0)
            } else {
                self.get_indent(l)
            };
            if let Some(c) = want {
                self.set_indent(l, c);
            }
            out.push(want);
        }
        out
    }
}

/// Whether typing `ch` re-indents the line in nvim's Python 'indentkeys' (`0{,0},0),0],:,0#,!^F,o,O,e`
/// plus `<:>,=elif,=except`): `before` is the line's text up to the caret after `ch` went in, and `white`
/// whether only blanks preceded the caret before it did (`inindent(0)`).
pub fn reindents_on(ch: char, before: &[u8], white: bool) -> bool {
    let word_end = |w: &str| {
        before.ends_with(w.as_bytes())
            && before
                .len()
                .checked_sub(w.len() + 1)
                .is_none_or(|i| !is_word(before[i]))
    };
    match ch {
        // `0{`, `0}`, `0)`, `0]`, `0#`: typed first on the line.
        '{' | '}' | '(' | ')' | '[' | ']' | '#' if white => !matches!(ch, '(' | '['),
        // `<:>` (and `:`): any colon.
        ':' => true,
        // `e`: the `e` ending an `else` at the line's start.
        'e' => {
            let lead = before.len() - before.trim_ascii_start().len();
            &before[lead..] == b"else"
        }
        // `=elif`, `=except`: the word's last char, after a non-word char or at the line start.
        'f' => word_end("elif"),
        't' => word_end("except"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(src: &[u8]) -> Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        p.parse(src, None).unwrap()
    }

    const OPTS: Opts = Opts {
        shiftwidth: 4,
        tabstop: 8,
        expandtab: true,
    };

    fn indent_of(src: &str, l: usize) -> Option<usize> {
        let tree = parse(src.as_bytes());
        Buf::new(src.as_bytes(), Some(&tree), OPTS).get_indent(l)
    }

    #[test]
    fn stop_statements_dedent() {
        assert_eq!(indent_of("def f():\n    return x\n    \n", 2), Some(0));
        assert_eq!(
            indent_of("for i in r:\n    if a:\n        break\n        \n", 3),
            Some(4)
        );
        // Already dedented by the user: kept.
        assert_eq!(indent_of("def f():\n    return x\ny\n", 2), None);
        // `returned = 1` is no `return`.
        assert_eq!(indent_of("def f():\n    returned = 1\n    \n", 2), None);
        // A top-level `pass` has nothing to dedent to: kept.
        assert_eq!(indent_of("pass\n    y\n", 1), None);
    }

    #[test]
    fn brackets_align_or_take_two_shiftwidths() {
        assert_eq!(indent_of("x = foo(a,\nb)\n", 1), Some(8));
        assert_eq!(indent_of("x = [1,\n2]\n", 1), Some(5));
        assert_eq!(indent_of("x = foo(\nb)\n", 1), Some(8));
        assert_eq!(indent_of("x = foo(bar(\na\n", 1), Some(4));
        // A bracket in a string or comment opens nothing.
        assert_eq!(indent_of("x = '('\ny = 1\nq\n", 2), None);
        assert_eq!(indent_of("x = 1  # (\ny = 1\nq\n", 2), None);
        // A comment after the colon is no text.
        assert_eq!(indent_of("if a:  # c\nq\n", 1), Some(4));
        assert_eq!(indent_of("if a == '#':\nq\n", 1), Some(4));
    }

    #[test]
    fn reindent_runs_top_down() {
        let src = "def f():\n    x = foo(a,\nb,\nc)\n    y = 2\n";
        let tree = parse(src.as_bytes());
        let mut buf = Buf::new(src.as_bytes(), Some(&tree), OPTS);
        assert_eq!(
            buf.reindent(0, 4),
            vec![Some(0), Some(4), Some(12), Some(12), Some(4)]
        );
    }

    #[test]
    fn indentkeys() {
        assert!(reindents_on('e', b"  else", false));
        assert!(!reindents_on('e', b"  x else", false));
        assert!(reindents_on(':', b"x = d[1:", false));
        assert!(reindents_on('f', b"elif", false));
        assert!(reindents_on('f', b"x elif", false));
        assert!(!reindents_on('f', b"xelif", false));
        assert!(reindents_on('t', b"    except", false));
        assert!(reindents_on(')', b"    )", true));
        assert!(!reindents_on(')', b"  a)", false));
        assert!(reindents_on('#', b"#", true));
        assert!(!reindents_on('(', b"(", true));
    }
}
