//! The Vim engine's count / operator / one-shot grammar, for the cases the parity corpus CANNOT hold.
//!
//! The corpus oracle feeds keys with `feedkeys(..., 'x')`, and Neovim flushes the remaining typeahead on the
//! beep an aborted operator raises — so "what the NEXT typed key does after an abort" is invisible to it.
//! Every expectation below was captured from Neovim v0.12.6 with keys sent ONE PER RPC INPUT
//! (`nvim --headless --listen`, `--remote-send` per key), which is how a user types them (#789-#800).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{apply_command, EditorState, Mode};
use ruse_tui::input::{Feed, InputEngine};

/// One key, or the frontend's `q/` (it opens the command-line window; the engine never sees `q`).
enum Step {
    Key(KeyEvent),
    CmdwinSearch,
}

fn steps(keys: &str) -> Vec<Step> {
    let ev = |c: KeyCode, m: KeyModifiers| Step::Key(KeyEvent::new(c, m));
    let mut out = Vec::new();
    let mut rest = keys;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            if let Some(end) = rest.find('>') {
                let name = &rest[1..end];
                let step = match name {
                    "Esc" => Some(ev(KeyCode::Esc, KeyModifiers::NONE)),
                    "CR" => Some(ev(KeyCode::Enter, KeyModifiers::NONE)),
                    "q/" => Some(Step::CmdwinSearch),
                    _ => name.strip_prefix("C-").and_then(|r| {
                        let ch = r.chars().next()?;
                        Some(ev(KeyCode::Char(ch), KeyModifiers::CONTROL))
                    }),
                };
                if let Some(step) = step {
                    out.push(step);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        out.push(ev(KeyCode::Char(c), KeyModifiers::NONE));
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Drive `keys` through the engine into a fresh buffer; return (lines, 1-based row, 0-based col, mode).
fn run(lines: &[&str], keys: &str) -> (Vec<String>, usize, usize, Mode) {
    let mut st = EditorState::new(lines.join("\n").into_bytes());
    let mut engine = InputEngine::new();
    for step in steps(keys) {
        let key = match step {
            Step::Key(key) => key,
            Step::CmdwinSearch => {
                engine.open_cmdwin('/');
                continue;
            }
        };
        let cmds = match engine.feed(key, st.mode()) {
            Feed::Cmd(cmd) => vec![cmd],
            Feed::Replay(cmds) => cmds,
            Feed::Repeat(r) => r.expand(),
            _ => Vec::new(),
        };
        for cmd in &cmds {
            apply_command(&mut st, cmd);
        }
    }
    let text = st.as_str().expect("utf-8").to_string();
    let before = &text[..st.cursor().0.min(text.len())];
    let row = before.matches('\n').count() + 1;
    let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1);
    (
        text.split('\n').map(str::to_string).collect(),
        row,
        col,
        st.mode(),
    )
}

#[track_caller]
fn check(lines: &[&str], keys: &str, want: &[&str], at: (usize, usize), insert: bool) {
    let (text, row, col, mode) = run(lines, keys);
    assert_eq!(text, want, "text after {keys:?}");
    assert_eq!((row, col), at, "cursor after {keys:?}");
    assert_eq!(
        mode == Mode::Insert,
        insert,
        "mode after {keys:?}: {mode:?}"
    );
}

const LINE: &[&str] = &["foo bar baz qux"];

/// #790: a key that cannot continue the armed operator aborts it and is CONSUMED; the next key is fresh.
#[test]
fn an_aborting_key_is_consumed_and_the_next_key_runs_fresh() {
    let same = &["foo bar baz qux"];
    let cut = &["oo bar baz qux"];
    check(LINE, "ydw", same, (1, 4), false); // `d` aborts `y`, `w` moves
    check(LINE, "cdw", same, (1, 4), false);
    check(LINE, "gUdw", same, (1, 4), false);
    check(LINE, "dgUw", same, (1, 4), false); // the `g` tier aborts too
    check(LINE, "drx", cut, (1, 0), false); // `r` aborts, `x` cuts
    check(LINE, "dgpx", cut, (1, 0), false);
    check(LINE, "d]px", cut, (1, 0), false);
    check(LINE, "dux", cut, (1, 0), false);
    check(LINE, "d!x", cut, (1, 0), false);
    check(LINE, "!dx", cut, (1, 0), false);
    check(LINE, "d2<Esc>x", cut, (1, 0), false);
    check(LINE, "dmaw", &["fwoo bar baz qux"], (1, 2), true); // `m` aborts, `a` appends
    check(LINE, "d\"adw", &["fdwoo bar baz qux"], (1, 3), true); // `"` aborts under an operator
}

/// #796: `"x` is a prefix — it keeps the count and an `i_CTRL-O` one-shot alive for the next command.
#[test]
fn a_register_prefix_keeps_the_one_shot_and_the_count() {
    let lines = &["one two three", "line2 a", "line3 b"];
    check(
        lines,
        "j\"ayyki<C-o>\"ap<Esc>",
        &["one two three", "line2 a", "line2 a", "line3 b"],
        (2, 0),
        false,
    );
    check(
        &["l1", "l2", "l3", "l4", "l5"],
        "2\"ad\"ad",
        &["ld1", "l2", "l3", "l4", "l5"],
        (1, 2),
        true,
    );
    // An aborted prefix is dropped: `"b<Esc>p` pastes the unnamed register, not the empty `b`.
    check(
        &["abcdef"],
        "yl\"ayl$\"b<Esc>p",
        &["abcdefa"],
        (1, 6),
        false,
    );
}

/// #798: `d/<Esc>` abandons its operator; a later `q/` search must not fold into it and delete.
#[test]
fn an_abandoned_search_line_drops_its_operator() {
    check(LINE, "/baz<CR>ggd/<Esc><q/>k<CR>", LINE, (1, 8), false);
    check(LINE, "/baz<CR>gg2d/<Esc>/baz<CR>", LINE, (1, 8), false);
    check(
        LINE,
        "/baz<CR>0d/<Esc>x",
        &["oo bar baz qux"],
        (1, 0),
        false,
    );
}
