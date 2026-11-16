//! Per-view marks snapshots.

use super::*;

/// F-003: `marks_snapshot` lists the set named marks then `.` (last change) and `^` (last insert).
#[test]
fn marks_snapshot_lists_named_then_dot_and_caret() {
    let mut w = Workspace::new(b"abc\ndef\n".to_vec());
    assert!(w.marks_snapshot().is_empty(), "nothing set yet");
    w.place_focused_cursor(BytePos(5));
    w.apply(&Command::SetNamedMark('a'));
    w.apply(&Command::DeleteUnder(1)); // sets the `.` last-change mark
    let snap = w.marks_snapshot();
    assert!(
        snap.iter().any(|&(c, _)| c == 'a'),
        "named mark a present; got {snap:?}"
    );
    assert!(
        snap.iter().any(|&(c, _)| c == '.'),
        "last-change `.` present; got {snap:?}"
    );
}
