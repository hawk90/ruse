//! Tree-aware indentation (F-015): compute each line's indent LEVEL from the tree-sitter parse tree, for
//! the `=` reindent operator ([`indent_levels`], Phase 1) and for auto-indent on `o`/`O`/`<CR>`
//! ([`suggest_indent`], Phase 2). The frontend owns the tree (the core is dependency-free), so it derives
//! the levels here and hands them to the core, which applies `level × shiftwidth` (the `*`/`#` split).
//!
//! **Model — node-depth (language-agnostic, no `indents.scm` needed).** A line's level is the number of
//! DISTINCT start-rows among the multi-line ancestor nodes it sits inside (a node counts when it spans
//! more than one row and starts on an earlier row than the line). De-duplicating by start-row folds nested
//! nodes that open together (`function_item` + its `block` both start on the `fn` line) into one level. A
//! line whose first non-blank is a closer (`}`/`)`/`]`) dedents one, so closers align with their opener.
//! Robust where raw bracket-depth is not: a `{` inside a string or comment is a leaf/`string` node, so it
//! never opens a block. A heuristic, not full Vim/Helix indent-query fidelity.
//!
//! Two kinds of node never open a level (#818): the ROOT (`source_file`, `module`, …), which encloses
//! every top-level line, and a `block` that starts on its first statement rather than on an opener token
//! (Python's; Rust's starts on `{`): the `def` above it already opened the level, so counting the block as
//! well would put the second body line a level deeper than the first. Markdown is not indented by node depth at all
//! ([`uses_node_depth`]): its `section` nodes nest per heading, so depth says nothing about indent.
//!
//! Python is not indented by node depth either: [`python`] ports nvim's indent script, which answers in
//! columns (a bracket continuation aligns with its bracket) and re-indents after the edit that triggers it
//! (`crate::app::indent_keys`, #1205, #1206).

use std::borrow::Cow;

use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

pub mod python;

/// How far back [`IndentTree::window`] looks for a top-level line to start at.
const WINDOW_ROWS: usize = 1000;

/// A parse tree to compute indent levels from, over a buffer or a window of it (#1214).
///
/// A buffer whose tree has no error is indented by that tree. Once a block is left open somewhere, though,
/// re-parsing the whole buffer with its closers appended costs tens of milliseconds a keystroke on a
/// 100k-line file, and nests everything after the open block inside it. So the lines asked about are
/// parsed in a WINDOW instead: from the nearest top-level line above them (a non-blank line that starts in
/// column 0 and does not continue or close a block, at most [`WINDOW_ROWS`] back) to the end of the last of
/// them, with the window's own open blocks closed ([`closed_tree`]). Its depth is then counted from that
/// top-level line, as nvim's indent scripts count from the lines above. All positions taken and given are
/// the buffer's.
pub struct IndentTree<'a> {
    tree: Cow<'a, Tree>,
    bytes: Cow<'a, [u8]>,
    /// The buffer byte and row the tree's text starts at.
    base: usize,
    base_row: usize,
}

impl<'a> IndentTree<'a> {
    /// The tree for the buffer lines from row `first_row` (which starts at byte `first_ls`) to the line
    /// ending at byte `last_le`: `tree` itself when it has no error, else a window parse.
    pub fn new(
        tree: &'a Tree,
        bytes: &'a [u8],
        first_row: usize,
        first_ls: usize,
        last_le: usize,
    ) -> IndentTree<'a> {
        let whole = || IndentTree {
            tree: Cow::Borrowed(tree),
            bytes: Cow::Borrowed(bytes),
            base: 0,
            base_row: 0,
        };
        if !tree.root_node().has_error() {
            return whole();
        }
        IndentTree::window(tree, bytes, first_row, first_ls, last_le).unwrap_or_else(whole)
    }

    /// A window parse (see the type) in the language of `like`, for the same lines as [`IndentTree::new`].
    /// Only the window's own lines are read, so the cost does not grow with the buffer.
    pub fn window(
        like: &Tree,
        bytes: &'a [u8],
        first_row: usize,
        first_ls: usize,
        last_le: usize,
    ) -> Option<IndentTree<'a>> {
        let (base, back) = window_start(bytes, first_ls);
        let text = &bytes[base..last_le.min(bytes.len())];
        let mut parser = Parser::new();
        parser.set_language(&like.language()).ok()?;
        let tree = parser.parse(text, None)?;
        let tree = closed_tree(&tree, text).into_owned();
        Some(IndentTree {
            tree: Cow::Owned(tree),
            bytes: Cow::Borrowed(text),
            base,
            base_row: first_row - back,
        })
    }

    /// [`indent_levels`] for buffer lines `first_line..=last_line`.
    pub fn levels(&self, first_line: usize, last_line: usize) -> Vec<usize> {
        indent_levels(
            &self.tree,
            &self.bytes,
            first_line - self.base_row,
            last_line - self.base_row,
        )
    }

    /// [`suggest_indent`] at buffer byte `at` for buffer row `new_row`.
    pub fn suggest_indent(&self, at: usize, new_row: usize) -> usize {
        suggest_indent(&self.tree, at - self.base, new_row - self.base_row)
    }

    /// [`suggest_open_below`] for the buffer line `[ls, le)` and buffer row `new_row`.
    pub fn open_below(&self, ls: usize, le: usize, new_row: usize) -> usize {
        suggest_open_below(
            &self.tree,
            &self.bytes,
            ls - self.base,
            le - self.base,
            new_row - self.base_row,
        )
    }

    /// [`broken_at`] at buffer byte `at`.
    pub fn broken_at(&self, at: usize) -> bool {
        broken_at(&self.tree, &self.bytes, at - self.base)
    }
}

