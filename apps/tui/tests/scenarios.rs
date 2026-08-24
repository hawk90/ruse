//! Curated cross-feature SCENARIO suite (testing-and-benchmarks.md §1.9): hand-written realistic
//! multi-step editing sessions with EXACT expected end-states, driven through the same stack `main.rs`
//! composes (input engine + Workspace). Where the oracle corpus checks single ops and the fuzzer checks
//! invariants, these pin the KNOWN-TRICKY cross-feature interactions a modal editor must get right —
//! dot-repeat × count, named registers, visual + operator, search → change → repeat, substitute + undo,
//! insert-session undo grouping, blockwise-insert replication, and multi-window buffer sharing — each as
//! a readable regression guard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, SplitDir, SubFlags, SubRange, Workspace};
use ruse_tui::input::{parse_ex, Ex, Feed, GlobalPayload, InputEngine};

/// Route one engine outcome exactly as `main.rs::run` does: edits through the swap-trick `Workspace`,
/// `:s`/`:g` through the substitute/global engines, everything else per its `Feed`.
fn step(e: &mut InputEngine, ws: &mut Workspace, key: KeyEvent) {
    let mode = ws.focused().view.mode();
    // Mirror session.rs's `i_CTRL-E` / `i_CTRL-Y` frontend intercept: the engine has no buffer, so the
    // frontend resolves the char below / above the caret and hands it to the engine, which emits (and
    // dot-records) a concrete `InsertChar`. `None` (short/absent adjacent line) is a no-op.
    if matches!(mode, ruse_core::Mode::Insert)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('e' | 'y'))
        && e.insert_plain_text_ctx()
    {
        let ch = ws.adjacent_line_char(matches!(key.code, KeyCode::Char('y')));
        if let Feed::Cmd(cmd) = e.insert_copy_char(ch) {
            ws.apply(&cmd);
        }
        return;
    }
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

// ---------------------------------------------------------------------------------------------------
// Count-on-insert repetition (VIM-CNT-INS, issue #470). A count before a pure insert-entry replays the
// typed text that many times on `<Esc>`. All expected bytes + caret offsets verified against nvim
// v0.12.4 (`nvim -u NONE` headless).
// ---------------------------------------------------------------------------------------------------

/// The focused window's caret as a byte offset — count-on-insert must land the caret exactly where Vim
/// leaves it (on the last inserted char after the `<Esc>` left-shift).
fn cur(ws: &Workspace) -> usize {
    ws.focused().view.cursor()
}

#[test]
fn count_insert_i_repeats_typed_text() {
    // nvim: `3ihello<Esc>` on an empty line -> "hellohellohello", caret on the last 'o' (col 14).
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "3ihello<Esc>");
    assert_eq!(buf(&ws), "hellohellohello");
    assert_eq!(cur(&ws), 14, "caret rests on the last inserted char");
}

#[test]
fn count_insert_a_appends_repeats() {
    // nvim: `3ahello<Esc>` on "abc" -> "ahellohellohellobc" (append after the char under the caret).
    let (mut e, mut ws) = session("abc");
    feed_str(&mut e, &mut ws, "3ahello<Esc>");
    assert_eq!(buf(&ws), "ahellohellohellobc");
}

#[test]
fn count_insert_cap_i_and_cap_a() {
    // nvim: `3Ifoo<Esc>` inserts at the first non-blank; `3Afoo<Esc>` appends at end-of-line.
    let (mut e, mut ws) = session("  abc");
    feed_str(&mut e, &mut ws, "3Ifoo<Esc>");
    assert_eq!(buf(&ws), "  foofoofooabc");

    let (mut e, mut ws) = session("abc");
    feed_str(&mut e, &mut ws, "3Afoo<Esc>");
    assert_eq!(buf(&ws), "abcfoofoofoo");
    assert_eq!(cur(&ws), 11, "caret on the last appended char");
}

#[test]
fn count_insert_o_opens_new_lines_below() {
    // nvim: `3ox<Esc>` opens three new lines below, each containing the typed "x"; caret on the last.
    let (mut e, mut ws) = session("abc\ndef");
    feed_str(&mut e, &mut ws, "3ox<Esc>");
    assert_eq!(buf(&ws), "abc\nx\nx\nx\ndef");
    // "abc\nx\nx\nx\n" is 10 bytes; the last 'x' (line 4) sits at offset 8.
    assert_eq!(cur(&ws), 8, "caret on the last opened line's char");
}

