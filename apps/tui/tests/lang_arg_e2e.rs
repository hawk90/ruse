//! INTEGRATION test for F-027 Lang-Arg (D-048): drive the REAL input engine and the REAL core together
//! the way `main.rs::run` composes them — feed a keystroke with the current document mode, then apply
//! whatever the engine emits to the core — so the test observes the full vertical the inline unit tests
//! cannot: keystroke -> (Lang-Arg translate) -> Feed -> Command -> EDIT on a real document.
//!
//! Adversarial: the cases here hammer the "and to nothing else" boundary (acceptance #2) at the level
//! that actually matters — a translated INSERT changes the document bytes and dot-repeat replays the
//! TRANSLATED char; an operator/motion in Normal stays immune; a single-char ARGUMENT (`f`) is
//! translated but a register name (`"`) is NOT; an `i_CTRL-O` one-shot Normal key is NOT translated;
//! and the whole `:lmap` typed-command path wires the map. Each is a way the feature could be subtly
//! wrong that a happy-path `feed` unit test would miss.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{apply_command, Command, EditorState, Motion};
use ruse_tui::input::{parse_ex, Ex, Feed, InputEngine};

fn k(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}
fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

/// Feed one key the way `main.rs::run` does — with the CURRENT document mode — and apply the engine's
/// output to the core. Returns the raw `Feed` so a test can also assert on routing. The `ExecuteEx` arm
/// mirrors main.rs for the Lang-Arg verbs (the only ex commands these tests drive).
fn step(engine: &mut InputEngine, st: &mut EditorState, key: KeyEvent) -> Feed {
    let out = engine.feed(key, st.mode());
    match &out {
        Feed::Cmd(cmd) => {
            apply_command(st, cmd);
        }
        Feed::Replay(cmds) => {
            for c in cmds {
                apply_command(st, c);
            }
        }
        Feed::ExecuteEx(text) => match parse_ex(text) {
            Ex::Lmap { lhs, rhs } => engine.set_lang_mapping(lhs, rhs),
            Ex::Lunmap { lhs } => engine.clear_lang_mapping(lhs),
            _ => {}
        },
        Feed::CmdlineInsertUnder { .. } | Feed::Pending | Feed::Ignored => {}
    }
    out
}

/// Turn the language map on via the real user path — there is no public setter, so we enter Insert,
/// toggle with `i_CTRL-^`, and return to Normal. The empty insert session leaves the buffer unchanged.
fn activate_lang(e: &mut InputEngine, st: &mut EditorState) {
    step(e, st, k('i'));
    step(e, st, ctrl('^'));
    step(e, st, esc());
}

fn text(st: &EditorState) -> String {
    String::from_utf8_lossy(st.bytes()).into_owned()
}

#[test]
fn translated_insert_changes_document_and_dot_repeat_replays_the_translation() {
    // The vertical the unit tests skip: a mapped key becomes a translated EDIT, and `.` replays the
    // TRANSLATED char (proof that the dot-repeat recorder captured the post-translation command, not
    // the raw keystroke).
    let mut e = InputEngine::new();
    let mut st = EditorState::new(Vec::new());
    e.set_lang_mapping('a', 'б');
    // i  CTRL-^  a  <Esc>  — insert one translated char as a single recorded change.
    step(&mut e, &mut st, k('i'));
    step(&mut e, &mut st, ctrl('^'));
    step(&mut e, &mut st, k('a'));
    step(&mut e, &mut st, esc());
    assert_eq!(text(&st), "б", "the mapped key must insert its translation");

    // `.` replays the change: another 'б', never the raw 'a'.
    step(&mut e, &mut st, k('.'));
    let t = text(&st);
    assert_eq!(
        t.matches('б').count(),
        2,
        "dot-repeat must replay the translated char"
    );
    assert!(
        !t.contains('a'),
        "the raw key must never reach the document"
    );
}

#[test]
fn normal_mode_operator_and_motion_are_immune() {
    // Acceptance #2 at the document level: even with the map active, `d` is delete and `w` is a motion.
    // Map `d`->`x`; `dw` must still DELETE a word, not type/translate anything.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(b"foo bar".to_vec());
    e.set_lang_mapping('d', 'x');
    activate_lang(&mut e, &mut st);

    let armed = step(&mut e, &mut st, k('d'));
    assert_eq!(
        armed,
        Feed::Pending,
        "`d` must ARM the operator, not translate to a motion"
    );
    step(&mut e, &mut st, k('w'));
    assert_eq!(text(&st), "bar", "`dw` must delete the first word");
}