/// Where a window for the lines from the one starting at byte `ls` on starts: the start of the nearest
/// top-level line above it (or the buffer's first line), at most [`WINDOW_ROWS`] back, and how many rows
/// back that is. A top-level
/// line is non-blank, starts in column 0, and does not begin with a closer or a word that continues a
/// statement (`else`, `fi`, …); a closer in column 0 above the first line ends a top-level block, so the
/// window may start on the line after it.
fn window_start(bytes: &[u8], ls: usize) -> (usize, usize) {
    let mut start = ls;
    let mut below = None; // the line after a column-0 closer: (start, rows back)
    for back in 0..=WINDOW_ROWS {
        let le = bytes[start..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(bytes.len(), |i| start + i);
        // The first line's own column is what is being decided (a line just dedented by `fi` reads `fin`
        // in column 0), so the window starts above it unless it is the buffer's first.
        let line = if back == 0 {
            &[][..]
        } else {
            &bytes[start..le]
        };
        if let Some(&c) = line.first().filter(|c| !c.is_ascii_whitespace()) {
            let word = line
                .split(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                .next()
                .unwrap_or(b"");
            let continues = matches!(
                word,
                b"else"
                    | b"elif"
                    | b"fi"
                    | b"done"
                    | b"esac"
                    | b"then"
                    | b"do"
                    | b"catch"
                    | b"finally"
            );
            if !continues && !matches!(c, b'}' | b')' | b']') {
                return (start, back);
            }
            if !continues && back > 0 {
                if let Some(b) = below {
                    return b;
                }
            }
        }
        if start == 0 {
            return (0, back);
        }
        below = Some((start, back));
        start = bytes[..start - 1]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |i| i + 1);
    }
    below.unwrap_or((ls, 0))
}

/// The tree to indent `bytes` by (#1214). While a block is being typed it is not closed yet, and
/// tree-sitter's error recovery then folds the whole function into an `ERROR` node (Rust) or ends the
/// block at the end of the file (C, JavaScript), so its depth no longer says where a line belongs. When
/// the tree has an error, `bytes` is parsed again with the closers of its unclosed brackets appended (on
/// lines of their own, at the end of the file), as the user is about to type them; the rows of `bytes`
/// keep their positions. A tree without errors, or with none of its brackets left open, is used as it is.
pub fn closed_tree<'t>(tree: &'t Tree, bytes: &[u8]) -> Cow<'t, Tree> {
    if !tree.root_node().has_error() {
        return Cow::Borrowed(tree);
    }
    let closers = unclosed_blocks(tree);
    if closers.is_empty() {
        return Cow::Borrowed(tree);
    }
    let mut patched = bytes.to_vec();
    for c in closers.iter().rev() {
        patched.push(b'\n');
        patched.extend_from_slice(c.as_bytes());
    }
    let end = point_at_end(bytes);
    let mut old = tree.clone();
    old.edit(&InputEdit {
        start_byte: bytes.len(),
        old_end_byte: bytes.len(),
        new_end_byte: patched.len(),
        start_position: end,
        old_end_position: end,
        new_end_position: point_at_end(&patched),
    });
    let mut parser = Parser::new();
    if parser.set_language(&tree.language()).is_err() {
        return Cow::Borrowed(tree);
    }
    parser
        .parse(&patched, Some(&old))
        .map_or(Cow::Borrowed(tree), Cow::Owned)
}