#[test]
fn count_insert_cap_o_opens_new_lines_above() {
    // nvim: `3Ox<Esc>` opens three new lines ABOVE, top-to-bottom, caret on the third (just above "abc").
    let (mut e, mut ws) = session("abc\ndef");
    feed_str(&mut e, &mut ws, "3Ox<Esc>");
    assert_eq!(buf(&ws), "x\nx\nx\nabc\ndef");
    assert_eq!(cur(&ws), 4, "caret on the third (last-typed) opened line");
}

#[test]
fn count_insert_replays_resulting_text_after_backspace() {
    // nvim: `3ixy<BS>z<Esc>` on "abc" -> the RESULTING typed text is "xz" (xy, BS deletes y, then z),
    // replayed three times -> "xzxzxz".
    let (mut e, mut ws) = session("abc");
    feed_str(&mut e, &mut ws, "3ixy<BS>z<Esc>");
    assert_eq!(buf(&ws), "xzxzxzabc");
}

#[test]
fn count_insert_is_one_undo_group() {
    // The whole `3ihello` collapses into a SINGLE undo unit — one `u` removes all three copies.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "3ihello<Esc>");
    assert_eq!(buf(&ws), "hellohellohello");
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(
        buf(&ws),
        "",
        "a single `u` undoes the whole count-on-insert run"
    );

    // Same for the line-opening form: `3ox` (open + type, three times) is one undo group.
    let (mut e, mut ws) = session("abc\ndef");
    feed_str(&mut e, &mut ws, "3ox<Esc>");
    assert_eq!(buf(&ws), "abc\nx\nx\nx\ndef");
    feed_str(&mut e, &mut ws, "u");
    assert_eq!(buf(&ws), "abc\ndef", "`3ox` undoes as one group");
}

#[test]
fn count_insert_dot_repeats_with_same_count() {
    // nvim: `3ix<Esc>` then `j0.` re-applies the WHOLE count-3 insert on the next line -> both "xxx…".
    let (mut e, mut ws) = session("abc\ndef");
    feed_str(&mut e, &mut ws, "3ix<Esc>");
    assert_eq!(buf(&ws), "xxxabc\ndef");
    feed_str(&mut e, &mut ws, "j0.");
    assert_eq!(
        buf(&ws),
        "xxxabc\nxxxdef",
        "`.` repeats with the recorded count 3"
    );
}

#[test]
fn count_insert_dot_with_new_count_overrides() {
    // nvim: `3ix<Esc>` then `j02.` -> the leading `2.` overrides the recorded count on the next line.
    let (mut e, mut ws) = session("abc\ndef");
    feed_str(&mut e, &mut ws, "3ix<Esc>");
    feed_str(&mut e, &mut ws, "j02.");
    assert_eq!(
        buf(&ws),
        "xxxabc\nxxdef",
        "`2.` overrides the recorded count"
    );
}

#[test]
fn count_insert_o_is_dot_repeatable() {
    // nvim: `3ox<Esc>.` -> the dot repeats the whole `3o`, opening three MORE lines (six total).
    let (mut e, mut ws) = session("abc\ndef");
    feed_str(&mut e, &mut ws, "3ox<Esc>");
    feed_str(&mut e, &mut ws, ".");
    assert_eq!(buf(&ws), "abc\nx\nx\nx\nx\nx\nx\ndef");
}

#[test]
fn count_one_insert_is_unchanged() {
    // Regression: a count-less (implicit 1) insert behaves exactly as before — no replication, and the
    // Esc still leaves a single, normally-grouped insert session that `.` repeats once.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    assert_eq!(buf(&ws), "hello");
    assert_eq!(cur(&ws), 4);
    feed_str(&mut e, &mut ws, ".");
    assert_eq!(
        buf(&ws),
        "hellhelloo",
        "`.` repeats the single insert once at the caret"
    );
}

// `i_CTRL-A` / `i_CTRL-@` (issue: insert previously-inserted text). Every expectation below was captured
// from nvim v0.12.4 via `nvim -u NONE` + `nvim_feedkeys`, so these pin Vim-faithful behavior.

