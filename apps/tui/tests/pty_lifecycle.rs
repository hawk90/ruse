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

use std::time::Duration;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";
const LEAVE_ALT: &str = "\u{1b}[?1049l";
const SHOW_CURSOR: &str = "\u{1b}[?25h";
const PROMPT: &str = "Recovery journal for";
/// The prompt's key line, drawn after its other lines: once it shows, the whole prompt is on screen.
const PROMPT_KEYS: &str = "[k] discard";

fn f_txt() -> &'static Path {
    Path::new("f.txt")
}

/// Wait for the whole recovery prompt.
fn wait_for_prompt(ed: &mut Editor) -> String {
    ed.wait_for(PROMPT_KEYS)
}

/// Wait until the editor shows `text` (the opened buffer), then check no recovery prompt came first.
fn opened_without_offer(ed: &mut Editor, text: &str, what: &str) {
    let screen = ed.wait_for(text);
    assert!(!screen.contains(PROMPT), "{what}: nothing offered");
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
    wait_for_prompt(&mut ed);
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
    ed.sync(); // the edit is applied before the signal
    kill(&mut ed);
    assert_eq!(ed.exit_status(), want_status, "{what}: exit status");
    if what != "master close" {
        assert_restored(&mut ed, what); // after a hang-up nothing the child writes can be read back
    }
    drop(ed);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"orig\n");
    assert_eq!(restart_and_recover(&dir, what), b"abcdeorig\n", "{what}");
    assert!(
        !dir.journals().path_for(&dir.path("f.txt")).exists(),
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
    ed.sync();
    assert!(ed.is_running(), "Ctrl-C does not end the session");
    ed.keys(":w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"abcorig\n");
}

/// RFC-0023 §4: SIGKILL runs no handler, so a hard kill loses the changes since the last sync. Edits are
/// synced after 4 s idle; after that a SIGKILL loses only what came later. The restart offers the synced
/// text as one change over the disk text (#863): `:q` refuses it, and `u` takes it back to the disk text,
/// after which the buffer is clean.
#[test]
fn sigkill_after_an_idle_sync_recovers_the_synced_edits() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let journals = dir.journals();
    let journal = journals.path_for(&file);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.keys(&format!("iabc{ESC}"));
    assert!(!journal.exists(), "nothing is synced at once");
    assert!(
        support::wait_until(Duration::from_secs(10), || journals
            .replay(Some(&file))
            .as_deref()
            == Some(&b"abcorig\n"[..])),
        "the idle sync writes the edit"
    );
    ed.keys("x"); // a change after the sync: the kill below loses it
    ed.signal(libc::SIGKILL);
    assert_eq!(ed.exit_status(), -libc::SIGKILL);
    drop(ed);
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");

    let mut ed = Editor::start(&dir, &[f_txt()]);
    wait_for_prompt(&mut ed);
    ed.keys("r:w copy.txt\r");
    assert_eq!(
        support::wait_for_file(&dir.path("copy.txt"), b"abcorig\n"),
        b"abcorig\n",
        "the synced text, without the later `x`"
    );
    // The recovery is a change: `:q` refuses it.
    ed.keys_then_wait_for(":q\r", "E37");
    ed.keys("u:q\r");
    assert_eq!(ed.exit_status(), 0, "`u` returns to the disk text: clean");
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
    assert!(!journal.exists(), "a clean exit leaves no journal");
}

