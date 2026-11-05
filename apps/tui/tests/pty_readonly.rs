//! `'readonly'` on a real PTY (#1211): `ruse -R`, the status line's `[RO]` after `[+]` (nvim v0.12.6's
//! `%m%r`: `f.txt [+][RO]`), W10 on the first change, E45 on `:w`, and `:w!`, which writes and resets
//! `'readonly'`. The child's `XDG_STATE_HOME` is the scratch directory's.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

const W10: &str = "W10: Warning: Changing a readonly file";

/// The screen output since `from` bytes.
fn since(ed: &Editor, from: usize) -> String {
    ed.screen_bytes()[from..].to_string()
}

#[test]
fn dash_r_opens_read_only_and_w_bang_resets_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("-R"), Path::new("f.txt")]);
    assert!(
        ed.screen_bytes().contains("f.txt [RO]"),
        "{:?}",
        ed.screen_bytes()
    );
    let before = ed.screen_bytes().len();
    ed.keys("x");
    let out = since(&ed, before);
    assert!(out.contains(W10), "{out:?}");
    // Only the changed cells are drawn: `f.txt [RO]` became `f.txt [+][RO]`.
    assert!(out.contains("+][RO]"), "{out:?}");
    let before = ed.screen_bytes().len();
    ed.keys(":w\r");
    assert!(since(&ed, before).contains("E45"));
    assert_eq!(std::fs::read(&file).unwrap(), b"one\n");
    let before = ed.screen_bytes().len();
    ed.keys(":w!\r");
    support::wait_for_file(&file, b"ne\n");
    // The command line gave the status line back whole: no `[RO]` after the name.
    let out = since(&ed, before);
    assert!(
        out.contains("f.txt  \"f.txt\""),
        "'readonly' is reset: {out:?}"
    );
    // `:set ro` puts it back.
    let before = ed.screen_bytes().len();
    ed.keys(":set ro\r");
    assert!(since(&ed, before).contains("f.txt [RO]"));
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Without `-R` nothing is read-only, and `:e` of another file under `-R` is read-only too (nvim's
/// readonly mode), until a `:set noro`.
#[test]
fn readonly_mode_covers_files_opened_later() {
    let dir = Scratch::new();
    dir.file("f.txt", b"one\n");
    dir.file("g.txt", b"two\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    assert!(!ed.screen_bytes().contains("[RO]"));
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    let mut ed = Editor::start(&dir, &[Path::new("-R"), Path::new("f.txt")]);
    let before = ed.screen_bytes().len();
    ed.keys(":e g.txt\r");
    assert!(since(&ed, before).contains("g.txt [RO]"));
    ed.keys(":set noro\r:e! f.txt\r");
    let before = ed.screen_bytes().len();
    ed.keys(":e g.txt\r");
    let out = since(&ed, before);
    assert!(out.contains("g.txt") && !out.contains("[RO]"), "{out:?}");
    ed.keys(":qa\r");
    assert_eq!(ed.exit_status(), 0);
}
