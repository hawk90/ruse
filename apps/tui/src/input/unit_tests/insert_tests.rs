use super::{ctrl, esc, feed, k};
use crate::input::*;

#[test]
fn insert_ctrl_w_and_ctrl_u_emit_delete_commands() {
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(ctrl('w'), Mode::Insert),
        Feed::Cmd(Command::InsertDeleteWordBack)
    );
    assert_eq!(
        e.feed(ctrl('u'), Mode::Insert),
        Feed::Cmd(Command::InsertDeleteToLineStart)
    );
}

/// Record one insert session (`i<body><Esc>`) so `last_inserted` is populated, then return the engine
/// ready for a fresh `i` in Normal mode. Body chars self-insert as `InsertChar`s.
fn engine_after_insert(body: &str) -> InputEngine {
    let mut e = InputEngine::new();
    e.feed(k('i'), Mode::Normal);
    for c in body.chars() {
        e.feed(k(c), Mode::Insert);
    }
    e.feed(esc(), Mode::Insert);
    e
}

#[test]
fn insert_ctrl_a_replays_the_previous_insert_body() {
    // `i_CTRL-A` re-emits the last insert session's typed body (no `EnterNormal` — it stays in Insert).
    let mut e = engine_after_insert("hello");
    e.feed(k('i'), Mode::Normal);
    assert_eq!(
        e.feed(ctrl('a'), Mode::Insert),
        Feed::Replay(vec![
            Command::InsertChar('h'),
            Command::InsertChar('e'),
            Command::InsertChar('l'),
            Command::InsertChar('l'),
            Command::InsertChar('o'),
        ])
    );
}

#[test]
fn insert_ctrl_at_replays_then_leaves_insert() {
    // `i_CTRL-@` is "`CTRL-A` then `<Esc>`": the same body plus a trailing `EnterNormal`.
    let mut e = engine_after_insert("hi");
    e.feed(k('i'), Mode::Normal);
    assert_eq!(
        e.feed(ctrl('@'), Mode::Insert),
        Feed::Replay(vec![
            Command::InsertChar('h'),
            Command::InsertChar('i'),
            Command::EnterNormal,
        ])
    );
}

#[test]
fn insert_ctrl_at_via_ctrl_space_is_accepted() {
    // Terminals that deliver `CTRL-@` as `Ctrl+Space` reach the same `i_CTRL-@` path.
    let mut e = engine_after_insert("x");
    e.feed(k('i'), Mode::Normal);
    assert_eq!(
        e.feed(ctrl(' '), Mode::Insert),
        Feed::Replay(vec![Command::InsertChar('x'), Command::EnterNormal])
    );
}

#[test]
fn insert_ctrl_a_with_no_previous_insert_is_ignored() {
    // No prior insert -> `last_inserted` is empty -> `CTRL-A` is a clean no-op that stays in Insert.
    let mut e = InputEngine::new();
    e.feed(k('i'), Mode::Normal);
    assert_eq!(e.feed(ctrl('a'), Mode::Insert), Feed::Ignored);
}

#[test]
fn insert_ctrl_at_with_no_previous_insert_still_leaves_insert() {
    // No prior insert -> `CTRL-@` inserts nothing but STILL leaves Insert (nvim-verified).
    let mut e = InputEngine::new();
    e.feed(k('i'), Mode::Normal);
    assert_eq!(
        e.feed(ctrl('@'), Mode::Insert),
        Feed::Replay(vec![Command::EnterNormal])
    );
}

#[test]
fn insert_ctrl_a_survives_a_non_insert_change() {
    // A non-insert change (`x`) overwrites dot-repeat's `last_change` but must NOT touch the
    // last-inserted text, so a later `CTRL-A` still replays the insert body.
    let mut e = engine_after_insert("ab");
    e.feed(k('x'), Mode::Normal); // Immediate change — updates last_change, not last_inserted.
    e.feed(k('i'), Mode::Normal);
    assert_eq!(
        e.feed(ctrl('a'), Mode::Insert),
        Feed::Replay(vec![Command::InsertChar('a'), Command::InsertChar('b')])
    );
}

