//! Characterization of the session-level key intercepts in `app/session.rs` `run()` (#878): the `C-w` window
//! prefix, the `z` scroll/fold prefix and `zf{motion}`, the `[`/`]` fold-nav prefix, `H`/`M`/`L` (bare and
//! under an operator), the Normal and Insert chords the session resolves itself, and the terminal escape.
//! No other test reaches this routing: the parity corpora call `InputEngine::feed` directly.
//!
//! Each case runs the real binary on an 80x24 PTY, writes the buffer, and checks the file. Expected results
//! match Neovim 0.12.6 (`nvim --headless -u NONE --cmd 'set so=3'`; ruse hardcodes `scrolloff=3`) unless a
//! comment says the case pins a known divergence, named by its issue. Those pins are meant to flip when the
//! issue is fixed. One layout difference is not a divergence of the intercepts: ruse's text area is 23 rows
//! (the status line doubles as the command line), Neovim's is 22, so viewport-relative lines differ by one.

#![cfg(unix)]

mod support;

use std::path::Path;
use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";
const CTRL_W: &str = "\u{17}";

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

/// Open `text` in ruse, type `keys`, write with `:w`, quit, and return the file's contents.
fn edit(text: &str, keys: &str) -> String {
    let dir = Scratch::new();
    let file = dir.file("f.txt", text.as_bytes());
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(keys);
    // `:qa!`, not `:q!`: with a split left open, `:q!` closes only the focused window (#611, as in nvim).
    ed.keys(&format!("{ESC}:w\r:qa!\r"));
    assert_eq!(ed.exit_status(), 0, "keys {keys:?} left ruse running");
    String::from_utf8(std::fs::read(&file).unwrap()).unwrap()
}

// ---- `C-w` window prefix (observed through per-window cursors on one buffer) ---------------------------

#[test]
fn ctrl_w_s_and_v_split_and_ctrl_w_w_moves_focus() {
    // Without a split, `C-w w` is a no-op and the second `dd` hits line 3.
    assert_eq!(
        edit(&numbered(5), &format!("{CTRL_W}sjdd{CTRL_W}wdd")),
        without(5, &[1, 2])
    );
    assert_eq!(
        edit(&numbered(5), &format!("{CTRL_W}vjdd{CTRL_W}wdd")),
        without(5, &[1, 2])
    );
}

#[test]
fn ctrl_w_c_closes_the_focused_window() {
    assert_eq!(
        edit(&numbered(5), &format!("{CTRL_W}sjdd{CTRL_W}cdd")),
        without(5, &[1, 2])
    );
}

#[test]
fn ctrl_w_prefix_swallows_an_unknown_second_key() {
    // `C-w x` is not a window command here: the `x` is consumed by the prefix, not run as a delete.
    assert_eq!(edit("abc\n", &format!("{CTRL_W}x")), "abc\n");
}

#[test]
fn ctrl_w_in_insert_deletes_a_word_not_a_window_prefix() {
    assert_eq!(edit("one two\n", &format!("A{CTRL_W}{ESC}")), "one \n");
}

/// #851: the `q` of `C-w q` reaches the window command, so on the last window it quits, as in Neovim. A
/// modified buffer refuses it (E37) first, and `:q!` still quits after that.
#[test]
fn ctrl_w_q_quits_on_the_last_window() {
    let dir = Scratch::new();
    dir.file("f.txt", b"a\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&format!("{CTRL_W}q"));
    assert_eq!(ed.exit_status(), 0);

    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&format!("x{CTRL_W}q"));
    assert!(ed.is_running(), "`C-w q` dropped unsaved changes");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
}

// ---- `z` scroll prefix (observed through `H`/`M`/`L`, which read the viewport) ---------------------------

#[test]
fn zt_then_h_lands_on_the_cursor_line() {
    assert_eq!(edit(&numbered(100), "50GztHdd"), without(100, &[50]));
}

#[test]
fn zb_then_h_and_l() {
    // 23-row window ending at line 53 starts at line 31; `H` keeps the scrolloff margin (nvim: 35, 22 rows).
    assert_eq!(edit(&numbered(100), "50GzbHdd"), without(100, &[34]));
    assert_eq!(edit(&numbered(100), "50GzbLdd"), without(100, &[50]));
}

#[test]
fn zz_then_m_lands_on_the_cursor_line() {
    assert_eq!(edit(&numbered(100), "50GzzMdd"), without(100, &[50]));
}

// ---- `H`/`M`/`L` with a count and under an operator ---------------------------------------------------

#[test]
fn counted_h_counts_from_the_top() {
    assert_eq!(edit(&numbered(100), "50Gzt2Hdd"), without(100, &[50]));
}

#[test]
fn d_l_deletes_through_the_true_bottom_line_ignoring_scrolloff() {
    // Window 47..=69 (nvim: 47..=68).
    let gone: Vec<usize> = (50..=69).collect();
    assert_eq!(edit(&numbered(100), "50GztdL"), without(100, &gone));
}

#[test]
fn d_h_deletes_through_the_true_top_line_ignoring_scrolloff() {
    // Window 31..=53 (nvim: 32..=53).
    let gone: Vec<usize> = (31..=50).collect();
    assert_eq!(edit(&numbered(100), "50GzbdH"), without(100, &gone));
}

