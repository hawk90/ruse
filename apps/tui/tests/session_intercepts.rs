//! The session-level key intercepts of `pty_intercepts.rs`, driven through the headless session (#895):
//! the same `Session::on_key` the binary runs, without a PTY, so each case takes microseconds instead of a
//! process start. `pty_intercepts.rs` stays as the end-to-end check that the binary wires the same path.
//!
//! The terminal is 80x24 as in the PTY harness (a 23-row text area; ruse hardcodes `scrolloff=3`). Expected
//! results match Neovim 0.12.6 unless a comment pins a known divergence by its issue.

use ruse_tui::headless::Headless;

/// `line1\n` … `line{n}\n`.
fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line{i}\n")).collect()
}

/// `numbered(n)` with the given 1-based lines removed.
fn without(n: usize, gone: &[usize]) -> String {
    (1..=n)
        .filter(|i| !gone.contains(i))
        .map(|i| format!("line{i}\n"))
        .collect()
}

/// Open `text`, type `keys` then `<Esc>`, and return the buffer.
fn edit(text: &str, keys: &str) -> String {
    let mut h = Headless::vim(text);
    h.keys(keys).keys("<Esc>");
    assert!(!h.quit(), "keys {keys:?} quit the session");
    h.text()
}

// ---- `C-w` window prefix ------------------------------------------------------------------------------

#[test]
fn ctrl_w_s_and_v_split_and_ctrl_w_w_moves_focus() {
    assert_eq!(edit(&numbered(5), "<C-w>sjdd<C-w>wdd"), without(5, &[1, 2]));
    assert_eq!(edit(&numbered(5), "<C-w>vjdd<C-w>wdd"), without(5, &[1, 2]));
    let mut h = Headless::vim(&numbered(5));
    h.keys("<C-w>s");
    assert_eq!(h.workspace().window_count(), 2);
}

#[test]
fn ctrl_w_c_closes_the_focused_window() {
    let mut h = Headless::vim(&numbered(5));
    h.keys("<C-w>sjdd<C-w>c");
    assert_eq!(h.workspace().window_count(), 1);
    h.keys("dd");
    assert_eq!(h.text(), without(5, &[1, 2]));
}

#[test]
fn ctrl_w_prefix_swallows_an_unknown_second_key() {
    assert_eq!(edit("abc\n", "<C-w>x"), "abc\n");
}

#[test]
fn ctrl_w_in_insert_deletes_a_word_not_a_window_prefix() {
    assert_eq!(edit("one two\n", "A<C-w><Esc>"), "one \n");
}

/// #851: the `q` of `C-w q` reaches the window command, not the macro layer. On the last window it quits.
#[test]
fn ctrl_w_q_quits_on_the_last_clean_window() {
    let mut h = Headless::vim("a\n");
    h.keys("<C-w>q");
    assert!(h.quit());
}

/// `C-w q` on the last window is `:quit`, so unsaved changes refuse it with E37, as in Neovim.
#[test]
fn ctrl_w_q_refuses_a_modified_last_window() {
    let mut h = Headless::vim("a\n");
    h.keys("x<C-w>q");
    assert!(!h.quit());
    assert!(h.status().starts_with("E37"), "status {:?}", h.status());
    // Nothing is left armed: the next key is an ordinary command.
    h.keys("ib<Esc>");
    assert_eq!(h.text(), "b\n");
}

#[test]
fn ctrl_w_q_closes_a_split_without_quitting() {
    let mut h = Headless::vim("a\n");
    h.keys("x<C-w>s<C-w>q");
    assert!(!h.quit());
    assert_eq!(h.workspace().window_count(), 1);
}

