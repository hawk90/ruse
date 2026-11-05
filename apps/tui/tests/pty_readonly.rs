//! `'readonly'` on a real PTY (#1211): `ruse -R`, the status line's `[RO]` after `[+]` (nvim v0.12.6's
//! `%m%r`: `f.txt [+][RO]`), W10 on the first change, E45 on `:w`, and `:w!`, which writes and resets
//! `'readonly'`. The child's `XDG_STATE_HOME` is the scratch directory's.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

const W10: &str = "W10: Warning: Changing a readonly file";

#[test]
fn dash_r_opens_read_only_and_w_bang_resets_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("-R"), Path::new("f.txt")]);
    ed.wait_for("f.txt [RO]");
    // Only the changed cells are drawn: `f.txt [RO]` became `f.txt [+][RO]`.
    let mark = ed.mark();
    ed.keys("x");
    let out = ed.wait_for_after(mark, W10);
    assert!(out[mark..].contains("+][RO]"), "{:?}", &out[mark..]);
    ed.keys_then_wait_for(":w\r", "E45");
    assert_eq!(std::fs::read(&file).unwrap(), b"one\n");
    // The command line gives the status line back whole: no `[RO]` after the name.
    ed.keys_then_wait_for(":w!\r", "f.txt  \"f.txt\"");
    support::wait_for_file(&file, b"ne\n");
    // `:set ro` puts it back.
    ed.keys_then_wait_for(":set ro\r", "f.txt [RO]");
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
    // `:set ts?` redraws the whole status line: no flag after the name.
    ed.keys_then_wait_for(":set ts?\r", "f.txt    tabstop=4");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    let mut ed = Editor::start(&dir, &[Path::new("-R"), Path::new("f.txt")]);
    ed.keys_then_wait_for(":e g.txt\r", "g.txt [RO]");
    // `:set noro` resets the focused g.txt and ends the readonly mode.
    ed.keys(":set noro\r:e! f.txt\r");
    ed.keys_then_wait_for(":e g.txt\r", "g.txt  \"g.txt\"");
    ed.keys(":qa\r");
    assert_eq!(ed.exit_status(), 0);
}