// ---- `zf{motion}`, Visual `zf`, `zo`/`zc` (the fold's extent is observed through `zo` then `]z`) --------

#[test]
fn zf_with_a_motion_folds_the_lines_it_covers() {
    assert_eq!(edit(&numbered(10), "zfjzo]zdd"), without(10, &[2]));
    assert_eq!(edit(&numbered(10), "zf2jzo]zdd"), without(10, &[3]));
    assert_eq!(edit(&numbered(10), "5Gzfggzo]zdd"), without(10, &[5]));
}

#[test]
fn zf_with_a_count_before_it() {
    assert_eq!(edit(&numbered(10), "3zfjzo]zdd"), without(10, &[4]));
}

#[test]
fn zf_cancelled_by_escape_makes_no_fold() {
    assert_eq!(
        edit(&numbered(10), &format!("zf{ESC}zo]zdd")),
        without(10, &[1])
    );
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

/// #679: an intercepted letter that is the argument of a pending command reaches it. `dfL` once deleted
/// the whole buffer (`L` ran as `dL`); `fq` armed a macro prefix that ate the `:w` after it.
#[test]
fn pending_arguments_are_not_intercepted() {
    assert_eq!(edit("abLdef\nline2\nline3\n", "dfL"), "def\nline2\nline3\n");
    assert_eq!(edit("abqcq\n", "fqx"), "abcq\n");
    assert_eq!(edit("abc\n", "\"qyy\"qP"), "abc\nabc\n");
}

/// #850: `5zz` goes to line 5 and its count does not leak into the `x` after it.
#[test]
fn counted_zz_drops_its_count() {
    assert_eq!(
        edit("alpha beta\n".repeat(6).as_str(), "5zzx"),
        "alpha beta\n".repeat(4) + "lpha beta\nalpha beta\n"
    );
}

// ---- Normal chords resolved in the session -----------------------------------------------------------

#[test]
fn ampersand_repeats_the_last_substitute_on_the_current_line() {
    assert_eq!(
        edit("ab\nab\n", ":s/b/X/\rj&"),
        "aX\naX\n",
        "`&` repeats `:s` (ruse keeps the flags, like `:&&`)"
    );
}

#[test]
fn ctrl_d_and_ctrl_u_move_half_a_page() {
    assert_eq!(edit(&numbered(30), "\u{4}dd"), without(30, &[12]));
    assert_eq!(edit(&numbered(30), "\u{4}\u{15}dd"), without(30, &[1]));
}

#[test]
fn ctrl_e_scrolls_and_drags_the_cursor_inside_scrolloff() {
    assert_eq!(edit(&numbered(30), "\u{5}dd"), without(30, &[5]));
    assert_eq!(edit(&numbered(30), "3\u{5}dd"), without(30, &[7]));
}

#[test]
fn pickers_own_the_keystream_until_escape() {
    // `C-p`, `C-l`, `C-b`, `C-f`: each picker takes the typed `x` into its query and Esc closes it.
    for open in ["\u{10}", "\u{c}", "\u{2}", "\u{6}"] {
        assert_eq!(
            edit("abc\n", &format!("{open}x{ESC}")),
            "abc\n",
            "picker {open:?}"
        );
    }
}

#[test]
fn line_picker_jumps_to_the_chosen_line() {
    assert_eq!(edit(&numbered(10), "\u{c}line7\rdd"), without(10, &[7]));
}

// ---- Insert chords resolved in the session -----------------------------------------------------------

#[test]
fn insert_ctrl_y_and_ctrl_e_copy_from_the_adjacent_lines() {
    assert_eq!(
        edit(&numbered(3), "jo\u{19}\u{19}\u{5}"),
        "line1\nline2\nlin\nline3\n"
    );
}

#[test]
fn insert_ctrl_n_completes_a_keyword() {
    assert_eq!(
        edit(&numbered(3), "Goli\u{e}"),
        "line1\nline2\nline3\nline1\n"
    );
}

/// Known divergence #602: the LSP coordinator swallows `CTRL-X`, so the submode never arms and the lone
/// `CTRL-L` is unbound, inserting nothing (it typed `l` before #599). Neovim completes the whole line `line3`.
#[test]
fn insert_ctrl_x_ctrl_l_does_not_complete() {
    assert_eq!(
        edit(&numbered(3), "Goli\u{18}\u{c}"),
        "line1\nline2\nline3\nli\n"
    );
}

// ---- Terminal escape (`CTRL-\ CTRL-N`); the child is `/bin/sh` since the harness sets no SHELL --------

/// Known divergence #603: under the legacy key encoding `CTRL-\` arrives as `CTRL-4`, so the escape prefix
/// never arms and the keys go to the shell. Neovim leaves Terminal mode and `:q!` quits.
#[test]
fn terminal_escape_does_not_arm_under_legacy_encoding() {
    let dir = Scratch::new();
    let mut ed = Editor::start(&dir, &[]);
    ed.keys(":terminal\r");
    ed.keys("\u{1c}\u{e}:q!\r");
    assert!(ed.is_running(), "`:q!` went to the shell");
}