/// #1068: `{count}C-w w` goes to window `count`, clamped to the last one; the count may also follow `C-w`.
/// nvim 0.12.6 with three windows: `2<C-w>w` -> 2, `9<C-w>w` -> 3, `<C-w>3w` -> 3, `1<C-w>W` -> 1.
/// (ruse opens a split below the focused window, nvim above, so the window numbers differ; the count
/// semantics are what is pinned.)
#[test]
fn counted_ctrl_w_w_goes_to_that_window() {
    let focus = |keys: &str| {
        let mut h = Headless::vim(
            "a
b
",
        );
        h.keys("<C-w>s<C-w>s").keys(keys);
        assert_eq!(h.workspace().window_count(), 3);
        h.workspace().focus()
    };
    assert_eq!(focus(""), 2);
    assert_eq!(focus("1<C-w>w"), 0);
    assert_eq!(focus("2<C-w>w"), 1);
    assert_eq!(
        focus("9<C-w>w"),
        2,
        "a count past the last window goes to the last"
    );
    assert_eq!(focus("1<C-w><C-w>"), 0);
    assert_eq!(focus("<C-w>1w"), 0, "the count may follow C-w");
    assert_eq!(focus("<C-w>12w"), 2);
    assert_eq!(focus("1<C-w>W"), 0);
    // Without a count `w` / `C-w` step forward and `W` back, wrapping.
    assert_eq!(focus("<C-w>w"), 0);
    assert_eq!(focus("<C-w><C-w>"), 0);
    assert_eq!(focus("<C-w>W"), 1);
    assert_eq!(focus("1<C-w>w<C-w>W"), 2);
    // The count is the window command's: it does not leak into the next command.
    let mut h = Headless::vim(
        "abcdef
",
    );
    h.keys("<C-w>s2<C-w>wx");
    assert_eq!(
        h.text(),
        "bcdef
"
    );
}

// ---- `z` scroll prefix (observed through `H`/`M`/`L`, which read the viewport) ---------------------------

#[test]
fn zt_then_h_lands_on_the_cursor_line() {
    assert_eq!(edit(&numbered(100), "50GztHdd"), without(100, &[50]));
}

#[test]
fn zb_then_h_and_l() {
    assert_eq!(edit(&numbered(100), "50GzbHdd"), without(100, &[34]));
    assert_eq!(edit(&numbered(100), "50GzbLdd"), without(100, &[50]));
}

#[test]
fn zz_then_m_lands_on_the_cursor_line() {
    assert_eq!(edit(&numbered(100), "50GzzMdd"), without(100, &[50]));
}

#[test]
fn zt_sets_the_view_top_inside_scrolloff() {
    let mut h = Headless::vim(&numbered(100));
    h.keys("50Gzt");
    assert_eq!(
        h.workspace().focused().view.top(),
        46,
        "line 50 sits 3 rows below the top"
    );
}

// ---- `H`/`M`/`L` with a count and under an operator ---------------------------------------------------

#[test]
fn counted_h_counts_from_the_top() {
    assert_eq!(edit(&numbered(100), "50Gzt2Hdd"), without(100, &[50]));
}

#[test]
fn d_l_deletes_through_the_true_bottom_line_ignoring_scrolloff() {
    let gone: Vec<usize> = (50..=69).collect();
    assert_eq!(edit(&numbered(100), "50GztdL"), without(100, &gone));
}

#[test]
fn d_h_deletes_through_the_true_top_line_ignoring_scrolloff() {
    let gone: Vec<usize> = (31..=50).collect();
    assert_eq!(edit(&numbered(100), "50GzbdH"), without(100, &gone));
}

// ---- `zf{motion}`, Visual `zf`, `zo`/`zc` -------------------------------------------------------------

#[test]
fn zf_with_a_motion_folds_the_lines_it_covers() {
    assert_eq!(edit(&numbered(10), "zfjzo]zdd"), without(10, &[2]));
    assert_eq!(edit(&numbered(10), "zf2jzo]zdd"), without(10, &[3]));
    assert_eq!(edit(&numbered(10), "5Gzfggzo]zdd"), without(10, &[5]));
}

#[test]
fn zf_reports_the_folded_line_count() {
    let mut h = Headless::vim(&numbered(10));
    h.keys("zf2j");
    assert_eq!(h.status(), "folded 3 lines");
}

#[test]
fn zf_with_a_count_before_it() {
    assert_eq!(edit(&numbered(10), "3zfjzo]zdd"), without(10, &[4]));
}

#[test]
fn zf_cancelled_by_escape_makes_no_fold() {
    assert_eq!(edit(&numbered(10), "zf<Esc>zo]zdd"), without(10, &[1]));
}

#[test]
fn visual_zf_folds_the_selection() {
    assert_eq!(edit(&numbered(10), "Vjzfzo]zdd"), without(10, &[2]));
}

#[test]
fn zo_opens_the_fold_so_j_enters_it() {
    assert_eq!(edit(&numbered(10), "zfjzojdd"), without(10, &[2]));
}

/// Known divergence #819: a closed fold snaps the cursor to its first line every frame, so `zc` moves the
/// cursor and `j` cannot leave the fold. Neovim keeps the cursor on line 2 after `zc` and `j` goes to line 3.
#[test]
fn zc_closes_the_fold_and_snaps_the_cursor() {
    assert_eq!(edit(&numbered(10), "zfjzojzczodd"), without(10, &[1]));
    assert_eq!(edit(&numbered(10), "zfjjzodd"), without(10, &[1]));
}