#[test]
fn insert_ctrl_t_and_ctrl_d_emit_indent_commands() {
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(ctrl('t'), Mode::Insert),
        Feed::Cmd(Command::InsertIndent)
    );
    assert_eq!(
        e.feed(ctrl('d'), Mode::Insert),
        Feed::Cmd(Command::InsertDedent)
    );
}

#[test]
fn insert_tab_emits_insert_tab_not_ignored() {
    // Regression: `<Tab>` in Insert used to fall through to the unmatched-Insert policy (which only
    // emits for `Char` keys) and silently do nothing. It must produce `InsertTab`.
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            Mode::Insert
        ),
        Feed::Cmd(Command::InsertTab)
    );
}

#[test]
fn insert_ctrl_r_inserts_the_named_register() {
    let mut e = InputEngine::new();
    // `<C-r>` arms the prefix (pending), then the register name completes it.
    assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Cmd(Command::InsertRegister('a'))
    );
    // `<C-r>"` reads the unnamed register; `<C-r>1`/`<C-r>-` reach the delete slots.
    e.feed(ctrl('r'), Mode::Insert);
    assert_eq!(
        e.feed(k('"'), Mode::Insert),
        Feed::Cmd(Command::InsertRegister('"'))
    );
    e.feed(ctrl('r'), Mode::Insert);
    assert_eq!(
        e.feed(k('1'), Mode::Insert),
        Feed::Cmd(Command::InsertRegister('1'))
    );
    // A non-register second key aborts the prefix without inserting.
    assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k(' '), Mode::Insert), Feed::Ignored);
}

#[test]
fn insert_ctrl_r_accepts_the_read_only_special_registers() {
    // `<C-r>/`, `<C-r>:`, `<C-r>.`, `<C-r>%` name the read-only special registers (`:help i_CTRL-R`);
    // the frontend resolves their content. Each completes the prefix into an `InsertRegister`.
    for name in ['/', ':', '.', '%'] {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(k(name), Mode::Insert),
            Feed::Cmd(Command::InsertRegister(name)),
            "<C-r>{name} inserts the special register"
        );
    }
}

#[test]
fn insert_ctrl_r_ctrl_w_and_ctrl_a_are_a_no_op() {
    // Vim insert-mode `i_CTRL-R_CTRL-W` / `_CTRL-A` insert NOTHING — the word-under-cursor inserts are
    // COMMAND-LINE mode only (`c_CTRL-R_CTRL-W`). Previously the engine mis-read the ctrl-modified char
    // as a register name and emitted `InsertRegister('w')`/`('a')`. Every ctrl-modified selector after
    // insert `C-r` is now a clean no-op (the `C-r C-r`/`C-o`/`C-p` insert-literally variants are not
    // implemented). Verified against nvim v0.12.4.
    for sel in ['w', 'a', 'r', 'o', 'p'] {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
        assert_eq!(
            e.feed(ctrl(sel), Mode::Insert),
            Feed::Ignored,
            "ctrl-modified `{sel}` after insert C-r is a no-op, not a register insert"
        );
    }
    // Regression guard: plain register names / special registers / the expr prompt still work.
    for name in ['a', '"', '/', ':', '.', '%'] {
        let mut e = InputEngine::new();
        e.feed(ctrl('r'), Mode::Insert);
        assert_eq!(
            e.feed(k(name), Mode::Insert),
            Feed::Cmd(Command::InsertRegister(name)),
            "plain `C-r {name}` still inserts the register"
        );
    }
    let mut e = InputEngine::new();
    e.feed(ctrl('r'), Mode::Insert);
    assert_eq!(e.feed(k('='), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.cmdline().map(|c| c.0),
        Some('='),
        "`C-r =` still opens the expression prompt"
    );
}

#[test]
fn special_registers_parse_as_paste_register_names() {
    // `"/`, `":`, `".`, `"%` arm register selection so a following `p`/`P` pastes from them.
    for name in ['/', ':', '.', '%'] {
        let mut e = InputEngine::new();
        e.feed(k('"'), Mode::Normal);
        assert_eq!(
            e.feed(k(name), Mode::Normal),
            Feed::Cmd(Command::SetRegister(Some(name))),
            "\"{name} selects the read-only special register"
        );
    }
}

#[test]
fn last_inserted_text_reconstructs_the_insert_body() {
    // `".` (Vim's last-insert register): typing `abc` yields "abc".
    let mut e = InputEngine::new();
    assert_eq!(e.last_inserted_text(), None, "nothing inserted yet");
    e.feed(k('i'), Mode::Normal);
    for c in ['a', 'b', 'c'] {
        e.feed(k(c), Mode::Insert);
    }
    e.feed(esc(), Mode::Insert);
    assert_eq!(e.last_inserted_text().as_deref(), Some("abc"));
    // A backspace mid-insert removes the last char (net text, matching an i_CTRL-A replay): `aX<BS>b`.
    let mut e = InputEngine::new();
    e.feed(k('i'), Mode::Normal);
    e.feed(k('a'), Mode::Insert);
    e.feed(k('X'), Mode::Insert);
    e.feed(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        Mode::Insert,
    );
    e.feed(k('b'), Mode::Insert);
    e.feed(esc(), Mode::Insert);
    assert_eq!(e.last_inserted_text().as_deref(), Some("ab"));
}

#[test]
fn insert_ctrl_r_equals_opens_the_expression_prompt() {
    // `<C-r>=` opens the expression-register prompt (`:help i_CTRL-R`): typing the expression is
    // Pending, and `<CR>` yields `InsertEval` carrying the collected string for the editor to evaluate.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('r'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('='), Mode::Insert), Feed::Pending);
    // The prompt renders with the `=` glyph and owns the typed buffer.
    for c in "1+2".chars() {
        assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
    }
    assert_eq!(e.cmdline(), Some(('=', "1+2", 3)));
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Mode::Insert
        ),
        Feed::Cmd(Command::InsertEval("1+2".into()))
    );
    assert_eq!(e.cmdline(), None, "the prompt closes on <CR>");
}

