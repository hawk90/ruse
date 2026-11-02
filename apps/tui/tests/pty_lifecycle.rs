//! Signal, exit and crash lifecycle of the real binary on a real PTY, through a real restart (#897): what a
//! signal or a hard kill leaves on disk and on the terminal, and what the next start in the same directory
//! offers back. Fatal-signal handling itself is #857 (`apps/tui/src/signals.rs`); `pty_e2e.rs` covers the
//! journal contents right after SIGTERM / SIGHUP, and these tests add the restart.
//!
//! nvim v0.12.6, probed on the same PTY setup, preserves its swap file and exits with status 1 on SIGTERM,
//! SIGHUP, an external SIGINT and a master close, after leaving the alt screen. ruse journals instead and
//! exits as killed by the signal (`-sig` here, 128 + sig in a shell), the convention for a re-raised signal.

#![cfg(unix)]

mod support;

use std::path::Path;

use ruse_tui::persist::journal;
use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";
const LEAVE_ALT: &str = "\u{1b}[?1049l";
const SHOW_CURSOR: &str = "\u{1b}[?25h";
const PROMPT: &str = "recovered from a previous session";

fn f_txt() -> &'static Path {
    Path::new("f.txt")
}

/// The terminal teardown is the last thing the child wrote: it left the alt screen and showed the cursor.
fn assert_restored(ed: &mut Editor, what: &str) {
    ed.settle();
    let screen = ed.screen_bytes();
    let leave = screen.rfind(LEAVE_ALT);
    assert!(leave.is_some(), "{what}: leaves the alt screen");
    assert!(
        screen[leave.unwrap()..].contains(SHOW_CURSOR),
        "{what}: shows the cursor after leaving the alt screen"
    );
}

/// Restart in the same directory, expect the recovery offer, take it with `r`, save, quit; returns the file.
fn restart_and_recover(dir: &Scratch, what: &str) -> Vec<u8> {
    let mut ed = Editor::start(dir, &[f_txt()]);
    assert!(
        ed.screen_bytes().contains(PROMPT),
        "{what}: offers recovery"
    );
    ed.keys("r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0, "{what}: the recovered session quits");
    std::fs::read(dir.path("f.txt")).unwrap()
}

/// Edit, end the session with `kill`, check the exit and the teardown, then restart and recover the edit.
fn edit_kill_restart(kill: impl FnOnce(&mut Editor), want_status: i32, what: &str) {
    let dir = Scratch::new();
    dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.keys(&format!("iabcde{ESC}"));
    kill(&mut ed);
    assert_eq!(ed.exit_status(), want_status, "{what}: exit status");
    if what != "master close" {
        assert_restored(&mut ed, what); // after a hang-up nothing the child writes can be read back
    }
    drop(ed);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"orig\n");
    assert_eq!(restart_and_recover(&dir, what), b"abcdeorig\n", "{what}");
    assert!(
        !journal::journal_path(Some(&dir.path("f.txt"))).exists(),
        "{what}: the save clears the journal"
    );
}

#[test]
fn sigterm_then_restart_recovers_the_edit() {
    edit_kill_restart(|ed| ed.signal(libc::SIGTERM), -libc::SIGTERM, "SIGTERM");
}

#[test]
fn sighup_then_restart_recovers_the_edit() {
    edit_kill_restart(|ed| ed.signal(libc::SIGHUP), -libc::SIGHUP, "SIGHUP");
}

/// Closing the terminal (every master handle) hangs the session up: the same path as SIGHUP.
#[test]
fn master_close_then_restart_recovers_the_edit() {
    edit_kill_restart(Editor::hang_up, -libc::SIGHUP, "master close");
}

/// An external SIGINT is fatal like SIGTERM (nvim also exits on one): journal, restore, exit as killed.
#[test]
fn external_sigint_then_restart_recovers_the_edit() {
    edit_kill_restart(|ed| ed.signal(libc::SIGINT), -libc::SIGINT, "SIGINT");
}

/// Ctrl-C typed in Normal mode is a key in raw mode, not a SIGINT: the editor and the edit stay, and the
/// buffer is untouched (it used to decode as `c`, so `<C-c><C-c>` was `cc`). nvim also keeps running (it
/// shows "Type :qa and press <Enter> to exit Nvim").
#[test]
fn ctrl_c_in_normal_mode_does_not_end_the_session() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.keys(&format!("iabc{ESC}\u{3}\u{3}"));
    assert!(ed.is_running(), "Ctrl-C does not end the session");
    ed.keys(":w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"abcorig\n");
}

/// The throttle journals every 8th change (`JOURNAL_THROTTLE`); SIGKILL runs no handler, so a hard kill loses
/// the changes since the last frame. Pins the loss window: no journal after 7 single-character inserts, one
/// after the 8th, and a SIGKILL after a 9th recovers the 8-character text on restart.
#[test]
fn sigkill_loses_at_most_the_changes_since_the_last_journal_frame() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let journal = journal::journal_path(Some(&file));
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.keys("i");
    for (n, c) in "12345678".chars().enumerate() {
        assert!(
            !journal.exists(),
            "no journal frame before change {}",
            n + 1
        );
        ed.keys(&c.to_string()); // one key per frame, so each insert is one observed change
    }
    assert_eq!(
        journal::replay(Some(&file)).as_deref(),
        Some(&b"12345678orig\n"[..])
    );
    ed.keys("9");
    ed.signal(libc::SIGKILL);
    assert_eq!(ed.exit_status(), -libc::SIGKILL);
    drop(ed);
    assert_eq!(
        restart_and_recover(&dir, "SIGKILL"),
        b"12345678orig\n",
        "the 9th change is lost"
    );
}

