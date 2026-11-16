//! The buffer list and reload.

use super::*;

/// `:e!` (reload) replaces the focused buffer with fresh bytes and marks it saved, discarding the
/// unsaved edit.
#[test]
fn reload_focused_replaces_and_marks_saved() {
    let mut w = Workspace::new(b"old\n".to_vec());
    w.apply(&Command::EnterInsert);
    w.apply(&Command::InsertChar('x'));
    assert!(w.focused().doc.is_modified(), "the edit dirtied the buffer");
    w.reload_focused(b"fresh\ndisk\n".to_vec());
    assert_eq!(w.focused().doc.bytes(), b"fresh\ndisk\n");
    assert!(
        !w.focused().doc.is_modified(),
        "the reloaded buffer is clean"
    );
}

/// #687: a reload continues the buffer's revisions (never `Revision(0)` again under one id), and the
/// other views of the buffer are clamped into the new text.
#[test]
fn reload_continues_the_revision_and_clamps_other_views() {
    let mut w = Workspace::new("a한글 x\n".as_bytes().to_vec());
    w.split(SplitDir::Horizontal);
    w.apply(&Command::Move(1, Motion::LineEnd));
    let before = w.focused().doc.revision();
    w.apply(&Command::EnterInsert);
    w.apply(&Command::InsertChar('y'));
    let edited = w.focused().doc.revision();
    assert!(edited > before);
    w.focus_next();
    w.reload_focused(b"ab".to_vec());
    assert!(w.focused().doc.revision() > edited, "revisions continue");
    for i in 0..2 {
        assert!(w.pane(i).view.cursor().0 <= 2, "pane {i} clamped");
    }
}

/// #686: an edit through one window moves the other window's cursor and marks with their lines, as
/// nvim's `mark_adjust` does. nvim: `:split`, `jj`, `CTRL-W w`, `O999<Esc>`, `CTRL-W w`, `x` leaves
/// `999 11 22 3`.
#[test]
fn an_edit_in_one_window_moves_the_other_windows_cursor_and_marks() {
    let mut w = Workspace::new(b"11\n22\n33\n".to_vec());
    w.split(SplitDir::Horizontal);
    w.apply(&Command::Move(2, Motion::Down));
    w.apply(&Command::SetNamedMark('a'));
    w.focus_next();
    w.apply(&Command::OpenAbove);
    for c in "999".chars() {
        w.apply(&Command::InsertChar(c));
    }
    w.apply(&Command::EnterNormal);
    w.focus_next();
    assert_eq!(
        w.focused().view.named_mark('a'),
        Some(BytePos(10)),
        "the mark followed its line"
    );
    w.apply(&Command::DeleteUnder(1));
    assert_eq!(w.focused().doc.bytes(), b"999\n11\n22\n3\n");
}

/// `:bd` retires a buffer and repoints the window to the alternate; deleting the last buffer opens a
/// fresh scratch (never zero buffers); a non-live id is a no-op.
#[test]
fn remove_buffer_repoints_and_retires() {
    let mut w = Workspace::new(b"first\n".to_vec()); // buffer 1
    let b2 = w.add_buffer(b"second\n".to_vec(), Some("second".to_string()));
    w.focus_buffer(b2); // focused = b2, alternate = buffer 1
    assert_eq!(w.focused_buffer(), b2);

    assert!(w.remove_buffer(b2), "delete the focused buffer");
    assert!(!w.doc_is_live(b2), "b2 is retired");
    assert_eq!(w.focused_buffer().0, 1, "window shows the alternate");
    assert_eq!(w.buffers().len(), 1);

    // Deleting the last buffer opens a fresh scratch — never zero buffers.
    let last = w.focused_buffer();
    assert!(w.remove_buffer(last));
    assert_eq!(w.buffers().len(), 1, "a scratch replaces the last buffer");
    assert_ne!(w.focused_buffer(), last, "focused a new buffer");

    // A non-live id is a no-op.
    assert!(!w.remove_buffer(last));
}

/// F-007 multi-buffer: a fresh workspace lists exactly its one buffer; `add_buffer` appends without
/// changing focus, and `focus_buffer` switches the focused window while preserving each buffer's cursor.
#[test]
fn add_and_switch_buffers_preserves_per_buffer_cursor() {
    let mut w = ws();
    w.set_focused_buffer_name("a.txt");
    let a = w.focused_buffer();
    assert_eq!(w.buffers().len(), 1, "one buffer to start");

    let b = w.add_buffer(b"second buffer\ntext\n".to_vec(), Some("b.txt".into()));
    assert_eq!(w.buffers().len(), 2, "add_buffer appends to the list");
    assert_eq!(w.focused_buffer(), a, "add_buffer does not change focus");

    // Move the cursor in buffer A, then switch to B — B starts at its own cursor (0).
    w.apply(&Command::Move(1, Motion::Down));
    let a_cursor = w.focused().view.cursor().0;
    assert!(a_cursor > 0);
    assert!(w.focus_buffer(b), "switch to a live buffer succeeds");
    assert_eq!(w.focused_buffer(), b);
    assert_eq!(w.focused().view.cursor().0, 0, "B has its own cursor");

    // Back to A — its cursor is preserved (the hidden view survived).
    assert!(w.focus_buffer(a));
    assert_eq!(
        w.focused().view.cursor().0,
        a_cursor,
        "A's cursor is restored on return"
    );
}

/// F-007 multi-buffer: `:ls` flags (current/alt/modified), the `#` alternate, and `:bn`/`:bp` cycling.
#[test]
fn buffer_list_flags_alternate_and_cycling() {
    let mut w = ws();
    w.set_focused_buffer_name("a.txt");
    let a = w.focused_buffer();
    let b = w.add_buffer(b"bbb".to_vec(), Some("b.txt".into()));
    let c = w.add_buffer(b"ccc".to_vec(), Some("c.txt".into()));

    w.focus_buffer(b); // leaving A → A is the alternate
    assert_eq!(w.alternate(), Some(a));
    let info = w.buffers();
    assert!(info.iter().find(|i| i.id == b).unwrap().current);
    assert!(info.iter().find(|i| i.id == a).unwrap().alt);

    // Cycle forward from B: order is [a, b, c] → next after b is c; wraps c → a.
    w.cycle_buffer(true);
    assert_eq!(w.focused_buffer(), c);
    w.cycle_buffer(true);
    assert_eq!(w.focused_buffer(), a, "cycling wraps");
    // Backward from A wraps to c.
    w.cycle_buffer(false);
    assert_eq!(w.focused_buffer(), c);

    // A modified buffer shows the flag; switching to a dead id fails.
    w.focus_buffer(a);
    w.apply(&Command::EnterInsert);
    w.apply(&Command::InsertChar('X'));
    assert!(w.buffers().iter().find(|i| i.id == a).unwrap().modified);
    assert!(
        !w.focus_buffer(DocumentId(999)),
        "unknown buffer id is a no-op"
    );
}