#[test]
fn insert_ctrl_k_inserts_a_digraph() {
    // `i_CTRL-K {c1}{c2}`: arm (Pending), first code char (Pending), second char resolves the glyph.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('k'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('a'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k(':'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('ä'))
    );
    // A symbol digraph resolves the same way.
    e.feed(ctrl('k'), Mode::Insert);
    e.feed(k('O'), Mode::Insert);
    assert_eq!(
        e.feed(k('K'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('✓'))
    );
    // After a completed digraph the engine is back to plain Insert (no lingering prefix state).
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('x'))
    );
}

#[test]
fn insert_ctrl_k_unknown_pair_falls_back_to_second_char() {
    // Vim's fallback for an unknown code: insert the SECOND char literally.
    let mut e = InputEngine::new();
    e.feed(ctrl('k'), Mode::Insert);
    e.feed(k('z'), Mode::Insert);
    assert_eq!(
        e.feed(k('q'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('q'))
    );
}

#[test]
fn insert_ctrl_k_esc_mid_sequence_aborts_cleanly() {
    // A non-printable key mid-digraph cancels the pending state without inserting; Insert then resumes.
    let mut e = InputEngine::new();
    e.feed(ctrl('k'), Mode::Insert);
    e.feed(k('a'), Mode::Insert); // first code char collected
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Mode::Insert
        ),
        Feed::Ignored
    );
    // The abort left no residue: the next key inserts literally.
    assert_eq!(
        e.feed(k('b'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('b'))
    );
}

