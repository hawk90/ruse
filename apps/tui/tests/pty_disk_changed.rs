//! `[disk changed]` on a real PTY (RFC-0023 §8, #938): the file on disk differs from what ruse read or
//! saved. It is checked after a shell command, on `:checktime`, before a save and on the 4000 ms idle
//! timer; a stat that moved is confirmed by the file's content, so a `touch` is not a change. W11/W12 are
//! nvim v0.12.6's texts (probed headless with `:checktime`). The child's `XDG_STATE_HOME` is the scratch
//! directory's.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use support::{Editor, Scratch};

/// nvim's W11 and the start of its W12 (`… has changed and the buffer was changed in Vim as well`, one
/// cell wider than the 80 columns here, so it wraps above the hit-enter prompt, #1222).
const W11: &str = "W11: Warning: File \"f.txt\" has changed since editing started";
const W12: &str =
    "W12: Warning: File \"f.txt\" has changed and the buffer was changed in Vim as we";
/// nvim's hit-enter prompt under a message too long for the status line (#1222).
const HIT_ENTER: &str = "Press ENTER or type command to continue";

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
    // W11 is what nvim says without 'autoread' (#619).
    ed.keys_then_wait_for(":set noar\r", "f.txt");
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
    ed.wait_for(HIT_ENTER);
    assert!(bar(&mut ed).contains("f.txt [+][disk changed]    tabstop=4"));
    // nvim's warning, then ruse's `(add ! to write anyway)`, whole above the hit-enter prompt.
    ed.keys_then_wait_for(
        ":w\r",
        "WARNING: The file has been changed since reading it!!! (add ! to write anyway)",
    );
    ed.wait_for(HIT_ENTER);
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
    // W11 is what nvim says without 'autoread' (#619).
    ed.keys_then_wait_for(":set noar\r", "f.txt");
    let before = ed.mark();
    std::fs::write(&file, b"two, longer\n").unwrap();
    ed.wait_for_after(before, W11);
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// A focus gain looks again (nvim's `:checktime` on FocusGained, #1220): ruse asks the terminal for focus
/// reports (mode 1004) on entry and turns them off on exit. A key every 200 ms keeps the idle timer from
/// running, so only the focus report can find the change.
#[test]
fn a_focus_gain_notices_a_change() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    // W11 is what nvim says without 'autoread' (#619).
    ed.keys_then_wait_for(":set noar\r", "f.txt");
    assert!(
        ed.screen_bytes().contains("\x1b[?1004h"),
        "focus reports are asked for"
    );
    ed.sync();
    std::fs::write(&file, b"two, longer\n").unwrap();
    ed.send(b"\x1b[I");
    let start = Instant::now();
    while !ed.screen_bytes().contains(W11) {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{}",
            ed.report("no W11 after a focus gain", start)
        );
        std::thread::sleep(Duration::from_millis(200));
        ed.send(b"0");
    }
    // W11 waits at the hit-enter prompt (#1222) unless a `0` already cleared it; `:` works either way.
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    let out = ed.screen_bytes();
    let off = out
        .rfind("\x1b[?1004l")
        .expect("focus reports are turned off");
    assert!(
        off > out.rfind("\x1b[?1004h").unwrap(),
        "turned off after they were turned on"
    );
}

/// A file deleted under the buffer is nvim's E211 (probed headless with `:checktime`, modified or not),
/// shown once and without the `[disk changed]` flag; a save writes it again without `!` (#1221).
#[test]
fn a_deleted_file_is_e211_not_a_change() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys("x");
    std::fs::remove_file(&file).unwrap();
    ed.keys_then_wait_for(":checktime\r", "E211: File \"f.txt\" no longer available");
    let b = bar(&mut ed);
    assert!(b.contains("f.txt [+]    tabstop=4"), "{b:?}");
    // Once per deletion: a second look says nothing.
    ed.keys(":checktime\r");
    let b = bar(&mut ed);
    assert!(!b.contains("E211"), "{b:?}");
    ed.keys(":w\r");
    support::wait_for_file(&file, b"ne\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// `:w >> %` appends the buffer to its own file: ruse's own write, so no W11 and no flag (nvim v0.12.6
/// fires no FileChangedShell for it), and a later `:w` needs no `!` (#1221).
#[test]
fn an_append_to_the_own_file_is_not_a_change() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys_then_wait_for(":w >> %\r", "appended");
    assert_eq!(std::fs::read(&file).unwrap(), b"one\none\n");
    ed.keys(":checktime\r");
    let b = bar(&mut ed);
    assert!(b.contains("f.txt    tabstop=4"), "{b:?}");
    ed.keys("x");
    ed.keys(":w\r");
    support::wait_for_file(&file, b"ne\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// nvim v0.12.6 with `'autoread'` (its default): an unmodified buffer whose file changed is re-read
/// silently on `:checktime`, and `u` takes the re-read back (#619); a modified one still gets W12.
#[test]
fn autoread_rereads_an_unmodified_buffer() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys_then_wait_for(":set ar?\r", "  autoread");
    std::fs::write(&file, b"two, longer\n").unwrap();
    ed.keys_then_wait_for(":checktime\r", "two, longer");
    let b = bar(&mut ed);
    assert!(!b.contains("[disk changed]") && !b.contains("[+]"), "{b:?}");
    ed.keys_then_wait_for("u", "one");
    ed.keys_then_wait_for(":e!\r", "two, longer");
    ed.keys("x");
    std::fs::write(&file, b"three\n").unwrap();
    ed.keys_then_wait_for(":checktime\r", "W12");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
}