/// The closers of the blocks `tree` leaves open, innermost last: its brackets, and in a shell script
/// `if`/`do`/`case` (`fi`, `done`, `esac`). Only TOKENS count, so a `{` in a string or a comment (part of a
/// larger leaf) opens nothing; a closer tree-sitter only assumed (`MISSING`) closes nothing.
fn unclosed_blocks(tree: &Tree) -> Vec<&'static str> {
    let shell = tree.language().name() == Some("bash");
    let mut open: Vec<&'static str> = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.child_count() == 0 && !node.is_missing() {
            let opened = match node.kind() {
                "{" => Some("}"),
                "(" => Some(")"),
                "[" => Some("]"),
                "if" if shell => Some("fi"),
                "do" if shell => Some("done"),
                "case" if shell => Some("esac"),
                _ => None,
            };
            match opened {
                Some(c) => open.push(c),
                None if open.last() == Some(&node.kind()) => {
                    open.pop();
                }
                None => {}
            }
        }
        if cursor.goto_first_child() || cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return open;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// The row and byte column just past `bytes`.
fn point_at_end(bytes: &[u8]) -> Point {
    let row = bytes.iter().filter(|&&c| c == b'\n').count();
    let column = bytes.len() - bytes.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
    Point { row, column }
}

/// Whether the tree is broken where a line is opened at `at` (#1214): the node at `at`, or at the last
/// non-blank byte before it on its line, lies in an `ERROR` node or is a `MISSING` one. The tree's depth
/// says nothing there, so the line keeps the indent of the one it is opened from (autoindent), as nvim's
/// indent scripts, which read the lines above rather than a tree, in effect do.
pub fn broken_at(tree: &Tree, bytes: &[u8], at: usize) -> bool {
    let root = tree.root_node();
    let at = at.min(bytes.len());
    let ls = bytes[..at]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let mut last = at;
    while last > ls && matches!(bytes[last - 1], b' ' | b'\t' | b'\r') {
        last -= 1;
    }
    let mut probes = vec![at];
    if last > ls {
        probes.push(last - 1);
    }
    probes.into_iter().any(|p| {
        let mut node: Option<Node<'_>> = root.descendant_for_byte_range(p, p);
        while let Some(nd) = node {
            if nd.is_error() || nd.is_missing() {
                return true;
            }
            node = nd.parent();
        }
        false
    })
}

/// Whether the tree is Python's, which [`python`] indents the way nvim's indent script does (#1205).
pub fn is_python(tree: &Tree) -> bool {
    tree.language().name() == Some("python")
}

/// The byte offset of each line start in `bytes` (line 0 starts at 0; one entry per line, including the
/// final line after a trailing newline).
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// The first non-blank byte on the line `[ls, le)`, or `le` when the line is blank.
fn first_non_blank(bytes: &[u8], ls: usize, le: usize) -> usize {
    let mut i = ls;
    while i < le && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Whether the tree's language is indented by node depth. Markdown is not: a heading opens a `section`
/// that runs to the next heading of its level, so every line under `## Sub` would sit two sections deep.
/// Without a tree-aware level the core's plain `=` and `o` apply, as in nvim, which has no Markdown indent.
pub fn uses_node_depth(tree: &Tree) -> bool {
    tree.language().name() != Some("markdown")
}

/// The number of indent levels the ancestors of `node` (itself included) open for a line on `row`: the
/// DISTINCT start rows among the multi-line nodes that start on an earlier row, leaving out the root and
/// a statement-led `block` (see the module docs).
fn depth(node: Option<tree_sitter::Node<'_>>, row: usize) -> usize {
    let mut level = 0usize;
    let mut last_row: Option<usize> = None;
    let mut node = node;
    // The node is a clause of the statement above it, which then opens no level of its own.
    let mut in_clause = false;
    while let Some(nd) = node {
        let Some(parent) = nd.parent() else {
            break; // the root encloses every line, so it opens no level
        };
        let sr = nd.start_position().row;
        let er = nd.end_position().row;
        // Go's `statement_list` is the run of statements inside a block, which already opened the level.
        let opens = match nd.kind() {
            "block" => nd.child(0).is_some_and(|c| !c.is_named()),
            "statement_list" => false,
            _ => true,
        };
        // Nested nodes that start on the SAME row count once (de-dup by start-row, non-increasing as we
        // walk up).
        if er > sr && sr < row && opens && Some(sr) != last_row {
            // A statement a clause continues takes its row (a node wrapping it there counts no level
            // either) but adds no level.
            if !in_clause {
                level += 1;
            }
            last_row = Some(sr);
        }
        in_clause = is_clause(nd.kind());
        node = Some(parent);
    }
    level
}

/// Whether a node of `kind` is a clause that continues the statement it belongs to at that statement's own
/// level (#1142): Python's `elif`/`else`/`except`/`finally` (of `if`, `for`, `while`, `try`), Rust's
/// `else`. The statement then opens no level for what the clause holds, so `else:` lines up with its `if`
/// and the `else` body sits one level under both, as nvim's Python indent puts them.
///
/// Go's `case`/`default` line up with their `switch`, as nvim's Go indent puts them (#1206); C's and
/// JavaScript's sit a level in, as cindent and nvim's JavaScript indent put them. A shell `do_group` (`do …
/// done`) continues its `for`/`while`, so a `do` on a line of its own lines up with it.
fn is_clause(kind: &str) -> bool {
    matches!(
        kind,
        "else_clause"
            | "elif_clause"
            | "except_clause"
            | "except_group_clause"
            | "finally_clause"
            | "expression_case"
            | "type_case"
            | "default_case"
            | "communication_case"
            | "do_group"
    )
}

/// Whether `node`, the first token of a line, is a shell keyword that lines up with the statement it
/// belongs to (#1206): `then`, `fi`, `done`, `esac`, as nvim's `GetShIndent` puts them. (`else`, `elif`
/// and a `do` that starts its `do_group` are clauses, which [`depth`] already lines up.)
fn closes_shell_block(tree: &Tree, node: Option<Node<'_>>) -> bool {
    tree.language().name() == Some("bash")
        && node
            .is_some_and(|n| !n.is_named() && matches!(n.kind(), "then" | "fi" | "done" | "esac"))
}

/// What a typed key of nvim's default 'indentkeys' / 'cinkeys' for a tree-indented language does to its
/// line (#1206).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyReindent {
    /// The line takes the indent of its depth in the tree.
    Depth,
    /// The line moves to column 0 (cindent's `0#`: a preprocessor line).
    Column0,
}

