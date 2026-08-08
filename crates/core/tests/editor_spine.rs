//! Headless tests for the editor spine (RFC-0012 Phase C1): the plan/commit command pipeline and the
//! command-level trace with its determinism contract. No terminal — the crossterm TUI is C2.

use ruse_core::{apply_command, Command, EditorState, Effect, Mode, Motion, Trace, TraceError};

/// The command sequence for "type ` world` at end of line, then leave insert" — reused across tests.
fn insert_world() -> Vec<Command> {
    let mut cmds = vec![Command::MoveLineEnd, Command::EnterInsert];
    for c in " world".chars() {
        cmds.push(Command::InsertChar(c));
    }
    cmds.push(Command::EnterNormal);
    cmds
}

fn drive(initial: &str, cmds: &[Command]) -> EditorState {
    let mut st = EditorState::new(initial.as_bytes().to_vec());
    for c in cmds {
        apply_command(&mut st, c);
    }
    st
}

#[test]
fn modal_insert_session_edits_the_document() {
    let st = drive("hello", &insert_world());
    assert_eq!(st.as_str(), Some("hello world"));
    assert_eq!(st.mode(), Mode::Normal);
    assert_eq!(
        st.cursor(),
        10,
        "leaving insert nudges the cursor left onto 'd'"
    );
    assert!(st.is_modified());
}

#[test]
fn an_insert_session_is_one_undo_step() {
    // Six InsertChars typed consecutively coalesce into ONE undo group (F-005 grouping via the spine).
    let mut st = drive("hello", &insert_world());
    apply_command(&mut st, &Command::Undo);
    assert_eq!(
        st.as_str(),
        Some("hello"),
        "one undo reverts the whole insert session"
    );
    apply_command(&mut st, &Command::Redo);
    assert_eq!(
        st.as_str(),
        Some("hello world"),
        "redo restores it in one step"
    );
}

#[test]
fn motions_and_deletes() {
    let mut st = EditorState::new(b"abc".to_vec());
    apply_command(&mut st, &Command::MoveRight); // cursor 1
    apply_command(&mut st, &Command::MoveRight); // cursor 2
    apply_command(&mut st, &Command::DeleteUnder(1)); // delete 'c'
    assert_eq!(st.as_str(), Some("ab"));
    apply_command(&mut st, &Command::MoveLineStart);
    assert_eq!(st.cursor(), 0);
    apply_command(&mut st, &Command::DeleteUnder(1)); // delete 'a'
    assert_eq!(st.as_str(), Some("b"));
}

#[test]
fn multibyte_cursor_moves_by_char_not_byte() {
    // "café" — é is 2 bytes; MoveRight must land on char boundaries, never mid-codepoint.
    let mut st = EditorState::new("café".as_bytes().to_vec());
    for _ in 0..4 {
        apply_command(&mut st, &Command::MoveRight);
    }
    assert_eq!(st.cursor(), 5, "4 chars = 5 bytes (é is 2)");
    apply_command(&mut st, &Command::MoveLeft);
    assert_eq!(
        st.cursor(),
        3,
        "back over é lands on its boundary, not byte 4"
    );
}

#[test]
fn save_and_quit_emit_effects_but_no_io() {
    let mut st = EditorState::new(b"x".to_vec());
    assert_eq!(apply_command(&mut st, &Command::Save), vec![Effect::Save]);
    assert_eq!(apply_command(&mut st, &Command::Quit), vec![Effect::Quit]);
}

// --- trace: the v0 pillar ---

#[test]
fn trace_replay_is_deterministic() {
    let cmds = insert_world();
    let trace = Trace::record(b"hello", cmds);

    let a = trace
        .replay(b"hello")
        .unwrap_or_else(|e| panic!("replay: {e:?}"));
    let b = trace
        .replay(b"hello")
        .unwrap_or_else(|e| panic!("replay: {e:?}"));
    // Same initial document + same trace ⇒ identical final state (the determinism contract).
    assert_eq!(a.as_str(), b.as_str());
    assert_eq!(a.cursor(), b.cursor());
    assert_eq!(a.mode(), b.mode());
    // …and it equals driving the commands directly.
    let direct = drive("hello", &insert_world());
    assert_eq!(a.as_str(), direct.as_str());
    assert_eq!(a.cursor(), direct.cursor());
}

#[test]
fn trace_text_round_trips() {
    let trace = Trace::record(b"hello", insert_world());
    let text = trace.to_text();
    assert!(text.starts_with("# ruse-trace v1 doc_hash="));
    let parsed = Trace::from_text(&text).unwrap();
    assert_eq!(parsed, trace);
    // Replaying the parsed trace reproduces the document.
    assert_eq!(
        parsed
            .replay(b"hello")
            .unwrap_or_else(|e| panic!("{e:?}"))
            .as_str(),
        Some("hello world")
    );
}

#[test]
fn trace_refuses_a_mismatched_initial_document() {
    let trace = Trace::record(b"hello", insert_world());
    let err = trace
        .replay(b"HELLO")
        .err()
        .expect("replay on a different doc must error");
    assert!(
        matches!(err, TraceError::HashMismatch { .. }),
        "a trace only replays on its recorded document"
    );
}

// --- editing grammar (Phase D): count + operator + motion ---