// ---- `[z`/`]z` fold navigation, and the engine's own bracket commands --------------------------------

#[test]
fn bracket_z_moves_to_the_fold_start_and_end() {
    assert_eq!(edit(&numbered(10), "3Gzf3jzo5G[zdd"), without(10, &[3]));
    assert_eq!(edit(&numbered(10), "3Gzf3jzo5G]zdd"), without(10, &[6]));
}

#[test]
fn bracket_p_still_reaches_the_engine() {
    assert_eq!(edit("a\nb\n", "yyj]p"), "a\nb\na\n");
}

/// #679: the `[` given to `r` is only its argument, so `zz` recenters and `x` deletes the `[`, as in Neovim.
#[test]
fn r_bracket_does_not_arm_the_bracket_prefix() {
    assert_eq!(edit("abc\n", "r[zzx"), "bc\n");
}

// ---- Keys the engine is waiting for are never intercepted (#679) ---------------------------------------

#[test]
fn intercept_letters_reach_a_pending_find_or_replace() {
    let line = "abc [z zHLq&@ xyz\nnext\n";
    for (keys, want) in [
        ("fzx", "abc [ zHLq&@ xyz\nnext\n"),
        ("fHx", "abc [z zLq&@ xyz\nnext\n"),
        ("fLx", "abc [z zHq&@ xyz\nnext\n"),
        ("fqx", "abc [z zHL&@ xyz\nnext\n"),
        ("f@x", "abc [z zHLq& xyz\nnext\n"),
        ("f&x", "abc [z zHLq@ xyz\nnext\n"),
        ("rz", "zbc [z zHLq&@ xyz\nnext\n"),
        ("rM", "Mbc [z zHLq&@ xyz\nnext\n"),
        ("rq", "qbc [z zHLq&@ xyz\nnext\n"),
    ] {
        assert_eq!(edit(line, keys), want, "keys {keys:?}");
    }
}

#[test]
fn an_argument_q_leaves_nothing_armed() {
    // After `rq`/`fq` the macro layer holds no `q` prefix, so `:q` still quits.
    let mut h = Headless::vim("abq\n");
    h.keys("rqfq:q!<CR>");
    assert!(h.quit());
}

#[test]
fn marks_and_registers_named_z_and_q_work() {
    assert_eq!(edit("abc def\n", "mzw`zx"), "bc def\n");
    assert_eq!(edit("abc\n", "\"qyy\"qP"), "abc\nabc\n");
    assert_eq!(edit("abc\n", "\"zyy\"zp"), "abc\nabc\n");
}

#[test]
fn the_second_key_of_g_reaches_the_engine() {
    assert_eq!(edit("aa\nbb\ncc\n", "gqj"), "aa bb\ncc\n");
    assert_eq!(edit("aa\nbb\ncc\n", "2gqq"), "aa bb\ncc\n");
    assert_eq!(edit("a a\na a\n", ":s/a/b/g<CR>g&"), "b b\nb b\n");
}

#[test]
fn native_leader_q_quits() {
    let mut h = Headless::new(ruse_tui::headless::Profile::Native, "a\n");
    h.keys("<Space>q");
    assert!(h.quit());
}

// ---- Intercepts consume their count and cancel an armed operator (#850) ---------------------------------

#[test]
fn counted_z_goes_to_the_line_and_drops_the_count() {
    let ab = "alpha beta\n".repeat(10);
    let line5 = |s: &str| {
        let mut v: Vec<&str> = vec!["alpha beta"; 10];
        v[4] = s;
        v.iter().map(|l| format!("{l}\n")).collect::<String>()
    };
    assert_eq!(edit(&ab, "5zzx"), line5("lpha beta"));
    assert_eq!(edit(&ab, "lll5zzx"), line5("alpa beta"));
    assert_eq!(edit(&ab, "4jlll5ztx"), line5("alpa beta"));
}

#[test]
fn session_chords_do_not_leak_their_count() {
    let ab = "alpha beta\n";
    for keys in [
        "3&x",
        "3<C-w>wx",
        "2<C-l><Esc>x",
        "2<C-p><Esc>x",
        "2<C-b><Esc>x",
        "2<C-f><Esc>x",
    ] {
        assert_eq!(edit(ab, keys), "lpha beta\n", "keys {keys:?}");
    }
}

