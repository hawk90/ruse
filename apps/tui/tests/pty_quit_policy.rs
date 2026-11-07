//! Writing instead of refusing (#619), against nvim v0.12.6 on the same files: `'autowrite'` writes the
//! changed buffers before `:!`, `'autowriteall'` before `:q` and `:e {other}`, and the `:confirm` dialog's
//! `y` writes the buffer and runs the command.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{wait_for_file, Editor, Scratch};

#[test]
fn autowrite_writes_before_a_shell_command() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":set aw\rx");
    ed.keys(":!true\r");
    wait_for_file(&file, b"ne\n");
    ed.keys("\r:q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn autowriteall_writes_on_quit_and_edit() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let other = dir.file("g.txt", b"two\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":set awa nohid\rx");
    ed.keys_then_wait_for(":e g.txt\r", "g.txt");
    wait_for_file(&file, b"ne\n");
    ed.keys("x:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&other).unwrap(), b"wo\n");
}

#[test]
fn confirm_yes_writes_and_quits() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys("x");
    ed.keys_then_wait_for(
        ":confirm q\r",
        "Save changes to \"f.txt\"? [Y]es, (N)o, (C)ancel:",
    );
    ed.keys("y");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"ne\n");
}