/// SIGKILL straight after a single edit: inside the 4 s idle window, so nothing is synced and the restart
/// opens the disk file with no offer.
#[test]
fn sigkill_after_one_edit_loses_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[f_txt()]);
    ed.keys(&format!("ix{ESC}"));
    ed.sync(); // the edit is applied, and still inside the idle window
    ed.signal(libc::SIGKILL);
    assert_eq!(ed.exit_status(), -libc::SIGKILL);
    drop(ed);
    assert!(!dir.journals().path_for(&file).exists());
    let mut ed = Editor::start(&dir, &[f_txt()]);
    opened_without_offer(&mut ed, "orig", "after SIGKILL");
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
        dir.journals()
            .write_crashed(Some(&file), b"unsaved work\n")
            .unwrap();
        let mut ed = Editor::start(&dir, &[f_txt()]);
        wait_for_prompt(&mut ed);
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
    let j = dir.journals();
    j.write_crashed(Some(&b), b"UNSAVED B\n").unwrap();
    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    let before = ed.mark();
    ed.keys(&format!("iAA{ESC}:e b.txt\r"));
    ed.wait_for_after(before, PROMPT_KEYS);
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    assert_restored(&mut ed, "signal at the :e prompt");
    assert_eq!(j.replay(Some(&a)).as_deref(), Some(&b"AAa\n"[..]));
    assert_eq!(j.replay(Some(&b)).as_deref(), Some(&b"UNSAVED B\n"[..]));
}

/// Two journaled files: each is offered when it is opened, and both recover.
#[test]
fn two_journals_are_each_offered_and_recovered() {
    let dir = Scratch::new();
    let a = dir.file("a.txt", b"a\n");
    let b = dir.file("b.txt", b"b\n");
    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    ed.keys(&format!("iAA{ESC}:e b.txt\riBB{ESC}"));
    ed.sync();
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    drop(ed);

    let mut ed = Editor::start(&dir, &[Path::new("a.txt")]);
    wait_for_prompt(&mut ed); // a.txt is offered
    ed.keys("r:w\r");
    ed.keys_then_wait_for(":e b.txt\r", PROMPT_KEYS); // and b.txt
    ed.keys("r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&a).unwrap(), b"AAa\n");
    assert_eq!(std::fs::read(&b).unwrap(), b"BBb\n");
}

/// A journal older than the file it belongs to (the file changed on disk since) is still offered, with a
/// warning like nvim's "NEWER than swap file!"; `k` keeps the newer disk file and archives the journal.
#[test]
fn stale_journal_is_offered_with_a_warning() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let journal = dir
        .journals()
        .write_crashed(Some(&file), b"old unsaved\n")
        .unwrap();
    std::fs::write(&file, b"changed elsewhere\n").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    let screen = wait_for_prompt(&mut ed);
    assert!(screen.contains("NEWER"), "warns that the file changed");
    ed.keys("k:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"changed elsewhere\n");
    assert!(!journal.exists());
    assert_eq!(
        std::fs::read_dir(dir.journals().archive_dir())
            .unwrap()
            .count(),
        1
    );

    // A journal written after the file is not flagged.
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    dir.journals()
        .write_crashed(Some(&file), b"unsaved\n")
        .unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    assert!(!wait_for_prompt(&mut ed).contains("NEWER"));
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
    let journal = dir
        .journals()
        .write_crashed(Some(&file), b"unsaved\n")
        .unwrap();
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    // Names the unreadable journal, and offers nothing.
    opened_without_offer(&mut ed, "unreadable", "unreadable journal");
    // An edit synced (`:!` syncs first) goes beside the unreadable journal, never over it.
    let bytes_mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    ed.keys(&format!("iNEW {ESC}:!true\r\r"));
    let beside = || dir.journals().candidates(&file).len();
    assert!(support::wait_until(Duration::from_secs(5), || beside() == 2));
    assert_eq!(bytes_mode(&journal), 0o000, "its mode is unchanged");
    ed.keys(":q!\r");
    let status = ed.exit_status();
    assert_eq!(beside(), 1, ":q! drops this session's own journal only");
    std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(status, 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
    assert_eq!(
        dir.journals().replay(Some(&file)).as_deref(),
        Some(&b"unsaved\n"[..])
    );
}

/// An empty journal (e.g. created, then killed before its first frame) holds nothing: the file opens with no
/// offer, and edits after it journal and recover normally.
#[test]
fn empty_journal_opens_the_file_and_journaling_resumes() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let journals = dir.journals();
    journals.ensure_dir().unwrap();
    std::fs::write(journals.path_for(&file), b"").unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    opened_without_offer(&mut ed, "orig", "empty journal");
    ed.keys(&format!("iabc{ESC}"));
    ed.sync();
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    drop(ed);
    assert_eq!(restart_and_recover(&dir, "empty journal"), b"abcorig\n");
}

/// Type 200 one-character changes (`journal::SYNC_EVERY`), which syncs the journal without waiting for the
/// idle timer, and leave Insert mode.
fn type_synced_changes(ed: &mut Editor) {
    ed.keys(&format!("i{}{ESC}", "1".repeat(200)));
}

/// #862: a normal exit leaves no journal behind, so the next start offers nothing. Each way of letting the
/// changes go: undo back to the saved text then `:q`, `:e!` then `:q`, `:q!`, `:qa!`, `:cq`, and `:bd!`.
/// nvim v0.12.6 deletes its swap file on `:q!`, `:qa!`, `:cq` and `:bd!`; after `u` or `:e!` its swap file
/// holds the disk text, which offers nothing either.
#[test]
fn a_normal_exit_leaves_no_journal_to_offer() {
    for (quit, status) in [
        ("u:q\r", 0),
        (":e!\r:q\r", 0),
        (":q!\r", 0),
        (":qa!\r", 0),
        (":cq\r", 1),
        (":bd!\r:q\r", 0),
    ] {
        let dir = Scratch::new();
        let file = dir.file("f.txt", b"orig\n");
        let mut ed = Editor::start(&dir, &[f_txt()]);
        type_synced_changes(&mut ed);
        let journal = dir.journals().path_for(&file);
        assert!(
            support::wait_until(Duration::from_secs(5), || journal.exists()),
            "{quit:?}: the edits are journaled"
        );
        ed.keys(quit);
        assert_eq!(ed.exit_status(), status, "{quit:?}: exit status");
        drop(ed);
        assert!(!journal.exists(), "{quit:?}: the journal is gone");
        let mut ed = Editor::start(&dir, &[f_txt()]);
        opened_without_offer(&mut ed, "orig", &format!("{quit:?}"));
        ed.keys(":q\r");
        assert_eq!(ed.exit_status(), 0);
        assert_eq!(std::fs::read(&file).unwrap(), b"orig\n", "{quit:?}");
    }
}

/// #862: undoing back to the saved text drops the journal at once, so a crash after it offers nothing.
#[test]
fn undo_back_to_the_saved_text_drops_the_journal() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[f_txt()]);
    type_synced_changes(&mut ed);
    let journal = dir.journals().path_for(&file);
    assert!(support::wait_until(Duration::from_secs(5), || journal.exists()));
    ed.keys("u");
    ed.sync();
    assert!(!journal.exists());
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    drop(ed);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    opened_without_offer(&mut ed, "orig", "after undo");
}

