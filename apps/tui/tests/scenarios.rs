//! Curated cross-feature SCENARIO suite (testing-and-benchmarks.md §1.9): hand-written realistic
//! multi-step editing sessions with EXACT expected end-states, driven through the same stack `main.rs`
//! composes (the headless session over `Session::on_key`, #895). Where the oracle corpus checks single ops and the fuzzer checks
//! invariants, these pin the KNOWN-TRICKY cross-feature interactions a modal editor must get right —
//! dot-repeat × count, named registers, visual + operator, search → change → repeat, substitute + undo,
//! insert-session undo grouping, blockwise-insert replication, and multi-window buffer sharing — each as
//! a readable regression guard.

use ruse_tui::headless::Headless;

/// Feed a Vim-notation keystroke script (`<Esc>`, `<CR>`, `<BS>`, `<C-x>`, …) through the session's real
/// key path — the headless harness over `Session::on_key` (#895): the macro layer, the session intercepts,
/// the input engine and the Ex dispatch, exactly as the binary routes each key.
fn feed_str(h: &mut Headless, s: &str) {
    h.keys(s);
}

fn buf(h: &Headless) -> String {
    h.text()
}

/// A fresh single-window session over `initial`. Search case stays Vim's factory default (these scenarios
/// were verified against `nvim -u NONE`), not ruse's shipped smartcase.
fn session(initial: &str) -> Headless {
    Headless::vim(initial).vanilla_search_case()
}

#[test]
fn insert_session_is_a_single_undo_group() {
    let mut h = session("");
    feed_str(&mut h, "ihello<Esc>");
    assert_eq!(buf(&h), "hello");
    feed_str(&mut h, "u");
    assert_eq!(buf(&h), "", "the whole insert session undoes as one group");
}

#[test]
fn dot_repeats_a_change_on_the_next_word() {
    let mut h = session("foo foo");
    feed_str(&mut h, "cwbar<Esc>");
    assert_eq!(buf(&h), "bar foo");
    feed_str(&mut h, "w.");
    assert_eq!(
        buf(&h),
        "bar bar",
        "`.` replays the change on the next word"
    );
}

#[test]
fn count_multiplies_a_dot_repeat() {
    let mut h = session("aaaaa");
    feed_str(&mut h, "x"); // delete one -> "aaaa"
    assert_eq!(buf(&h), "aaaa");
    feed_str(&mut h, "3."); // repeat `x` with a fresh count of 3
    assert_eq!(buf(&h), "a", "`3.` re-applies the last change three times");
}

#[test]
fn named_register_yank_then_paste() {
    let mut h = session("one\ntwo");
    feed_str(&mut h, "\"ayy"); // yank line 1 into register a
    feed_str(&mut h, "j\"ap"); // paste it after line 2
    assert_eq!(
        buf(&h),
        "one\ntwo\none",
        "named register round-trips through paste"
    );
}

#[test]
fn ex_put_pastes_a_yanked_line_linewise() {
    // Yank line 1 (charwise register content still puts as a whole line — the linewise-forcing rule is
    // exercised in the core tests; here the whole parse→dispatch→core path is driven from keystrokes).
    let mut h = session("alpha\nbeta\ngamma\n");
    feed_str(&mut h, "yy"); // yank line 1 (alpha) linewise into the unnamed register
    feed_str(&mut h, ":2put<CR>"); // put after line 2 (beta)
    assert_eq!(
        buf(&h),
        "alpha\nbeta\nalpha\ngamma\n",
        ":put opens the yanked line below the addressed line"
    );
}

#[test]
fn ex_zero_put_inserts_at_the_top() {
    let mut h = session("alpha\nbeta\n");
    feed_str(&mut h, "yy"); // yank alpha
    feed_str(&mut h, ":0put<CR>"); // put before line 1
    assert_eq!(
        buf(&h),
        "alpha\nalpha\nbeta\n",
        ":0put lands at the very top"
    );
}

#[test]
fn ex_read_file_inserts_below_addressed_line() {
    // Write a real temp file and read it in with `:2r {path}` — the whole parse→dispatch→core path, driven
    // from keystrokes exactly as a user types it. The cursor lands on the first inserted line (nvim quirk).
    let path = std::env::temp_dir().join(format!("ruse_read_it_{}.txt", std::process::id()));
    std::fs::write(&path, b"X1\nX2\n").expect("write temp file");

    let mut h = session("alpha\nbeta\ngamma\n");
    feed_str(&mut h, &format!(":2r {}<CR>", path.display()));
    assert_eq!(
        buf(&h),
        "alpha\nbeta\nX1\nX2\ngamma\n",
        ":2r inserts the file's lines below line 2"
    );
    // Cursor on the first inserted line (X1 = line index 2, byte offset 11).
    assert_eq!(h.cursor(), 11, "cursor on the first inserted line");

    std::fs::remove_file(&path).ok();
}