#[test]
fn session_chords_cancel_an_armed_operator() {
    let text = numbered(5);
    for keys in [
        "dzzj", "dztj", "d<C-l>j", "d<C-w>wj", "d<C-e>j", "d<C-d>j", "d&j",
    ] {
        assert_eq!(edit(&text, keys), text, "keys {keys:?}");
    }
}

#[test]
fn counted_ampersand_substitutes_count_lines_and_rejects_a_range_past_the_end() {
    assert_eq!(edit("a a\na a\na a\n", ":s/a/b/<CR>j2&"), "b a\nb a\nb a\n");
    let mut h = Headless::vim("a a\na a\na a\n");
    h.keys(":s/a/b/<CR>j5&");
    assert_eq!(h.status(), "E16: Invalid range");
    assert_eq!(h.text(), "b a\na a\na a\n");
}

// ---- The macro layer never takes a key another layer owns (#851) ----------------------------------------

#[test]
fn confirm_loop_q_ends_the_loop() {
    let mut h = Headless::vim("a\na\na\n");
    h.keys(":%s/a/b/gc<CR>yq");
    assert_eq!(h.status(), "1 substitutions on 1 lines");
    h.keys("x");
    assert_eq!(
        h.text(),
        "\na\na\n",
        "the next key is a command, not a register name"
    );
}

#[test]
fn picker_query_takes_q_and_at() {
    assert_eq!(edit("aaa\nqq@u\nccc\n", "<C-l>qq@u<CR>dd"), "aaa\nccc\n");
}

#[test]
fn recording_q_is_text_in_insert_and_a_find_target() {
    let mut h = Headless::vim("abqcq\n");
    h.keys("qqfqxq");
    assert_eq!(h.text(), "abcq\n");
    assert_eq!(h.status(), "recorded @q (3 keys)");
    let mut h = Headless::vim("alpha\n");
    h.keys("qaiq<Esc>q");
    assert_eq!(h.text(), "qalpha\n");
    assert_eq!(h.status(), "recorded @a (3 keys)");
}

#[test]
fn counted_q_drops_the_count() {
    assert_eq!(edit("foo bar baz\n", "3qaxq"), "oo bar baz\n");
    assert_eq!(edit("foo bar baz\n", "3qa<Esc>qx"), "oo bar baz\n");
}

// ---- Normal chords resolved in the session -----------------------------------------------------------

#[test]
fn ampersand_repeats_the_last_substitute_on_the_current_line() {
    assert_eq!(edit("ab\nab\n", ":s/b/X/<CR>j&"), "aX\naX\n");
}

#[test]
fn ampersand_without_a_substitute_reports_it() {
    let mut h = Headless::vim("ab\n");
    h.keys("&");
    assert_eq!(h.status(), "E33: No previous substitute regular expression");
}

#[test]
fn ctrl_d_and_ctrl_u_move_half_a_page() {
    assert_eq!(edit(&numbered(30), "<C-d>dd"), without(30, &[12]));
    assert_eq!(edit(&numbered(30), "<C-d><C-u>dd"), without(30, &[1]));
}

#[test]
fn ctrl_e_scrolls_and_drags_the_cursor_inside_scrolloff() {
    assert_eq!(edit(&numbered(30), "<C-e>dd"), without(30, &[5]));
    assert_eq!(edit(&numbered(30), "3<C-e>dd"), without(30, &[7]));
}

#[test]
fn pickers_own_the_keystream_until_escape() {
    for open in ["<C-p>", "<C-l>"] {
        assert_eq!(
            edit("abc\n", &format!("{open}x<Esc>")),
            "abc\n",
            "picker {open}"
        );
    }
    // `C-b` / `C-f` open the buffer / file pickers under Native only; under Vim they page-scroll.
    for open in ["<C-b>", "<C-f>"] {
        let mut h = Headless::new(ruse_tui::headless::Profile::Native, "abc\n");
        h.keys(&format!("{open}x<Esc>"));
        assert_eq!(h.text(), "abc\n", "native picker {open}");
    }
}

#[test]
fn line_picker_jumps_to_the_chosen_line() {
    assert_eq!(edit(&numbered(10), "<C-l>line7<CR>dd"), without(10, &[7]));
}

// ---- Insert chords resolved in the session -----------------------------------------------------------

#[test]
fn insert_ctrl_y_and_ctrl_e_copy_from_the_adjacent_lines() {
    assert_eq!(
        edit(&numbered(3), "jo<C-y><C-y><C-e>"),
        "line1\nline2\nlin\nline3\n"
    );
}