/// #863: a recovered buffer is modified: `:q` refuses with E37 and keeps the journal, `u` returns to the
/// disk text, and a saved trace starts from the disk text, so `ruse --replay` on the disk file reproduces
/// the recovered buffer.
#[test]
fn a_recovered_buffer_is_modified_and_traces_from_the_disk_text() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\ntwo\n");
    let journal = dir
        .journals()
        .write_crashed(Some(&file), b"one\nTWO RECOVERED\n")
        .unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    wait_for_prompt(&mut ed);
    ed.keys("r");
    // `:q` refuses a recovered buffer, and says why.
    ed.keys_then_wait_for(":q\r", "E37");
    assert!(ed.is_running(), "`:q` refuses a recovered buffer");
    assert!(journal.exists());
    ed.keys(":trace save t.trace\r");
    let trace = dir.path("t.trace");
    assert!(support::wait_until(Duration::from_secs(5), || trace.exists()));
    let out = support::run_detached(
        &["--replay", trace.to_str().unwrap(), file.to_str().unwrap()],
        std::process::Stdio::null(),
        std::process::Stdio::piped(),
    );
    assert_eq!(out.status, 0, "replay: {}", out.stderr);
    assert_eq!(out.stdout, b"one\nTWO RECOVERED\n");
    // `u` takes the recovery back: the buffer is the disk text again, and clean.
    ed.keys("u:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"one\ntwo\n");
}

