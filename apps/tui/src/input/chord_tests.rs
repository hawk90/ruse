//! The modifier table (#598, #599, #1059, #681): every CTRL chord `<C-a>`..`<C-z>` plus the punctuation
//! chords, in Normal, Visual, operator-pending, Insert and the command line. Each row is nvim v0.12.6 ground
//! truth, probed headless (`nvim --headless -u NONE -i NONE`, keys via `nvim_input`): a bound chord does
//! its job, an unbound one does nothing, and no chord ever runs the command of its bare letter.
//!
//! The rows are what the ENGINE does. In Normal the frontend takes `<C-w>`, `<C-d>`/`<C-u>`, `<C-e>`/`<C-y>`,
//! `<C-b>`/`<C-f>`/`<C-l>`/`<C-p>` and `<C-]>` before the engine sees them (pinned in
//! `tests/pty_intercepts.rs`), so here they are inert. Legacy terminals send `<C-\>`, `<C-]>`, `<C-^>`,
//! `<C-_>` as `Ctrl-4`..`Ctrl-7` and `<C-@>` as `Ctrl-Space`; the kitty protocol sends the punctuation.

use crate::input::*;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

const VISUAL: Mode = Mode::Visual {
    kind: SelectKind::Charwise,
};

/// Every chord the table covers: the 26 letters, then `<C-[>`, the legacy and kitty forms of
/// `<C-\> <C-]> <C-^> <C-_>`, and `<C-@>`/`<C-Space>`.
fn chords() -> Vec<char> {
    let mut v: Vec<char> = ('a'..='z').collect();
    v.extend(['[', '4', '5', '6', '7', '\\', ']', '^', '_', ' ', '@']);
    v
}

/// Feed `keys` (in `mode`) to a fresh engine and return the last result.
fn fresh(keys: &[KeyEvent], mode: Mode) -> Feed {
    let mut e = InputEngine::new();
    let mut last = Feed::Ignored;
    for &k in keys {
        last = e.feed(k, mode);
    }
    last
}

/// After a chord nvim ignores, nothing may be left pending: the next `w` is a plain word motion.
fn assert_clean(e: &mut InputEngine, mode: Mode, what: &str) {
    assert_eq!(
        e.feed(key('w'), mode),
        Feed::Cmd(Command::Move(1, Motion::WordFwd)),
        "{what}: no state may leak past an unbound chord"
    );
}

#[test]
fn normal_chord_table() {
    for c in chords() {
        let want = match c {
            'a' => Feed::Cmd(Command::IncrementNumber(1)),
            'x' => Feed::Cmd(Command::IncrementNumber(-1)),
            'g' => Feed::Cmd(Command::FileInfo),
            'h' => fresh(&[key('h')], Mode::Normal),
            'j' | 'n' => fresh(&[key('j')], Mode::Normal),
            'p' => fresh(&[key('k')], Mode::Normal), // frontend-owned (palette); `k` in nvim
            'm' => fresh(&[special(KeyCode::Enter)], Mode::Normal),
            'i' => Feed::Cmd(Command::GotoNewerJump),
            'o' => Feed::Cmd(Command::GotoOlderJump),
            'r' => Feed::Cmd(Command::Redo),
            'v' | 'q' => Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise,
            }),
            // `<C-\>` (legacy `Ctrl-4`) waits for `<C-n>`/`<C-g>`.
            '\\' | '4' => Feed::Pending,
            _ => Feed::Ignored,
        };
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl(c), Mode::Normal), want, "Normal <C-{c}>");
        if want == Feed::Ignored {
            assert_clean(&mut e, Mode::Normal, &format!("Normal <C-{c}>"));
        }
    }
}

#[test]
fn visual_chord_table() {
    for c in chords() {
        let want = match c {
            'a' | 'x' => Feed::Cmd(Command::IncrementSelection {
                delta: if c == 'a' { 1 } else { -1 },
                sequential: false,
            }),
            'g' => Feed::Cmd(Command::EnterSelect {
                kind: SelectKind::Charwise,
            }),
            'h' => fresh(&[key('h')], VISUAL),
            'j' | 'n' => fresh(&[key('j')], VISUAL),
            'p' => fresh(&[key('k')], VISUAL),
            'm' => fresh(&[special(KeyCode::Enter)], VISUAL),
            'v' | 'q' => Feed::Cmd(Command::EnterVisual {
                kind: SelectKind::Blockwise,
            }),
            'c' | '[' => Feed::Cmd(Command::EnterNormal),
            '\\' | '4' => Feed::Pending,
            // nvim scrolls on `<C-d>`/`<C-e>`/`<C-f>`/`<C-b>`/`<C-u>`/`<C-y>` in Visual; ruse has no Visual
            // scroll yet, so they are inert (they used to delete, yank, undo-case, …).
            _ => Feed::Ignored,
        };
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl(c), VISUAL), want, "Visual <C-{c}>");
        if want == Feed::Ignored {
            assert_clean(&mut e, VISUAL, &format!("Visual <C-{c}>"));
        }
    }
}