#[test]
fn ex_zero_read_inserts_at_the_top() {
    let path = std::env::temp_dir().join(format!("ruse_read0_it_{}.txt", std::process::id()));
    std::fs::write(&path, b"top1\ntop2\n").expect("write temp file");

    let mut h = session("alpha\nbeta\n");
    feed_str(&mut h, &format!(":0r {}<CR>", path.display()));
    assert_eq!(
        buf(&h),
        "top1\ntop2\nalpha\nbeta\n",
        ":0r lands the file at the very top"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn ex_filter_sorts_the_whole_file() {
    // `:%!sort` filters every line through the real `sort`. Skip gracefully if it is not on PATH.
    if ruse_tui::shell::capture("command -v sort").map_or(true, |s| s.text.trim().is_empty()) {
        eprintln!("skipping: `sort` not on PATH");
        return;
    }
    let mut h = session("gamma\nalpha\nbeta\n");
    feed_str(&mut h, ":%!sort<CR>");
    assert_eq!(
        buf(&h),
        "alpha\nbeta\ngamma\n",
        ":%!sort replaces the buffer with the sorted lines"
    );
}

#[test]
fn ex_filter_uppercases_a_line_range() {
    if ruse_tui::shell::capture("command -v tr").map_or(true, |s| s.text.trim().is_empty()) {
        eprintln!("skipping: `tr` not on PATH");
        return;
    }
    let mut h = session("one\ntwo\nthree\nfour\n");
    feed_str(&mut h, ":2,3!tr a-z A-Z<CR>");
    assert_eq!(
        buf(&h),
        "one\nTWO\nTHREE\nfour\n",
        ":2,3!tr filters only lines 2-3"
    );
}

#[test]
fn search_then_change_then_next_then_dot() {
    let mut h = session("x foo y foo");
    feed_str(&mut h, "/foo<CR>"); // to the first foo
    feed_str(&mut h, "cwZ<Esc>"); // -> "x Z y foo"
    assert_eq!(buf(&h), "x Z y foo");
    feed_str(&mut h, "n."); // next match, repeat the change
    assert_eq!(buf(&h), "x Z y Z", "search-next + dot-repeat compose");
}

#[test]
fn substitute_then_undo_restores() {
    let mut h = session("aba");
    feed_str(&mut h, ":s/a/X/g<CR>");
    assert_eq!(buf(&h), "XbX");
    feed_str(&mut h, "u");
    assert_eq!(buf(&h), "aba", "a substitute is one undo group");
}

#[test]
fn visual_operator_deletes_the_selection() {
    let mut h = session("hello world");
    feed_str(&mut h, "ved"); // v, e (to end of "hello"), d
    assert_eq!(
        buf(&h),
        " world",
        "visual `e` selects the word, `d` deletes it"
    );
}

#[test]
fn blockwise_insert_replicates_and_undoes_as_one_group() {
    let mut h = session("aa\nbb\ncc");
    feed_str(&mut h, "<C-v>jjIX<Esc>"); // block over col 0 of 3 rows, insert X
    assert_eq!(
        buf(&h),
        "Xaa\nXbb\nXcc",
        "block insert replicates down every row"
    );
    feed_str(&mut h, "u");
    assert_eq!(
        buf(&h),
        "aa\nbb\ncc",
        "the whole block insert undoes as one group"
    );
}

// Dot-repeat of a visual-block insert rebuilds the block (same width x height) at the caret and
// re-applies the same I/A/c with the same typed text. Each end-state below is the EXACT nvim v0.12.4
// output (headless `nvim_feedkeys`), so these pin ruse to nvim, not just to itself.

#[test]
fn dot_repeats_a_block_insert_i_with_height_preserved() {
    let mut h = session("aaaa\nbbbb\ncccc\ndddd");
    feed_str(&mut h, "<C-v>jIX<Esc>"); // 2-row block, insert X at col 0
    assert_eq!(buf(&h), "Xaaaa\nXbbbb\ncccc\ndddd");
    feed_str(&mut h, "jj."); // move 2 rows down, repeat: X on the next 2 rows
    assert_eq!(
        buf(&h),
        "Xaaaa\nXbbbb\nXcccc\nXdddd",
        "`.` re-applies the block-X over the SAME 2 rows at the new cursor (nvim)"
    );
}

#[test]
fn dot_repeats_a_block_insert_at_the_cursor_column() {
    // The rebuilt block's left edge is the CARET column, not the original block column.
    let mut h = session("aaaaaa\nbbbbbb\ncccccc\ndddddd");
    feed_str(&mut h, "<C-v>jIX<Esc>");
    feed_str(&mut h, "jjll."); // caret to row 2 col 2, then repeat
    assert_eq!(buf(&h), "Xaaaaaa\nXbbbbbb\nccXcccc\nddXdddd");
}

#[test]
fn dot_repeats_a_block_append_with_width_preserved() {
    // `A` appends at caret_col + width; width (3) is preserved from the original block.
    let mut h = session("aaaa\nbbbb\ncccc\ndddd");
    feed_str(&mut h, "<C-v>jllAX<Esc>"); // width-3 block, append X at col 3
    assert_eq!(buf(&h), "aaaXa\nbbbXb\ncccc\ndddd");
    feed_str(&mut h, "jj.");
    assert_eq!(buf(&h), "aaaXa\nbbbXb\ncccXc\ndddXd");
}

#[test]
fn dot_repeats_a_block_change_with_width_preserved() {
    // `c` deletes `width` chars from the caret column, then inserts the typed text.
    let mut h = session("aaaa\nbbbb\ncccc\ndddd");
    feed_str(&mut h, "<C-v>jlcXY<Esc>"); // width-2 block, delete 2, insert XY
    assert_eq!(buf(&h), "XYaa\nXYbb\ncccc\ndddd");
    feed_str(&mut h, "jj."); // caret rests at col 1 after the change; repeat there
    assert_eq!(buf(&h), "XYaa\nXYbb\ncXYc\ndXYd");
}

#[test]
fn a_block_change_backspaced_past_its_start_splits_no_char() {
    // #1149: `c`, two `<BS>` over the kept split-tab spaces, then `héh`: the insert start falls inside
    // `é`. nvim v0.12.6 copies the net 2 bytes from there and writes `\xa9h` (a split `é`) on the row
    // below; ruse copies from the next char, `h`. The `.` repeat replays the same keys and stays valid.
    let mut h = session("\t가\t\n가\n가\n");
    feed_str(&mut h, "jhb<C-v>hljc<BS><BS>héh<Esc>");
    assert_eq!(buf(&h), "héh\t\n가h\n가\n");
    feed_str(&mut h, "j0.");
    let ws = h.workspace();
    assert!(
        std::str::from_utf8(ws.focused().doc.bytes()).is_ok(),
        "{:?}",
        buf(&h)
    );
}

#[test]
fn dot_repeat_of_block_insert_ignores_the_count() {
    // nvim ignores `[count].` for a block insert: `2.` behaves exactly like `.` (one X, one row).
    let mut h = session("aaaa\nbbbb\ncccc");
    feed_str(&mut h, "<C-v>IX<Esc>"); // height-1 block
    feed_str(&mut h, "j2.");
    assert_eq!(
        buf(&h),
        "Xaaaa\nXbbbb\ncccc",
        "`2.` inserts a single X on one row — the count is ignored (nvim)"
    );
}

#[test]
fn dot_repeat_of_block_insert_clamps_rows_at_end_of_buffer() {
    let mut h = session("aaaa\nbbbb\ncccc\ndddd");
    feed_str(&mut h, "<C-v>jjIZ<Esc>"); // height-3 block
    feed_str(&mut h, "Gk."); // caret to row 2 (only 2 rows remain from there)
    assert_eq!(
        buf(&h),
        "Zaaaa\nZbbbb\nZZcccc\nZdddd",
        "the height-3 repeat clamps to the 2 available rows"
    );
}

#[test]
fn dot_repeats_a_ragged_dollar_block_append() {
    // `$A` appends at each row's OWN line-end; `.` keeps that ragged behaviour.
    let mut h = session("aa\nbbbb\nc\ndddd");
    feed_str(&mut h, "<C-v>j$AZ<Esc>"); // ragged append over rows 0..1
    assert_eq!(buf(&h), "aaZ\nbbbbZ\nc\ndddd");
    feed_str(&mut h, "jj.");
    assert_eq!(buf(&h), "aaZ\nbbbbZ\ncZ\nddddZ");
}

#[test]
fn split_windows_share_one_buffer_and_undo() {
    let mut h = session("hello");
    feed_str(&mut h, "<C-w>s"); // two windows onto the SAME buffer
    assert_eq!(h.workspace().window_count(), 2);
    feed_str(&mut h, "x"); // delete under cursor in the focused window
                           // Both panes borrow the one shared Document, so the edit is visible in BOTH.
    assert_eq!(
        String::from_utf8_lossy(h.workspace().pane(0).doc.bytes()),
        "ello"
    );
    assert_eq!(
        String::from_utf8_lossy(h.workspace().pane(1).doc.bytes()),
        "ello",
        "a split shares the buffer — the edit shows in both windows"
    );
    feed_str(&mut h, "u");
    assert_eq!(buf(&h), "hello", "undo restores the shared buffer");
}

/// Build a BRANCHED undo tree so chronological (`g-`/`g+`) traversal differs from a plain linear undo:
/// three separate insert sessions grow "" → "a" → "ba" → "cba" (nodes A,B,C — each `i` leaves the cursor
/// at column 0, so the next insert PREPENDS), then `g-g-` rewinds to "a" and a fourth insert forks a new
/// branch "Xa" (node D). The chronological creation order is root, A, B, C, D, so from D a single `g-`
/// lands on the chronologically-previous state "cba" — ACROSS the branch, exactly as Vim's `g-` does.
/// Deterministic, so two calls yield identical trees.
fn branched_history() -> Headless {
    let mut h = session("");
    feed_str(&mut h, "ia<Esc>"); // "a"   (node A)
    feed_str(&mut h, "ib<Esc>"); // "ba"  (node B — prepended)
    feed_str(&mut h, "ic<Esc>"); // "cba" (node C — prepended)
    feed_str(&mut h, "g-g-"); // rewind chronologically to "a"
    assert_eq!(
        buf(&h),
        "a",
        "precondition: two g- from \"cba\" reach \"a\""
    );
    feed_str(&mut h, "iX<Esc>"); // "Xa"  (node D — forks a new branch off A)
    assert_eq!(buf(&h), "Xa", "precondition: the fork produced \"Xa\"");
    h
}

#[test]
fn earlier_later_match_repeated_g_minus_g_plus() {
    // `:earlier {N}` must reach the SAME buffer state as N presses of `g-`, and `:later {N}` the same as
    // N presses of `g+` — the whole point of the ex commands. Drive both paths end-to-end through the real
    // InputEngine + Workspace over an identical branched tree, so this asserts the parse (`earlier N`),
    // the N-count, AND the branch-aware chronological walk all agree with the keystroke path.
    let mut h_ex = branched_history();
    let mut h_key = branched_history();

    // BACKWARD: `:earlier 3` == `g-` × 3. From "Xa": "Xa" → "cba" → "ba" → "a".
    feed_str(&mut h_ex, ":earlier 3<CR>");
    feed_str(&mut h_key, "g-g-g-");
    assert_eq!(
        buf(&h_ex),
        "a",
        "`:earlier 3` walks three chronological states back to \"a\""
    );
    assert_eq!(
        buf(&h_ex),
        buf(&h_key),
        "`:earlier 3` matches three presses of `g-`"
    );

    // FORWARD: `:later 2` == `g+` × 2. From "a": "a" → "ba" → "cba".
    feed_str(&mut h_ex, ":later 2<CR>");
    feed_str(&mut h_key, "g+g+");
    assert_eq!(
        buf(&h_ex),
        "cba",
        "`:later 2` walks two chronological states forward to \"cba\""
    );
    assert_eq!(
        buf(&h_ex),
        buf(&h_key),
        "`:later 2` matches two presses of `g+`"
    );

    // The `ea` / `lat` abbreviations drive the identical path.
    let mut h_ab = branched_history();
    feed_str(&mut h_ab, ":ea 3<CR>");
    assert_eq!(buf(&h_ab), "a", "`:ea` is `:earlier`");
    feed_str(&mut h_ab, ":lat 1<CR>");
    assert_eq!(buf(&h_ab), "ba", "`:lat` is `:later`");
}

#[test]
fn earlier_later_clamp_at_undo_tree_bounds() {
    // An over-large count must clamp at the ends of the chronological history and never panic — `:earlier`
    // past the root stops at the oldest state, `:later` past the tip stops at the newest.
    let mut h = branched_history(); // at "Xa", root + 4 states

    feed_str(&mut h, ":earlier 999<CR>");
    assert_eq!(
        buf(&h),
        "",
        "`:earlier` past the root clamps at the oldest state"
    );

    feed_str(&mut h, ":later 999<CR>");
    assert_eq!(
        buf(&h),
        "Xa",
        "`:later` past the tip clamps at the newest state"
    );

    // A bare `:earlier` (no count) is exactly one step, matching Vim's default of 1.
    feed_str(&mut h, ":earlier<CR>");
    assert_eq!(buf(&h), "cba", "bare `:earlier` steps one state back");
}

#[test]
fn ctrl_v_numeric_char_entry_end_to_end() {
    // `i_CTRL-V` drives the WHOLE stack (engine -> Command -> Workspace edit), not just the engine.
    // Decimal `065` -> A.
    let mut h = session("");
    feed_str(&mut h, "i<C-v>065<Esc>");
    assert_eq!(buf(&h), "A");

    // Hex byte, BMP unicode, and full unicode all land as the resolved UTF-8 char.
    let mut h = session("");
    feed_str(&mut h, "i<C-v>x41<C-v>u00e9<Esc>");
    assert_eq!(buf(&h), "Aé");

    // Early terminator: `9` is a valid decimal digit, `x` is not — so char 9 (a tab) is inserted AND
    // the `x` is processed as normal input. Both reach the Workspace via `Feed::Replay`.
    let mut h = session("");
    feed_str(&mut h, "i<C-v>9x<Esc>");
    assert_eq!(buf(&h), "\tx");
}

#[test]
fn ctrl_v_resolved_char_is_dot_repeatable() {
    // The resolved char is a plain `InsertChar`, so the insert session records it and `.` replays it.
    let mut h = session("");
    feed_str(&mut h, "i<C-v>065<Esc>"); // insert "A"
    assert_eq!(buf(&h), "A");
    feed_str(&mut h, "."); // repeat the whole insert session
    assert_eq!(buf(&h), "AA", "`.` replays the CTRL-V-resolved char");
}

// ---------------------------------------------------------------------------------------------------
// Count-on-insert repetition (VIM-CNT-INS, issue #470). A count before a pure insert-entry replays the
// typed text that many times on `<Esc>`. All expected bytes + caret offsets verified against nvim
// v0.12.4 (`nvim -u NONE` headless).
// ---------------------------------------------------------------------------------------------------

/// The focused window's caret as a byte offset — count-on-insert must land the caret exactly where Vim
/// leaves it (on the last inserted char after the `<Esc>` left-shift).
fn cur(h: &Headless) -> usize {
    h.cursor()
}

#[test]
fn count_insert_i_repeats_typed_text() {
    // nvim: `3ihello<Esc>` on an empty line -> "hellohellohello", caret on the last 'o' (col 14).
    let mut h = session("");
    feed_str(&mut h, "3ihello<Esc>");
    assert_eq!(buf(&h), "hellohellohello");
    assert_eq!(cur(&h), 14, "caret rests on the last inserted char");
}

#[test]
fn count_insert_a_appends_repeats() {
    // nvim: `3ahello<Esc>` on "abc" -> "ahellohellohellobc" (append after the char under the caret).
    let mut h = session("abc");
    feed_str(&mut h, "3ahello<Esc>");
    assert_eq!(buf(&h), "ahellohellohellobc");
}

#[test]
fn count_insert_cap_i_and_cap_a() {
    // nvim: `3Ifoo<Esc>` inserts at the first non-blank; `3Afoo<Esc>` appends at end-of-line.
    let mut h = session("  abc");
    feed_str(&mut h, "3Ifoo<Esc>");
    assert_eq!(buf(&h), "  foofoofooabc");

    let mut h = session("abc");
    feed_str(&mut h, "3Afoo<Esc>");
    assert_eq!(buf(&h), "abcfoofoofoo");
    assert_eq!(cur(&h), 11, "caret on the last appended char");
}

#[test]
fn count_insert_o_opens_new_lines_below() {
    // nvim: `3ox<Esc>` opens three new lines below, each containing the typed "x"; caret on the last.
    let mut h = session("abc\ndef");
    feed_str(&mut h, "3ox<Esc>");
    assert_eq!(buf(&h), "abc\nx\nx\nx\ndef");
    // "abc\nx\nx\nx\n" is 10 bytes; the last 'x' (line 4) sits at offset 8.
    assert_eq!(cur(&h), 8, "caret on the last opened line's char");
}

#[test]
fn count_insert_cap_o_opens_new_lines_above() {
    // nvim: `3Ox<Esc>` opens three new lines ABOVE, top-to-bottom, caret on the third (just above "abc").
    let mut h = session("abc\ndef");
    feed_str(&mut h, "3Ox<Esc>");
    assert_eq!(buf(&h), "x\nx\nx\nabc\ndef");
    assert_eq!(cur(&h), 4, "caret on the third (last-typed) opened line");
}

#[test]
fn count_insert_replays_resulting_text_after_backspace() {
    // nvim: `3ixy<BS>z<Esc>` on "abc" -> the RESULTING typed text is "xz" (xy, BS deletes y, then z),
    // replayed three times -> "xzxzxz".
    let mut h = session("abc");
    feed_str(&mut h, "3ixy<BS>z<Esc>");
    assert_eq!(buf(&h), "xzxzxzabc");
}

#[test]
fn count_insert_is_one_undo_group() {
    // The whole `3ihello` collapses into a SINGLE undo unit — one `u` removes all three copies.
    let mut h = session("");
    feed_str(&mut h, "3ihello<Esc>");
    assert_eq!(buf(&h), "hellohellohello");
    feed_str(&mut h, "u");
    assert_eq!(
        buf(&h),
        "",
        "a single `u` undoes the whole count-on-insert run"
    );

    // Same for the line-opening form: `3ox` (open + type, three times) is one undo group.
    let mut h = session("abc\ndef");
    feed_str(&mut h, "3ox<Esc>");
    assert_eq!(buf(&h), "abc\nx\nx\nx\ndef");
    feed_str(&mut h, "u");
    assert_eq!(buf(&h), "abc\ndef", "`3ox` undoes as one group");
}

#[test]
fn count_insert_dot_repeats_with_same_count() {
    // nvim: `3ix<Esc>` then `j0.` re-applies the WHOLE count-3 insert on the next line -> both "xxx…".
    let mut h = session("abc\ndef");
    feed_str(&mut h, "3ix<Esc>");
    assert_eq!(buf(&h), "xxxabc\ndef");
    feed_str(&mut h, "j0.");
    assert_eq!(
        buf(&h),
        "xxxabc\nxxxdef",
        "`.` repeats with the recorded count 3"
    );
}

#[test]
fn count_insert_dot_with_new_count_overrides() {
    // nvim: `3ix<Esc>` then `j02.` -> the leading `2.` overrides the recorded count on the next line.
    let mut h = session("abc\ndef");
    feed_str(&mut h, "3ix<Esc>");
    feed_str(&mut h, "j02.");
    assert_eq!(
        buf(&h),
        "xxxabc\nxxdef",
        "`2.` overrides the recorded count"
    );
}

#[test]
fn count_insert_o_is_dot_repeatable() {
    // nvim: `3ox<Esc>.` -> the dot repeats the whole `3o`, opening three MORE lines (six total).
    let mut h = session("abc\ndef");
    feed_str(&mut h, "3ox<Esc>");
    feed_str(&mut h, ".");
    assert_eq!(buf(&h), "abc\nx\nx\nx\nx\nx\nx\ndef");
}

#[test]
fn count_one_insert_is_unchanged() {
    // Regression: a count-less (implicit 1) insert behaves exactly as before — no replication, and the
    // Esc still leaves a single, normally-grouped insert session that `.` repeats once.
    let mut h = session("");
    feed_str(&mut h, "ihello<Esc>");
    assert_eq!(buf(&h), "hello");
    assert_eq!(cur(&h), 4);
    feed_str(&mut h, ".");
    assert_eq!(
        buf(&h),
        "hellhelloo",
        "`.` repeats the single insert once at the caret"
    );
}

// `i_CTRL-A` / `i_CTRL-@` (issue: insert previously-inserted text). Every expectation below was captured
// from nvim v0.12.4 via `nvim -u NONE` + `nvim_feedkeys`, so these pin Vim-faithful behavior.

#[test]
fn insert_ctrl_a_reinserts_last_insert_text() {
    // nvim: `ihello<Esc>A<C-a><Esc>` -> "hellohello" (C-A re-inserts the previous session's text).
    let mut h = session("");
    feed_str(&mut h, "ihello<Esc>");
    feed_str(&mut h, "A<C-a><Esc>");
    assert_eq!(buf(&h), "hellohello");
}

#[test]
fn insert_ctrl_a_replays_resulting_text_after_backspace() {
    // nvim: `ihel<BS>lo<Esc>` leaves "helo" (hel, BS deletes 'l', then "lo"); `A <C-a><Esc>` then appends
    // a space and re-inserts "helo" -> "helo helo". C-A replays the keystrokes (backspace and all).
    let mut h = session("");
    feed_str(&mut h, "ihel<BS>lo<Esc>");
    assert_eq!(buf(&h), "helo");
    feed_str(&mut h, "A <C-a><Esc>");
    assert_eq!(buf(&h), "helo helo");
}

#[test]
fn insert_ctrl_at_reinserts_then_leaves_insert() {
    // nvim: `ihello<Esc>A<C-@>x` -> C-@ inserts "hello" ("hellohello") AND leaves Insert, so the next `x`
    // is a Normal-mode delete of the last char -> "hellohell".
    let mut h = session("");
    feed_str(&mut h, "ihello<Esc>");
    feed_str(&mut h, "A<C-@>x");
    assert_eq!(buf(&h), "hellohell");
    assert_eq!(h.mode(), ruse_core::Mode::Normal);
}

#[test]
fn insert_ctrl_a_survives_a_non_insert_change() {
    // nvim: `ihello<Esc>0xA<C-a><Esc>` -> the intervening `x` (a non-insert change) does NOT clobber the
    // last-inserted text, so C-A still re-inserts "hello": "ello" + "hello" = "ellohello".
    let mut h = session("");
    feed_str(&mut h, "ihello<Esc>");
    feed_str(&mut h, "0xA<C-a><Esc>");
    assert_eq!(buf(&h), "ellohello");
}

#[test]
fn insert_ctrl_a_output_rolls_into_the_session_text() {
    // nvim: `ihello<Esc>iworld<C-a><Esc>` -> "hellworldhelloo" (world typed before 'o', then C-A inserts
    // "hello"); the session's last-inserted text is now "worldhello", so a further `A<C-a><Esc>` appends
    // "worldhello" -> "hellworldhellooworldhello".
    let mut h = session("");
    feed_str(&mut h, "ihello<Esc>");
    feed_str(&mut h, "iworld<C-a><Esc>");
    assert_eq!(buf(&h), "hellworldhelloo");
    feed_str(&mut h, "A<C-a><Esc>");
    assert_eq!(buf(&h), "hellworldhellooworldhello");
}

#[test]
fn insert_ctrl_a_with_no_previous_insert_is_a_noop() {
    // nvim: with no prior insert, `i<C-a>abc<Esc>` inserts only "abc" (C-A is a no-op — "E29: No inserted
    // text yet") and stays in Insert.
    let mut h = session("");
    feed_str(&mut h, "i<C-a>abc<Esc>");
    assert_eq!(buf(&h), "abc");
}

#[test]
fn insert_ctrl_at_with_no_previous_insert_still_leaves_insert() {
    // nvim: with no prior insert, `i<C-@>xy<Esc>` leaves the buffer EMPTY: C-@ inserts nothing but still
    // stops Insert, so `xy` runs in Normal mode (harmless on an empty line).
    let mut h = session("");
    feed_str(&mut h, "i<C-@>xy<Esc>");
    assert_eq!(buf(&h), "");
    assert_eq!(h.mode(), ruse_core::Mode::Normal);
}

#[test]
fn count_change_family_does_not_repeat_text() {
    // Regression: `c` count applies to the MOTION, never to text repetition. `3cwX<Esc>` changes three
    // words into a single "X" (nvim), NOT "XXX".
    let mut h = session("aa bb cc dd");
    feed_str(&mut h, "3cwX<Esc>");
    assert_eq!(buf(&h), "X dd", "`3cw` changes three words to one X");
}

/// Bare `&` repeats the last `:s` on the CURRENT LINE and KEEPS the previous flags: ruse's `&` is `:&&`, a
/// deliberate break from Vim (whose `&` drops them, so `:s/a/X/g` then `&` replaces only the first `a`); see
/// the `&` intercept in `app/session.rs`. The scenario used to assert Vim's flag drop through a hand mirror
/// of the session that no longer matched it; the real key path keeps the `g`.
#[test]
fn ampersand_repeats_last_substitute_on_current_line_keeping_flags() {
    let mut h = session("aaa aaa\nbbb aaa aaa\nccc aaa aaa\n");
    // A GLOBAL substitute on line 1 (all matches), then move to line 2 and press `&`.
    feed_str(&mut h, ":s/a/X/g<CR>");
    feed_str(&mut h, "j0&");
    assert_eq!(
        buf(&h),
        "XXX XXX\nbbb XXX XXX\nccc aaa aaa\n",
        "`&` keeps the stored `g` on line 2 only"
    );
}

/// `&` repeats a flag-less `:s` faithfully too (first match on the current line), and repeating it after
/// moving the cursor acts on the NEW line — matching nvim.
#[test]
fn ampersand_repeats_flagless_substitute_on_each_current_line() {
    let mut h = session("aaa\nbbb aaa\nccc aaa\n");
    feed_str(&mut h, ":s/a/X/<CR>"); // line 1: first `a`
    feed_str(&mut h, "j0&"); // line 2: first `a`
    feed_str(&mut h, "j0&"); // line 3: first `a`
    assert_eq!(
        buf(&h),
        "Xaa\nbbb Xaa\nccc Xaa\n",
        "`&` repeats the current-line, first-match substitute on each line it visits"
    );
}

/// `&` with NO previous `:s` is a safe no-op (nvim errors E33; ruse just does nothing to the buffer).
#[test]
fn ampersand_with_no_previous_substitute_is_a_noop() {
    let mut h = session("aaa aaa\n");
    feed_str(&mut h, "&");
    assert_eq!(
        buf(&h),
        "aaa aaa\n",
        "no prior `:s` — the buffer is untouched"
    );
}

/// Contrast guard: `g&` KEEPS the flags and runs over the WHOLE FILE, so the two forms stay distinct.
#[test]
fn g_ampersand_keeps_flags_over_the_whole_file() {
    let mut h = session("aaa aaa\nbbb aaa\n");
    feed_str(&mut h, ":s/a/X/g<CR>"); // record a global `:s`
    feed_str(&mut h, "g&"); // repeat globally, flags kept
    assert_eq!(
        buf(&h),
        "XXX XXX\nbbb XXX\n",
        "`g&` keeps the `g` and covers every line"
    );
}

// --- Ex repeat-substitute forms: bare `:s`, `:s {flags}`, `:&`, `:&&` (verified vs nvim v0.12.4) ---

/// Bare `:s` (no delimiter) repeats the last `:s` on the current line and DROPS the previous flags — like
/// normal-mode `&`. nvim: `:s/aaa/X/g` on line 1, then bare `:s` on line 2 replaces ONLY the first `aaa`.
#[test]
fn ex_bare_s_repeats_last_substitute_dropping_flags() {
    let mut h = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str(&mut h, ":s/aaa/X/g<CR>"); // line 1: all matches (g)
    feed_str(&mut h, "j0:s<CR>"); // line 2: bare `:s`, flags dropped
    assert_eq!(
        buf(&h),
        "X X X\nbbb X aaa aaa\n",
        "bare `:s` drops the stored `g`: only the FIRST `aaa` on line 2 is replaced"
    );
}

/// `:&` behaves exactly like bare `:s`: repeat on the current line, flags dropped.
#[test]
fn ex_ampersand_repeats_last_substitute_dropping_flags() {
    let mut h = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str(&mut h, ":s/aaa/X/g<CR>");
    feed_str(&mut h, "j0:&<CR>");
    assert_eq!(
        buf(&h),
        "X X X\nbbb X aaa aaa\n",
        "`:&` is bare `:s`: only the FIRST `aaa` on line 2 is replaced"
    );
}

/// `:&&` KEEPS the previous flags. nvim: `:s/aaa/X/g` then `:&&` on line 2 replaces ALL `aaa` (the `g` sticks).
#[test]
fn ex_double_ampersand_repeats_last_substitute_keeping_flags() {
    let mut h = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str(&mut h, ":s/aaa/X/g<CR>");
    feed_str(&mut h, "j0:&&<CR>");
    assert_eq!(
        buf(&h),
        "X X X\nbbb X X X\n",
        "`:&&` keeps the `g`: EVERY `aaa` on line 2 is replaced"
    );
}

/// `:s {flags}` (bare `s` followed by only flags) repeats with the GIVEN flags replacing the old ones. nvim:
/// `:s/aaa/X/` (no g) then `:s g` on line 2 applies `g`, replacing every `aaa`.
#[test]
fn ex_s_with_only_flags_uses_the_given_flags() {
    let mut h = session("aaa aaa aaa\nbbb aaa aaa aaa\n");
    feed_str(&mut h, ":s/aaa/X/<CR>"); // line 1: first match only (no g)
    feed_str(&mut h, "j0:s g<CR>"); // line 2: add g → all matches
    assert_eq!(
        buf(&h),
        "X aaa aaa\nbbb X X X\n",
        "`:s g` applies the given `g` flag: EVERY `aaa` on line 2 is replaced"
    );
}

/// `:[range]&&` repeats over a range, keeping flags. nvim: `:1s/aaa/X/g` then `:2,3&&` replaces all `aaa` on
/// lines 2 and 3.
#[test]
fn ex_double_ampersand_over_a_range_keeps_flags() {
    let mut h = session("aaa aaa\nbbb aaa aaa\nccc aaa aaa\nddd aaa aaa\n");
    feed_str(&mut h, ":1s/aaa/X/g<CR>");
    feed_str(&mut h, ":2,3&&<CR>");
    assert_eq!(
        buf(&h),
        "X X\nbbb X X\nccc X X\nddd aaa aaa\n",
        "`:2,3&&` repeats over the range keeping `g`; line 4 is untouched"
    );
}

/// A repeat form with NO previous `:s` is a safe no-op (nvim errors E33; ruse leaves the buffer untouched).
#[test]
fn ex_repeat_substitute_with_no_previous_substitute_is_a_noop() {
    let mut h = session("aaa aaa\n");
    feed_str(&mut h, ":s<CR>");
    feed_str(&mut h, ":&<CR>");
    feed_str(&mut h, ":&&<CR>");
    assert_eq!(
        buf(&h),
        "aaa aaa\n",
        "no prior `:s` — every repeat form leaves the buffer untouched"
    );
}

// --- i_CTRL-E / i_CTRL-Y: insert the char BELOW / ABOVE the caret (verified vs nvim v0.12.4) ---

/// `i_CTRL-E` at column 0 of the top line inserts the character directly below (same column).
#[test]
fn insert_ctrl_e_copies_char_below() {
    let mut h = session("hello\nworld");
    feed_str(&mut h, "i<C-e>");
    assert_eq!(buf(&h), "whello\nworld", "`C-e` inserts 'w' (below col 0)");
}

/// `i_CTRL-Y` at column 0 of the bottom line inserts the character directly above (same column).
#[test]
fn insert_ctrl_y_copies_char_above() {
    let mut h = session("hello\nworld");
    feed_str(&mut h, "ji<C-y>");
    assert_eq!(buf(&h), "hello\nhworld", "`C-y` inserts 'h' (above col 0)");
}

/// The caret advances after each insert, so a run of `C-Y` copies successive columns from the line above.
#[test]
fn insert_ctrl_y_run_copies_successive_columns() {
    let mut h = session("hello\nworld");
    feed_str(&mut h, "ji<C-y><C-y><C-y>");
    assert_eq!(buf(&h), "hello\nhelworld", "three `C-y` copy 'h','e','l'");
}

/// `i_CTRL-E` uses the caret's column, not column 0: from mid-line it copies the char below THAT column.
#[test]
fn insert_ctrl_e_uses_caret_column() {
    let mut h = session("hello\nworld");
    feed_str(&mut h, "lli<C-e>"); // caret at col 2 (on 'l')
    assert_eq!(buf(&h), "herllo\nworld", "col 2 below 'world' is 'r'");
}

/// No character at that column on the adjacent line (line too short) → a no-op (Vim's bell case).
#[test]
fn insert_ctrl_e_short_line_below_is_noop() {
    let mut h = session("hello\nab");
    feed_str(&mut h, "$i<C-e>"); // caret at col 4 ('o'); 'ab' has no col 4
    assert_eq!(
        buf(&h),
        "hello\nab",
        "no char below col 4 → nothing inserted"
    );
}

/// No line below the current line → a no-op.
#[test]
fn insert_ctrl_e_no_line_below_is_noop() {
    let mut h = session("hello");
    feed_str(&mut h, "i<C-e>");
    assert_eq!(buf(&h), "hello", "no line below → nothing inserted");
}

/// No line above the current line → a no-op.
#[test]
fn insert_ctrl_y_no_line_above_is_noop() {
    let mut h = session("hello\nworld");
    feed_str(&mut h, "i<C-y>");
    assert_eq!(buf(&h), "hello\nworld", "no line above → nothing inserted");
}

/// Dot-repeat replays the RESOLVED LITERAL char (matching nvim), not a re-resolution against the new line.
/// On line 2, `C-Y` copies 'a' from line 1; `.` on line 3 inserts that same 'a', NOT line 2's 'b'.
#[test]
fn insert_ctrl_y_dot_repeats_the_literal_char() {
    let mut h = session("ax\nby\ncz");
    feed_str(&mut h, "ji<C-y><Esc>"); // line 2 -> "aby"
    assert_eq!(buf(&h), "ax\naby\ncz");
    feed_str(&mut h, "j0."); // line 3: `.` inserts the literal 'a', not a re-resolved 'b'
    assert_eq!(
        buf(&h),
        "ax\naby\nacz",
        "`.` repeats the copied char literally"
    );
}

// --- i_CTRL-N / i_CTRL-P: current-buffer keyword completion (verified vs nvim v0.12.4, issue #502) ---

/// `i_CTRL-N` completes the word before the caret to the first buffer keyword that starts with it, and
/// repeated `C-N` cycles forward through the candidate set, then the ORIGINAL typed text, then wraps.
/// nvim ground truth for base "fo" on a fresh last line: foo, food, foobar, <orig "fo">, foo (the two
/// "foo" occurrences dedupe to one). Candidate order = forward-from-cursor then wrap-from-top.
#[test]
fn insert_ctrl_n_completes_and_cycles_forward() {
    let mut h = session("foo food\nfoobar\nfoo");
    feed_str(&mut h, "Gofo"); // open a fresh last line, type the base "fo"
    assert_eq!(buf(&h), "foo food\nfoobar\nfoo\nfo");
    feed_str(&mut h, "<C-n>");
    assert_eq!(buf(&h), "foo food\nfoobar\nfoo\nfoo", "first C-N → 'foo'");
    feed_str(&mut h, "<C-n>");
    assert_eq!(buf(&h), "foo food\nfoobar\nfoo\nfood", "next C-N → 'food'");
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foo food\nfoobar\nfoo\nfoobar",
        "next C-N → 'foobar'"
    );
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foo food\nfoobar\nfoo\nfo",
        "next C-N → back to original"
    );
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foo food\nfoobar\nfoo\nfoo",
        "next C-N wraps → 'foo'"
    );
}

