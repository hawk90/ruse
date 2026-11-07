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

/// nvim v0.12.6: `CTRL-W q` is `:quit` and `CTRL-W c` is `:close`, so with 'nohidden' neither closes the
/// only window over a changed buffer (E37), `'confirm'` asks for them, and `'autowriteall'` writes first.
#[test]
fn window_keys_run_the_quit_policy() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    dir.file("g.txt", b"two\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":set nohid\r:split\r:e g.txt\rx");
    ed.keys_then_wait_for("\x17q", "E37: No write since last change");
    ed.keys(":\x1b");
    ed.keys_then_wait_for("\x17c", "E37: No write since last change");
    ed.keys(":\x1b:set cf\r");
    ed.keys_then_wait_for("\x17q", "Save changes to \"g.txt\"?");
    ed.keys("\x1b:set nocf awa\r");
    ed.keys("\x17q");
    support::wait_for_file(&dir.path("g.txt"), b"wo\n");
    // f.txt is left, unchanged; the last window quits.
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"one\n");
}