#[test]
fn operator_pending_chord_table() {
    let d = key('d');
    for c in chords() {
        let want = match c {
            'h' => fresh(&[d, key('h')], Mode::Normal),
            'j' | 'n' => fresh(&[d, key('j')], Mode::Normal),
            'p' => fresh(&[d, key('k')], Mode::Normal),
            'm' => fresh(&[d, special(KeyCode::Enter)], Mode::Normal),
            'v' | 'q' => Feed::Pending, // `o_CTRL-V`: force the motion blockwise
            '\\' | '4' => Feed::Pending, // `<C-\><C-n>` cancels the operator
            _ => Feed::Ignored,         // aborts the operator, as `d<C-a>` / `d<C-c>` do in nvim
        };
        let mut e = InputEngine::new();
        e.feed(d, Mode::Normal);
        assert_eq!(e.feed(ctrl(c), Mode::Normal), want, "d<C-{c}>");
        if want == Feed::Ignored {
            assert_clean(&mut e, Mode::Normal, &format!("d<C-{c}>"));
        }
    }
    // `<C-\><C-n>` / `<C-\><C-g>` end in Normal with nothing pending; in Visual they leave it.
    for second in ['n', 'g'] {
        for bs in ['\\', '4'] {
            let mut e = InputEngine::new();
            e.feed(d, Mode::Normal);
            e.feed(ctrl(bs), Mode::Normal);
            assert_eq!(e.feed(ctrl(second), Mode::Normal), Feed::Ignored);
            assert_clean(&mut e, Mode::Normal, "d<C-\\><C-n>");
            assert_eq!(
                fresh(&[ctrl(bs), ctrl(second)], VISUAL),
                Feed::Cmd(Command::EnterNormal)
            );
        }
    }
    // `d<C-q>j` is `d<C-v>j`.
    assert_eq!(
        fresh(&[d, ctrl('q'), key('j')], Mode::Normal),
        fresh(&[d, ctrl('v'), key('j')], Mode::Normal)
    );
}

#[test]
fn insert_chord_table() {
    for c in chords() {
        let mut e = InputEngine::new();
        let got = e.feed(ctrl(c), Mode::Insert);
        assert!(
            !matches!(got, Feed::Cmd(Command::InsertChar(_))),
            "Insert <C-{c}> must not type a char: {got:?}"
        );
        let want = match c {
            'c' | '[' => Some(Feed::Cmd(Command::EnterNormal)),
            'h' => Some(Feed::Cmd(Command::DeleteBack)),
            'j' | 'm' => Some(Feed::Cmd(Command::InsertNewline)),
            'i' => Some(Feed::Cmd(Command::InsertTab)),
            'w' => Some(Feed::Cmd(Command::InsertDeleteWordBack)),
            'u' => Some(Feed::Cmd(Command::InsertDeleteToLineStart)),
            't' => Some(Feed::Cmd(Command::InsertIndent)),
            'd' => Some(Feed::Cmd(Command::InsertDedent)),
            // Prefixes: `CTRL-G`, `CTRL-R`, `CTRL-X`, `CTRL-K`, `CTRL-V`/`CTRL-Q`, `CTRL-O`, `CTRL-^`.
            'g' | 'r' | 'x' | 'k' | 'v' | 'q' | 'o' | '6' | '^' => Some(Feed::Pending),
            // `CTRL-A`/`CTRL-@` re-insert the last text (none yet); `CTRL-E`/`CTRL-Y`/`CTRL-N`/`CTRL-P`/
            // `CTRL-L` are resolved by the frontend, which needs the buffer.
            'a' | ' ' | '@' | 'e' | 'y' | 'n' | 'p' | 'l' => None,
            // Unbound: nothing. nvim inserts `<C-b>`, `<C-f>`, `<C-l>`, `<C-z>`, `<C-_>` as raw control
            // characters; ruse inserts nothing (`<C-v>` types one on purpose).
            _ => Some(Feed::Ignored),
        };
        if let Some(want) = want {
            assert_eq!(got, want, "Insert <C-{c}>");
        }
    }
}

