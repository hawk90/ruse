//! Curated cross-feature SCENARIO suite (testing-and-benchmarks.md §1.9): hand-written realistic
//! multi-step editing sessions with EXACT expected end-states, driven through the same stack `main.rs`
//! composes (input engine + Workspace). Where the oracle corpus checks single ops and the fuzzer checks
//! invariants, these pin the KNOWN-TRICKY cross-feature interactions a modal editor must get right —
//! dot-repeat × count, named registers, visual + operator, search → change → repeat, substitute + undo,
//! insert-session undo grouping, blockwise-insert replication, and multi-window buffer sharing — each as
//! a readable regression guard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, SplitDir, SubFlags, Workspace};
use ruse_tui::input::{parse_ex, Ex, Feed, GlobalPayload, InputEngine};

/// Route one engine outcome exactly as `main.rs::run` does: edits through the swap-trick `Workspace`,
/// `:s`/`:g` through the substitute/global engines, everything else per its `Feed`.
fn step(e: &mut InputEngine, ws: &mut Workspace, key: KeyEvent) {
    let mode = ws.focused().view.mode();
    match e.feed(key, mode) {
        Feed::Cmd(cmd) => {
            ws.apply(&cmd);
        }
        Feed::Replay(cmds) => {
            for c in &cmds {
                ws.apply(c);
            }
        }
        Feed::ExecuteEx(text) => match parse_ex(&text) {
            Ex::Substitute(s) => {
                let _ = ws.substitute(
                    s.range,
                    &s.pattern,
                    &s.replacement,
                    SubFlags {
                        global: s.global,
                        ignore_case: s.ignore_case,
                    },
                );
            }
            Ex::Global(g) => {
                // These scenarios drive only the core `d`/`s` payloads; a `normal` payload would need the
                // input engine (the frontend run loop's `run_global_normal`), out of scope for this helper.
                if let GlobalPayload::Core(cmd) = &g.cmd {
                    let _ = ws.global(g.range, &g.pattern, g.negate, cmd);
                }
            }
            // `:earlier {N}` / `:later {N}` — chronological undo-time travel. Mirrors the run loop
            // (session.rs): drive N of the `g-`/`g+` commands (`UndoOlder`/`UndoNewer`). The or-pattern
            // binds the count but not the direction, so recover the direction from a re-parse, exactly as
            // the run loop does.
            Ex::Earlier(n) | Ex::Later(n) => {
                let cmd = if matches!(parse_ex(&text), Ex::Earlier(_)) {
                    Command::UndoOlder
                } else {
                    Command::UndoNewer
                };
                for _ in 0..n {
                    ws.apply(&cmd);
                }
            }
            // `:[line]put [reg]` — put a register LINEWISE after the addressed line (mirrors dispatch.rs).
            Ex::Put { addr, reg } => {
                ws.put_lines(addr, reg);
            }
            _ => {}
        },
        Feed::Pending | Feed::Ignored => {}
    }
}

/// Feed a Vim-style keystroke script: literal chars, plus `<Esc>` / `<CR>` / `<BS>` / `<C-x>` tokens.
fn feed_str(e: &mut InputEngine, ws: &mut Workspace, s: &str) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let key = if c == '<' {
            let mut tag = String::new();
            for d in chars.by_ref() {
                if d == '>' {
                    break;
                }
                tag.push(d);
            }
            match tag.as_str() {
                "Esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                "CR" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                "BS" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                t if t.starts_with("C-") => KeyEvent::new(
                    KeyCode::Char(t.chars().nth(2).expect("C-<char>")),
                    KeyModifiers::CONTROL,
                ),
                _ => continue,
            }
        } else {
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
        };
        step(e, ws, key);
    }
}

fn buf(ws: &Workspace) -> String {
    String::from_utf8_lossy(ws.focused().doc.bytes()).into_owned()
}

/// A fresh engine + single-window workspace over `initial`.
fn session(initial: &str) -> (InputEngine, Workspace) {
    (
        InputEngine::new(),
        Workspace::new(initial.as_bytes().to_vec()),
    )
}

#[test]
fn insert_session_is_a_single_undo_group() {
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    assert_eq!(buf(&ws), "hello");
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "", "the whole insert session undoes as one group");
}

#[test]
fn dot_repeats_a_change_on_the_next_word() {
    let (mut e, mut ws) = session("foo foo");
    feed_str(&mut e, &mut ws, "cwbar<Esc>");
    assert_eq!(buf(&ws), "bar foo");
    feed_str(&mut e, &mut ws, "w.");
    assert_eq!(
        buf(&ws),
        "bar bar",
        "`.` replays the change on the next word"
    );
}

#[test]
fn count_multiplies_a_dot_repeat() {
    let (mut e, mut ws) = session("aaaaa");
    feed_str(&mut e, &mut ws, "x"); // delete one -> "aaaa"
    assert_eq!(buf(&ws), "aaaa");
    feed_str(&mut e, &mut ws, "3."); // repeat `x` with a fresh count of 3
    assert_eq!(buf(&ws), "a", "`3.` re-applies the last change three times");
}