/// `i_CTRL-P` walks the SAME candidate list backward: from the original the first `C-P` selects the LAST
/// candidate. nvim: base "fo" → C-P → foobar, food, foo, <orig "fo">, foobar (wrap).
#[test]
fn insert_ctrl_p_cycles_backward() {
    let mut h = session("foo food\nfoobar\nfoo");
    feed_str(&mut h, "Gofo");
    feed_str(&mut h, "<C-p>");
    assert_eq!(
        buf(&h),
        "foo food\nfoobar\nfoo\nfoobar",
        "first C-P → last candidate"
    );
    feed_str(&mut h, "<C-p>");
    assert_eq!(buf(&h), "foo food\nfoobar\nfoo\nfood", "next C-P → 'food'");
    feed_str(&mut h, "<C-p>");
    assert_eq!(buf(&h), "foo food\nfoobar\nfoo\nfoo", "next C-P → 'foo'");
    feed_str(&mut h, "<C-p>");
    assert_eq!(
        buf(&h),
        "foo food\nfoobar\nfoo\nfo",
        "next C-P → back to original"
    );
}

/// The candidate SCAN ORDER from a mid-buffer caret: forward to end-of-buffer, then wrap from the top —
/// and the word under construction is excluded. nvim for base "aa" on the middle line: aa4, aa5, aa6,
/// aa1, aa2, aa3 (the middle line's own "aa" is not a candidate).
#[test]
fn insert_ctrl_n_scan_order_forward_then_wrap() {
    let mut h = session("aa1 aa2\naa3\nXXX\naa4 aa5\naa6");
    feed_str(&mut h, "jjccaa"); // line 3: change line, type base "aa"
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "aa1 aa2\naa3\naa4\naa4 aa5\naa6",
        "first C-N → 'aa4' (forward)"
    );
    feed_str(&mut h, "<C-n><C-n>"); // aa5, aa6
    assert_eq!(
        buf(&h),
        "aa1 aa2\naa3\naa6\naa4 aa5\naa6",
        "third C-N → 'aa6' (end of forward)"
    );
    feed_str(&mut h, "<C-n>"); // wrap to top → aa1
    assert_eq!(
        buf(&h),
        "aa1 aa2\naa3\naa1\naa4 aa5\naa6",
        "fourth C-N wraps → 'aa1'"
    );
}