#[test]
fn insert_ctrl_n_completes_a_keyword() {
    assert_eq!(
        edit(&numbered(3), "Goli<C-n>"),
        "line1\nline2\nline3\nline1\n"
    );
}

/// Known divergence #602: the LSP coordinator swallows `CTRL-X`, so the submode never arms and the lone
/// `CTRL-L` is unbound, inserting nothing (it typed `l` before #599). Neovim completes the whole line `line3`.
#[test]
fn insert_ctrl_x_ctrl_l_does_not_complete() {
    assert_eq!(
        edit(&numbered(3), "Goli<C-x><C-l>"),
        "line1\nline2\nline3\nli\n"
    );
}

// ---- Macros and the Ex line through the session ------------------------------------------------------

#[test]
fn a_recorded_macro_reports_and_replays() {
    let mut h = Headless::vim("a\nb\nc\n");
    h.keys("qqA!<Esc>jq");
    assert_eq!(h.status(), "recorded @q (4 keys)");
    h.keys("2@q");
    assert_eq!(h.text(), "a!\nb!\nc!\n");
}

#[test]
fn at_colon_repeats_the_last_ex_line() {
    let mut h = Headless::vim("a\nb\nc\n");
    h.keys(":s/$/!/<CR>j@:j@:");
    assert_eq!(h.text(), "a!\nb!\nc!\n");
}

#[test]
fn at_colon_without_an_ex_line_is_e30() {
    let mut h = Headless::vim("a\n");
    h.keys("@:");
    assert_eq!(h.status(), "E30: No previous command line");
}

/// #1068: `@{reg}` and `Q` run from charwise and blockwise Visual as if the keys were typed there, the count
/// repeating them; `q` records from Visual. Ground truth: nvim 0.12.6, typed keys.
#[test]
fn at_and_q_run_a_macro_in_visual() {
    const ONE: &str = "abc def ghi\n";
    assert_eq!(edit(ONE, "qalq0v@ad"), "c def ghi\n");
    assert_eq!(edit(ONE, "qalq0v2@ad"), " def ghi\n");
    assert_eq!(edit(ONE, "qalq0vQd"), "c def ghi\n");
    assert_eq!(edit(ONE, "qaxq0v@a"), "c def ghi\n");
    assert_eq!(edit(ONE, "vqalqd"), "c def ghi\n");
    assert_eq!(
        edit("abc\ndef\nghi\njkl\n", "qaxqj<C-v>j@ad"),
        "bc\nef\nhi\njkl\n"
    );
    // `@:` from Visual repeats the Ex line over the selection (`:'<,'>`).
    assert_eq!(edit("a\na\na\na\n", ":s/a/X/<CR>jvj@:"), "X\nX\nX\na\n");
    // A Select-mode `@` is typed text, not a macro.
    assert_eq!(edit(ONE, "qalq0gh@"), "@bc def ghi\n");
}

/// #1068: in linewise Visual, `@{reg}` and `Q` run the register once per selected line from its column 0
/// and ignore the count (nvim's `v_@-default`, `:'<,'>normal! @{reg}`). Ground truth: nvim 0.12.6.
#[test]
fn linewise_visual_at_runs_the_macro_on_each_line() {
    const FOUR: &str = "abc\ndef\nghi\njkl\n";
    let run = |keys: &str| {
        let mut h = Headless::vim(FOUR);
        h.keys(keys);
        assert!(!h.quit());
        (h.text(), h.cursor_row_col())
    };
    assert_eq!(run("qaxqjVj@a"), ("bc\nef\nhi\njkl\n".into(), (3, 0)));
    assert_eq!(run("qaxqjVj3@a"), ("bc\nef\nhi\njkl\n".into(), (3, 0)));
    assert_eq!(run("qaxqjVjQ"), ("bc\nef\nhi\njkl\n".into(), (3, 0)));
    assert_eq!(run("qaxqGVk@a"), ("bc\ndef\nhi\nkl\n".into(), (4, 0)));
    // `@@` afterwards repeats it once, in Normal mode.
    assert_eq!(run("qaxqjVj@a@@"), ("bc\nef\ni\njkl\n".into(), (3, 0)));
    // Each run starts at column 0, and one left in Insert is ended before the next line.
    assert_eq!(
        run("qaAx<Esc>qjVj@a"),
        ("abcx\ndefx\nghix\njkl\n".into(), (3, 3))
    );
    // Lines are not renumbered after a run deletes some: line 2 of what is left is next. (Unterminated, as
    // `dd` on the last line of a terminated buffer leaves the cursor past it.)
    let mut h = Headless::vim("abc\ndef\nghi\njkl");
    h.keys("qaddqVj@a");
    assert_eq!((h.text(), h.cursor_row_col()), ("ghi".into(), (1, 0)));
}