/// #869: a FIFO where the journal belongs is never opened as one: the file opens at once with the journal
/// reported, a sync fails visibly (E303) instead of blocking, and the FIFO is left alone.
#[test]
fn a_fifo_journal_does_not_block_startup() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let journals = dir.journals();
    journals.ensure_dir().unwrap();
    let jp = journals.path_for(&file);
    let c = std::ffi::CString::new(jp.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    // The status line names the journal as unreadable; the reason (`is not a regular file`) is past the
    // 80th column.
    ed.wait_for("unreadable");
    // The append fails visibly.
    let before = ed.mark();
    type_synced_changes(&mut ed);
    ed.wait_for_after(before, "E303");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
    assert!(std::fs::symlink_metadata(&jp)
        .unwrap()
        .file_type()
        .is_fifo());
}

/// #867: a file name that is not UTF-8 opens; nothing panics. Linux allows such a name on disk; macOS
/// refuses to create one, so there it opens as a new buffer.
#[test]
fn a_non_utf8_file_name_opens() {
    use std::os::unix::ffi::OsStrExt;
    let dir = Scratch::new();
    let name = Path::new(std::ffi::OsStr::from_bytes(b"bad\xff.txt"));
    let on_disk = std::fs::write(dir.0.join(name), b"latin\n").is_ok();
    let mut ed = Editor::start(&dir, &[name]);
    if on_disk {
        ed.wait_for("latin"); // shows the file
    } else {
        ed.sync();
    }
    assert!(ed.is_running(), "opens without a panic");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
}

/// The unnamed-buffer journals in the state directory.
fn unnamed_journals(dir: &Scratch) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir.journals().dir())
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("unnamed.")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// #1192: `ruse` with no file rescues its buffer into a journal named when it started, so a `:cd` before
/// the crash does not move it, and the next pathless start offers it.
#[test]
fn the_unnamed_buffer_is_rescued_and_offered_after_cd() {
    let dir = Scratch::new();
    std::fs::create_dir(dir.path("sub")).unwrap();
    let mut ed = Editor::start(&dir, &[]);
    ed.keys(&format!("iunnamed work{ESC}:cd sub\r"));
    ed.sync();
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    drop(ed);
    assert!(!dir.path(".ruse-journal").exists());
    assert!(!dir.path("sub/.ruse-journal").exists());
    assert_eq!(
        unnamed_journals(&dir).len(),
        1,
        "rescued in the state directory"
    );

    let mut ed = Editor::start(&dir, &[]);
    wait_for_prompt(&mut ed); // the next pathless start offers it
    ed.keys_then_wait_for("r", "unnamed work");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
    assert!(unnamed_journals(&dir).is_empty(), "`:q!` lets it go");
}

/// RFC-0023 §3: a legacy `<file>.ruse-journal` (RJS0) from an older ruse is offered once, and moved to the
/// journal archive after the answer.
#[test]
fn a_legacy_sidecar_journal_is_offered_then_archived() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"disk\n");
    let sidecar = dir.path("f.txt.ruse-journal");
    std::fs::write(&sidecar, ruse_tui::persist::rjs0::frame(b"legacy work\n")).unwrap();
    let mut ed = Editor::start(&dir, &[f_txt()]);
    wait_for_prompt(&mut ed);
    ed.keys("r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"legacy work\n");
    assert!(!sidecar.exists(), "the sidecar left the project tree");
    let archived = std::fs::read_dir(dir.journals().archive_dir())
        .unwrap()
        .count();
    assert_eq!(archived, 1);
    let mut ed = Editor::start(&dir, &[f_txt()]);
    opened_without_offer(&mut ed, "legacy work", "offered once");
}