/// A base with no matching keyword is a no-op (Vim bells; the buffer is untouched and the caret stays).
#[test]
fn insert_ctrl_n_no_match_is_noop() {
    let mut h = session("foo bar\nbaz");
    feed_str(&mut h, "Gozz"); // base "zz" — no keyword starts with it
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foo bar\nbaz\nzz",
        "no candidate → nothing changes"
    );
    feed_str(&mut h, "x<Esc>"); // still in Insert: typing continues normally
    assert_eq!(
        buf(&h),
        "foo bar\nbaz\nzzx",
        "completion was inert, insert continues"
    );
}

/// The EXACT-match base is itself a candidate (distinct from the ORIGINAL cycle stop): base "foo" with a
/// longer "foobar" present cycles foo → foobar (nvim: "match 1 of 2" then "match 2 of 2").
#[test]
fn insert_ctrl_n_includes_exact_match() {
    let mut h = session("foo foobar");
    feed_str(&mut h, "ofoo"); // new line, base "foo"
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foo foobar\nfoo",
        "first C-N → 'foo' (exact match candidate)"
    );
    feed_str(&mut h, "<C-n>");
    assert_eq!(buf(&h), "foo foobar\nfoobar", "next C-N → 'foobar'");
}

/// Any other key ACCEPTS the current candidate and ends completion; the accepted text stays and typing
/// resumes normally. nvim: `fo` + C-N → "foobar", then "!" → "foobar!".
#[test]
fn insert_ctrl_n_accept_on_other_key() {
    let mut h = session("foobar foobaz");
    feed_str(&mut h, "ofo"); // base "fo"
    feed_str(&mut h, "<C-n>!<Esc>"); // complete → foobar, then '!' accepts + appends
    assert_eq!(
        buf(&h),
        "foobar foobaz\nfoobar!",
        "other key accepts the candidate"
    );
}