#[test]
fn named_register_yank_then_paste() {
    let (mut e, mut ws) = session("one\ntwo");
    feed_str(&mut e, &mut ws, "\"ayy"); // yank line 1 into register a
    feed_str(&mut e, &mut ws, "j\"ap"); // paste it after line 2
    assert_eq!(
        buf(&ws),
        "one\ntwo\none",
        "named register round-trips through paste"
    );
}

#[test]
fn ex_put_pastes_a_yanked_line_linewise() {
    // Yank line 1 (charwise register content still puts as a whole line — the linewise-forcing rule is
    // exercised in the core tests; here the whole parse→dispatch→core path is driven from keystrokes).
    let (mut e, mut ws) = session("alpha\nbeta\ngamma\n");
    feed_str(&mut e, &mut ws, "yy"); // yank line 1 (alpha) linewise into the unnamed register
    feed_str(&mut e, &mut ws, ":2put<CR>"); // put after line 2 (beta)
    assert_eq!(
        buf(&ws),
        "alpha\nbeta\nalpha\ngamma\n",
        ":put opens the yanked line below the addressed line"
    );
}

#[test]
fn ex_zero_put_inserts_at_the_top() {
    let (mut e, mut ws) = session("alpha\nbeta\n");
    feed_str(&mut e, &mut ws, "yy"); // yank alpha
    feed_str(&mut e, &mut ws, ":0put<CR>"); // put before line 1
    assert_eq!(
        buf(&ws),
        "alpha\nalpha\nbeta\n",
        ":0put lands at the very top"
    );
}

#[test]
fn search_then_change_then_next_then_dot() {
    let (mut e, mut ws) = session("x foo y foo");
    feed_str(&mut e, &mut ws, "/foo<CR>"); // to the first foo
    feed_str(&mut e, &mut ws, "cwZ<Esc>"); // -> "x Z y foo"
    assert_eq!(buf(&ws), "x Z y foo");
    feed_str(&mut e, &mut ws, "n."); // next match, repeat the change
    assert_eq!(buf(&ws), "x Z y Z", "search-next + dot-repeat compose");
}

#[test]
fn substitute_then_undo_restores() {
    let (mut e, mut ws) = session("aba");
    feed_str(&mut e, &mut ws, ":s/a/X/g<CR>");
    assert_eq!(buf(&ws), "XbX");
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "aba", "a substitute is one undo group");
}

#[test]
fn visual_operator_deletes_the_selection() {
    let (mut e, mut ws) = session("hello world");
    feed_str(&mut e, &mut ws, "ved"); // v, e (to end of "hello"), d
    assert_eq!(
        buf(&ws),
        " world",
        "visual `e` selects the word, `d` deletes it"
    );
}

#[test]
fn blockwise_insert_replicates_and_undoes_as_one_group() {
    let (mut e, mut ws) = session("aa\nbb\ncc");
    feed_str(&mut e, &mut ws, "<C-v>jjIX<Esc>"); // block over col 0 of 3 rows, insert X
    assert_eq!(
        buf(&ws),
        "Xaa\nXbb\nXcc",
        "block insert replicates down every row"
    );
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(
        buf(&ws),
        "aa\nbb\ncc",
        "the whole block insert undoes as one group"
    );
}

#[test]
fn split_windows_share_one_buffer_and_undo() {
    let (mut e, mut ws) = session("hello");
    ws.split(SplitDir::Horizontal); // two windows onto the SAME buffer (as `C-w s` does in main.rs)
    assert_eq!(ws.window_count(), 2);
    feed_str(&mut e, &mut ws, "x"); // delete under cursor in the focused window
                                    // Both panes borrow the one shared Document, so the edit is visible in BOTH.
    assert_eq!(String::from_utf8_lossy(ws.pane(0).doc.bytes()), "ello");
    assert_eq!(
        String::from_utf8_lossy(ws.pane(1).doc.bytes()),
        "ello",
        "a split shares the buffer — the edit shows in both windows"
    );
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "hello", "undo restores the shared buffer");
}

/// Build a BRANCHED undo tree so chronological (`g-`/`g+`) traversal differs from a plain linear undo:
/// three separate insert sessions grow "" → "a" → "ba" → "cba" (nodes A,B,C — each `i` leaves the cursor
/// at column 0, so the next insert PREPENDS), then `g-g-` rewinds to "a" and a fourth insert forks a new
/// branch "Xa" (node D). The chronological creation order is root, A, B, C, D, so from D a single `g-`
/// lands on the chronologically-previous state "cba" — ACROSS the branch, exactly as Vim's `g-` does.
/// Deterministic, so two calls yield identical trees.
fn branched_history() -> (InputEngine, Workspace) {
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ia<Esc>"); // "a"   (node A)
    feed_str(&mut e, &mut ws, "ib<Esc>"); // "ba"  (node B — prepended)
    feed_str(&mut e, &mut ws, "ic<Esc>"); // "cba" (node C — prepended)
    feed_str(&mut e, &mut ws, "g-g-"); // rewind chronologically to "a"
    assert_eq!(
        buf(&ws),
        "a",
        "precondition: two g- from \"cba\" reach \"a\""
    );
    feed_str(&mut e, &mut ws, "iX<Esc>"); // "Xa"  (node D — forks a new branch off A)
    assert_eq!(buf(&ws), "Xa", "precondition: the fork produced \"Xa\"");
    (e, ws)
}

