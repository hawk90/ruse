//! Vim special keys and quit-family parsing (#605, #607, #611, #601), driven through the real input engine
//! and core. Expected values were captured from nvim v0.12.6; the text/cursor/register cases also live in
//! the oracle corpus (`tests/parity/vim/fixtures/corpus.yaml`). What the oracle cannot observe — undo
//! after an Insert-mode cursor key (its own `nvim_buf_set_lines` is undoable), `ZZ`/`ZQ`, Ex parsing, and
//! the Emacs count motions that must keep crossing lines — is pinned here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{apply_command, Command, EditorState, LineAddr, SubRange};
use ruse_tui::input::{parse_ex, Ex, Feed, InputEngine};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// `<Left>`-style tokens plus literal chars, like the parity comparator's tokenizer.
fn keys(s: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            if let Some(end) = rest.find('>') {
                let code = match &rest[1..end] {
                    "Esc" => Some(KeyCode::Esc),
                    "Left" => Some(KeyCode::Left),
                    "Right" => Some(KeyCode::Right),
                    "Down" => Some(KeyCode::Down),
                    "Del" => Some(KeyCode::Delete),
                    "End" => Some(KeyCode::End),
                    _ => None,
                };
                if let Some(code) = code {
                    out.push(key(code));
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        out.push(key(KeyCode::Char(c)));
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn run(engine: &mut InputEngine, text: &str, seq: &str) -> (String, usize) {
    let mut st = EditorState::new(text.as_bytes().to_vec());
    for k in keys(seq) {
        match engine.feed(k, st.mode()) {
            Feed::Cmd(cmd) => {
                apply_command(&mut st, &cmd);
            }
            Feed::Replay(cmds) => {
                for cmd in &cmds {
                    apply_command(&mut st, cmd);
                }
            }
            _ => {}
        }
    }
    (st.as_str().expect("utf-8").to_string(), st.cursor())
}

fn vim(text: &str, seq: &str) -> (String, usize) {
    run(&mut InputEngine::new(), text, seq)
}

/// #607: an Insert-mode cursor key breaks the undo sequence — `u` undoes only what was typed after it.
#[test]
fn insert_cursor_key_breaks_undo() {
    assert_eq!(vim("abc", "lix<Left>y<Esc>u").0, "axbc");
    assert_eq!(vim("abc", "lix<Left>y<Esc>uu").0, "abc");
    assert_eq!(
        vim("abcdef\nab\nabcdef", "4li<Down>x<Esc>u").0,
        "abcdef\nab\nabcdef"
    );
}

/// #607: `i_<Del>` is part of the insert — one undo step with the typed text, replayed by `.`.
#[test]
fn insert_del_stays_in_the_insert() {
    assert_eq!(vim("abc", "lix<Del>y<Esc>u").0, "abc");
    assert_eq!(vim("abc\ndef", "llax<Del><Esc>.").0, "abcxxef");
}

/// #607: after a cursor key `.` repeats only the text typed after it, as a plain `i`.
#[test]
fn insert_cursor_key_restarts_the_dot_record() {
    assert_eq!(vim("abc", "lix<Right>y<Esc>.").0, "axbyyc");
    assert_eq!(vim("abc", "lix<End>y<Esc>0.").0, "yaxbcy");
}

/// #605: `ZZ` is `:x` and `ZQ` is `:q!` — the same Ex quit policy, so window-close vs exit and the
/// E37/E162 refusals live in one place. A shifted `Z` (how some terminals deliver it) works too, and
/// any other second key fails without leaving the engine armed.
#[test]
fn zz_and_zq_run_the_ex_quit_policy() {
    let mut e = InputEngine::new();
    let normal = ruse_core::Mode::Normal;
    assert_eq!(e.feed(key(KeyCode::Char('Z')), normal), Feed::Pending);
    assert_eq!(
        e.feed(key(KeyCode::Char('Z')), normal),
        Feed::ExecuteEx("x".into())
    );
    let shifted = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT);
    assert_eq!(e.feed(shifted('Z'), normal), Feed::Pending);
    assert_eq!(e.feed(shifted('Q'), normal), Feed::ExecuteEx("q!".into()));
    assert_eq!(e.feed(key(KeyCode::Char('Z')), normal), Feed::Pending);
    // `Zx` is no command: nvim beeps, which stops a macro (#1111).
    assert_eq!(
        e.feed(key(KeyCode::Char('x')), normal),
        Feed::Beep {
            clear_register: false
        }
    );
    assert_eq!(
        e.feed(key(KeyCode::Char('x')), normal),
        Feed::Cmd(Command::DeleteUnder(1)),
        "the aborted Z leaves nothing armed"
    );
    assert!(is_exit_write(&parse_ex("x")));
    assert_eq!(parse_ex("q!"), Ex::QuitForce);
}

/// The Emacs profile's counted `C-f`/`C-b` keep crossing lines (Emacs point steps over the newline),
/// unlike Vim's `h`/`l`, which now stop at the line's edge.
#[test]
fn emacs_counted_char_motions_still_cross_lines() {
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let mut e = InputEngine::emacs();
    let mut st = EditorState::new(b"ab\ncd".to_vec());
    for k in [ctrl('u'), key(KeyCode::Char('4')), ctrl('f')] {
        if let Feed::Cmd(cmd) = e.feed(k, st.mode()) {
            apply_command(&mut st, &cmd);
        }
    }
    assert_eq!(st.cursor(), 4, "C-u 4 C-f from 0 steps over the newline");
    for k in [ctrl('u'), key(KeyCode::Char('3')), ctrl('b')] {
        if let Feed::Cmd(cmd) = e.feed(k, st.mode()) {
            apply_command(&mut st, &cmd);
        }
    }
    assert_eq!(st.cursor(), 1);
}

/// Whether `ex` is `:x` (write when modified, then close the window).
fn is_exit_write(ex: &Ex) -> bool {
    matches!(
        ex,
        Ex::Write(ruse_tui::input::WriteSpec {
            quit: true,
            if_modified: true,
            bang: false,
            range: None,
            ..
        })
    )
}

/// #611: the quit and window-close family parses with Vim's abbreviations.
#[test]
fn quit_family_parses() {
    for q in ["q", "qu", "qui", "quit"] {
        assert_eq!(parse_ex(q), Ex::Quit, ":{q}");
    }
    for q in ["x", "xi", "xit", "exi", "exit"] {
        assert!(is_exit_write(&parse_ex(q)), ":{q}");
    }
    assert!(matches!(
        parse_ex("wq"),
        Ex::Write(ruse_tui::input::WriteSpec {
            quit: true,
            if_modified: false,
            ..
        })
    ));
    for q in ["qa", "qal", "qall", "quita", "quital", "quitall"] {
        assert_eq!(parse_ex(q), Ex::QuitAll { force: false }, ":{q}");
        assert_eq!(
            parse_ex(&format!("{q}!")),
            Ex::QuitAll { force: true },
            ":{q}!"
        );
    }
    assert_eq!(parse_ex("cq"), Ex::CQuit(1));
    assert_eq!(parse_ex("cquit!"), Ex::CQuit(1));
    assert_eq!(parse_ex("cq 3"), Ex::CQuit(3));
    assert!(matches!(parse_ex("cq x"), Ex::Unknown(_)));
    assert_eq!(parse_ex("hide"), Ex::Hide);
    assert_eq!(parse_ex("hid"), Ex::Hide);
    assert_eq!(parse_ex("close!"), Ex::Close);
    assert_eq!(parse_ex("clos"), Ex::Close);
    assert_eq!(parse_ex("only!"), Ex::Only);
    assert_eq!(parse_ex("onl"), Ex::Only);
}

/// #601: `:m`/`:t` destinations parse relative to the cursor (`+N`, `-N`, `.±N`, `$±N`, `N±M`); before
/// the fix `+1` parsed as the absolute line 1.
#[test]
fn move_destination_addresses_parse() {
    let cur = SubRange::CurrentLine;
    assert_eq!(parse_ex("m+1"), Ex::Move(cur, LineAddr::CurrentOffset(1)));
    assert_eq!(parse_ex("m+"), Ex::Move(cur, LineAddr::CurrentOffset(1)));
    assert_eq!(parse_ex("m++"), Ex::Move(cur, LineAddr::CurrentOffset(2)));
    assert_eq!(parse_ex("m-"), Ex::Move(cur, LineAddr::CurrentOffset(-1)));
    assert_eq!(
        parse_ex("m .-2"),
        Ex::Move(cur, LineAddr::CurrentOffset(-2))
    );
    assert_eq!(parse_ex("m.+0"), Ex::Move(cur, LineAddr::Current));
    assert_eq!(parse_ex("m$-1"), Ex::Move(cur, LineAddr::LastOffset(-1)));
    assert_eq!(parse_ex("m 3-1"), Ex::Move(cur, LineAddr::Line(2)));
    assert_eq!(parse_ex("t+2"), Ex::Copy(cur, LineAddr::CurrentOffset(2)));
    assert_eq!(
        parse_ex("2,3t0"),
        Ex::Copy(SubRange::Lines(2, 3), LineAddr::Line(0))
    );
    assert!(
        matches!(parse_ex("m 1-2"), Ex::Unknown(_)),
        "a negative line"
    );
    assert!(matches!(parse_ex("m+x"), Ex::Unknown(_)));
}