/// Dot-repeat replays the LITERAL accepted text (matches nvim): `fo` + C-N accepts "foobar"; `.` on a
/// fresh line re-inserts "foobar", NOT a re-resolution against the new context.
#[test]
fn insert_ctrl_n_dot_repeats_literal_text() {
    let mut h = session("foobar foobaz");
    feed_str(&mut h, "ofo<C-n><Esc>"); // line 2 -> "foobar" (last change = open+type+complete)
    assert_eq!(buf(&h), "foobar foobaz\nfoobar");
    feed_str(&mut h, "."); // repeat: open a line, type "fo", apply the recorded completion
    assert_eq!(
        buf(&h),
        "foobar foobaz\nfoobar\nfoobar",
        "`.` replays the accepted completion text literally"
    );
}

// --- i_CTRL-X CTRL-L: current-buffer WHOLE-LINE completion (verified vs nvim v0.12.4) ---

/// `i_CTRL-X CTRL-L` completes the current line to the first buffer line whose post-indent start matches
/// the typed prefix; a bare `CTRL-L` then cycles forward to the next matching line, then back to the
/// ORIGINAL typed text. Candidate order is Vim's backward-from-caret-with-wrap, deduped. nvim ground truth
/// for prefix "foobar" on a fresh last line: foobar baz, foobar qux, <orig "foobar"> (the two identical
/// "foobar baz" lines dedupe to one).
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_completes_and_cycles_lines() {
    let mut h = session("foobar baz\nfoobar qux\nfoobar baz\nhello world");
    feed_str(&mut h, "Gofoobar"); // open a fresh last line, type the prefix "foobar"
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar"
    );
    feed_str(&mut h, "<C-x><C-l>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar baz",
        "C-x C-l → first matching line"
    );
    feed_str(&mut h, "<C-l>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar qux",
        "bare C-l → next matching line"
    );
    feed_str(&mut h, "<C-l>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar",
        "next C-l → back to the original typed text"
    );
    feed_str(&mut h, "<C-l>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar baz",
        "next C-l wraps → first line again"
    );
}