#[test]
fn earlier_later_match_repeated_g_minus_g_plus() {
    // `:earlier {N}` must reach the SAME buffer state as N presses of `g-`, and `:later {N}` the same as
    // N presses of `g+` — the whole point of the ex commands. Drive both paths end-to-end through the real
    // InputEngine + Workspace over an identical branched tree, so this asserts the parse (`earlier N`),
    // the N-count, AND the branch-aware chronological walk all agree with the keystroke path.
    let (mut e_ex, mut ws_ex) = branched_history();
    let (mut e_key, mut ws_key) = branched_history();

    // BACKWARD: `:earlier 3` == `g-` × 3. From "Xa": "Xa" → "cba" → "ba" → "a".
    feed_str(&mut e_ex, &mut ws_ex, ":earlier 3<CR>");
    feed_str(&mut e_key, &mut ws_key, "g-g-g-");
    assert_eq!(
        buf(&ws_ex),
        "a",
        "`:earlier 3` walks three chronological states back to \"a\""
    );
    assert_eq!(
        buf(&ws_ex),
        buf(&ws_key),
        "`:earlier 3` matches three presses of `g-`"
    );

    // FORWARD: `:later 2` == `g+` × 2. From "a": "a" → "ba" → "cba".
    feed_str(&mut e_ex, &mut ws_ex, ":later 2<CR>");
    feed_str(&mut e_key, &mut ws_key, "g+g+");
    assert_eq!(
        buf(&ws_ex),
        "cba",
        "`:later 2` walks two chronological states forward to \"cba\""
    );
    assert_eq!(
        buf(&ws_ex),
        buf(&ws_key),
        "`:later 2` matches two presses of `g+`"
    );

    // The `ea` / `lat` abbreviations drive the identical path.
    let (mut e_ab, mut ws_ab) = branched_history();
    feed_str(&mut e_ab, &mut ws_ab, ":ea 3<CR>");
    assert_eq!(buf(&ws_ab), "a", "`:ea` is `:earlier`");
    feed_str(&mut e_ab, &mut ws_ab, ":lat 1<CR>");
    assert_eq!(buf(&ws_ab), "ba", "`:lat` is `:later`");
}

#[test]
fn earlier_later_clamp_at_undo_tree_bounds() {
    // An over-large count must clamp at the ends of the chronological history and never panic — `:earlier`
    // past the root stops at the oldest state, `:later` past the tip stops at the newest.
    let (mut e, mut ws) = branched_history(); // at "Xa", root + 4 states

    feed_str(&mut e, &mut ws, ":earlier 999<CR>");
    assert_eq!(
        buf(&ws),
        "",
        "`:earlier` past the root clamps at the oldest state"
    );

    feed_str(&mut e, &mut ws, ":later 999<CR>");
    assert_eq!(
        buf(&ws),
        "Xa",
        "`:later` past the tip clamps at the newest state"
    );

    // A bare `:earlier` (no count) is exactly one step, matching Vim's default of 1.
    feed_str(&mut e, &mut ws, ":earlier<CR>");
    assert_eq!(buf(&ws), "cba", "bare `:earlier` steps one state back");
}

#[test]
fn ctrl_v_numeric_char_entry_end_to_end() {
    // `i_CTRL-V` drives the WHOLE stack (engine -> Command -> Workspace edit), not just the engine.
    // Decimal `065` -> A.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "i<C-v>065<Esc>");
    assert_eq!(buf(&ws), "A");

    // Hex byte, BMP unicode, and full unicode all land as the resolved UTF-8 char.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "i<C-v>x41<C-v>u00e9<Esc>");
    assert_eq!(buf(&ws), "Aé");

    // Early terminator: `9` is a valid decimal digit, `x` is not — so char 9 (a tab) is inserted AND
    // the `x` is processed as normal input. Both reach the Workspace via `Feed::Replay`.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "i<C-v>9x<Esc>");
    assert_eq!(buf(&ws), "\tx");
}

#[test]
fn ctrl_v_resolved_char_is_dot_repeatable() {
    // The resolved char is a plain `InsertChar`, so the insert session records it and `.` replays it.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "i<C-v>065<Esc>"); // insert "A"
    assert_eq!(buf(&ws), "A");
    feed_str(&mut e, &mut ws, "."); // repeat the whole insert session
    assert_eq!(buf(&ws), "AA", "`.` replays the CTRL-V-resolved char");
}