#[test]
fn cmdline_chord_table() {
    for c in chords() {
        let mut e = InputEngine::new();
        e.feed(key(':'), Mode::Normal);
        for ch in "ab cd".chars() {
            e.feed(key(ch), Mode::Normal);
        }
        let got = e.feed(ctrl(c), Mode::Normal);
        let line = e.cmdline().map(|(_, text, _)| text.to_string());
        let (want, want_line) = match c {
            'c' | '[' => (Feed::Ignored, None),
            'j' | 'm' => (Feed::ExecuteEx("ab cd".into()), None),
            'h' => (Feed::Pending, Some("ab c")),
            'u' => (Feed::Pending, Some("")),
            'w' => (Feed::Pending, Some("ab ")),
            // `CTRL-R` arms its prefix; `CTRL-N`/`CTRL-P` walk an empty history. Neither edits the line.
            _ => (Feed::Pending, Some("ab cd")),
        };
        assert_eq!(got, want, "cmdline <C-{c}>");
        assert_eq!(line.as_deref(), want_line, "cmdline <C-{c}> line");
    }
}

/// `c_CTRL-W` deletes trailing blanks, then one run of keyword or of non-keyword chars (nvim v0.12.6:
/// `:ab cd<C-w>` = `ab `, `:ab<C-w>` = empty).
#[test]
fn cmdline_ctrl_w_deletes_one_word_class() {
    for (typed, left) in [
        ("ab", ""),
        ("ab cd  ", "ab "),
        ("a.b", "a."),
        ("a..", "a"),
        ("", ""),
    ] {
        let mut e = InputEngine::new();
        e.feed(key(':'), Mode::Normal);
        for ch in typed.chars() {
            e.feed(key(ch), Mode::Normal);
        }
        e.feed(ctrl('w'), Mode::Normal);
        assert_eq!(e.cmdline().map(|(_, t, _)| t), Some(left), ":{typed}<C-w>");
    }
}

/// The Emacs `M-x` minibuffer shares the line editor but keeps Emacs keys: `C-g` quits, `C-c`/`C-u` do
/// not edit it, and no chord appends its letter.
#[test]
fn emacs_minibuffer_quits_on_ctrl_g_and_appends_no_chord() {
    let mut e = InputEngine::emacs();
    e.feed(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
        Mode::Insert,
    );
    e.feed(key('a'), Mode::Insert);
    for c in ['c', 'u', 'w', 'h', 's'] {
        e.feed(ctrl(c), Mode::Insert);
        assert_eq!(e.cmdline().map(|(_, t, _)| t), Some("a"), "M-x a C-{c}");
    }
    assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Ignored);
    assert_eq!(e.cmdline(), None, "C-g quits the minibuffer");
}

/// An awaited key is never a chord's letter: `f<C-a>`, `r<C-a>`, `"<C-a>`, `m<C-a>`, `di<C-b>`, `g<C-e>`
/// all abort in nvim (`r<C-a>` would insert a raw `^A`; ruse aborts) and leave nothing pending.
#[test]
fn awaited_keys_abort_on_a_chord() {
    let prefixes: &[&str] = &[
        "f", "F", "t", "T", "r", "gr", "m", "`", "'", "\"", "di", "da", "[", "]", "g", "dg",
    ];
    for p in prefixes {
        for c in ['a', 'b', 'e', 'k'] {
            let mut e = InputEngine::new();
            for ch in p.chars() {
                e.feed(key(ch), Mode::Normal);
            }
            assert_eq!(e.feed(ctrl(c), Mode::Normal), Feed::Ignored, "{p}<C-{c}>");
            assert_clean(&mut e, Mode::Normal, &format!("{p}<C-{c}>"));
        }
    }
    // The `g` chords that ARE bound still resolve.
    assert_eq!(
        fresh(&[key('g'), ctrl('g')], Mode::Normal),
        Feed::Cmd(Command::CursorInfo)
    );
    assert_eq!(
        fresh(&[key('g'), ctrl('a')], VISUAL),
        Feed::Cmd(Command::IncrementSelection {
            delta: 1,
            sequential: true
        })
    );
}

