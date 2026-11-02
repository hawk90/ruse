//! The quit and window-close family through the real binary on a PTY (#611, `ZZ`/`ZQ` from #605): what
//! ends the process, with which status, and what reaches the disk. Expected behaviour is nvim v0.12.6's
//! (probed headless with `-u NONE`), whose 'hidden' is on: closing a window keeps its buffer loaded, and
//! no quit short of `:qa!`/`:cq` drops a modified buffer the user cannot see (#736, #847).

#![cfg(unix)]

mod support;

use std::path::Path;
use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

fn open(dir: &Scratch) -> Editor {
    dir.file("f.txt", b"abc\n");
    Editor::start(dir, &[Path::new("f.txt")])
}

#[test]
fn zz_on_an_unmodified_buffer_quits_without_writing() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    // Remove the file behind ruse's back: a write would bring it back.
    std::fs::remove_file(dir.path("f.txt")).unwrap();
    ed.keys("ZZ");
    assert_eq!(ed.exit_status(), 0);
    assert!(!dir.path("f.txt").exists(), "ZZ wrote an unmodified buffer");
}

#[test]
fn zz_writes_a_modified_buffer_then_quits() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys("xZZ");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"bc\n");
}

#[test]
fn zq_quits_without_writing() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys("xZQ");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"abc\n");
}

#[test]
fn quit_in_a_split_closes_the_window_and_keeps_the_edit() {
    let dir = Scratch::new();
    let mut ed = open(&dir);
    ed.keys(":split\rx:q\r");
    assert!(ed.is_running(), ":q in a split closes only the window");
    ed.keys(":q\r");
    assert!(ed.is_running(), ":q on the last window refuses (E37)");
    assert!(ed.screen_bytes().contains("E37"));
    ed.keys(":wq\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"bc\n");
}

#[test]
fn quit_bang_refuses_over_a_modified_hidden_buffer() {
    let dir = Scratch::new();
    dir.file("g.txt", b"ghi\n");
    let mut ed = open(&dir);
    ed.keys(":split\r:e g.txt\rx:close\r");
    for quit in [":q!\r", "ZQ", ":x\r", ":qa\r"] {
        ed.keys(quit);
        assert!(ed.is_running(), "{quit:?} must not drop g.txt's change");
    }
    assert!(ed.screen_bytes().contains("E162"));
    ed.keys(&format!("{ESC}:qa!\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("g.txt")).unwrap(), b"ghi\n");
}

#[test]
fn cquit_exits_with_its_status_and_writes_nothing() {
    for (cmd, status) in [(":cq\r", 1), (":cq 3\r", 3)] {
        let dir = Scratch::new();
        let mut ed = open(&dir);
        ed.keys(&format!("x{cmd}"));
        assert_eq!(ed.exit_status(), status, "{cmd:?}");
        assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"abc\n");
    }
}