#[test]
fn insert_ctrl_a_reinserts_last_insert_text() {
    // nvim: `ihello<Esc>A<C-a><Esc>` -> "hellohello" (C-A re-inserts the previous session's text).
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    feed_str(&mut e, &mut ws, "A<C-a><Esc>");
    assert_eq!(buf(&ws), "hellohello");
}

#[test]
fn insert_ctrl_a_replays_resulting_text_after_backspace() {
    // nvim: `ihel<BS>lo<Esc>` leaves "helo" (hel, BS deletes 'l', then "lo"); `A <C-a><Esc>` then appends
    // a space and re-inserts "helo" -> "helo helo". C-A replays the keystrokes (backspace and all).
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihel<BS>lo<Esc>");
    assert_eq!(buf(&ws), "helo");
    feed_str(&mut e, &mut ws, "A <C-a><Esc>");
    assert_eq!(buf(&ws), "helo helo");
}

#[test]
fn insert_ctrl_at_reinserts_then_leaves_insert() {
    // nvim: `ihello<Esc>A<C-@>x` -> C-@ inserts "hello" ("hellohello") AND leaves Insert, so the next `x`
    // is a Normal-mode delete of the last char -> "hellohell".
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    feed_str(&mut e, &mut ws, "A<C-@>x");
    assert_eq!(buf(&ws), "hellohell");
    assert_eq!(ws.focused().view.mode(), ruse_core::Mode::Normal);
}

#[test]
fn insert_ctrl_a_survives_a_non_insert_change() {
    // nvim: `ihello<Esc>0xA<C-a><Esc>` -> the intervening `x` (a non-insert change) does NOT clobber the
    // last-inserted text, so C-A still re-inserts "hello": "ello" + "hello" = "ellohello".
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    feed_str(&mut e, &mut ws, "0xA<C-a><Esc>");
    assert_eq!(buf(&ws), "ellohello");
}

#[test]
fn insert_ctrl_a_output_rolls_into_the_session_text() {
    // nvim: `ihello<Esc>iworld<C-a><Esc>` -> "hellworldhelloo" (world typed before 'o', then C-A inserts
    // "hello"); the session's last-inserted text is now "worldhello", so a further `A<C-a><Esc>` appends
    // "worldhello" -> "hellworldhellooworldhello".
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "ihello<Esc>");
    feed_str(&mut e, &mut ws, "iworld<C-a><Esc>");
    assert_eq!(buf(&ws), "hellworldhelloo");
    feed_str(&mut e, &mut ws, "A<C-a><Esc>");
    assert_eq!(buf(&ws), "hellworldhellooworldhello");
}

#[test]
fn insert_ctrl_a_with_no_previous_insert_is_a_noop() {
    // nvim: with no prior insert, `i<C-a>abc<Esc>` inserts only "abc" (C-A is a no-op — "E29: No inserted
    // text yet") and stays in Insert.
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "i<C-a>abc<Esc>");
    assert_eq!(buf(&ws), "abc");
}

#[test]
fn insert_ctrl_at_with_no_previous_insert_still_leaves_insert() {
    // nvim: with no prior insert, `i<C-@>xy<Esc>` leaves the buffer EMPTY: C-@ inserts nothing but still
    // stops Insert, so `xy` runs in Normal mode (harmless on an empty line).
    let (mut e, mut ws) = session("");
    feed_str(&mut e, &mut ws, "i<C-@>xy<Esc>");
    assert_eq!(buf(&ws), "");
    assert_eq!(ws.focused().view.mode(), ruse_core::Mode::Normal);
}

#[test]
fn count_change_family_does_not_repeat_text() {
    // Regression: `c` count applies to the MOTION, never to text repetition. `3cwX<Esc>` changes three
    // words into a single "X" (nvim), NOT "XXX".
    let (mut e, mut ws) = session("aa bb cc dd");
    feed_str(&mut e, &mut ws, "3cwX<Esc>");
    assert_eq!(buf(&ws), "X dd", "`3cw` changes three words to one X");
}