#[test]
fn single_char_find_argument_is_translated_but_operator_is_not() {
    // Acceptance #2 (positive half) at the document level: `d f q` — the `f` ARGUMENT `q` translates to
    // `X`, so it deletes through the `X`; the `d` and `f` themselves are untouched.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(b"fooXbar".to_vec());
    e.set_lang_mapping('q', 'X');
    activate_lang(&mut e, &mut st);

    step(&mut e, &mut st, k('d'));
    step(&mut e, &mut st, k('f'));
    step(&mut e, &mut st, k('q')); // translated to X -> df X (inclusive)
    assert_eq!(
        text(&st),
        "bar",
        "the find argument must be translated (dfq == dfX)"
    );
}

#[test]
fn register_name_argument_is_not_translated() {
    // Sharp boundary: a register name after `"` is ALSO a single character, but Vim's Lang-Arg does not
    // apply to it — `lang_eligible` lists only find/replace arguments. `"a` must select register `a`,
    // never the mapped `b`.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(b"hello".to_vec());
    e.set_lang_mapping('a', 'b');
    activate_lang(&mut e, &mut st);

    assert_eq!(step(&mut e, &mut st, k('"')), Feed::Pending);
    let out = step(&mut e, &mut st, k('a'));
    assert_eq!(
        out,
        Feed::Cmd(Command::SetRegister(Some('a'))),
        "a register name must be the RAW key, never translated"
    );
}

#[test]
fn ctrl_o_one_shot_normal_key_is_not_translated() {
    // During `i_CTRL-O` the borrowed key is a NORMAL command, not text entry — it must not translate.
    // Map `l`->`j`; under CTRL-O, `l` must stay the right-motion, not become the down-motion.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(b"abc".to_vec());
    e.set_lang_mapping('l', 'j');
    // Enter insert, activate the map, arm a one-shot, then press `l`.
    step(&mut e, &mut st, k('i'));
    step(&mut e, &mut st, ctrl('^'));
    step(&mut e, &mut st, ctrl('o'));
    let out = step(&mut e, &mut st, k('l'));
    assert_eq!(
        out,
        Feed::Cmd(Command::Move(1, Motion::Right)),
        "a one-shot Normal key must not be translated"
    );
}

#[test]
fn command_line_pattern_is_translated() {
    // The Command-line namespace is Lang-Arg-eligible: a `/`-search pattern is translated before it is
    // executed, so the resulting search carries the TRANSLATED text.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(b"zzz".to_vec());
    e.set_lang_mapping('a', 'z');
    activate_lang(&mut e, &mut st);

    assert_eq!(step(&mut e, &mut st, k('/')), Feed::Pending);
    step(&mut e, &mut st, k('a')); // appended to the line as translated 'z'
    let out = step(&mut e, &mut st, enter());
    match out {
        Feed::Cmd(Command::Search { pattern, .. }) => {
            assert_eq!(pattern, "z", "the search pattern must be translated")
        }
        other => panic!("expected a Search command, got {other:?}"),
    }
}

#[test]
fn typed_lmap_command_wires_the_map() {
    // The full real path: the user TYPES `:lmap a b` (with the map inactive, so the command itself is
    // not rewritten), which parses to Ex::Lmap and installs the mapping — then a later activated `a`
    // inserts `b`. Proves parse_ex + set_lang_mapping is the same wiring main.rs runs.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(Vec::new());
    for c in ":lmap a b".chars() {
        step(&mut e, &mut st, k(c));
    }
    step(&mut e, &mut st, enter()); // ExecuteEx("lmap a b") -> set_lang_mapping('a','b')

    step(&mut e, &mut st, k('i'));
    step(&mut e, &mut st, ctrl('^'));
    step(&mut e, &mut st, k('a'));
    step(&mut e, &mut st, esc());
    assert_eq!(
        text(&st),
        "b",
        "a typed `:lmap a b` must make `a` insert `b`"
    );
}

#[test]
fn lunmap_removes_a_mapping() {
    // `:lunmap` (via the typed path) restores the literal key.
    let mut e = InputEngine::new();
    let mut st = EditorState::new(Vec::new());
    for c in ":lmap a b".chars() {
        step(&mut e, &mut st, k(c));
    }
    step(&mut e, &mut st, enter());
    for c in ":lunmap a".chars() {
        step(&mut e, &mut st, k(c));
    }
    step(&mut e, &mut st, enter());

    step(&mut e, &mut st, k('i'));
    step(&mut e, &mut st, ctrl('^'));
    step(&mut e, &mut st, k('a'));
    step(&mut e, &mut st, esc());
    assert_eq!(
        text(&st),
        "a",
        "after :lunmap, `a` must insert the literal `a`"
    );
}
