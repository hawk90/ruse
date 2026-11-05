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

use tree_sitter::Tree;

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
        let opens = nd.kind() != "block" || nd.child(0).is_some_and(|c| !c.is_named());
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
fn is_clause(kind: &str) -> bool {
    matches!(
        kind,
        "else_clause" | "elif_clause" | "except_clause" | "except_group_clause" | "finally_clause"
    )
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

        let mut level = depth(root.descendant_for_byte_range(pos, pos), li);
        if !blank && matches!(bytes[fnb], b')' | b']' | b'}') {
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

    #[test]
    fn comment_braces_do_not_indent() {
        // A brace in a line comment is inside a `line_comment` node, not a block.
        let src = b"fn f() {\n// nope {\nx;\n}\n";
        let tree = parse_rust(src);
        assert_eq!(indent_levels(&tree, src, 0, 3), vec![0, 1, 1, 0]);
    }
}