#[test]
fn word_motions() {
    let mut st = EditorState::new(b"foo bar baz".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::WordFwd));
    assert_eq!(st.cursor(), 4, "start of bar");
    apply_command(&mut st, &Command::Move(1, Motion::WordFwd));
    assert_eq!(st.cursor(), 8, "start of baz");
    apply_command(&mut st, &Command::Move(1, Motion::WordBack));
    assert_eq!(st.cursor(), 4);
}

#[test]
fn dw_and_counted_dw() {
    let mut a = EditorState::new(b"foo bar".to_vec());
    apply_command(&mut a, &Command::Delete(1, Motion::WordFwd));
    assert_eq!(
        a.as_str(),
        Some("bar"),
        "dw deletes a word (with its trailing space)"
    );

    let mut b = EditorState::new(b"foo bar baz".to_vec());
    apply_command(&mut b, &Command::Delete(2, Motion::WordFwd));
    assert_eq!(b.as_str(), Some("baz"), "d2w deletes two words");
}

#[test]
fn de_is_inclusive() {
    let mut st = EditorState::new(b"foobar baz".to_vec());
    apply_command(&mut st, &Command::Delete(1, Motion::WordEnd));
    assert_eq!(
        st.as_str(),
        Some(" baz"),
        "de deletes to end-of-word inclusive"
    );
}

#[test]
fn change_deletes_and_enters_insert() {
    let mut st = EditorState::new(b"foo bar".to_vec());
    apply_command(&mut st, &Command::Change(1, Motion::WordFwd));
    assert_eq!(st.mode(), Mode::Insert);
    apply_command(&mut st, &Command::InsertChar('X'));
    assert_eq!(st.as_str(), Some("Xbar"));
}

#[test]
fn dd_deletes_a_line() {
    let mut st = EditorState::new(b"aaa\nbbb\nccc".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::Down)); // onto bbb
    apply_command(&mut st, &Command::Delete(1, Motion::Line));
    assert_eq!(st.as_str(), Some("aaa\nccc"));
}

#[test]
fn cc_changes_line_content_keeps_the_line() {
    let mut st = EditorState::new(b"aaa\nbbb\nccc".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::Down)); // onto bbb
    apply_command(&mut st, &Command::Change(1, Motion::Line));
    assert_eq!(st.as_str(), Some("aaa\n\nccc"));
    assert_eq!(st.mode(), Mode::Insert);
    apply_command(&mut st, &Command::InsertChar('Z'));
    assert_eq!(st.as_str(), Some("aaa\nZ\nccc"));
}

#[test]
fn operator_is_one_undo_and_traces() {
    let trace = Trace::record(b"foo bar", vec![Command::Delete(1, Motion::WordFwd)]);
    let st = trace.replay(b"foo bar").unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(st.as_str(), Some("bar"));
    // one undo reverts the whole operator
    let mut d = EditorState::new(b"foo bar".to_vec());
    apply_command(&mut d, &Command::Delete(1, Motion::WordFwd));
    apply_command(&mut d, &Command::Undo);
    assert_eq!(d.as_str(), Some("foo bar"));
    // the new command variants round-trip through the trace text format
    assert_eq!(Trace::from_text(&trace.to_text()).unwrap(), trace);
}

// --- text objects (Phase D-2): iw / aw ---

#[test]
fn diw_removes_just_the_word() {
    let mut st = EditorState::new(b"foo bar baz".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::WordFwd)); // start of bar
    apply_command(&mut st, &Command::Move(1, Motion::Right)); // inside bar
    apply_command(&mut st, &Command::Delete(1, Motion::InnerWord));
    assert_eq!(
        st.as_str(),
        Some("foo  baz"),
        "diw leaves both surrounding spaces"
    );
}

#[test]
fn daw_removes_word_and_trailing_space() {
    let mut st = EditorState::new(b"foo bar baz".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::WordFwd)); // at bar
    apply_command(&mut st, &Command::Delete(1, Motion::AWord));
    assert_eq!(st.as_str(), Some("foo baz"));
}

#[test]
fn ciw_changes_inner_word() {
    let mut st = EditorState::new(b"foo bar".to_vec());
    apply_command(&mut st, &Command::Move(1, Motion::WordFwd)); // at bar
    apply_command(&mut st, &Command::Change(1, Motion::InnerWord));
    assert_eq!(st.mode(), Mode::Insert);
    apply_command(&mut st, &Command::InsertChar('X'));
    assert_eq!(st.as_str(), Some("foo X"));
}

// --- search (Phase D-3) ---

#[test]
fn search_next_prev_with_wrap() {
    let mut st = EditorState::new(b"foo bar foo baz".to_vec());
    apply_command(&mut st, &Command::SearchNext("foo".into()));
    assert_eq!(st.cursor(), 8, "next foo after the cursor");
    apply_command(&mut st, &Command::SearchNext("foo".into()));
    assert_eq!(st.cursor(), 0, "wraps to the first");
    apply_command(&mut st, &Command::SearchPrev("foo".into()));
    assert_eq!(st.cursor(), 8, "prev wraps to the last");
}

#[test]
fn search_is_replayable() {
    let trace = Trace::record(b"a foo b foo", vec![Command::SearchNext("foo".into())]);
    let st = trace
        .replay(b"a foo b foo")
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(st.cursor(), 2, "moved to the first foo");
    assert_eq!(
        Trace::from_text(&trace.to_text()).unwrap(),
        trace,
        "pattern survives the text format"
    );
}
