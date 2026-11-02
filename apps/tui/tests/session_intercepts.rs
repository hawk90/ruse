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

/// Known divergence #851: the macro layer takes the `q` of `C-w q` as a recording prefix, so the window
/// command never runs, and the window prefix stays armed for the key after. Neovim quits on the last window.
#[test]
fn ctrl_w_q_is_taken_by_the_macro_layer() {
    let mut h = Headless::vim("a\n");
    h.keys("<C-w>q");
    assert!(!h.quit());
    h.keys("<Esc><Esc>:q<CR>");
    assert!(h.quit());
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

/// Known divergence #679: the `[` given to `r` also arms the session's bracket prefix, so the first `z` of
/// `zz` is read as `[z` and the second arms `z`, which then eats the `x`. Neovim gives `bc`.
#[test]
fn r_bracket_arms_the_bracket_prefix() {
    assert_eq!(edit("abc\n", "r[zzx"), "[bc\n");
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
    assert_eq!(h.status(), "no previous substitute");
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
    for open in ["<C-p>", "<C-l>", "<C-b>", "<C-f>"] {
        assert_eq!(
            edit("abc\n", &format!("{open}x<Esc>")),
            "abc\n",
            "picker {open}"
        );
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

/// Known divergence #602: the LSP coordinator swallows `CTRL-X`, so the submode never arms and `CTRL-L`
/// inserts a plain `l`. Neovim completes the whole line `line3`.
#[test]
fn insert_ctrl_x_ctrl_l_does_not_complete() {
    assert_eq!(
        edit(&numbered(3), "Goli<C-x><C-l>"),
        "line1\nline2\nline3\nlil\n"
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