/// #681: a real terminal sends an uppercase letter as SHIFT + the letter; it must act exactly like the
/// unshifted event the unit tests build.
#[test]
fn shifted_uppercase_letters_match_their_arms() {
    for c in 'A'..='Z' {
        let shifted = KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT);
        for mode in [Mode::Normal, VISUAL, Mode::Insert] {
            assert_eq!(
                fresh(&[shifted], mode),
                fresh(&[key(c)], mode),
                "SHIFT+{c} in {mode:?}"
            );
        }
    }
}

/// `<S-Left>`/`<C-Left>` = `b` and `<S-Right>`/`<C-Right>` = `w` (nvim), also under an operator.
#[test]
fn modified_left_and_right_move_by_word() {
    for m in [KeyModifiers::SHIFT, KeyModifiers::CONTROL] {
        let left = KeyEvent::new(KeyCode::Left, m);
        let right = KeyEvent::new(KeyCode::Right, m);
        assert_eq!(
            fresh(&[left], Mode::Normal),
            fresh(&[key('b')], Mode::Normal)
        );
        assert_eq!(
            fresh(&[right], Mode::Normal),
            fresh(&[key('w')], Mode::Normal)
        );
        assert_eq!(
            fresh(&[key('d'), right], Mode::Normal),
            fresh(&[key('d'), key('w')], Mode::Normal)
        );
        assert_eq!(fresh(&[right], VISUAL), fresh(&[key('w')], VISUAL));
    }
    // The plain arrows are still one char.
    assert_eq!(
        fresh(&[special(KeyCode::Left)], Mode::Normal),
        fresh(&[key('h')], Mode::Normal)
    );
}

/// A count reaches a chord motion: `3<C-h>` = `3h`.
#[test]
fn a_count_reaches_a_chord_motion() {
    assert_eq!(
        fresh(&[key('3'), ctrl('h')], Mode::Normal),
        Feed::Cmd(Command::Move(3, Motion::Left))
    );
    assert_eq!(
        fresh(&[key('2'), ctrl('n')], Mode::Normal),
        Feed::Cmd(Command::Move(2, Motion::Down))
    );
}

/// `i_CTRL-C` leaves Insert like `<Esc>` but drops a count's repeats: nvim's `3iab<C-c>` inserts `ab` once,
/// where `3iab<Esc>` inserts it three times.
#[test]
fn insert_ctrl_c_leaves_insert_without_the_count_repeat() {
    let run = |leave: KeyEvent| {
        let mut e = InputEngine::new();
        e.feed(key('3'), Mode::Normal);
        e.feed(key('i'), Mode::Normal);
        e.feed(key('a'), Mode::Insert);
        e.feed(key('b'), Mode::Insert);
        e.feed(leave, Mode::Insert)
    };
    assert_eq!(run(ctrl('c')), Feed::Cmd(Command::EnterNormal));
    assert!(matches!(run(special(KeyCode::Esc)), Feed::Replay(_)));
}

/// In Select, Replace and a pending digraph a chord neither types nor replaces.
#[test]
fn select_replace_and_digraph_take_no_chord_as_text() {
    let select = Mode::Select {
        kind: SelectKind::Charwise,
    };
    assert_eq!(fresh(&[ctrl('k')], select), Feed::Ignored);
    assert_eq!(
        fresh(&[ctrl('c')], Mode::Replace),
        Feed::Cmd(Command::EnterNormal)
    );
    assert_eq!(
        fresh(&[ctrl('h')], Mode::Replace),
        Feed::Cmd(Command::ReplaceBackspace)
    );
    assert_eq!(fresh(&[ctrl('s')], Mode::Replace), Feed::Ignored);
    assert_eq!(fresh(&[ctrl('s')], Mode::VirtualReplace), Feed::Ignored);
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('k'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('a'), Mode::Insert),
        Feed::Ignored,
        "a chord aborts the digraph"
    );
    assert_eq!(
        e.feed(key('x'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('x'))
    );
    // `CTRL-G` then `CTRL-U` is not `CTRL-G u`.
    let mut e = InputEngine::new();
    e.feed(ctrl('g'), Mode::Insert);
    assert_eq!(e.feed(ctrl('u'), Mode::Insert), Feed::Ignored);
}