/// Feed a keystroke script while threading the last-`:s` state exactly as `session.rs` does, so `&`
/// (`RepeatSubstituteLine`) and `g&` (`RepeatSubstituteGlobal`) resolve against a real substitute history.
/// This is the ONLY place the two frontend-resolved repeat commands can be exercised end to end (the plain
/// `step` helper has no history), and it drives the SAME substitute executor the run loop uses.
fn feed_str_subst(
    e: &mut InputEngine,
    ws: &mut Workspace,
    last: &mut Option<(String, String, SubFlags)>,
    s: &str,
) {
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
                _ => continue,
            }
        } else {
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
        };
        let mode = ws.focused().view.mode();
        match e.feed(key, mode) {
            // `&` — current line, flags DROPPED (default flags): the crux of this feature.
            Feed::Cmd(Command::RepeatSubstituteLine) => {
                if let Some((pat, rep, _flags)) = last.as_ref() {
                    let _ = ws.substitute(SubRange::CurrentLine, pat, rep, SubFlags::default());
                }
            }
            // `g&` — whole file, flags KEPT: mirrored here for the contrast assertion.
            Feed::Cmd(Command::RepeatSubstituteGlobal) => {
                if let Some((pat, rep, flags)) = last.as_ref() {
                    let _ = ws.substitute(SubRange::WholeFile, pat, rep, *flags);
                }
            }
            Feed::Cmd(cmd) => {
                ws.apply(&cmd);
            }
            Feed::Replay(cmds) => {
                for cmd in &cmds {
                    ws.apply(cmd);
                }
            }
            Feed::ExecuteEx(text) => match parse_ex(&text) {
                Ex::Substitute(spec) => {
                    if !spec.pattern.is_empty() {
                        *last = Some((
                            spec.pattern.clone(),
                            spec.replacement.clone(),
                            SubFlags {
                                global: spec.global,
                                ignore_case: spec.ignore_case,
                            },
                        ));
                    }
                    let _ = ws.substitute(
                        spec.range,
                        &spec.pattern,
                        &spec.replacement,
                        SubFlags {
                            global: spec.global,
                            ignore_case: spec.ignore_case,
                        },
                    );
                }
                // Bare `:s` / `:s {flags}` / `:&` / `:&&` — resolved against `last` exactly as `session.rs`
                // does: `flags = None` (`:&&`) keeps the stored flags, `Some(f)` replaces them.
                Ex::RepeatSubstitute { range, flags } => {
                    if let Some((pat, rep, stored)) = last.as_ref() {
                        let _ = ws.substitute(range, pat, rep, flags.unwrap_or(*stored));
                    }
                }
                _ => {}
            },
            Feed::Pending | Feed::Ignored => {}
        }
    }
}

/// Bare `&` repeats the last `:s` on the CURRENT LINE and DROPS the previous flags — verified against
/// nvim v0.12.4: `:s/a/X/g` then `&` on a later line replaces ONLY the first `a` on that line (the `g` is
/// gone). The flag-drop is the whole point of `&`, so this is the load-bearing assertion.
#[test]
fn ampersand_repeats_last_substitute_on_current_line_dropping_flags() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa\nbbb aaa aaa\nccc aaa aaa\n");
    // A GLOBAL substitute on line 1 (all matches), then move to line 2 and press `&`.
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/a/X/g<CR>");
    feed_str_subst(&mut e, &mut ws, &mut last, "j0&");
    assert_eq!(
        buf(&ws),
        "XXX XXX\nbbb Xaa aaa\nccc aaa aaa\n",
        "`&` drops the stored `g`: only the FIRST `a` on line 2 is replaced"
    );
}

/// `&` repeats a flag-less `:s` faithfully too (first match on the current line), and repeating it after
/// moving the cursor acts on the NEW line — matching nvim.
#[test]
fn ampersand_repeats_flagless_substitute_on_each_current_line() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa\nbbb aaa\nccc aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/a/X/<CR>"); // line 1: first `a`
    feed_str_subst(&mut e, &mut ws, &mut last, "j0&"); // line 2: first `a`
    feed_str_subst(&mut e, &mut ws, &mut last, "j0&"); // line 3: first `a`
    assert_eq!(
        buf(&ws),
        "Xaa\nbbb Xaa\nccc Xaa\n",
        "`&` repeats the current-line, first-match substitute on each line it visits"
    );
}

