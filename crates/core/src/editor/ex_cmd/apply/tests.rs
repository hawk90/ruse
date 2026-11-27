use super::super::{ExCmd, ExOutcome};
use crate::command::{Command, SelectKind};
use crate::editor::{apply_command, EditorState, GlobalCmd, SubAnswer, SubFlags};
use crate::pos::BytePos;

#[test]
fn a_resolved_substitute_runs_on_its_lines_with_its_answers() {
    let mut st = EditorState::new(b"a a\na a\n".to_vec());
    let all = SubFlags {
        global: true,
        ignore_case: None,
    };
    st.apply_ex(&ExCmd::Substitute {
        first: 2,
        last: 2,
        pattern: "a".into(),
        replacement: "b".into(),
        flags: all,
        answers: None,
    });
    assert_eq!(st.bytes(), b"a a\nb b\n");
    // Confirm answers: only the "yes" matches change; matches never prompted for are "no".
    st.apply_ex(&ExCmd::Substitute {
        first: 1,
        last: 2,
        pattern: "[ab]".into(),
        replacement: "c".into(),
        flags: all,
        answers: Some(vec![SubAnswer::No, SubAnswer::Yes, SubAnswer::Yes]),
    });
    assert_eq!(st.bytes(), b"a c\nc b\n");
}

#[test]
fn a_hand_edited_global_mark_set_is_clamped_and_ordered() {
    let mut st = EditorState::new(b"a\nb\nc\n".to_vec());
    let out = st.apply_ex(&ExCmd::Global {
        marks: vec![3, 0, 1, 99, 1],
        cmd: GlobalCmd::Delete,
    });
    assert_eq!(out, ExOutcome::Lines(2));
    assert_eq!(st.bytes(), b"b\n");
}

/// #1241: `:mark` puts a mark on the line's first non-blank and sets the numbered, context, change
/// and Visual marks too; a trace from before (`ex_mark {line} {name}`) still sets a letter at the line
/// start, as it did live.
#[test]
fn ex_mark_sets_every_mark_and_old_records_replay_at_the_line_start() {
    let mark = |line, name, nonblank| ExCmd::Mark {
        line,
        name,
        nonblank,
    };
    assert_eq!(
        Command::from_line("ex_mark 2 97").unwrap(),
        Command::Ex(mark(2, 'a', false))
    );
    assert_eq!(
        Command::from_line("ex_mark 2 60 ^").unwrap(),
        Command::Ex(mark(2, '<', true))
    );
    let mut st = EditorState::new(b"a1\n  b2\n  a3\n   \nb5\n".to_vec());
    st.apply_ex(&mark(2, 'a', false));
    assert_eq!(st.view.named_mark('a'), Some(BytePos(3)), "the old record");
    st.apply_ex(&mark(2, 'a', true));
    assert_eq!(
        st.view.named_mark('a'),
        Some(BytePos(5)),
        "the first non-blank"
    );
    st.apply_ex(&mark(4, 'b', true));
    assert_eq!(
        st.view.named_mark('b'),
        Some(BytePos(15)),
        "a blank line's last char"
    );
    st.apply_ex(&mark(3, '1', true));
    assert_eq!(st.view.named_mark('1'), Some(BytePos(10)));
    st.apply_ex(&mark(3, '\'', true));
    assert_eq!(st.view.context_mark(), Some(BytePos(10)));
    st.apply_ex(&mark(2, '[', true));
    assert_eq!(st.view.change_mark_start(), Some(BytePos(5)));
    assert_eq!(st.view.change_mark_end(), None, "only `[` is set");
    // `<` with no Visual area is both ends, and `gv` has nothing to reselect until `>` is set too.
    st.apply_ex(&mark(3, '<', true));
    assert_eq!(
        st.view.last_visual_bounds(),
        Some((BytePos(10), BytePos(10), SelectKind::Charwise))
    );
    apply_command(&mut st, &Command::ReselectVisual);
    assert_eq!(st.mode(), crate::Mode::Normal);
    st.apply_ex(&mark(2, '>', true));
    assert_eq!(
        st.view.last_visual_bounds(),
        Some((BytePos(5), BytePos(10), SelectKind::Charwise))
    );
    apply_command(&mut st, &Command::ReselectVisual);
    assert!(st.mode().selection().is_some(), "both ends are set");
    assert_eq!(st.cursor(), BytePos(5), "the cursor on the `>` end");
}

#[test]
fn read_and_filter_replay_their_recorded_bytes() {
    let mut st = EditorState::new(b"a\nb\n".to_vec());
    st.apply_ex(&ExCmd::ReadText {
        dest: 1,
        cursor_on_last: true,
        bytes: b"x\ny\n".to_vec(),
    });
    assert_eq!(st.bytes(), b"a\nx\ny\nb\n");
    st.apply_ex(&ExCmd::FilterReplace {
        first: 1,
        last: 4,
        bytes: b"z\n".to_vec(),
    });
    assert_eq!(st.bytes(), b"z\n");
}