#[test]
fn star_searches_the_word_and_sets_hlsearch_pattern() {
    let mut h = Headless::vim("foo bar\nfoo\n");
    h.keys("*");
    assert_eq!(h.cursor_row_col(), (2, 0));
}

#[test]
fn info_commands_print_to_the_status_line() {
    let mut h = Headless::vim("Hi\n");
    h.keys("lga");
    assert_eq!(h.status(), "<i>  105,  Hex 69,  Octal 151");
}

#[test]
fn alt_key_in_the_vim_profile_is_escape_then_the_key() {
    // `<M-x>` (`ESC x` read as one Alt chord, #896) leaves Insert, then `x` deletes the char under the cursor.
    let mut h = Headless::vim("\n");
    h.keys("ihello<M-x>");
    assert_eq!(h.text(), "hell\n");
}

/// Open `text` with Vim's factory search case, type `keys`, and return the session.
fn run(text: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text).vanilla_search_case();
    h.keys(keys);
    h
}

#[test]
fn put_from_an_empty_register_is_e353_and_spends_the_register() {
    let h = run("abc\n", "\"ap");
    assert_eq!(h.status(), "E353: Nothing in register a");
    assert_eq!(h.text(), "abc\n");
    // The `"a` is spent: the next `p` reads the unnamed register (#901, nvim v0.12.6).
    assert_eq!(run("abc\n", "\"apyl$p").text(), "abca\n");
    assert_eq!(run("abc\n", "p").status(), "E353: Nothing in register \"");
    assert_eq!(
        run("abc\n", "\":p").status(),
        "E30: No previous command line"
    );
    assert_eq!(
        run("abc\n", "\"_p").status(),
        "",
        "the black hole puts nothing quietly"
    );
}

#[test]
fn an_unset_mark_is_e20_and_nothing_runs() {
    let h = run("abc\ndef\n", "jd'a");
    assert_eq!(h.status(), "E20: Mark not set");
    assert_eq!(h.text(), "abc\ndef\n");
    assert_eq!(h.cursor_row_col(), (2, 0));
}

#[test]
fn a_search_that_matches_nowhere_is_e486_and_nothing_runs() {
    let h = run("abc def\n", "wd/zzz<CR>");
    assert_eq!(h.status(), "E486: pattern not found: zzz");
    assert_eq!(h.text(), "abc def\n");
    assert_eq!(h.cursor_row_col(), (1, 4));
    // The keys typed after it still run (they are typed, not flushed).
    assert_eq!(run("abc\n", "l/zzz<CR>x").text(), "ac\n");
}

#[test]
fn quiet_substitute_and_global_misses_are_not_errors() {
    assert_eq!(run("abc\n", ":s/zzz/y/e<CR>").status(), "");
    assert_eq!(
        run("abc\n", ":g/zzz/d<CR>").status(),
        "Pattern not found: zzz"
    );
    assert_eq!(
        run("abc\n", ":foo<CR>").status(),
        "E492: Not an editor command: foo"
    );
}

#[test]
fn at_at_before_any_macro_is_e748() {
    let h = run("abc\n", "@@x");
    assert_eq!(h.emsg(), "E748");
    assert_eq!(h.text(), "bc\n");
}

#[test]
fn visual_colon_leaves_the_cursor_at_the_selection_start() {
    // nvim v0.12.6: the colon operator moves to the area's start before the line opens; `<Esc>` keeps it.
    assert_eq!(run("abcd\nefgh\n", "jllvkh:<Esc>").cursor_row_col(), (1, 1));
    assert_eq!(run("abcd\nefgh\n", "llVj:<Esc>").cursor_row_col(), (1, 0));
    assert_eq!(run("abcd\nefgh\n", "jllVk:<Esc>").cursor_row_col(), (1, 2));
    assert_eq!(
        run("abcd\nefgh\n", "jl<C-v>kl:<Esc>").cursor_row_col(),
        (1, 1)
    );
    // `'<,'>` still names the whole selection.
    assert_eq!(run("a\nb\nc\n", "jVj:d<CR>").text(), "a\n");
}
