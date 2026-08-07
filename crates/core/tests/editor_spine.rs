//! Headless tests for the editor spine (RFC-0012 Phase C1): the plan/commit command pipeline and the
//! command-level trace with its determinism contract. No terminal — the crossterm TUI is C2.

use ruse_core::{apply_command, Command, EditorState, Effect, Mode, Trace, TraceError};

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
    apply_command(&mut st, &Command::DeleteUnder); // delete 'c'
    assert_eq!(st.as_str(), Some("ab"));
    apply_command(&mut st, &Command::MoveLineStart);
    assert_eq!(st.cursor(), 0);
    apply_command(&mut st, &Command::DeleteUnder); // delete 'a'
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