/// `CTRL-N` / `CTRL-P` cycle an ACTIVE whole-line completion just like the keyword cycle: from the first
/// selected line `C-N` advances and `C-P` retreats, through the same `[candidates…, ORIGINAL]` stops.
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_cycles_with_ctrl_n_and_ctrl_p() {
    let mut h = session("foobar baz\nfoobar qux\nfoobar baz\nhello world");
    feed_str(&mut h, "Gofoobar<C-x><C-l>"); // → "foobar baz" (first candidate)
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar qux",
        "C-n advances to the next line"
    );
    feed_str(&mut h, "<C-n>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar",
        "C-n reaches the original text"
    );
    feed_str(&mut h, "<C-p>");
    assert_eq!(
        buf(&h),
        "foobar baz\nfoobar qux\nfoobar baz\nhello world\nfoobar qux",
        "C-p retreats to the previous line"
    );
}

/// Leading indent is IGNORED for matching AND the current line KEEPS its own indent: prefix "  in" (typed
/// with two leading spaces) matches a 4-space-indented "    indented alpha" and the result is
/// "  indented alpha" — the typed indent preserved, the candidate's own indent dropped (nvim v0.12.4).
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_ignores_indent_but_keeps_current() {
    let mut h = session("    indented alpha\nplain beta");
    feed_str(&mut h, "Go  in"); // fresh line, two spaces + "in"
    feed_str(&mut h, "<C-x><C-l>");
    assert_eq!(
        buf(&h),
        "    indented alpha\nplain beta\n  indented alpha",
        "indent ignored for match; current line's own indent preserved"
    );
}

