//! Indentation and case operators over line spans.

use super::*;

/// `SetIndents` (the tree-aware `=` seam) sets each line's leading whitespace to `level × shiftwidth`,
/// leaves blank lines empty, and is one undo group.
#[test]
fn set_indents_applies_explicit_levels() {
    let mut w = Workspace::new("a\nb\nc\n".as_bytes().to_vec());
    w.apply(&Command::SetIndents {
        first_line: 0,
        last_line: 2,
        levels: vec![0, 1, 2],
    });
    assert_eq!(w.focused().doc.bytes(), "a\n    b\n        c\n".as_bytes());
    w.apply(&Command::Undo);
    assert_eq!(
        w.focused().doc.bytes(),
        "a\nb\nc\n".as_bytes(),
        "one undo group"
    );

    // A blank line stays empty regardless of its level.
    let mut w = Workspace::new("{\n\nx\n".as_bytes().to_vec());
    w.apply(&Command::SetIndents {
        first_line: 0,
        last_line: 2,
        levels: vec![0, 5, 1],
    });
    assert_eq!(w.focused().doc.bytes(), "{\n\n    x\n".as_bytes());
}

/// `SetIndentCols` (#1205) sets indents in columns and keeps a line for `None`; in Insert mode it
/// re-indents the caret's line and the caret stays on its text (#1206).
#[test]
fn set_indent_cols_sets_columns_or_keeps() {
    let mut w = Workspace::new("a\n  b\n c\n".as_bytes().to_vec());
    w.apply(&Command::SetIndentCols {
        first_line: 0,
        last_line: 2,
        cols: vec![Some(3), None, Some(0)],
    });
    assert_eq!(w.focused().doc.bytes(), "   a\n  b\nc\n".as_bytes());
    w.apply(&Command::Undo);
    assert_eq!(w.focused().doc.bytes(), "a\n  b\n c\n".as_bytes());

    // Insert mode: `    else` with the caret after it dedents and the caret follows the text.
    let mut w = Workspace::new("if a:\n    x\n    else\n".as_bytes().to_vec());
    w.apply(&Command::Move(2, Motion::Down));
    w.apply(&Command::AppendLineEnd);
    w.apply(&Command::SetIndentCols {
        first_line: 2,
        last_line: 2,
        cols: vec![Some(0)],
    });
    assert_eq!(w.focused().doc.bytes(), "if a:\n    x\nelse\n".as_bytes());
    assert_eq!(w.focused().view.cursor().0, 16);
    assert_eq!(w.focused().view.mode(), crate::Mode::Insert);
}

/// `=` reindents lines by the C rules (nvim's `get_c_indent`, #1163: an unclosed `(` takes cino-`(`,
/// two shiftwidths; blank lines stay empty), as one undo group. nvim gives the same columns.
#[test]
fn reindent_by_the_c_rules() {
    let src = "fn main() {\nlet x = foo(\n1,\n2,\n);\n}\n";
    let mut w = Workspace::new(src.as_bytes().to_vec());
    // Default config: shiftwidth 4, spaces. `=G` reindents the whole buffer.
    w.apply(&Command::Reindent {
        count: 1,
        motion: Motion::LastLine,
    });
    let expected =
        "fn main() {\n    let x = foo(\n            1,\n            2,\n            );\n}\n";
    assert_eq!(w.focused().doc.bytes(), expected.as_bytes());

    // One undo restores the original in a single step.
    w.apply(&Command::Undo);
    assert_eq!(w.focused().doc.bytes(), src.as_bytes());

    // Shiftwidth drives the unit; a blank line stays empty.
    let mut w = Workspace::new("{\nx\n\n}\n".as_bytes().to_vec());
    w.set_option(crate::editor::EditorOption::ShiftWidth(2));
    w.apply(&Command::Reindent {
        count: 1,
        motion: Motion::LastLine,
    });
    assert_eq!(w.focused().doc.bytes(), "{\n  x\n\n}\n".as_bytes());
}

/// `>`/`<` {motion} shift the motion's LINES one indent level (linewise), regardless of the motion's
/// own wise-ness.
#[test]
fn shift_motion_indents_the_lines() {
    // `>j` from line 0 indents lines 0 and 1 by one level (default 4 spaces); line 2 is untouched.
    let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
    w.apply(&Command::ShiftMotion {
        left: false,
        count: 1,
        motion: Motion::Down,
    });
    assert_eq!(w.focused().doc.bytes(), b"    a\n    b\nc\n");

    // `<j` removes one level from those two lines.
    w.apply(&Command::ShiftMotion {
        left: true,
        count: 1,
        motion: Motion::Down,
    });
    assert_eq!(w.focused().doc.bytes(), b"a\nb\nc\n");
}

/// `gu`/`gU`/`g~` {motion} recase the operator span (lower / upper / toggle) as one edit, leaving
/// the cursor at the span start.
#[test]
fn case_motion_recases_the_span() {
    use crate::command::WordCase;
    // gUw → uppercase the word span "hello " (the space is unaffected).
    let mut w = Workspace::new(b"hello world\n".to_vec());
    w.apply(&Command::CaseMotion {
        count: 1,
        motion: Motion::WordFwd,
        case: WordCase::Upcase,
    });
    assert_eq!(w.focused().doc.bytes(), b"HELLO world\n");

    // g~$ → toggle to end of line.
    let mut w = Workspace::new(b"Hello\n".to_vec());
    w.apply(&Command::CaseMotion {
        count: 1,
        motion: Motion::LineEnd,
        case: WordCase::Toggle,
    });
    assert_eq!(w.focused().doc.bytes(), b"hELLO\n");

    // guu (linewise) → lowercase the whole line.
    let mut w = Workspace::new(b"MixedCase Line\n".to_vec());
    w.apply(&Command::CaseMotion {
        count: 1,
        motion: Motion::Line,
        case: WordCase::Downcase,
    });
    assert_eq!(w.focused().doc.bytes(), b"mixedcase line\n");
}