/// SIGKILL straight after a single edit: inside the loss window, so nothing is journaled and the restart
/// opens the disk file with no offer.
#[test]
fn sigkill_after_one_edit_loses_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.keys(&format!("ix{ESC}"));
    ed.signal(libc::SIGKILL);
    assert_eq!(ed.exit_status(), -libc::SIGKILL);
    drop(ed);
    assert!(!journal::journal_path(Some(&file)).exists());
    let mut ed = Editor::start(&dir, &[f_txt()]);
    assert!(!ed.screen_bytes().contains(PROMPT));
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// #857 left a gap: a signal while the startup recovery prompt shows killed ruse in raw mode on the alt
/// screen. The prompt now polls the signal flag: the terminal is restored and the journal is kept.
#[test]
fn signal_during_the_startup_recovery_prompt_restores_the_terminal() {
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        let dir = Scratch::new();
        let file = dir.file("f.txt", b"on disk\n");
        journal::append(Some(&file), b"unsaved work\n").unwrap();
        let mut ed = Editor::start(&dir, &[f_txt()]);
        assert!(ed.screen_bytes().contains(PROMPT));
        ed.signal(sig);
        assert_eq!(ed.exit_status(), -sig, "signal {sig}");
        assert_restored(&mut ed, "signal at the prompt");
        drop(ed);
        assert_eq!(std::fs::read(&file).unwrap(), b"on disk\n");
        assert_eq!(
            restart_and_recover(&dir, "after the prompt"),
            b"unsaved work\n"
        );
    }
}

/// The same for the prompt `:e` shows (#859): the open is cancelled, every modified buffer is journaled.
#[test]
fn signal_during_the_edit_recovery_prompt_journals_open_buffers() {
    let dir = Scratch::new();
    let a = dir.file("a.txt", b"a\n");
    let b = dir.file("b.txt", b"on disk\n");
    journal::append(Some(&b), b"UNSAVED B\n").unwrap();
    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    ed.keys(&format!("iAA{ESC}:e b.txt\r"));
    assert!(ed.screen_bytes().contains(PROMPT));
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    assert_restored(&mut ed, "signal at the :e prompt");
    assert_eq!(journal::replay(Some(&a)).as_deref(), Some(&b"AAa\n"[..]));
    assert_eq!(
        journal::replay(Some(&b)).as_deref(),
        Some(&b"UNSAVED B\n"[..])
    );
}

/// Two journaled files: each is offered when it is opened, and both recover.
#[test]
fn two_journals_are_each_offered_and_recovered() {
    let dir = Scratch::new();
    let a = dir.file("a.txt", b"a\n");
    let b = dir.file("b.txt", b"b\n");
    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    ed.keys(&format!("iAA{ESC}:e b.txt\riBB{ESC}"));
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    drop(ed);

    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    assert!(ed.screen_bytes().contains(PROMPT), "a.txt is offered");
    ed.keys("r:w\r");
    let before = ed.screen_bytes().len();
    ed.keys(":e b.txt\r");
    assert!(
        ed.screen_bytes()[before..].contains(PROMPT),
        "b.txt is offered"
    );
    ed.keys("r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&a).unwrap(), b"AAa\n");
    assert_eq!(std::fs::read(&b).unwrap(), b"BBb\n");
}

/// A journal older than the file it belongs to (the file changed on disk since) is still offered, with a
/// warning like nvim's "NEWER than swap file!"; `d` keeps the newer disk file.
#[test]
fn stale_journal_is_offered_with_a_warning() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    journal::append(Some(&file), b"old unsaved\n").unwrap();
    std::fs::write(&file, b"changed elsewhere\n").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    let screen = ed.screen_bytes();
    assert!(screen.contains(PROMPT));
    assert!(screen.contains("NEWER"), "warns that the file changed");
    ed.keys("d:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"changed elsewhere\n");
    assert!(!journal::journal_path(Some(&file)).exists());

    // A journal written after the file is not flagged.
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    journal::append(Some(&file), b"unsaved\n").unwrap();
    let ed = Editor::start(&dir, &[f_txt()]);
    assert!(ed.screen_bytes().contains(PROMPT));
    assert!(!ed.screen_bytes().contains("NEWER"));
}

/// A journal that cannot be read (mode 000) is named on the status line, not skipped silently, and the file
/// opens; the journal itself is left alone.
#[test]
fn unreadable_journal_is_reported_and_kept() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions. Root ignores the file mode, so the read would succeed.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    journal::append(Some(&file), b"unsaved\n").unwrap();
    let journal = journal::journal_path(Some(&file));
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    let screen = ed.screen_bytes();
    assert!(!screen.contains(PROMPT));
    assert!(
        screen.contains("unreadable"),
        "names the unreadable journal"
    );
    ed.keys(":q\r");
    let status = ed.exit_status();
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(status, 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
    assert_eq!(
        journal::replay(Some(&file)).as_deref(),
        Some(&b"unsaved\n"[..])
    );
}

/// An empty journal (e.g. created, then killed before its first frame) holds nothing: the file opens with no
/// offer, and edits after it journal and recover normally.
#[test]
fn empty_journal_opens_the_file_and_journaling_resumes() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    std::fs::write(journal::journal_path(Some(&file)), b"").unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    assert!(!ed.screen_bytes().contains(PROMPT));
    ed.keys(&format!("iabc{ESC}"));
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    drop(ed);
    assert_eq!(restart_and_recover(&dir, "empty journal"), b"abcorig\n");
}
