//! The write policy on a real PTY (#619), against nvim v0.12.6 and Emacs 31.1 run headless on the same
//! files: `'backup'` keeps the text before each write, `'patchmode'` the oldest, `'write'` off is E142,
//! `'writeany'` writes over another file, a backup that cannot be made is E510 until `:w!`, `'endofline'`
//! / `'endoffile'` / `'bomb'` shape the file, and the Emacs profile's first save of a visit keeps `f~`.
//! The scratch directory is under the temp directory, which nvim's `'backupskip'` and Emacs's
//! `temporary-file-directory` exempt from backups, so each test lifts that first.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{wait_for_file, Editor, Scratch};

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_default()
}

#[test]
fn backup_keeps_the_text_before_each_write() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":set bsk= bk\r");
    ed.keys("ccTWO\x1b");
    ed.keys_then_wait_for(":w\r", "written");
    wait_for_file(&file, b"TWO\n");
    assert_eq!(read(&dir.path("f.txt~")), b"one\n");
    ed.keys("ccTHREE\x1b");
    ed.keys(":w\r");
    wait_for_file(&file, b"THREE\n");
    assert_eq!(read(&dir.path("f.txt~")), b"TWO\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn patchmode_keeps_the_oldest_version_once() {
    let dir = Scratch::new();
    let file = dir.file("g.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("g.txt")]);
    ed.keys(":set bsk= pm=.orig\r");
    ed.keys("ccTWO\x1b:w\r");
    wait_for_file(&file, b"TWO\n");
    ed.keys("ccTHREE\x1b:w\r");
    wait_for_file(&file, b"THREE\n");
    assert_eq!(read(&dir.path("g.txt.orig")), b"one\n");
    assert!(!dir.path("g.txt~").exists(), "a write backup is removed");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn nowrite_refuses_and_writeany_writes_over_another_file() {
    let dir = Scratch::new();
    let file = dir.file("a.txt", b"one\n");
    let other = dir.file("b.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    ed.keys("ccTWO\x1b");
    ed.keys_then_wait_for(
        ":set nowrite\r:w\r",
        "E142: File not written: Writing is disabled by 'write' option",
    );
    assert_eq!(read(&file), b"one\n");
    ed.keys_then_wait_for(":set write\r:w b.txt\r", "E13: File exists");
    ed.keys_then_wait_for(":set wa\r:w b.txt\r", "written");
    wait_for_file(&other, b"TWO\n");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn a_backup_that_cannot_be_made_is_e510_until_forced() {
    let dir = Scratch::new();
    let file = dir.file("h.txt", b"one\n");
    dir.file("blocker", b"");
    let mut ed = Editor::start(&dir, &[Path::new("h.txt")]);
    ed.keys(":set bsk= bk bdir=blocker/sub\r");
    ed.keys("ccTWO\x1b");
    ed.keys_then_wait_for(":w\r", "E510: Can't make backup file (add ! to override)");
    assert_eq!(read(&file), b"one\n");
    ed.keys(":w!\r");
    wait_for_file(&file, b"TWO\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn endofline_endoffile_and_bomb_shape_the_file() {
    let dir = Scratch::new();
    let file = dir.file("e.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("e.txt")]);
    ed.keys_then_wait_for(":set eol?\r", "  endofline");
    ed.keys(":set noeol\r:w\r");
    wait_for_file(&file, b"one");
    // nvim: `:set bomb` modifies the buffer, `:set noeol` does not.
    ed.keys_then_wait_for(":set bomb\r", "e.txt [+]");
    ed.keys(":set eol\r:w\r");
    wait_for_file(&file, b"\xEF\xBB\xBFone\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    // A DOS file's final CTRL-Z is 'endoffile', written back as read.
    let dos = dir.file("z.txt", b"one\r\ntwo\r\n\x1a");
    let mut ed = Editor::start(&dir, &[Path::new("z.txt")]);
    ed.keys_then_wait_for(":set eof?\r", "  endoffile");
    ed.keys("x:w\r");
    wait_for_file(&dos, b"ne\r\ntwo\r\n\x1a");
    // With 'fixeol' it is dropped, as nvim does.
    ed.keys(":set fixeol\r:w\r");
    wait_for_file(&dos, b"ne\r\ntwo\r\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Emacs 31.1 (`emacs -Q`): two saves in one visit leave `f.txt~` with the text first visited.
#[test]
fn emacs_profile_backs_up_the_first_save_of_a_visit() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    // The scratch directory is under the temp directory, which Emacs exempts from backups.
    std::fs::write(
        dir.path("home/ruse/config.toml"),
        "[emacs]\ntemporary_file_directory = \"/nonexistent-ruse-tmp\"\n",
    )
    .unwrap();
    let mut ed = Editor::start_env(&dir, &[Path::new("f.txt")], &["RUSE_PROFILE=emacs"]);
    ed.keys("X\x18\x13");
    wait_for_file(&file, b"Xone\n");
    ed.keys("Y\x18\x13");
    wait_for_file(&file, b"XYone\n");
    assert_eq!(read(&dir.path("f.txt~")), b"one\n");
    ed.keys("\x18\x03");
    assert_eq!(ed.exit_status(), 0);
}