/// Whether typing `ch` re-indents its line in the language `lang` (a tree-sitter language name), by
/// nvim's 'indentkeys' for that file type: `before` is the line up to the caret after `ch` went in, and
/// `white` whether only blanks preceded `ch` (`0` in the key list). The closers `0}`, `0)`, `0]` are
/// [`ruse_core::Command::InsertCloser`]'s, and `0{` is not honoured (node depth puts an Allman-style
/// `{` a level too deep).
/// - C (cindent's 'cinkeys' `0{,0},0),0],:,0#,!^F,o,O,e`): `0#`, `e`, and `:` ending a `case`/`default`.
/// - JavaScript (`indentkeys` default `+0],0)`): `e` and `:` as for C (its `0#` changes nothing).
/// - Go (`+<:>,0=},0=)`): `:` as for C.
/// - sh (`+0=then,0=do,0=else,0=elif,0=fi,0=esac,0=done,0=end,),0=;;,0=;&,0=fin,0=fil,0=fip,0=fir,0=fix`,
///   `-:,0#`): the words, as the line's first word; `e`. Not `)` (case patterns).
/// - Rust, JSON, CSS: only the brackets.
pub fn key_reindent(lang: &str, ch: char, before: &[u8], white: bool) -> Option<KeyReindent> {
    let word = before.trim_ascii_start();
    let starts_case = || {
        let w = word
            .split(|&c| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or(b"");
        w == b"case" || w == b"default"
    };
    match (lang, ch) {
        ("c", '#') if white => Some(KeyReindent::Column0),
        ("c" | "javascript" | "bash", 'e') if word == b"else" => Some(KeyReindent::Depth),
        ("c" | "javascript" | "go", ':') if starts_case() => Some(KeyReindent::Depth),
        ("bash", _)
            if matches!(
                word,
                b"then"
                    | b"do"
                    | b"elif"
                    | b"fi"
                    | b"esac"
                    | b"done"
                    | b"end"
                    | b";;"
                    | b";&"
                    | b"fin"
                    | b"fil"
                    | b"fip"
                    | b"fir"
                    | b"fix"
            ) =>
        {
            Some(KeyReindent::Depth)
        }
        _ => None,
    }
}

/// A statement to fill in after a just-typed keyword `word` of `lang` that waits for one (`else`,
/// `elif`), so the tree places the keyword's line; empty when there is none.
pub fn key_placeholder(lang: &str, word: &[u8]) -> &'static [u8] {
    match (lang, word) {
        ("c" | "javascript", b"else") => b" ;",
        ("bash", b"else" | b"then" | b"do") => b" :",
        ("bash", b"elif") => b" :; then :",
        _ => b"",
    }
}

