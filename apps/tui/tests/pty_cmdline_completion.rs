//! Command-line completion in the real binary on a PTY (#613): `<Tab>` completes a relative file name
//! that the command then opens, and several matches paint the wildmenu's popup menu.

#![cfg(unix)]

mod support;

use std::path::Path;
use support::{Editor, Scratch};

fn tree() -> Scratch {
    let dir = Scratch::new();
    std::fs::create_dir_all(dir.path("d")).unwrap();
    dir.file("f.txt", b"abc\n");
    dir.file("d/foobar.rs", b"hello\n");
    dir.file("d/fa.txt", b"other\n");
    dir
}

#[test]
fn tab_completes_the_file_edit_opens() {
    let dir = tree();
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":e d/foo\t\r");
    // The completed name is the buffer `x` edits and `:w` writes.
    ed.keys("x:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("d/foobar.rs")).unwrap(), b"ello\n");
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"abc\n");
}

#[test]
fn several_matches_show_the_popup_menu() {
    let dir = tree();
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    let before = ed.screen_bytes().len();
    ed.keys(":e d/f\t");
    let shown = ed.screen_bytes()[before..].to_string();
    // Both tails are painted (the menu), and the line holds the first match.
    assert!(shown.contains("fa.txt"), "{shown:?}");
    assert!(shown.contains("foobar.rs"), "{shown:?}");
    ed.keys("\x1b:q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn ctrl_d_lists_the_matches_above_the_line() {
    let dir = tree();
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    let before = ed.screen_bytes().len();
    ed.keys(":e d/\x04");
    let shown = ed.screen_bytes()[before..].to_string();
    assert!(shown.contains("fa.txt     foobar.rs"), "{shown:?}");
    ed.keys("\x1b:q\r");
    assert_eq!(ed.exit_status(), 0);
}