/// `&` with NO previous `:s` is a safe no-op (nvim errors E33; ruse just does nothing to the buffer).
#[test]
fn ampersand_with_no_previous_substitute_is_a_noop() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, "&");
    assert_eq!(
        buf(&ws),
        "aaa aaa\n",
        "no prior `:s` — the buffer is untouched"
    );
}

/// Contrast guard: `g&` KEEPS the flags and runs over the WHOLE FILE, so the two forms stay distinct.
#[test]
fn g_ampersand_keeps_flags_over_the_whole_file() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa\nbbb aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/a/X/g<CR>"); // record a global `:s`
    feed_str_subst(&mut e, &mut ws, &mut last, "g&"); // repeat globally, flags kept
    assert_eq!(
        buf(&ws),
        "XXX XXX\nbbb XXX\n",
        "`g&` keeps the `g` and covers every line"
    );
}

// --- Ex repeat-substitute forms: bare `:s`, `:s {flags}`, `:&`, `:&&` (verified vs nvim v0.12.4) ---

/// Bare `:s` (no delimiter) repeats the last `:s` on the current line and DROPS the previous flags — like
/// normal-mode `&`. nvim: `:s/aaa/X/g` on line 1, then bare `:s` on line 2 replaces ONLY the first `aaa`.
#[test]
fn ex_bare_s_repeats_last_substitute_dropping_flags() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/aaa/X/g<CR>"); // line 1: all matches (g)
    feed_str_subst(&mut e, &mut ws, &mut last, "j0:s<CR>"); // line 2: bare `:s`, flags dropped
    assert_eq!(
        buf(&ws),
        "X X X\nbbb X aaa aaa\n",
        "bare `:s` drops the stored `g`: only the FIRST `aaa` on line 2 is replaced"
    );
}

/// `:&` behaves exactly like bare `:s`: repeat on the current line, flags dropped.
#[test]
fn ex_ampersand_repeats_last_substitute_dropping_flags() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/aaa/X/g<CR>");
    feed_str_subst(&mut e, &mut ws, &mut last, "j0:&<CR>");
    assert_eq!(
        buf(&ws),
        "X X X\nbbb X aaa aaa\n",
        "`:&` is bare `:s`: only the FIRST `aaa` on line 2 is replaced"
    );
}

/// `:&&` KEEPS the previous flags. nvim: `:s/aaa/X/g` then `:&&` on line 2 replaces ALL `aaa` (the `g` sticks).
#[test]
fn ex_double_ampersand_repeats_last_substitute_keeping_flags() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/aaa/X/g<CR>");
    feed_str_subst(&mut e, &mut ws, &mut last, "j0:&&<CR>");
    assert_eq!(
        buf(&ws),
        "X X X\nbbb X X X\n",
        "`:&&` keeps the `g`: EVERY `aaa` on line 2 is replaced"
    );
}

/// `:s {flags}` (bare `s` followed by only flags) repeats with the GIVEN flags replacing the old ones. nvim:
/// `:s/aaa/X/` (no g) then `:s g` on line 2 applies `g`, replacing every `aaa`.
#[test]
fn ex_s_with_only_flags_uses_the_given_flags() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s/aaa/X/<CR>"); // line 1: first match only (no g)
    feed_str_subst(&mut e, &mut ws, &mut last, "j0:s g<CR>"); // line 2: add g → all matches
    assert_eq!(
        buf(&ws),
        "X aaa aaa\nbbb X X X\n",
        "`:s g` applies the given `g` flag: EVERY `aaa` on line 2 is replaced"
    );
}

/// `:[range]&&` repeats over a range, keeping flags. nvim: `:1s/aaa/X/g` then `:2,3&&` replaces all `aaa` on
/// lines 2 and 3.
#[test]
fn ex_double_ampersand_over_a_range_keeps_flags() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa\nbbb aaa aaa\nccc aaa aaa\nddd aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":1s/aaa/X/g<CR>");
    feed_str_subst(&mut e, &mut ws, &mut last, ":2,3&&<CR>");
    assert_eq!(
        buf(&ws),
        "X X\nbbb X X\nccc X X\nddd aaa aaa\n",
        "`:2,3&&` repeats over the range keeping `g`; line 4 is untouched"
    );
}

