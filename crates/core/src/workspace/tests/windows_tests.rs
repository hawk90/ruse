//! Window layout, splits and focus.

use super::*;

/// F-007 #1: splitting shows two Views of ONE Document with INDEPENDENT cursors and scroll.
#[test]
fn split_gives_two_views_one_buffer_independent_cursors() {
    let mut w = ws();
    assert_eq!(w.window_count(), 1);
    let a_doc = w.focused().view.doc();

    w.split(SplitDir::Horizontal);
    assert_eq!(w.window_count(), 2, "split adds a window");

    // Both panes name the SAME buffer …
    let did0 = w.pane(0).view.doc();
    let did1 = w.pane(1).view.doc();
    assert_eq!(did0, a_doc);
    assert_eq!(did1, a_doc, "both views share one document");
    // … but the two Views are DISTINCT handles.
    assert_ne!(
        w.windows[0].view, w.windows[1].view,
        "distinct view handles"
    );

    // Move the cursor in the focused (new) pane; the other pane's cursor is untouched.
    w.focus = 0;
    let before = w.pane(1).view.cursor().0;
    w.apply(&Command::Move(1, Motion::Down));
    assert_ne!(w.pane(0).view.cursor().0, before, "focused cursor moved");
    assert_eq!(
        w.pane(1).view.cursor().0,
        before,
        "the other view's cursor is independent"
    );

    // Independent scroll: setting one pane's top does not move the other's.
    w.set_top(0, 2);
    assert_eq!(w.pane(0).view.top(), 2);
    assert_eq!(
        w.pane(1).view.top(),
        0,
        "the other view scrolls independently"
    );
}

/// `:only` keeps the focused window and closes the rest, but leaves every buffer loaded (windows,
/// not buffers) — the others become hidden and stay `:ls`-listed / reopenable via `:b`.
#[test]
fn only_keeps_focused_window_and_leaves_buffers_loaded() {
    let mut w = ws();
    let b = w.add_buffer(b"second\n".to_vec(), Some("second".to_string()));
    w.split(SplitDir::Horizontal); // 2 windows on the original buffer
    w.focus_buffer(b); // focused window now shows the second buffer
    w.split(SplitDir::Vertical); // 3 windows
    assert_eq!(w.window_count(), 3);
    let focused_doc = w.focused().view.doc();

    let closed = w.only();
    assert_eq!(closed, 2, "the two non-focused windows are closed");
    assert_eq!(w.window_count(), 1, "only the focused window remains");
    assert_eq!(
        w.focused().view.doc(),
        focused_doc,
        "focused buffer unchanged"
    );
    assert!(
        w.doc_is_live(b),
        "the second buffer is still loaded (hidden)"
    );
    assert_eq!(w.buffers().len(), 2, "buffer list intact after :only");

    assert_eq!(w.only(), 0, ":only on a sole window is a no-op");
}

/// F-007 #2: view-local state (cursor/selection/mode) lives in the View; an edit through one View
/// is visible in the OTHER View (shared buffer), while that other View keeps its own cursor.
#[test]
fn edit_is_shared_but_view_state_is_local() {
    let mut w = ws();
    w.split(SplitDir::Vertical);
    w.focus = 0;

    // Enter insert at the start and type into pane 0.
    w.apply(&Command::EnterInsert);
    w.apply(&Command::InsertChar('Z'));
    assert_eq!(w.pane(0).view.mode(), crate::editor::Mode::Insert);

    // Pane 1 sees the edited bytes (shared Document) …
    assert!(
        w.pane(1).doc.bytes().starts_with(b"Zhello"),
        "the edit is visible through the other view"
    );
    // … yet pane 1 is still in its own Normal mode (mode is view-local, not a buffer fact).
    assert_eq!(w.pane(1).view.mode(), crate::editor::Mode::Normal);
}

/// F-007 #3: closing one Window keeps the shared buffer while another View holds it.
#[test]
fn closing_a_window_keeps_the_shared_buffer() {
    let mut w = ws();
    let did = w.focused().view.doc();
    w.split(SplitDir::Horizontal); // 2 windows, 1 shared doc
    assert!(w.close_focused(), "closes the focused window");
    assert_eq!(w.window_count(), 1);
    assert!(
        w.doc_is_live(did),
        "the buffer survives — the other view still holds it"
    );
    // The surviving view can still edit the buffer.
    w.apply(&Command::EnterInsert);
    w.apply(&Command::InsertChar('X'));
    assert!(w.focused().doc.bytes().starts_with(b"Xhello"));
}

/// F-007 #3 (retirement): closing the LAST View that names a buffer retires the buffer. Here both
/// windows share one doc, so it is retired only after the second close — which is refused (last
/// window), proving the buffer is never orphaned while a Window holds it.
#[test]
fn last_window_close_is_refused() {
    let mut w = ws();
    w.split(SplitDir::Horizontal);
    assert!(w.close_focused());
    assert!(
        !w.close_focused(),
        "the last window cannot be closed via C-w c"
    );
    assert_eq!(w.window_count(), 1);
}

/// F-007 #4: Buffer, View, and Window are distinct — two Windows, two Views, one Document.
#[test]
fn buffer_view_window_are_distinct() {
    let mut w = ws();
    w.split(SplitDir::Horizontal);
    assert_eq!(w.window_count(), 2, "two windows");
    let v0 = w.windows[0].view;
    let v1 = w.windows[1].view;
    assert_ne!(v0, v1, "two distinct views");
    assert_eq!(
        w.pane(0).view.doc(),
        w.pane(1).view.doc(),
        "one shared document"
    );
}

/// `C-w w` cycles focus and wraps.
#[test]
fn focus_next_wraps() {
    let mut w = ws();
    w.split(SplitDir::Horizontal); // focus now on the new pane (index 1)
    assert_eq!(w.focus(), 1);
    w.focus_next();
    assert_eq!(w.focus(), 0, "wraps to the first window");
    w.focus_next();
    assert_eq!(w.focus(), 1);
}
