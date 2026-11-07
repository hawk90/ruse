//! The Emacs profile's file variables from the user config on a real PTY (#619), against Emacs 31.1
//! (`emacs -Q --batch` with the same `setq`): `confirm-kill-emacs`, `view-read-only`,
//! `query-about-changed-file` off and `revert-without-query`.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{wait_for_file, Editor, Scratch};

fn emacs(dir: &Scratch, config: &str) -> Editor {
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    std::fs::write(
        dir.path("home/ruse/config.toml"),
        format!("[emacs]\n{config}"),
    )
    .unwrap();
    Editor::start_env(dir, &[Path::new("f.txt")], &["RUSE_PROFILE=emacs"])
}

#[test]
fn confirm_kill_emacs_asks_before_exiting() {
    let dir = Scratch::new();
    dir.file("f.txt", b"one\n");
    let mut ed = emacs(&dir, "confirm_kill_emacs = \"y-or-n-p\"\n");
    {
        ed.keys("\x18\x03");
        ed.wait_for_row("Really exit Emacs? (y or n)");
    }
    ed.keys("n");
    assert!(ed.is_running());
    {
        ed.keys("\x18\x03");
        ed.wait_for_row("Really exit Emacs? (y or n)");
    }
    ed.keys("y");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn view_read_only_enters_view_mode_and_q_leaves_it_writable() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = emacs(&dir, "view_read_only = true\n");
    {
        ed.keys("\x18\x11");
        ed.wait_for_row("Read-Only mode enabled in current buffer");
    }
    // `q` is View mode's: it quits View mode and makes the buffer writable again, so `z` inserts.
    ed.keys("qz\x18\x13");
    wait_for_file(&file, b"zone\n");
    // `e` leaves View mode but keeps the buffer read-only.
    ed.keys("\x18\x11e");
    {
        ed.keys("a");
        ed.wait_for_row("Buffer is read-only: #<buffer f.txt>");
    }
    ed.keys("\x18\x03");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn query_about_changed_file_off_only_tells() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = emacs(&dir, "query_about_changed_file = false\n");
    {
        ed.keys("\x18\x06");
        ed.wait_for_row("ile: /");
    }
    std::fs::write(&file, b"two\n").unwrap();
    {
        ed.keys("f.txt\r");
        ed.wait_for_row("File f.txt changed on disk.  C-x x g to load new contents");
    }
    ed.keys("\x18\x03");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn revert_without_query_rereads_silently() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = emacs(&dir, "revert_without_query = [\"f\\\\.txt\"]\n");
    {
        ed.keys("\x18\x06");
        ed.wait_for_row("ile: /");
    }
    std::fs::write(&file, b"two\n").unwrap();
    {
        ed.keys("f.txt\r");
        ed.wait_for_row("two");
    }
    ed.keys("X\x18\x13");
    wait_for_file(&file, b"Xtwo\n");
    ed.keys("\x18\x03");
    assert_eq!(ed.exit_status(), 0);
}