#[test]
fn insert_ctrl_k_code_chars_are_not_lang_mapped() {
    // The two digraph-code chars are literal selectors, so an active Lang-Arg map must NOT rewrite
    // them (else `a:` under a map `a->б` would look up the wrong code).
    let mut e = InputEngine::new();
    e.lang_map.insert('a', 'б');
    e.lang_active = true;
    e.feed(ctrl('k'), Mode::Insert);
    e.feed(k('a'), Mode::Insert);
    assert_eq!(
        e.feed(k(':'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('ä'))
    );
}

// --- `i_CTRL-V` literal / numeric char entry (all expectations verified against nvim v0.12.4). ---

#[test]
fn insert_ctrl_v_decimal_octal_hex_and_unicode() {
    let mut e = InputEngine::new();
    // `C-v 065` -> decimal 65 -> `A` (resolves on the 3rd digit; each earlier key is Pending).
    assert_eq!(e.feed(ctrl('v'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('0'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('6'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('5'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('A'))
    );
    // `C-v x41` -> hex byte 0x41 -> `A` (resolves on the 2nd hex digit).
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('x'), Mode::Insert);
    assert_eq!(e.feed(k('4'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('1'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('A'))
    );
    // `C-v o101` -> octal 101 (= 65) -> `A`.
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('o'), Mode::Insert);
    e.feed(k('1'), Mode::Insert);
    e.feed(k('0'), Mode::Insert);
    assert_eq!(
        e.feed(k('1'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('A'))
    );
    // `C-v u00e9` -> BMP U+00E9 -> `é` (resolves on the 4th hex digit).
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('u'), Mode::Insert);
    e.feed(k('0'), Mode::Insert);
    e.feed(k('0'), Mode::Insert);
    e.feed(k('e'), Mode::Insert);
    assert_eq!(
        e.feed(k('9'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('é'))
    );
    // `C-v U0001F600` -> full Unicode U+1F600 -> 😀 (resolves on the 8th hex digit).
    e.feed(ctrl('v'), Mode::Insert);
    for c in "U0001F60".chars() {
        assert_eq!(e.feed(k(c), Mode::Insert), Feed::Pending);
    }
    assert_eq!(
        e.feed(k('0'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('😀'))
    );
}

#[test]
fn insert_ctrl_v_decimal_clamps_to_a_single_byte() {
    // nvim caps a decimal (and octal) `C-v` code at 255: `C-v 999` -> `ÿ` (U+00FF), `C-v 200` -> `È`.
    let mut e = InputEngine::new();
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('9'), Mode::Insert);
    e.feed(k('9'), Mode::Insert);
    assert_eq!(
        e.feed(k('9'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('\u{ff}'))
    );
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('2'), Mode::Insert);
    e.feed(k('0'), Mode::Insert);
    assert_eq!(
        e.feed(k('0'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('È'))
    );
}

#[test]
fn insert_ctrl_v_early_terminator_resolves_then_reprocesses_the_key() {
    // `C-v 9x`: `9` alone is a valid decimal digit (Pending); `x` is not, so the code resolves to
    // char 9 (a tab) AND `x` is then inserted as normal input. Both reach the driver via `Replay`.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('v'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('9'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Replay(vec![Command::InsertChar('\t'), Command::InsertChar('x'),])
    );
    // A two-digit decimal then a letter terminator: `C-v 65a` -> `A` then `a`.
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('6'), Mode::Insert);
    e.feed(k('5'), Mode::Insert);
    assert_eq!(
        e.feed(k('a'), Mode::Insert),
        Feed::Replay(vec![Command::InsertChar('A'), Command::InsertChar('a'),])
    );
}

#[test]
fn insert_ctrl_v_terminator_that_leaves_insert() {
    // `C-v 65<Esc>` inserts `A` then the `<Esc>` leaves Insert (nvim behaviour), so the resolved char
    // and `EnterNormal` both reach the driver.
    let mut e = InputEngine::new();
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('6'), Mode::Insert);
    e.feed(k('5'), Mode::Insert);
    assert_eq!(
        e.feed(esc(), Mode::Insert),
        Feed::Replay(vec![Command::InsertChar('A'), Command::EnterNormal])
    );
}

#[test]
fn insert_ctrl_v_max_digits_then_next_key_is_normal_input() {
    // After the digit cap is reached the collector is done: the following key is ordinary Insert input.
    let mut e = InputEngine::new();
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('0'), Mode::Insert);
    e.feed(k('6'), Mode::Insert);
    assert_eq!(
        e.feed(k('5'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('A'))
    );
    assert_eq!(
        e.feed(k('Z'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('Z'))
    );
}

#[test]
fn count_on_insert_esc_replays_extra_repeats_then_leaves_insert() {
    // `3ihello<Esc>` (VIM-CNT-INS): entry + first "hello" flow through as `Cmd`s; the terminating
    // `<Esc>` returns the (count-1) EXTRA "hello"s plus `EnterNormal` as ONE `Feed::Replay`, so the
    // whole run collapses into a single undo group at the driver.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('3'), Mode::Normal), Feed::Pending);
    assert_eq!(
        e.feed(k('i'), Mode::Normal),
        Feed::Cmd(Command::EnterInsert)
    );
    for c in "hello".chars() {
        assert_eq!(
            e.feed(k(c), Mode::Insert),
            Feed::Cmd(Command::InsertChar(c))
        );
    }
    // The repeats are lazy (#683): the body twice more, then the `<Esc>`.
    assert_eq!(
        e.feed(esc(), Mode::Insert),
        Feed::Repeat(Repeat {
            head: Vec::new(),
            unit: "hello".chars().map(Command::InsertChar).collect(),
            times: 2,
            tail: vec![Command::EnterNormal],
        })
    );
}

#[test]
fn count_on_insert_open_below_replay_reopens_a_line_each_repeat() {
    // `2ox<Esc>`: the extra repeat must OPEN a new line (not append to the same one), so the replay
    // tail is `[OpenBelow, InsertChar('x'), EnterNormal]`.
    let mut e = InputEngine::new();
    e.feed(k('2'), Mode::Normal);
    assert_eq!(e.feed(k('o'), Mode::Normal), Feed::Cmd(Command::OpenBelow));
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('x'))
    );
    assert_eq!(
        e.feed(esc(), Mode::Insert),
        Feed::Repeat(Repeat {
            head: Vec::new(),
            unit: vec![Command::OpenBelow, Command::InsertChar('x')],
            times: 1,
            tail: vec![Command::EnterNormal],
        })
    );
}

#[test]
fn count_one_insert_esc_is_a_plain_enter_normal() {
    // Regression: a count-less insert must be UNCHANGED — the `<Esc>` is still a bare `Cmd(EnterNormal)`,
    // never a `Feed::Replay`.
    let mut e = InputEngine::new();
    assert_eq!(
        e.feed(k('i'), Mode::Normal),
        Feed::Cmd(Command::EnterInsert)
    );
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('x'))
    );
    assert_eq!(e.feed(esc(), Mode::Insert), Feed::Cmd(Command::EnterNormal));
}

#[test]
fn insert_ctrl_v_prefix_with_no_digits_inserts_the_terminator_literally() {
    // `C-v x z`: the `x` hex prefix collects zero digits, so nvim inserts the terminator `z` and no
    // code char. Likewise `C-v o 8` -> `8` (8 is not an octal digit).
    let mut e = InputEngine::new();
    e.feed(ctrl('v'), Mode::Insert);
    assert_eq!(e.feed(k('x'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('z'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('z'))
    );
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('o'), Mode::Insert);
    assert_eq!(
        e.feed(k('8'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('8'))
    );
}

#[test]
fn insert_ctrl_v_inserts_special_keys_literally() {
    // `C-v` then a non-form key inserts it verbatim: a real tab, a `<CR>`, the escape char, or the
    // control byte of another `C-<key>` (`C-v C-v` -> 0x16). All verified against nvim.
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('v'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(tab, Mode::Insert),
        Feed::Cmd(Command::InsertChar('\t'))
    );
    e.feed(ctrl('v'), Mode::Insert);
    assert_eq!(
        e.feed(enter, Mode::Insert),
        Feed::Cmd(Command::InsertChar('\r'))
    );
    e.feed(ctrl('v'), Mode::Insert);
    assert_eq!(
        e.feed(esc(), Mode::Insert),
        Feed::Cmd(Command::InsertChar('\u{1b}'))
    );
    e.feed(ctrl('v'), Mode::Insert);
    assert_eq!(
        e.feed(ctrl('v'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('\u{16}'))
    );
    // A plain non-form char is also literal: `C-v z` -> `z`.
    e.feed(ctrl('v'), Mode::Insert);
    assert_eq!(
        e.feed(k('z'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('z'))
    );
}

#[test]
fn insert_ctrl_v_code_keys_are_not_lang_mapped() {
    // The `C-v` form selector and code digits are raw selectors, so an active Lang-Arg map must NOT
    // rewrite them. With `0 -> 9` mapped, `C-v 065` still resolves the raw `065` (= `A`), not `965`.
    let mut e = InputEngine::new();
    e.lang_map.insert('0', '9');
    e.lang_active = true;
    e.feed(ctrl('v'), Mode::Insert);
    e.feed(k('0'), Mode::Insert);
    e.feed(k('6'), Mode::Insert);
    assert_eq!(
        e.feed(k('5'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('A'))
    );
}

#[test]
fn normal_quote_equals_opens_the_expression_prompt_for_paste() {
    // `"=` opens the expression prompt; `<CR>` yields `SetExprRegister`, which arms the `"=` register so
    // the FOLLOWING p/P pastes the result (`:help quote=`).
    let mut e = InputEngine::new();
    assert_eq!(e.feed(k('"'), Mode::Normal), Feed::Pending);
    assert_eq!(e.feed(k('='), Mode::Normal), Feed::Pending);
    for c in "'a'.'b'".chars() {
        assert_eq!(e.feed(k(c), Mode::Normal), Feed::Pending);
    }
    assert_eq!(e.cmdline(), Some(('=', "'a'.'b'", 7)));
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Mode::Normal
        ),
        Feed::Cmd(Command::SetExprRegister("'a'.'b'".into()))
    );
}

#[test]
fn expression_prompt_aborts_on_escape() {
    // `<Esc>` during the expression prompt closes it without emitting a command (Vim's cancel).
    let mut e = InputEngine::new();
    e.feed(k('"'), Mode::Normal);
    e.feed(k('='), Mode::Normal);
    e.feed(k('1'), Mode::Normal);
    assert_eq!(
        e.feed(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Mode::Normal
        ),
        Feed::Ignored
    );
    assert_eq!(e.cmdline(), None, "the prompt is gone after <Esc>");
}

#[test]
fn ctrl_o_runs_one_normal_command_then_returns_to_insert() {
    // In Insert, CTRL-O arms a one-shot (Pending); the next key resolves through the NORMAL grammar
    // (here `x` → DeleteUnder), then the engine returns to plain Insert routing.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Cmd(Command::DeleteUnder(1))
    );
    // Disarmed: the next key is an inserted char again, not a Normal command.
    assert_eq!(
        e.feed(k('x'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('x'))
    );
}

#[test]
fn ctrl_o_spans_a_multi_key_normal_command() {
    // A one-shot survives the intermediate Pending keys of a multi-key command (`dw`), disarming only
    // when the command completes.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('o'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('d'), Mode::Insert), Feed::Pending); // operator armed
    assert_eq!(
        e.feed(k('w'), Mode::Insert),
        Feed::Cmd(Command::Delete(1, Motion::WordFwd))
    );
    assert_eq!(
        e.feed(k('z'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('z')),
        "back to Insert after the one-shot command completes"
    );
}

#[test]
fn ctrl_g_u_breaks_undo_and_other_second_keys_abort() {
    // CTRL-G is a one-key prefix in Insert: `u` (or `U`) emits BreakUndo.
    let mut e = InputEngine::new();
    assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('u'), Mode::Insert), Feed::Cmd(Command::BreakUndo));
    // A non-`u` second key aborts the prefix without inserting; Insert then resumes normally.
    assert_eq!(e.feed(ctrl('g'), Mode::Insert), Feed::Pending);
    assert_eq!(e.feed(k('x'), Mode::Insert), Feed::Ignored);
    assert_eq!(
        e.feed(k('y'), Mode::Insert),
        Feed::Cmd(Command::InsertChar('y'))
    );
}

#[test]
fn insert_entry_keys() {
    assert_eq!(feed("o"), Feed::Cmd(Command::OpenBelow));
    assert_eq!(feed("O"), Feed::Cmd(Command::OpenAbove));
    assert_eq!(feed("A"), Feed::Cmd(Command::AppendLineEnd));
    assert_eq!(feed("I"), Feed::Cmd(Command::InsertLineStart));
}
