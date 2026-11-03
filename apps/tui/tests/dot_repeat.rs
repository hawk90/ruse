//! Dot-repeat cases the Neovim oracle corpus cannot observe (#795), driven through the REAL input engine
//! and core exactly like `parity_compare.rs`. The oracle's `nvim_buf_set_lines` is not an undo boundary,
//! so a fixture whose keys undo (`u`) records an artifact; these pins assert what `.` REPLAYS instead.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{apply_command, Command, EditorState};
use ruse_tui::input::{Feed, InputEngine};

/// `<Esc>`, `<C-x>` and literal characters — the only tokens these cases use.
fn tokenize(keys: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut rest = keys;
    while let Some(c) = rest.chars().next() {
        if let Some(tail) = rest.strip_prefix("<Esc>") {
            out.push(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            rest = tail;
        } else if rest.starts_with("<C-") && rest[3..].chars().nth(1) == Some('>') {
            let ch = rest[3..].chars().next().expect("guarded");
            out.push(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL));
            rest = &rest[3 + ch.len_utf8() + 1..];
        } else {
            out.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// Drive `keys` over `text`; return the command list the LAST `.` replayed.
fn last_replay(text: &str, keys: &str) -> Vec<Command> {
    let mut st = EditorState::new(text.as_bytes().to_vec());
    let mut engine = InputEngine::new();
    let mut replayed = Vec::new();
    for key in tokenize(keys) {
        match engine.feed(key, st.mode()) {
            Feed::Cmd(cmd) => {
                apply_command(&mut st, &cmd);
            }
            Feed::Replay(cmds) => {
                for cmd in &cmds {
                    apply_command(&mut st, cmd);
                }
                replayed = cmds;
            }
            Feed::Repeat(r) => {
                let cmds = r.expand();
                for cmd in &cmds {
                    apply_command(&mut st, cmd);
                }
                replayed = cmds;
            }
            _ => {}
        }
    }
    replayed
}

/// #795: `u` run through `i_CTRL-O` is a Normal command — it ends the insert change and is NOT replayed
/// by `.`; the text typed after the return is a fresh `i` change. nvim v0.12.6 (undo history starting at
/// the loaded `x\ny`): `iab<C-o>ucd<Esc>j.` gives `cdx\ncdy` — `.` inserts exactly `cd`.
#[test]
fn ctrl_o_undo_is_not_part_of_the_repeated_insert() {
    assert_eq!(
        last_replay("x\ny", "iab<C-o>ucd<Esc>j."),
        vec![
            Command::EnterInsert,
            Command::InsertChar('c'),
            Command::InsertChar('d'),
            Command::EnterNormal,
        ]
    );
}

/// #795: with nothing typed after the return, `.` keeps the change the one-shot ENDED (`iab`); the
/// `<Esc>` straight after the one-shot does not open an empty change.
#[test]
fn ctrl_o_with_no_text_after_keeps_the_ended_change() {
    assert_eq!(
        last_replay("x\ny", "iab<C-o>0<Esc>j."),
        vec![
            Command::EnterInsert,
            Command::InsertChar('a'),
            Command::InsertChar('b'),
            Command::EnterNormal,
        ]
    );
}
