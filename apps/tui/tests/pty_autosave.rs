//! The recovery journal as Vim's swap file and Emacs's auto-save file, and Emacs's
//! `auto-save-visited-mode`, on a real PTY (#619, RFC-0023). `:!true` syncs the journal at once (a risky
//! command syncs first), so a test need not wait for the idle sync. The child's `XDG_STATE_HOME` is the
//! scratch directory's.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{wait_for_file, Editor, Scratch};

/// Whether `f.txt` has a live journal after a change and a sync.
fn journaled(dir: &Scratch, setup: &str, env: &[&str]) -> bool {
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start_env(dir, &[Path::new("f.txt")], env);
    ed.wait_for_row("f.txt");
    ed.keys(setup);
    ed.keys("x:!true\r");
    ed.wait_for_row(":!true");
    let there = dir.journals().path_for(&file).exists();
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
    there
}

#[test]
fn swapfile_and_updatecount_decide_the_journal() {
    assert!(journaled(&Scratch::new(), "", &[]), "'swapfile' is on");
    assert!(
        !journaled(&Scratch::new(), ":set noswf\r", &[]),
        "'noswapfile'"
    );
    assert!(
        !journaled(&Scratch::new(), ":set uc=0\r", &[]),
        "'updatecount' 0"
    );
}

fn emacs_config(dir: &Scratch, config: &str) {
    std::fs::create_dir_all(dir.path("home/ruse")).unwrap();
    std::fs::write(
        dir.path("home/ruse/config.toml"),
        format!("[emacs]\n{config}"),
    )
    .unwrap();
}

#[test]
fn emacs_auto_save_default_off_keeps_no_journal() {
    let dir = Scratch::new();
    emacs_config(&dir, "auto_save_default = false\n");
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start_env(&dir, &[Path::new("f.txt")], &["RUSE_PROFILE=emacs"]);
    ed.wait_for_row("f.txt");
    ed.keys("X");
    ed.wait_for_row("f.txt [+]");
    // Past the 4 s idle sync a journaled buffer would have its journal.
    std::thread::sleep(std::time::Duration::from_millis(4500));
    ed.keys("\x02");
    ed.settle();
    assert!(!dir.journals().path_for(&file).exists());
    ed.keys("\x18\x03n");
    ed.wait_for_row("exit anyway?");
    ed.keys("yes\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Emacs 31.1: with `auto-save-visited-mode` the visited file itself is saved after the idle interval.
#[test]
fn emacs_auto_save_visited_mode_saves_the_file() {
    let dir = Scratch::new();
    emacs_config(
        &dir,
        "auto_save_visited_mode = true\nauto_save_visited_interval = 1\n",
    );
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start_env(&dir, &[Path::new("f.txt")], &["RUSE_PROFILE=emacs"]);
    ed.wait_for_row("f.txt");
    ed.keys("X");
    wait_for_file(&file, b"Xone\n");
    ed.keys("\x18\x03");
    assert_eq!(ed.exit_status(), 0);
}
