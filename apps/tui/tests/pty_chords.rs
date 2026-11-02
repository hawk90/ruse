//! Modifier handling through a real PTY (#598, #599, #681): the control bytes and uppercase letters a
//! terminal sends, decoded by the real binary and run through the session. Unit tests build their key events
//! by hand, which is how `Q` passed them for months while a terminal's SHIFT+`Q` never fired (#681).
//!
//! Expected file contents are nvim v0.12.6's for the same keys (`nvim --headless -u NONE -i NONE`).

#![cfg(unix)]

mod support;

use std::path::Path;
use support::{Editor, Scratch};

/// Open `text` in ruse, type `keys`, then write and quit with `:wq` (typed in Normal), and return the file.
fn edit(text: &str, keys: &str) -> String {
    let dir = Scratch::new();
    let file = dir.file("f.txt", text.as_bytes());
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(keys);
    ed.keys(":wq\r");
    assert_eq!(ed.exit_status(), 0, "keys {keys:?} left ruse running");
    String::from_utf8(std::fs::read(&file).unwrap()).unwrap()
}

#[test]
fn normal_ctrl_s_and_ctrl_k_do_not_run_s_and_k() {
    // #598: `<C-s>` ran `s` (substitute a char), so `<C-s><Esc>` deleted `a`. nvim: no-op.
    assert_eq!(edit("abc\n", "\u{13}\u{1b}"), "abc\n");
    // `<C-k>` was the `k` motion: `j<C-k>x` deleted from line 1. nvim: no-op, `x` hits line 2.
    assert_eq!(edit("ab\ncd\n", "j\u{0b}x"), "ab\nd\n");
    // `<C-t>` (tag pop) and `<C-_>` (sent as 0x1f) are unbound: nothing happens.
    assert_eq!(edit("abc\n", "\u{14}\u{1f}x"), "bc\n");
}

#[test]
fn normal_ctrl_h_ctrl_j_and_ctrl_n_are_motions() {
    // nvim: `<C-h>` = `h`, `<C-j>`/`<C-n>` = `j`.
    assert_eq!(edit("abc\n", "l\u{08}x"), "bc\n");
    assert_eq!(edit("ab\ncd\nef\n", "\u{0a}\u{0e}x"), "ab\ncd\nf\n");
}

#[test]
fn insert_ctrl_c_leaves_insert() {
    // #599: `i_CTRL-C` typed `c` and stayed in Insert, so the `:wq` became text. nvim leaves Insert.
    assert_eq!(edit("abc\n", "iX\u{03}"), "Xabc\n");
    // `3iab<C-c>` inserts once (the interrupt drops the count's repeats).
    assert_eq!(edit("z\n", "3iab\u{03}"), "abz\n");
}

#[test]
fn insert_ctrl_h_ctrl_j_and_ctrl_q_are_bound() {
    // `<C-h>` = `<BS>`, `<C-j>` = `<CR>` (#599: they typed `h` and `j`).
    assert_eq!(edit("x\n", "iab\u{08}c\u{0a}d\u{1b}"), "ac\ndx\n");
    // `<C-q>` is `<C-v>`: `<C-q>065` inserts `A`.
    assert_eq!(edit("x\n", "i\u{11}065\u{1b}"), "Ax\n");
}

#[test]
fn insert_unbound_chords_type_nothing() {
    // `<C-]>`, `<C-\>`, `<C-_>` arrive as Ctrl-5/4/7 and typed `5`, `4`, `7` (#599); `<C-b>`, `<C-s>` typed
    // their letters. nvim inserts nothing for `<C-]>`/`<C-s>`; ruse inserts nothing for all of them.
    assert_eq!(
        edit("x\n", "i\u{1d}\u{1c}\u{1f}\u{02}\u{13}Z\u{1b}"),
        "Zx\n"
    );
}

#[test]
fn cmdline_ctrl_u_ctrl_w_and_ctrl_c() {
    // #599: `c_CTRL-U` appended `u`. Here it clears the junk so `:wq` runs.
    assert_eq!(edit("abc\n", "x:junk\u{15}w\r"), "bc\n");
    // `c_CTRL-W` deletes one word: `:wq junk` → `:wq ` + `<CR>` writes the file.
    assert_eq!(edit("abc\n", "x:w junk\u{17}\r"), "bc\n");
    // `c_CTRL-C` abandons the line: `:q!` is never run, and the later `x` still edits.
    assert_eq!(edit("abc\n", ":q!\u{03}x"), "bc\n");
}

#[test]
fn shifted_q_replays_the_last_recording() {
    // #681: `Q` arrives as SHIFT+`Q` and never fired. nvim: `qaxqQ` on `abcd` leaves `cd`.
    assert_eq!(edit("abcd\n", "qaxqQ"), "cd\n");
    assert_eq!(edit("foo bar\n", "qaxqQ"), "o bar\n");
}

#[test]
fn a_recorded_ctrl_bracket_replays_as_nothing() {
    // A macro holding `i<C-]>Z<Esc>` used to record `<C-]>` as `5`; replayed, it typed `5Z`.
    assert_eq!(edit("x\n", "qai\u{1d}Z\u{1b}q@a"), "ZZx\n");
}
