use super::{ctrl, esc, k};
use crate::input::*;

fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn enter() -> KeyEvent {
    special(KeyCode::Enter)
}

#[test]
fn colon_opens_the_namespace_the_engine_owns_the_line_and_cr_executes() {
    // F-026 #1/#2: `:` enters the namespace; the engine (not the UI) owns the buffer; <CR> runs it.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
    assert_eq!(e.cmdline(), Some((':', "", 0)));
    assert_eq!(e.feed(k('w'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('q'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.cmdline(),
        Some((':', "wq", 2)),
        "the namespace owns the line buffer + cursor"
    );
    assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("wq".into()));
    assert_eq!(e.cmdline(), None, "<CR> closes the one-shot command line");
}

#[test]
fn cmdline_ctrl_r_ctrl_w_and_ctrl_a_splice_the_word_under_cursor() {
    // `c_CTRL-R_CTRL-W` (<cword>) / `c_CTRL-R_CTRL-A` (<cWORD>): the engine owns the cmdline buffer but
    // has no document, so `C-r C-w`/`C-r C-a` emit a request; the frontend resolves the word and
    // splices it via `cmdline_splice`. e.g. buffer word `foobar` under cursor, `:%s/` + `C-r C-w`
    // → `:%s/foobar` (verified against nvim v0.12.4).
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k(':'), Mode::Normal), Feed::Pending);
    for c in "%s/".chars() {
        assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
    }
    // `C-r` arms the prefix (no register infra leaks here — just Pending); `C-w` requests the <cword>.
    assert_eq!(e.feed(ctrl('r'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('w'), Mode::Normal),
        Feed::CmdlineInsertUnder { big: false }
    );
    // The frontend's response splices the resolved word at the caret; the line stays open.
    e.cmdline_splice("foobar");
    assert_eq!(e.cmdline(), Some((':', "%s/foobar", 9)));

    // `C-r C-a` requests the <cWORD>.
    assert_eq!(e.feed(ctrl('r'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(ctrl('a'), Mode::Normal),
        Feed::CmdlineInsertUnder { big: true }
    );

    // An unsupported selector after `C-r` aborts cleanly. Crucially `C-p` is consumed as the (deferred)
    // selector WHILE armed, NOT as `<C-p>` history recall — and the buffer is untouched.
    assert_eq!(e.feed(ctrl('r'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(ctrl('p'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.cmdline(),
        Some((':', "%s/foobar", 9)),
        "unsupported C-r selector leaves the line unchanged"
    );

    // It also works on a `/` search line, splicing more text at the caret.
    let mut e = InputEngine::new();
    e.feed(k('/'), Mode::Normal);
    e.feed(ctrl('r'), Mode::Normal);
    assert_eq!(
        e.feed(ctrl('w'), Mode::Normal),
        Feed::CmdlineInsertUnder { big: false }
    );
    e.cmdline_splice("needle");
    assert_eq!(e.cmdline(), Some(('/', "needle", 6)));
}

#[test]
fn slash_search_now_flows_entirely_through_the_engine() {
    // F-026: no ad-hoc UI buffer — `/`, the pattern chars, and <CR> all go through feed().
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
    for c in "foo".chars() {
        assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
    }
    assert_eq!(
        e.feed(enter(), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
    assert_eq!(e.cmdline(), None);
    // And the operator-fold still works because the buffer moved INTO the engine: `d/bar`.
    assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('/'), Mode::Normal), Feed::Pending);
    for c in "bar".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(
        e.feed(enter(), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Delete,
            count: 1,
            pattern: "bar".into(),
            backward: false,
            offset: SearchOffset::None,
        })
    );
}

#[test]
fn esc_aborts_the_command_line_without_executing() {
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    e.feed(k('x'), Mode::Normal);
    assert_eq!(e.feed(special(KeyCode::Esc), Mode::Normal), Feed::Ignored);
    assert_eq!(e.cmdline(), None, "<Esc> closes the line and runs nothing");
}

#[test]
fn backspace_deletes_back_in_the_owned_buffer() {
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    e.feed(k('a'), Mode::Normal);
    e.feed(k('b'), Mode::Normal);
    e.feed(special(KeyCode::Backspace), Mode::Normal);
    assert_eq!(e.cmdline(), Some((':', "a", 1)));
}

#[test]
fn gq_enters_ex_mode_reprompts_and_visual_exits() {
    // F-026 #3: `gQ` (only) enters Ex mode; <CR> runs AND re-opens; `:visual` leaves it.
    let mut e = InputEngine::new();
    e.feed(k('g'), Mode::Normal);
    assert_eq!(e.feed(k('Q'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.cmdline(),
        Some((':', "", 0)),
        "gQ opens the Ex command line"
    );
    for c in "set".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("set".into()));
    assert_eq!(
        e.cmdline(),
        Some((':', "", 0)),
        "Ex mode re-prompts after <CR>"
    );
    for c in "visual".chars() {
        e.feed(k(c), Mode::Normal);
    }
    assert_eq!(e.feed(enter(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.cmdline(), None, "`:visual` exits Ex mode");
}

#[test]
fn bare_q_is_not_an_ex_mode_key() {
    // At the pinned Neovim revision `Q` is replay-last-register, NOT Ex mode — only `gQ` opens it.
    let mut e = InputEngine::new();
    let out = e.feed(k('Q'), Mode::Normal);
    assert_ne!(
        out,
        Feed::Pending,
        "a bare Q must not open the Ex command line"
    );
    assert_eq!(e.cmdline(), None);
}

// ---- Command-line & search history recall (`:help cmdline-history`) ------------------------------
// Integration tests driving the `:`/`/` prompt state machine through the full engine. Behaviour is
// matched to nvim v0.12.4 (see apps/tui/src/input/history.rs for the ground-truth probe results).

fn up() -> KeyEvent {
    KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
}
fn down() -> KeyEvent {
    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
}

/// Type `line` into an already-open prompt and accept it with `<CR>`.
fn accept_line(e: &mut InputEngine, line: &str) {
    for c in line.chars() {
        e.feed(k(c), Mode::Normal);
    }
    e.feed(enter(), Mode::Normal);
}

/// The current command-line buffer text (`None` when no prompt is open).
fn line(e: &InputEngine) -> Option<String> {
    e.cmdline().map(|(_, t, _)| t.to_string())
}

#[test]
fn ex_history_up_recalls_newest_then_older() {
    // nvim: after `:set foo<CR>` `:echo x<CR>`, open `:` and press <Up> twice → 'echo x', 'set foo'.
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, "set foo");
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, "echo x");
    e.feed(k(':'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("echo x"));
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("set foo"));
    // No older entry: the walk stays put.
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("set foo"));
}

#[test]
fn ex_history_up_prefix_filters() {
    // nvim: `:e` + <Up> recalls only `:e…` entries, newest first.
    let mut e = InputEngine::new();
    for cmd in ["edit a", "echo y", "edit b"] {
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, cmd);
    }
    e.feed(k(':'), Mode::Normal);
    e.feed(k('e'), Mode::Normal);
    e.feed(k('d'), Mode::Normal); // draft = "ed"
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("edit b"));
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("edit a")); // 'echo y' skipped by the "ed" prefix
}

#[test]
fn ctrl_p_walks_raw_history_unfiltered() {
    // nvim-VERIFIED distinction: <C-p> ignores the typed prefix. `:e` + <C-p> → 'edit b'
    // (raw newest), <C-p> again → 'echo y' (raw next, NOT prefix-filtered).
    let mut e = InputEngine::new();
    for cmd in ["edit a", "echo y", "edit b"] {
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, cmd);
    }
    e.feed(k(':'), Mode::Normal);
    e.feed(k('e'), Mode::Normal); // draft = "e"
    e.feed(ctrl('p'), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("edit b"));
    e.feed(ctrl('p'), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("echo y"));
}

#[test]
fn down_restores_typed_draft() {
    // nvim: `:e` <Up> <Down> → 'e' (the draft returns); <C-n> mirrors <Down>.
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, "edit b");
    e.feed(k(':'), Mode::Normal);
    e.feed(k('e'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("edit b"));
    e.feed(down(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("e"));
    e.feed(ctrl('n'), Mode::Normal); // already at draft → no-op
    assert_eq!(line(&e).as_deref(), Some("e"));
}

#[test]
fn search_history_is_separate_from_ex() {
    // nvim keeps `:` and `/` histories apart; `/` and `?` share the search ring.
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, "foo");
    e.feed(k('/'), Mode::Normal);
    accept_line(&mut e, "bar");
    // The `:` prompt recalls only ex history.
    e.feed(k(':'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("foo"));
    e.feed(enter(), Mode::Normal);
    // The `/` prompt recalls only search history.
    e.feed(k('/'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("bar"));
    // `?` shares the same search ring.
    e.feed(enter(), Mode::Normal);
    e.feed(k('?'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("bar"));
}

#[test]
fn immediate_repeat_is_deduped_in_history() {
    // nvim: re-entering an identical ex line moves it to the most-recent slot (single entry).
    let mut e = InputEngine::new();
    for cmd in ["foo", "bar", "foo"] {
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, cmd);
    }
    e.feed(k(':'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("foo"));
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("bar"));
    // Only two distinct entries remain — a third <Up> stays on the oldest.
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("bar"));
}

#[test]
fn empty_line_is_not_recorded() {
    // An accepted empty `:` line stores nothing; a later <Up> finds an earlier real entry.
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, "real");
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, ""); // empty → not stored
    e.feed(k(':'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("real"));
}

// ---- Command-line window (`:help cmdwin`, q: q/ q?) -----------------------------------------------
// The engine side of the reduced list-overlay slice. `q:`/`q/`/`q?` are routed to `open_cmdwin` by the
// macro layer (keys.rs, tested there); here we open the window directly and drive its keys. Behaviour
// mirrors nvim v0.12.4: the list is the history ring with a trailing empty line the cursor starts on;
// `<CR>` runs the selected line through the SAME ex/search dispatch the `:`/`/` prompt uses; `<Esc>` /
// `<C-c>` close without running.

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

/// #897: Normal `<C-c>` is an interrupt, not the `c` operator: `<C-c><C-c>` was `cc`. It cancels a
/// pending operator or awaited key, and leaves Visual (nvim v0.12.6).
#[test]
fn normal_ctrl_c_interrupts_instead_of_changing() {
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
    assert_eq!(
        e.feed(k('d'), Mode::Normal),
        Feed::Pending,
        "the cancelled d is gone"
    );
    assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.feed(k('f'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
    let vis = Mode::Visual {
        kind: SelectKind::Charwise,
    };
    assert_eq!(e.feed(ctrl_c(), vis), Feed::Cmd(Command::EnterNormal));
}

#[test]
fn cmdwin_cr_executes_the_selected_ex_line_and_closes() {
    let mut e = InputEngine::new();
    for cmd in ["set nu", "write"] {
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, cmd);
    }
    e.open_cmdwin(':');
    assert_eq!(e.cmdwin(), Some(':'), "the window is open over the ex ring");
    // The cursor starts on the empty last line; running it is a no-op close (Vim).
    assert_eq!(e.feed(enter(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.cmdwin(), None, "<CR> on the empty line just closes");
    // Re-open and navigate up to the newest entry, then run it.
    e.open_cmdwin(':');
    e.feed(k('k'), Mode::Normal); // `k` = up → "write"
    assert_eq!(
        e.feed(enter(), Mode::Normal),
        Feed::ExecuteEx("write".into()),
        "the selected historical ex line executes"
    );
    assert_eq!(e.cmdwin(), None, "the window closes after running");
    // The run line was recorded in the ex ring via the same accept path a `:` prompt uses.
    e.feed(k(':'), Mode::Normal);
    e.feed(up(), Mode::Normal);
    assert_eq!(line(&e).as_deref(), Some("write"));
}

#[test]
fn cmdwin_navigates_with_j_k_and_arrows() {
    let mut e = InputEngine::new();
    for cmd in ["one", "two", "three"] {
        e.feed(k(':'), Mode::Normal);
        accept_line(&mut e, cmd);
    }
    e.open_cmdwin(':');
    // rows(): oldest first, then the empty selected last line (4 rows).
    let rows = e.cmdwin_rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].0, "one");
    assert!(rows[3].1, "the empty last row is selected initially");
    // `k`/<Up> move toward older; `j`/<Down> back toward the empty line.
    e.feed(up(), Mode::Normal); // → three
    e.feed(k('k'), Mode::Normal); // → two
    assert_eq!(e.feed(enter(), Mode::Normal), Feed::ExecuteEx("two".into()));
}

#[test]
fn cmdwin_esc_and_ctrl_c_close_without_executing() {
    let mut e = InputEngine::new();
    e.feed(k(':'), Mode::Normal);
    accept_line(&mut e, "quitcmd");
    // <Esc> closes without running.
    e.open_cmdwin(':');
    e.feed(up(), Mode::Normal); // select "quitcmd"
    assert_eq!(e.feed(esc(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.cmdwin(), None, "<Esc> closes the window");
    // <C-c> likewise closes without running.
    e.open_cmdwin(':');
    e.feed(up(), Mode::Normal);
    assert_eq!(e.feed(ctrl_c(), Mode::Normal), Feed::Ignored);
    assert_eq!(e.cmdwin(), None, "<C-c> closes the window");
}

#[test]
fn cmdwin_search_line_runs_as_a_search() {
    let mut e = InputEngine::new();
    // Populate the SEARCH ring with a `/` line.
    e.feed(k('/'), Mode::Normal);
    accept_line(&mut e, "foo");
    // `q/` mirrors the search ring; running the line performs the search (folds through submit_search).
    e.open_cmdwin('/');
    e.feed(k('k'), Mode::Normal); // select "foo"
    assert_eq!(
        e.feed(enter(), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "foo".into(),
            backward: false,
            offset: SearchOffset::None,
        }),
        "q/ + <CR> runs the chosen pattern as a forward search"
    );
    assert_eq!(e.cmdwin(), None);
}

#[test]
fn cmdwin_q_question_searches_backward() {
    let mut e = InputEngine::new();
    e.feed(k('/'), Mode::Normal);
    accept_line(&mut e, "bar");
    // `q?` mirrors the same shared search ring but runs the line BACKWARD.
    e.open_cmdwin('?');
    e.feed(k('k'), Mode::Normal);
    assert_eq!(
        e.feed(enter(), Mode::Normal),
        Feed::Cmd(Command::Search {
            op: SearchOp::Move,
            count: 1,
            pattern: "bar".into(),
            backward: true,
            offset: SearchOffset::None,
        })
    );
}
