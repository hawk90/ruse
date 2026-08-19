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

        let mut level = 0usize;
        let mut last_row: Option<usize> = None;
        let mut node = root.descendant_for_byte_range(pos, pos);
        while let Some(nd) = node {
            let sr = nd.start_position().row;
            let er = nd.end_position().row;
            // A multi-line ancestor that starts before this line contributes an indent; nested nodes that
            // start on the SAME row count once (de-dup by start-row, non-increasing as we walk up).
            if er > sr && sr < li && Some(sr) != last_row {
                level += 1;
                last_row = Some(sr);
            }
            node = nd.parent();
        }
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
    let root = tree.root_node();
    let mut level = 0usize;
    let mut last_row: Option<usize> = None;
    let mut node = root.descendant_for_byte_range(at, at);
    while let Some(nd) = node {
        let sr = nd.start_position().row;
        let er = nd.end_position().row;
        if er > sr && sr < new_row && Some(sr) != last_row {
            level += 1;
            last_row = Some(sr);
        }
        node = nd.parent();
    }
    level
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

    #[test]
    fn comment_braces_do_not_indent() {
        // A brace in a line comment is inside a `line_comment` node, not a block.
        let src = b"fn f() {\n// nope {\nx;\n}\n";
        let tree = parse_rust(src);
        assert_eq!(indent_levels(&tree, src, 0, 3), vec![0, 1, 1, 0]);
    }
}
