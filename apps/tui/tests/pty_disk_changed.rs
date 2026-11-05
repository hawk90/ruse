//! `[disk changed]` on a real PTY (RFC-0023 §8, #938): the file on disk differs from what ruse read or
//! saved. It is checked after a shell command, on `:checktime`, before a save and on the 4000 ms idle
//! timer; a stat that moved is confirmed by the file's content, so a `touch` is not a change. W11/W12 are
//! nvim v0.12.6's texts (probed headless with `:checktime`). The child's `XDG_STATE_HOME` is the scratch
//! directory's.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

/// The starts of nvim's W11 (`… has changed since editing started`) and W12 (`… has changed and the
/// buffer was changed in Vim as well`): the 80-column status line cuts the rest after the flags.
const W11: &str = "W11: Warning: File \"f.txt\" has changed since edit";
const W12: &str = "W12: Warning: File \"f.txt\" has changed and the";

/// The status line, drawn whole once its `tabstop=4` is there (a command line gives it back whole).
/// Every check waits for what it looks for: a shell or a save's fsync can keep the screen quiet past the
/// harness's settle time on a loaded runner.
fn bar(ed: &mut Editor) -> String {
    let before = ed.mark();
    ed.keys_then_wait_for(":set ts?\r", "tabstop=4")[before..].to_string()
}

/// The file changes under an unmodified buffer: W11 after the shell command that changed it, the flag
/// until `:e!` reads it again. A `touch` that leaves the bytes alone is no change.
#[test]
fn an_external_write_flags_an_unmodified_buffer() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":!touch f.txt\r\r");
    assert!(
        !bar(&mut ed).contains("[disk changed]"),
        "a touch is not a change"
    );
    ed.keys_then_wait_for(":!printf 'two\\n' > f.txt\r", W11);
    ed.keys("\r");
    assert!(
        bar(&mut ed).contains("f.txt [disk changed]    tabstop=4"),
        "flagged"
    );
    ed.keys(":e!\r");
    let b = bar(&mut ed);
    assert!(b.contains("f.txt    tabstop=4"), "{b:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"two\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Under a modified buffer it is W12, and a save without `!` is refused (design doc §5); `:w!` writes and
/// the flag goes.
#[test]
fn a_save_over_an_external_change_needs_a_bang() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys("x");
    std::fs::write(&file, b"theirs\n").unwrap();
    ed.keys_then_wait_for(":checktime\r", W12);
    assert!(bar(&mut ed).contains("f.txt [+][disk changed]    tabstop=4"));
    // nvim's warning, then ruse's `(add ! to write anyway)`, past the 80 columns here.
    ed.keys_then_wait_for(":w\r", "WARNING: The fil");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"theirs\n",
        "not overwritten"
    );
    ed.keys(":wq\r");
    assert!(ed.is_running(), "`:wq` is refused too");
    ed.keys(":w!\r");
    support::wait_for_file(&file, b"ne\n");
    let b = bar(&mut ed);
    assert!(b.contains("f.txt    tabstop=4"), "{b:?}");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// The idle timer looks again after 4000 ms without a key.
#[test]
fn the_idle_timer_notices_a_change() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    let before = ed.mark();
    std::fs::write(&file, b"two, longer\n").unwrap();
    ed.wait_for_after(before, W11);
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
