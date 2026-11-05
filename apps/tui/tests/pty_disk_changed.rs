//! `[disk changed]` on a real PTY (RFC-0023 §8, #938): the file on disk differs from what ruse read or
//! saved. It is checked after a shell command, on `:checktime`, before a save and on the 4000 ms idle
//! timer; a stat that moved is confirmed by the file's content, so a `touch` is not a change. W11/W12 are
//! nvim v0.12.6's texts (probed headless with `:checktime`). The child's `XDG_STATE_HOME` is the scratch
//! directory's.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::time::Duration;

use support::{wait_until, Editor, Scratch};

/// The starts of nvim's W11 (`… has changed since editing started`) and W12 (`… has changed and the
/// buffer was changed in Vim as well`): the 80-column status line cuts the rest after the flags.
const W11: &str = "W11: Warning: File \"f.txt\" has changed since edit";
const W12: &str = "W12: Warning: File \"f.txt\" has changed and the";

fn since(ed: &Editor, from: usize) -> String {
    ed.screen_bytes()[from..].to_string()
}

/// Whether `want` is drawn after `from` within 10 s: a shell or a save's fsync can keep the screen quiet
/// past the harness's settle time on a loaded runner, so every check waits for what it looks for.
fn shows(ed: &Editor, from: usize, want: &str) -> bool {
    wait_until(Duration::from_secs(10), || since(ed, from).contains(want))
}

/// The status line, drawn whole once its `tabstop=4` is there (a command line gives it back whole).
fn bar(ed: &mut Editor) -> String {
    let before = ed.screen_bytes().len();
    ed.keys(":set ts?\r");
    assert!(shows(ed, before, "tabstop=4"), "{:?}", since(ed, before));
    since(ed, before)
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
    let before = ed.screen_bytes().len();
    ed.keys(":!printf 'two\\n' > f.txt\r");
    assert!(shows(&ed, before, W11), "{:?}", since(&ed, before));
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
    let before = ed.screen_bytes().len();
    ed.keys(":checktime\r");
    assert!(shows(&ed, before, W12), "{:?}", since(&ed, before));
    assert!(bar(&mut ed).contains("f.txt [+][disk changed]    tabstop=4"));
    let before = ed.screen_bytes().len();
    ed.keys(":w\r");
    assert!(
        // nvim's warning, then ruse's `(add ! to write anyway)`, past the 80 columns here.
        shows(&ed, before, "WARNING: The fil"),
        "{:?}",
        since(&ed, before)
    );
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
    let before = ed.screen_bytes().len();
    std::fs::write(&file, b"two, longer\n").unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || since(&ed, before).contains(W11)),
        "{:?}",
        since(&ed, before)
    );
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
