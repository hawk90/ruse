//! Ex command-line specials through the real binary on a PTY (#601): `%` / `#` file names, the `!` that
//! repeats the previous shell command (`:!!`, `:r !!`), and `|` chaining. Expected behaviour is nvim
//! v0.12.6's (probed headless with `-u NONE`). What reaches the disk is the observable.

#![cfg(unix)]

mod support;

use std::path::Path;
use support::{Editor, Scratch};

fn open(dir: &Scratch) -> Editor {
    dir.file("f.txt", b"abc\n");
    Editor::start(dir, &[Path::new("f.txt")])
}

fn saved(dir: &Scratch, ed: &mut Editor) -> String {
    ed.keys(":wq\r");
    assert_eq!(ed.exit_status(), 0);
    String::from_utf8(std::fs::read(dir.path("f.txt")).unwrap()).unwrap()
}

#[test]
fn read_percent_reads_the_current_file() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys(":r %\r");
    assert_eq!(saved(&dir, &mut ed), "abc\nabc\n");
    assert!(!dir.path("%").exists());
}

#[test]
fn edit_percent_rereads_an_unmodified_buffer() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    // Change the file behind ruse's back: `:e %` re-reads it (it does not open a file named `%`).
    std::fs::write(dir.path("f.txt"), b"xyz\n").unwrap();
    ed.keys(":e %\r");
    assert_eq!(saved(&dir, &mut ed), "xyz\n");
    assert!(!dir.path("%").exists());
}

#[test]
fn edit_percent_refuses_over_unsaved_changes() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys_then_wait_for("x:e %\r", "E37");
    assert_eq!(saved(&dir, &mut ed), "bc\n");
}

#[test]
fn bang_bang_repeats_the_previous_shell_command() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys(":r !echo hi\r:r !!\r");
    assert_eq!(saved(&dir, &mut ed), "abc\nhi\nhi\n");
}

#[test]
fn escaped_bang_is_literal_and_a_bang_without_history_is_e34() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys_then_wait_for(":!!\r", "E34");
    ed.keys(":r !echo a\\!b\r");
    assert_eq!(saved(&dir, &mut ed), "abc\na!b\n");
}

#[test]
fn bar_chains_a_substitute_and_a_delete() {
    let dir = Scratch::new();
    dir.file("f.txt", b"a a\na a\nx\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":s/a/b/|d\r");
    assert_eq!(saved(&dir, &mut ed), "a a\nx\n");
}