/// The indent LEVEL for each line in `[first_line, last_line]` (inclusive, clamped), from the parse tree.
/// See the module docs for the node-depth model. Blank lines get a level too, but the core leaves blank
/// lines empty, so it is unused there.
pub fn indent_levels(tree: &Tree, bytes: &[u8], first_line: usize, last_line: usize) -> Vec<usize> {
    let root = tree.root_node();
    let starts = line_starts(bytes);
    let last = last_line.min(starts.len().saturating_sub(1));
    let mut out = Vec::new();
    for li in first_line..=last {
        let ls = starts[li];
        let le = starts
            .get(li + 1)
            .map_or(bytes.len(), |&n| n.saturating_sub(1)); // exclude the '\n'
        let fnb = first_non_blank(bytes, ls, le);
        let blank = fnb >= le;
        let pos = if blank { ls } else { fnb };

        let node = root.descendant_for_byte_range(pos, pos);
        let mut level = depth(node, li);
        if !blank && (matches!(bytes[fnb], b')' | b']' | b'}') || closes_shell_block(tree, node)) {
            level = level.saturating_sub(1);
        }
        out.push(level);
    }
    out
}

/// The indent LEVEL to seed a NEW line at, from the parse tree (F-015 Phase 2 — auto-indent on `o`/`O`/
/// `<CR>`). `at` is the byte offset the newline is inserted at (end of the current line for `o`, its start
/// for `O`, the cursor for `<CR>`); `new_row` is the 0-based row the new line will occupy. Counts the
/// DISTINCT start-rows among the multi-line ancestor nodes that enclose `at` and start on an earlier row —
/// the same node-depth model as [`indent_levels`], but for an insertion point rather than an existing line
/// (no closer-dedent: the new line is blank). Querying at `at` (past a just-typed `{`) makes an opener on
/// the current line deepen the new line, so `o` after `fn f() {` indents one further.
pub fn suggest_indent(tree: &Tree, at: usize, new_row: usize) -> usize {
    depth(tree.root_node().descendant_for_byte_range(at, at), new_row)
}

