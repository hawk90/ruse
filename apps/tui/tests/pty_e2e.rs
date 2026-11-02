//! End-to-end tests of the real binary on a real PTY (#896): keys go through raw-mode input decoding and the
//! session loop, and results are read from disk and the exit status.

#![cfg(unix)]

mod support;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

#[test]
fn insert_then_write_then_quit() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("inew {ESC}:w\r"));
    assert_eq!(std::fs::read(&file).unwrap(), b"new orig\n");
    assert!(ed.is_running());
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn wq_creates_a_missing_file() {
    let dir = Scratch::new();
    let file = dir.path("new.txt");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("ihello{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    assert!(std::fs::read(&file).unwrap().starts_with(b"hello"));
}

#[test]
fn q_refuses_a_dirty_buffer_and_q_bang_discards_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("ix{ESC}:q\r"));
    assert!(ed.is_running(), ":q must refuse while the buffer is dirty");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
}

#[test]
fn quit_restores_the_terminal() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[&file]);
    assert!(
        ed.screen_bytes().contains("\u{1b}[?1049h"),
        "enters the alt screen"
    );
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    ed.settle();
    assert!(
        ed.screen_bytes().contains("\u{1b}[?1049l"),
        "leaves the alt screen"
    );
}