/// A repeat form with NO previous `:s` is a safe no-op (nvim errors E33; ruse leaves the buffer untouched).
#[test]
fn ex_repeat_substitute_with_no_previous_substitute_is_a_noop() {
    let mut last = None;
    let (mut e, mut ws) = session("aaa aaa\n");
    feed_str_subst(&mut e, &mut ws, &mut last, ":s<CR>");
    feed_str_subst(&mut e, &mut ws, &mut last, ":&<CR>");
    feed_str_subst(&mut e, &mut ws, &mut last, ":&&<CR>");
    assert_eq!(
        buf(&ws),
        "aaa aaa\n",
        "no prior `:s` — every repeat form leaves the buffer untouched"
    );
}

// --- i_CTRL-E / i_CTRL-Y: insert the char BELOW / ABOVE the caret (verified vs nvim v0.12.4) ---

/// `i_CTRL-E` at column 0 of the top line inserts the character directly below (same column).
#[test]
fn insert_ctrl_e_copies_char_below() {
    let (mut e, mut ws) = session("hello\nworld");
    feed_str(&mut e, &mut ws, "i<C-e>");
    assert_eq!(buf(&ws), "whello\nworld", "`C-e` inserts 'w' (below col 0)");
}

/// `i_CTRL-Y` at column 0 of the bottom line inserts the character directly above (same column).
#[test]
fn insert_ctrl_y_copies_char_above() {
    let (mut e, mut ws) = session("hello\nworld");
    feed_str(&mut e, &mut ws, "ji<C-y>");
    assert_eq!(buf(&ws), "hello\nhworld", "`C-y` inserts 'h' (above col 0)");
}

/// The caret advances after each insert, so a run of `C-Y` copies successive columns from the line above.
#[test]
fn insert_ctrl_y_run_copies_successive_columns() {
    let (mut e, mut ws) = session("hello\nworld");
    feed_str(&mut e, &mut ws, "ji<C-y><C-y><C-y>");
    assert_eq!(buf(&ws), "hello\nhelworld", "three `C-y` copy 'h','e','l'");
}

/// `i_CTRL-E` uses the caret's column, not column 0: from mid-line it copies the char below THAT column.
#[test]
fn insert_ctrl_e_uses_caret_column() {
    let (mut e, mut ws) = session("hello\nworld");
    feed_str(&mut e, &mut ws, "lli<C-e>"); // caret at col 2 (on 'l')
    assert_eq!(buf(&ws), "herllo\nworld", "col 2 below 'world' is 'r'");
}

/// No character at that column on the adjacent line (line too short) → a no-op (Vim's bell case).
#[test]
fn insert_ctrl_e_short_line_below_is_noop() {
    let (mut e, mut ws) = session("hello\nab");
    feed_str(&mut e, &mut ws, "$i<C-e>"); // caret at col 4 ('o'); 'ab' has no col 4
    assert_eq!(
        buf(&ws),
        "hello\nab",
        "no char below col 4 → nothing inserted"
    );
}

/// No line below the current line → a no-op.
#[test]
fn insert_ctrl_e_no_line_below_is_noop() {
    let (mut e, mut ws) = session("hello");
    feed_str(&mut e, &mut ws, "i<C-e>");
    assert_eq!(buf(&ws), "hello", "no line below → nothing inserted");
}

/// No line above the current line → a no-op.
#[test]
fn insert_ctrl_y_no_line_above_is_noop() {
    let (mut e, mut ws) = session("hello\nworld");
    feed_str(&mut e, &mut ws, "i<C-y>");
    assert_eq!(buf(&ws), "hello\nworld", "no line above → nothing inserted");
}

/// Dot-repeat replays the RESOLVED LITERAL char (matching nvim), not a re-resolution against the new line.
/// On line 2, `C-Y` copies 'a' from line 1; `.` on line 3 inserts that same 'a', NOT line 2's 'b'.
#[test]
fn insert_ctrl_y_dot_repeats_the_literal_char() {
    let (mut e, mut ws) = session("ax\nby\ncz");
    feed_str(&mut e, &mut ws, "ji<C-y><Esc>"); // line 2 -> "aby"
    assert_eq!(buf(&ws), "ax\naby\ncz");
    feed_str(&mut e, &mut ws, "j0."); // line 3: `.` inserts the literal 'a', not a re-resolved 'b'
    assert_eq!(
        buf(&ws),
        "ax\naby\nacz",
        "`.` repeats the copied char literally"
    );
}