/// Candidate lines are deduped by their POST-INDENT content, so lines differing only in indent collapse:
/// "foo bar" and "    foo bar" offer ONE "foo bar" candidate. Order is backward-from-caret, so "foo baz"
/// (just above) comes first.
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_dedupes_by_content() {
    let mut h = session("foo bar\n    foo bar\nfoo baz");
    feed_str(&mut h, "Gofoo"); // new last line, prefix "foo"
    feed_str(&mut h, "<C-x><C-l>");
    assert_eq!(
        buf(&h),
        "foo bar\n    foo bar\nfoo baz\nfoo baz",
        "first candidate is the nearest line above (backward scan)"
    );
    feed_str(&mut h, "<C-l>");
    assert_eq!(
        buf(&h),
        "foo bar\n    foo bar\nfoo baz\nfoo bar",
        "next candidate is 'foo bar' (both indented/plain copies dedupe to one)"
    );
    feed_str(&mut h, "<C-l>");
    assert_eq!(
        buf(&h),
        "foo bar\n    foo bar\nfoo baz\nfoo",
        "only two candidates → next C-l returns to the original"
    );
}

/// A prefix no line starts with is a no-op (Vim bells; the buffer is untouched and Insert continues).
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_no_match_is_noop() {
    let mut h = session("foo bar\nbaz");
    feed_str(&mut h, "Gozz"); // prefix "zz" — no line starts with it
    feed_str(&mut h, "<C-x><C-l>");
    assert_eq!(
        buf(&h),
        "foo bar\nbaz\nzz",
        "no candidate → nothing changes"
    );
    feed_str(&mut h, "x<Esc>"); // still in Insert: typing continues normally
    assert_eq!(
        buf(&h),
        "foo bar\nbaz\nzzx",
        "completion was inert, insert continues"
    );
}

/// Any other key ACCEPTS the current line and ends completion; typing then resumes normally.
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_accept_on_other_key() {
    let mut h = session("hello world");
    feed_str(&mut h, "ohel<C-x><C-l>!<Esc>"); // complete → "hello world", then '!' appends
    assert_eq!(
        buf(&h),
        "hello world\nhello world!",
        "other key accepts the completed line"
    );
}

/// A `CTRL-X` submode armed then cancelled by an out-of-scope key does NOT complete; the deferred
/// `CTRL-X CTRL-N` (local keyword) is such a cancel here — the buffer is untouched and Insert continues.
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_then_unhandled_key_cancels_submode() {
    let mut h = session("hello world");
    feed_str(&mut h, "ohel<C-x><C-n>"); // C-x C-n is out of scope → cancels, no completion
    assert_eq!(
        buf(&h),
        "hello world\nhel",
        "C-x C-n cancels; no whole-line completion"
    );
    feed_str(&mut h, "p<Esc>");
    assert_eq!(
        buf(&h),
        "hello world\nhelp",
        "insert continues after the cancelled submode"
    );
}

/// Dot-repeat replays the LITERAL completed line (matches nvim): `hel` + C-x C-l accepts "hello world";
/// `.` on a fresh line re-inserts it, NOT a re-resolution against the new context.
#[test]
#[ignore = "#602: the LSP coordinator swallows CTRL-X in Insert, so CTRL-X CTRL-L never reaches the engine"]
fn insert_ctrl_x_ctrl_l_dot_repeats_literal_line() {
    let mut h = session("hello world");
    feed_str(&mut h, "ohel<C-x><C-l><Esc>");
    assert_eq!(buf(&h), "hello world\nhello world");
    feed_str(&mut h, ".");
    assert_eq!(
        buf(&h),
        "hello world\nhello world\nhello world",
        "`.` replays the accepted completion line literally"
    );
}

// ---- Read-only special registers "/ ": ". "%  (issue #487) ----

/// `"/p` pastes the last search pattern, and `C-r /` inserts it in Insert — both resolved through the
/// frontend sync mirrored in `sync_special` (nvim: after `/bar`, `getreg("/")` == "bar").
#[test]
fn special_slash_register_pastes_and_inserts_last_search() {
    let mut h = session("bar baz\n");
    feed_str(&mut h, "/bar<CR>"); // records "/ = "bar"
    feed_str(&mut h, "gg0"); // back to line start
    feed_str(&mut h, "\"/p"); // paste "/ after the caret → "bbarar baz"
    assert_eq!(
        buf(&h),
        "bbarar baz\n",
        "\"/p pastes the last search pattern"
    );

    // `C-r /` in Insert splices the same pattern at the caret.
    let mut h = session("x\n");
    feed_str(&mut h, "/foo<CR>");
    feed_str(&mut h, "gg0i<C-r>/<Esc>");
    assert_eq!(buf(&h), "foox\n", "<C-r>/ inserts the last search pattern");
}

/// `".p` / `C-r .` reproduce the last inserted text (Vim's `".`, matching an `i_CTRL-A` replay).
#[test]
fn special_dot_register_repeats_last_inserted_text() {
    let mut h = session("\n");
    feed_str(&mut h, "ihello<Esc>"); // ". = "hello"; caret rests on the final 'o'
    feed_str(&mut h, "\".p"); // paste it after the caret → "hellohello"
    assert_eq!(
        buf(&h),
        "hellohello\n",
        "\".p pastes the last inserted text"
    );

    let mut h = session("\n");
    feed_str(&mut h, "iAB<Esc>");
    feed_str(&mut h, "o<C-r>.<Esc>"); // open a line, insert ". via C-r .
    assert_eq!(
        buf(&h),
        "AB\nAB\n",
        "<C-r>. re-inserts the last inserted text"
    );
}

/// `"/` is READ-ONLY: a yank NAMING it is swallowed, so a following `"/p` still pastes the search pattern,
/// not the yanked line (nvim: `"/yy` leaves `@/` unchanged).
#[test]
fn special_slash_register_is_read_only() {
    let mut h = session("bar\nZZZ\n");
    feed_str(&mut h, "/bar<CR>"); // "/ = "bar"
    feed_str(&mut h, "j\"/yy"); // try to yank line 2 ("ZZZ") into "/ — a no-op
    feed_str(&mut h, "gg0\"/p"); // "/p still pastes "bar"
    assert_eq!(
        buf(&h),
        "bbarar\nZZZ\n",
        "yank into \"/ is swallowed; \"/p pastes the intact pattern"
    );
}