/// [`suggest_indent`] for `o` on the line `[ls, le)`. In Python a block ends with its last statement, so
/// the line end after a block's last line lies outside the block: the line's own last character is asked
/// instead, and `o` there stays in the block, as nvim's Python indent keeps the line's indent (#1142).
pub fn suggest_open_below(
    tree: &Tree,
    bytes: &[u8],
    ls: usize,
    le: usize,
    new_row: usize,
) -> usize {
    let at = if tree.language().name() == Some("python") {
        let mut e = le;
        while e > ls && matches!(bytes[e - 1], b' ' | b'\t' | b'\r') {
            e -= 1;
        }
        if e > ls {
            e - 1
        } else {
            le
        }
    } else {
        le
    };
    suggest_indent(tree, at, new_row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_rust(src: &[u8]) -> Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        p.parse(src, None).unwrap()
    }

    #[test]
    fn node_depth_levels_are_tree_aware() {
        // A `{` inside the string on line 1 must NOT add a level (the bracket-depth `=` gets this wrong).
        let src = b"fn f() {\nlet s = \"x { y\";\nif a {\nb;\n}\n}\n";
        let tree = parse_rust(src);
        assert_eq!(
            indent_levels(&tree, src, 0, 5),
            vec![0, 1, 1, 2, 1, 0],
            "fn body = 1, if body = 2, closers dedent, string-brace ignored"
        );
    }

    #[test]
    fn suggest_indent_uses_node_depth_at_insertion_point() {
        let src = b"fn f() {\nlet s = \"x { y\";\nif a {\nb;\n}\n}\n";
        let tree = parse_rust(src);
        let starts = line_starts(src);
        let eol = |row: usize| starts[row + 1] - 1; // the byte before the row's '\n'
                                                    // `o` after `fn f() {` → the new row 1 is one level deep (the opener on row 0 counts).
        assert_eq!(suggest_indent(&tree, eol(0), 1), 1);
        // `o` after `if a {` → the new row 3 is two levels deep.
        assert_eq!(suggest_indent(&tree, eol(2), 3), 2);
        // `O` above `b;` (row 3) → the inserted row-3 line sits inside the if-block = level 2.
        assert_eq!(suggest_indent(&tree, starts[3], 3), 2);
        // The `{` inside the string on row 1 must NOT deepen: `o` there opens row 2 at level 1, not 2.
        assert_eq!(suggest_indent(&tree, eol(1), 2), 1);
    }

    fn parse(lang: tree_sitter::Language, src: &[u8]) -> Tree {
        let mut p = Parser::new();
        p.set_language(&lang).unwrap();
        p.parse(src, None).unwrap()
    }

    /// #818: the root node encloses every line, so it must not indent the top-level ones. The old walk
    /// counted it, and a multi-line node starting on row 0 hid that in the tests above.
    #[test]
    fn top_level_lines_stay_at_level_zero() {
        let src = b"fn a() {}\nfn b() {\n    let x = 1;\n}\n\nstruct S;\n";
        let tree = parse_rust(src);
        assert_eq!(indent_levels(&tree, src, 0, 5), vec![0, 0, 1, 0, 0, 0]);
        // `o` after a closed top-level fn opens a top-level line.
        let src = b"fn a() {}\nfn b() {}\n";
        let tree = parse_rust(src);
        assert_eq!(suggest_indent(&tree, 9, 1), 0);
    }

    /// Python: a body is one level under its `def`; the statement-led `block` adds none (#818).
    #[test]
    fn python_bodies_are_one_level_deep() {
        let src = b"import os\ndef f():\n    x = 1\n    y = 2\n\nz = 3\n";
        let tree = parse(tree_sitter_python::LANGUAGE.into(), src);
        assert_eq!(indent_levels(&tree, src, 0, 5), vec![0, 0, 1, 1, 0, 0]);
    }

    /// #1142: `elif`/`else`/`except`/`finally` line up with the statement they continue, and their bodies
    /// sit one level under it, as nvim's Python indent has them.
    #[test]
    fn python_clauses_line_up_with_their_statement() {
        let py = |src: &[u8]| parse(tree_sitter_python::LANGUAGE.into(), src);
        let src = b"if a:\n    x = 1\nelif b:\n    y = 2\nelse:\n    w = 0\nz = 3\n";
        assert_eq!(
            indent_levels(&py(src), src, 0, 6),
            vec![0, 1, 0, 1, 0, 1, 0]
        );
        let src = b"try:\n    x\nexcept E:\n    y\nelse:\n    v\nfinally:\n    w\n";
        assert_eq!(
            indent_levels(&py(src), src, 0, 7),
            vec![0, 1, 0, 1, 0, 1, 0, 1]
        );
        let src = b"def f():\n    for i in r:\n        x\n    else:\n        y\n    return 3\n";
        assert_eq!(indent_levels(&py(src), src, 0, 5), vec![0, 1, 2, 1, 2, 1]);
        // `o` on `else:` and in its body opens a body line; `O` above `else:` keeps its level, as nvim's
        // `O` copies the indent of the `else:` line.
        let src = b"if a:\n    x = 1\nelse:\n    y = 2\n    w = 3\nz = 3\n";
        let tree = py(src);
        let starts = line_starts(src);
        let eol = |row: usize| starts[row + 1] - 1;
        assert_eq!(suggest_indent(&tree, eol(2), 3), 1);
        assert_eq!(suggest_indent(&tree, eol(3), 4), 1);
        assert_eq!(suggest_indent(&tree, starts[2], 2), 0);
        // `o` on the block's LAST line (`w = 3`, then `z = 3` one level out) stays in the block.
        assert_eq!(suggest_open_below(&tree, src, starts[4], eol(4), 5), 1);
        assert_eq!(suggest_open_below(&tree, src, starts[5], eol(5), 6), 0);
        // Rust asks at the line end: `o` after a closing `}` opens outside its block.
        let src = b"fn a() {\n    x;\n}\nfn b() {}\n";
        let starts = line_starts(src);
        assert_eq!(
            suggest_open_below(&parse_rust(src), src, starts[2], starts[3] - 1, 3),
            0
        );
    }

    /// Rust's `else` body sits one level under the `if`, not two (#1142).
    #[test]
    fn rust_else_bodies_are_one_level_deep() {
        let src = b"fn f() {\nif a {\nx;\n} else {\ny;\n}\n}\n";
        let tree = parse_rust(src);
        assert_eq!(indent_levels(&tree, src, 0, 6), vec![0, 1, 2, 1, 2, 1, 0]);
    }

    /// Markdown is not indented by node depth: its sections nest per heading (#818).
    #[test]
    fn markdown_is_not_node_depth_indented() {
        let src = b"# Top\n\n## Sub\n\nSome paragraph\nmore\n";
        let tree = parse(tree_sitter_md::LANGUAGE.into(), src);
        assert!(!uses_node_depth(&tree));
        assert!(uses_node_depth(&parse_rust(b"fn a() {}\n")));
    }

    /// #1214: a block still being typed indents by the depth it will have once closed. Rust folds an
    /// unclosed function into an `ERROR`, so the tree is parsed again with its closers appended.
    #[test]
    fn unclosed_blocks_indent_as_if_closed() {
        let src = b"fn f() {\n    x;\n    if a {";
        let tree = parse_rust(src);
        assert!(tree.root_node().has_error());
        assert_eq!(unclosed_blocks(&tree), vec!["}", "}"]);
        let closed = closed_tree(&tree, src);
        assert!(!closed.root_node().has_error());
        assert!(!broken_at(&closed, src, src.len()));
        let starts = line_starts(src);
        assert_eq!(suggest_open_below(&closed, src, starts[2], src.len(), 3), 2);
        assert_eq!(
            suggest_open_below(&closed, src, starts[1], starts[2] - 1, 2),
            1
        );
        // `=` over the unclosed function.
        let src = b"fn f() {\nif a {\nx;\n}\ny;";
        let tree = parse_rust(src);
        assert_eq!(
            indent_levels(&closed_tree(&tree, src), src, 0, 4),
            vec![0, 1, 2, 1, 1]
        );
        // A `{` in a string opens nothing.
        let src = b"int f() {\n    s = \"{\";";
        let tree = parse(tree_sitter_c::LANGUAGE.into(), src);
        assert_eq!(unclosed_blocks(&tree), vec!["}"]);
        // A clean tree is used as it is.
        let src = b"fn f() {}\n";
        let tree = parse_rust(src);
        assert!(matches!(closed_tree(&tree, src), Cow::Borrowed(_)));
    }

    /// #1214, #1206: a shell script's `if`/`do`/`case` are closed by `fi`/`done`/`esac`, and those lines,
    /// with `then` and `do` on lines of their own, line up with their statement.
    #[test]
    fn shell_blocks_close_and_line_up() {
        let sh = |src: &[u8]| parse(tree_sitter_bash::LANGUAGE.into(), src);
        let src = b"if a; then\n    for i in b; do\n        x";
        assert_eq!(unclosed_blocks(&sh(src)), vec!["fi", "done"]);
        let src = b"if a; then\nx\nfi\nfor i in a; do\ny\ndone\nwhile b\ndo\nz\ndone\n";
        assert_eq!(
            indent_levels(&sh(src), src, 0, 9),
            vec![0, 1, 0, 0, 1, 0, 0, 0, 1, 0]
        );
    }

    /// Go: a body line sits one level under its `func` (its `statement_list` opens none), and
    /// `case`/`default` line up with their `switch`.
    #[test]
    fn go_bodies_and_cases() {
        let src = b"func f() {\n\tx()\n\ty()\n}\n";
        let tree = parse(tree_sitter_go::LANGUAGE.into(), src);
        let starts = line_starts(src);
        assert_eq!(
            suggest_open_below(&tree, src, starts[1], starts[2] - 1, 2),
            1
        );
        let src = b"func f() {\nswitch a {\ncase 1:\nx()\ndefault:\ny()\n}\n}\n";
        let tree = parse(tree_sitter_go::LANGUAGE.into(), src);
        assert_eq!(
            indent_levels(&tree, src, 0, 7),
            vec![0, 1, 1, 2, 1, 2, 1, 0]
        );
    }

    /// #1206: the keys of nvim's default 'indentkeys' / 'cinkeys' for the node-depth languages.
    #[test]
    fn indent_keys_of_the_node_depth_languages() {
        use KeyReindent::{Column0, Depth};
        assert_eq!(key_reindent("c", '#', b"    #", true), Some(Column0));
        assert_eq!(key_reindent("c", '#', b"    x; #", false), None);
        assert_eq!(key_reindent("javascript", '#', b"#", true), None);
        assert_eq!(key_reindent("c", 'e', b"    else", false), Some(Depth));
        assert_eq!(key_reindent("c", 'e', b"    x = else", false), None);
        assert_eq!(key_reindent("go", ':', b"\tcase 2:", false), Some(Depth));
        assert_eq!(key_reindent("go", ':', b"\tdefault:", false), Some(Depth));
        assert_eq!(key_reindent("go", ':', b"\tx := 1", false), None);
        assert_eq!(key_reindent("bash", ':', b"    case a:", false), None);
        for w in [
            "then", "do", "elif", "fi", "esac", "done", "fin", "fix", ";;",
        ] {
            let line = format!("    {w}");
            let ch = w.chars().last().unwrap();
            assert_eq!(
                key_reindent("bash", ch, line.as_bytes(), false),
                Some(Depth),
                "{w}"
            );
        }
        assert_eq!(key_reindent("bash", 'd', b"    find", false), None);
        assert_eq!(key_reindent("rust", 'e', b"    else", false), None);
        assert_eq!(key_placeholder("bash", b"elif"), b" :; then :");
        assert_eq!(key_placeholder("go", b"else"), b"");
    }

    /// #1214: the window starts at the top-level line above, so a block left open far above does not
    /// nest the lines below it, and the first line's own column (`fin` just dedented) is not read.
    #[test]
    fn indent_tree_windows_a_broken_buffer() {
        let src = b"fn open() {\n    let y = 1;\n\nfn g() {\n    x;\n}\n\nfn h() {\n    z;";
        let tree = parse_rust(src);
        let starts = line_starts(src);
        let le = |r: usize| starts[r + 1] - 1;
        let it = IndentTree::new(&tree, src, 4, starts[4], le(4));
        assert!(!it.broken_at(le(4)));
        assert_eq!(it.open_below(starts[4], le(4), 5), 1);
        let it = IndentTree::new(&tree, src, 8, starts[8], src.len());
        assert_eq!(it.open_below(starts[8], src.len(), 9), 1);
        assert_eq!(it.levels(8, 8), vec![1]);
        let src = b"if a; then\n    x\nfin";
        let tree = parse(tree_sitter_bash::LANGUAGE.into(), src);
        let it = IndentTree::window(&tree, src, 2, starts_of(src, 2), src.len()).unwrap();
        assert_eq!(it.levels(2, 2), vec![1]);
    }

    fn starts_of(src: &[u8], row: usize) -> usize {
        line_starts(src)[row]
    }

    /// #1214 cost of the indent tree on big files: clean, one block left open at the top, and the same
    /// with no top-level line for the window to start at. Run with
    /// `cargo test --release -p ruse-tui --lib indent_tree_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    #[allow(clippy::print_stdout)] // the timings are the output
    fn indent_tree_cost() {
        use std::time::Instant;
        let item = |lang: &str, i: usize, pad: &str| {
            match lang {
            "rust" => format!(
                "{pad}fn f{i}(a: u32) -> u32 {{\n{pad}    let x = a + {i};\n{pad}    if x > 3 {{\n{pad}        return x * 2;\n{pad}    }}\n{pad}    x\n{pad}}}\n\n"
            ),
            _ => format!(
                "{pad}int f{i}(int a) {{\n{pad}    int x = a + {i};\n{pad}    if (x > 3) {{\n{pad}        return x * 2;\n{pad}    }}\n{pad}    return x;\n{pad}}}\n\n"
            ),
        }
        };
        let open = |lang: &str| match lang {
            "rust" => "fn open() {\n    let y = 1;\n",
            _ => "int open() {\n    int y = 1;\n",
        };
        for (lang, name) in [
            (
                tree_sitter::Language::from(tree_sitter_rust::LANGUAGE),
                "rust",
            ),
            (tree_sitter::Language::from(tree_sitter_c::LANGUAGE), "c"),
        ] {
            for lines in [20_000usize, 100_000] {
                // clean; one unclosed block at the top; the same with every later line indented (no
                // top-level line for the window to start at).
                for case in ["clean", "unclosed", "unclosed-indented"] {
                    let pad = if case == "unclosed-indented" {
                        "    "
                    } else {
                        ""
                    };
                    let mut src = String::new();
                    if case != "clean" {
                        src.push_str(open(name));
                    }
                    let mut i = 0;
                    while i * 8 < lines {
                        src.push_str(&item(name, i, pad));
                        i += 1;
                    }
                    let bytes = src.as_bytes();
                    let starts = line_starts(bytes);
                    let n = starts.len() - 1;
                    let tree = parse(lang.clone(), bytes);
                    let mut o_ms = Vec::new();
                    let mut o_lv = Vec::new();
                    for &row in &[1usize, n / 10, n / 2, n - 3] {
                        let ls = starts[row];
                        let le = starts[row + 1] - 1;
                        let t = Instant::now();
                        let it = IndentTree::new(&tree, bytes, row, ls, le);
                        let level = (!it.broken_at(le)).then(|| it.open_below(ls, le, row + 1));
                        o_ms.push((t.elapsed().as_secs_f64() * 1e5).round() / 100.0);
                        o_lv.push(level);
                    }
                    let t = Instant::now();
                    let le50 = starts[n / 2 + 50] - 1;
                    let it = IndentTree::new(&tree, bytes, n / 2, starts[n / 2], le50);
                    let _ = it.levels(n / 2, n / 2 + 49);
                    let eq = t.elapsed().as_secs_f64() * 1e3;
                    println!(
                        "BENCH {name} {n} lines {case} has_error={} o_ms={o_ms:?} levels={o_lv:?} eq50_ms={eq:.2}",
                        tree.root_node().has_error(),
                    );
                }
            }
        }
    }

    #[test]
    fn comment_braces_do_not_indent() {
        // A brace in a line comment is inside a `line_comment` node, not a block.
        let src = b"fn f() {\n// nope {\nx;\n}\n";
        let tree = parse_rust(src);
        assert_eq!(indent_levels(&tree, src, 0, 3), vec![0, 1, 1, 0]);
    }
}