/// `":` (last Ex line) and `"%` (file name) resolve from session state: the session records every executed
/// Ex line and syncs both registers before dispatch. `C-r :` inserts the last Ex line, `"%p` pastes the
/// focused buffer's file name as it was opened, and `"%` is empty for an unnamed buffer.
#[test]
fn special_colon_and_percent_registers_resolve_and_empty_when_unnamed() {
    // `C-r :` inserts the last Ex line in Insert.
    let mut h = session("x\n");
    feed_str(&mut h, ":noh<CR>A<C-r>:<Esc>");
    assert_eq!(buf(&h), "xnoh\n", "<C-r>: inserts the last Ex line");

    // `"%p` pastes the file name after the caret.
    let dir = std::env::temp_dir().join(format!("ruse-scenarios-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("percent.txt");
    std::fs::write(&file, "x\n").unwrap();
    let name = file.display().to_string();
    let mut h = session("");
    feed_str(&mut h, &format!(":e {name}<CR>\"%p"));
    assert_eq!(buf(&h), format!("x{name}\n"), "\"%p pastes the file name");
    std::fs::remove_dir_all(&dir).unwrap();

    // Unnamed buffer: "% is empty, so "%p is a no-op.
    let mut h = session("x\n");
    feed_str(&mut h, "\"%p");
    assert_eq!(buf(&h), "x\n", "\"% is empty for an unnamed buffer");
}

// --- Neovim `Q` = repeat the last RECORDED macro (issue: feat(vim) Q) -------------------------------
// These drive the FULL macro stack end to end through the session — the `keys::MacroState` record/replay
// machine, the input engine and the register I/O — so a real `Q` mutates a real buffer (not just the state
// machine), and `3Q` exercises the same path as `3@a`.

#[test]
fn at_colon_repeats_last_ex_substitute_on_the_current_line() {
    // `:s/a/X/` on line 1, then `@:` on line 2 re-runs the same substitute there (verified vs nvim v0.12.4).
    let mut h = session("aaa\naaa\naaa");
    feed_str(&mut h, ":s/a/X/<CR>");
    feed_str(&mut h, "j0@:");
    assert_eq!(
        buf(&h),
        "Xaa\nXaa\naaa",
        "`@:` re-ran `:s/a/X/` on line 2 (first `a` only)"
    );
}

#[test]
fn count_at_colon_repeats_the_ex_line_n_times() {
    // `2@:` runs the stored `:s/a/X/` twice on the current line (two matches → two X's). nvim v0.12.4.
    let mut h = session("aaa\naaaa");
    feed_str(&mut h, ":s/a/X/<CR>"); // line 1: Xaa
    feed_str(&mut h, "j02@:"); // line 2: two substitutes
    assert_eq!(
        buf(&h),
        "Xaa\nXXaa",
        "`2@:` repeated the substitute twice on line 2"
    );
}

#[test]
fn at_at_after_at_colon_repeats_the_ex_line() {
    // After `@:`, a bare `@@` repeats the Ex line too (not a register) — matches nvim v0.12.4.
    let mut h = session("aaa\naaa\naaa");
    feed_str(&mut h, ":s/a/X/<CR>"); // line 1
    feed_str(&mut h, "j0@:"); // line 2 via @:
    feed_str(&mut h, "j0@@"); // line 3 via @@
    assert_eq!(
        buf(&h),
        "Xaa\nXaa\nXaa",
        "`@@` after `@:` repeated the Ex substitute on line 3"
    );
}

#[test]
fn at_colon_with_no_previous_ex_line_is_a_noop() {
    // No Ex command has run yet: `@:` changes nothing (nvim reports E30; edit-wise a no-op).
    let mut h = session("aaa\nbbb");
    feed_str(&mut h, "@:");
    assert_eq!(buf(&h), "aaa\nbbb", "`@:` before any Ex line is a no-op");
}

#[test]
fn q_replays_the_last_recorded_macro_end_to_end() {
    let mut h = session("aaa\nbbb\nccc\nddd");
    // Record into `a`: append '!' at EOL, then move to the next line. `qa A ! <Esc> j q`.
    feed_str(&mut h, "qaA!<Esc>jq");
    assert_eq!(
        buf(&h),
        "aaa!\nbbb\nccc\nddd",
        "recording `a` mutated line 1"
    );
    // `Q` replays register `a` on line 2, then advances to line 3.
    feed_str(&mut h, "Q");
    assert_eq!(
        buf(&h),
        "aaa!\nbbb!\nccc\nddd",
        "Q replayed the last recorded macro (a)"
    );
}

#[test]
fn count_q_replays_the_last_recorded_macro_n_times() {
    let mut h = session("a\nb\nc\nd\ne");
    feed_str(&mut h, "qzA!<Esc>jq"); // record `z`: append '!' then down; line 1 done
    assert_eq!(buf(&h), "a!\nb\nc\nd\ne");
    feed_str(&mut h, "3Q"); // `3Q` runs it on lines 2,3,4
    assert_eq!(
        buf(&h),
        "a!\nb!\nc!\nd!\ne",
        "`3Q` repeated the last recorded macro three times"
    );
}

#[test]
fn q_uses_the_most_recently_recorded_register() {
    let mut h = session("aaa\nbbb\nccc\nddd");
    feed_str(&mut h, "qaA1<Esc>jq"); // record `a` (append '1'); line 1 -> aaa1
    feed_str(&mut h, "qbA2<Esc>jq"); // record `b` (append '2'); line 2 -> bbb2
    assert_eq!(buf(&h), "aaa1\nbbb2\nccc\nddd");
    // Recording `b` LAST makes `Q` play `b` (append '2') on line 3, not `a`.
    feed_str(&mut h, "Q");
    assert_eq!(
        buf(&h),
        "aaa1\nbbb2\nccc2\nddd",
        "Q follows the most recently recorded register (b)"
    );
}

/// #680: a `q` typed as TEXT or as an argument does not stop a recording — only a `q` at a clean Normal or
/// Visual base does. Every expectation is nvim v0.12.6 (`nvim_feedkeys(…, 'xt')`, typed keys).
#[test]
fn q_stops_a_recording_only_as_a_normal_or_visual_command() {
    let cases: &[(&str, &str, &str)] = &[
        // Insert: `q` is text; the recording ends at the Normal `q`, and `@a` replays `iq1<Esc>`.
        ("x", "qaiquick<Esc>q", "quickx"),
        ("x", "qaiq1<Esc>q@a", "qq11x"),
        // The command line: `/q<CR>` searches for `q`.
        ("ab q def", "qa/q<CR>xq", "ab  def"),
        // A char argument (`f`, `m`) and an operator-pending `q` (`dq` aborts the `d`, as nvim beeps).
        ("ab q def q", "qafqxq", "ab  def q"),
        ("abc", "qamqxq", "bc"),
        ("abc", "qadqxq", "bc"),
        // A bare count does not shield it: `2q` stops, then `x` runs outside the recording.
        ("abc", "qa2qx", "bc"),
        // Visual: `q` stops the recording and the selection stays live for `d`.
        ("abc", "qavlqd", "c"),
    ];
    for &(text, keys, want) in cases {
        let mut h = session(text);
        feed_str(&mut h, keys);
        assert_eq!(buf(&h), want, "keys {keys:?}");
    }
    // `fq` with nothing recording: the `q` is the find target, not the start of `qx…` (which would
    // swallow the `x` as a register name).
    let mut h = session("abqc");
    feed_str(&mut h, "fqx");
    assert_eq!(buf(&h), "abc");
}

#[test]
fn q_with_no_recording_is_a_noop_end_to_end() {
    let mut h = session("hello\nworld");
    feed_str(&mut h, "Q"); // nothing recorded yet
    assert_eq!(
        buf(&h),
        "hello\nworld",
        "Q before any recording does nothing"
    );
}

#[test]
fn keyword_line_lookup_end_to_end_echo_and_list() {
    let text =
        "alpha foo beta\n    indented foo here\nfoo epsilon\nzeta eta\ntheta foo iota\nfoobar notmatch\n";
    // From line 3 (`foo epsilon`), cursor on `foo`.
    let echo = |keys: &str| {
        let mut h = session(text);
        feed_str(&mut h, &format!("2j{keys}"));
        h.status().to_string()
    };
    // `[i` from line 3 → first whole-word match from the TOP = line 1 (`foobar` on line 6 is excluded).
    assert_eq!(echo("[i"), "alpha foo beta");
    // `]i` from line 3 → first match strictly BELOW = line 5.
    assert_eq!(echo("]i"), "theta foo iota");
    // `2[i` → second match from the top = line 2, with its indentation preserved verbatim.
    assert_eq!(echo("2[i"), "    indented foo here");
    // The list forms (`[I`/`]I`) open a listing: lines 1, 2, 3, 5 above-and-at, line 5 below.
    assert_eq!(echo("[I"), "4 line(s) matching \"foo\"");
    assert_eq!(echo("]I"), "1 line(s) matching \"foo\"");
}

#[test]
fn keyword_line_lookup_reports_e349_off_a_keyword() {
    let mut h = session("foo\n\nbar\n");
    // Cursor starts on line 1 (`foo`); move onto the empty line 2, which has no keyword.
    feed_str(&mut h, "j[i");
    assert_eq!(h.status(), "E349: No identifier under cursor");
}
