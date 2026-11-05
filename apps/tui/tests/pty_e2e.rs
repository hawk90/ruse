//! End-to-end tests of the real binary on a real PTY (#896): keys go through raw-mode input decoding and the
//! session loop, and results are read from disk and the exit status.

#![cfg(unix)]

mod support;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

#[test]
fn insert_then_write_then_quit() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("inew {ESC}:w\r"));
    // #1110: the editor keeps running, so poll the file instead of reading it once.
    assert_eq!(support::wait_for_file(&file, b"new orig\n"), b"new orig\n");
    assert!(ed.is_running());
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn wq_creates_a_missing_file() {
    let dir = Scratch::new();
    let file = dir.path("new.txt");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("ihello{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    // #1107: a new buffer has `'endofline'`, so it saves with a final newline, as in nvim.
    assert_eq!(std::fs::read(&file).unwrap(), b"hello\n");
}

#[test]
fn keys_typed_during_the_startup_probe_are_not_lost() {
    // #1013: typed before the editor is up, so the startup capability probe reads them. This harness
    // answers no query, so the probe reads the keys, waits out its liveness net and gives up.
    let dir = Scratch::new();
    dir.file("f.txt", b"orig\n");
    let file = std::path::Path::new("f.txt");
    let mut ed = Editor::start_typed(&dir, &[file], format!("ihey {ESC}").as_bytes());
    ed.keys(":wq\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"hey orig\n");
}

#[test]
fn q_refuses_a_dirty_buffer_and_q_bang_discards_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    // A relative path keeps the status message on screen (#831 pushes it off after a long name).
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys_then_wait_for(&format!("ix{ESC}:q\r"), "E37: No write since last change");
    assert!(ed.is_running(), ":q must refuse while the buffer is dirty");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
}

#[test]
fn quit_restores_the_terminal() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[&file]);
    assert!(
        ed.screen_bytes().contains("\u{1b}[?1049h"),
        "enters the alt screen"
    );
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    ed.wait_for("\u{1b}[?1049l");
}

/// #842: a failed write must keep the editor, and the unsaved edit, alive. The write is of a NEW file in a
/// read-only directory (an existing writable file there is written in place, #864).
#[test]
fn wq_stays_open_when_the_write_fails() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions. Root ignores the directory mode, so the write would succeed.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = Scratch::new();
    let locked = dir.path("locked");
    std::fs::create_dir(&locked).unwrap();
    let file = locked.join("f.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Writable again on drop, also when a wait below fails, so the scratch directory can be removed.
    struct Unlock<'a>(&'a std::path::Path);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _unlock = Unlock(&locked);

    let mut ed = Editor::start(&dir, &[std::path::Path::new("locked/f.txt")]);
    ed.keys_then_wait_for(&format!("inew {ESC}:wq\r"), "write failed");
    assert!(ed.is_running(), ":wq must not exit after a failed write");
    drop(ed);
    assert!(!file.exists());
}

/// #843: a file that exists but cannot be read must not open as an empty buffer that `:w` then overwrites.
#[test]
fn unreadable_file_refuses_to_open() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions. Root ignores the file mode, so the read would succeed.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"precious\n");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut ed = Editor::start(&dir, &[&file]);
    let status = ed.exit_status();
    let shown = ed.screen_bytes();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(status, 1);
    assert!(shown.contains("ruse: "), "names the failure: {shown:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"precious\n");
}

/// #843: a directory argument is a read error too.
#[test]
fn directory_argument_refuses_to_open() {
    let dir = Scratch::new();
    let sub = dir.path("sub");
    std::fs::create_dir(&sub).unwrap();
    let mut ed = Editor::start(&dir, &[&sub]);
    assert_eq!(ed.exit_status(), 1);
}

/// Start on `file` with a journal of unsaved work, so the recovery prompt shows.
fn start_with_journal(dir: &Scratch) -> (Editor, std::path::PathBuf, std::path::PathBuf) {
    let file = dir.file("f.txt", b"on disk\n");
    let journal = dir
        .journals()
        .write_crashed(Some(&file), b"unsaved work\n")
        .unwrap();
    let ed = Editor::start(dir, &[&file]);
    (ed, file, journal)
}

/// #846: Esc, q and Ctrl-C leave the prompt without deleting the only copy of the work.
#[test]
fn recovery_prompt_quit_keys_keep_the_journal() {
    for key in [ESC, "q", "\u{3}"] {
        let dir = Scratch::new();
        let (mut ed, file, journal) = start_with_journal(&dir);
        ed.keys(key);
        assert_eq!(ed.exit_status(), 0, "key {key:?} quits");
        assert!(journal.exists(), "key {key:?} must keep the journal");
        assert_eq!(std::fs::read(&file).unwrap(), b"on disk\n");
    }
}

/// #846: other keys are ignored, `k` discards (archived, never deleted), and `r` recovers.
#[test]
fn recovery_prompt_needs_an_explicit_choice() {
    let dir = Scratch::new();
    let (mut ed, file, journal) = start_with_journal(&dir);
    ed.keys("x\r");
    assert!(
        ed.is_running() && journal.exists(),
        "unmapped keys do nothing"
    );
    ed.keys("r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"unsaved work\n");

    let dir = Scratch::new();
    let (mut ed, file, journal) = start_with_journal(&dir);
    ed.keys("k:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert!(!journal.exists());
    assert_eq!(std::fs::read(&file).unwrap(), b"on disk\n");
    assert_eq!(
        std::fs::read_dir(dir.journals().archive_dir())
            .unwrap()
            .count(),
        1
    );
}

/// #847: the palette's Quit must not discard unsaved changes.
#[test]
fn palette_quit_refuses_a_dirty_buffer() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    // A relative path keeps the status message on screen (#831 pushes it off after a long name).
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys_then_wait_for(&format!("iUNSAVED{ESC}\u{10}quit\r"), "E162");
    assert!(
        ed.is_running(),
        "palette Quit must refuse while a buffer is dirty"
    );
    ed.keys(":w\r\u{10}quit\r");
    assert_eq!(ed.exit_status(), 0, "palette Quit exits once saved");
    assert_eq!(std::fs::read(&file).unwrap(), b"UNSAVEDorig\n");
}

/// #849: `:w` with no edit must not rewrite the line endings of a mixed file.
#[test]
fn write_keeps_mixed_line_endings() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"a\r\nb\r\nc\n");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(":w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"a\r\nb\r\nc\n");
}

/// #736: closing the last window on a modified buffer hides the buffer; it is not freed.
#[test]
fn close_keeps_a_modified_buffer_and_q_refuses() {
    let dir = Scratch::new();
    dir.file("a.txt", b"one\n");
    let b = dir.file("b.txt", b"two\n");
    // Relative paths keep the status message on screen (#831).
    let mut ed = Editor::start(&dir, &[std::path::Path::new("a.txt")]);
    ed.keys_then_wait_for(":split\r:e b.txt\rx:close\r:q\r", "E162");
    assert!(
        ed.is_running(),
        ":q must refuse while the hidden b.txt is modified"
    );
    ed.keys(":b 2\r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&b).unwrap(), b"wo\n");
}

/// #854: `:%!cat` on input larger than the pipe buffers must finish, not deadlock the editor.
#[test]
fn filter_of_a_large_buffer_does_not_deadlock() {
    let dir = Scratch::new();
    let body = "0123456789abcdef\n".repeat(16 * 1024); // 256 KiB
    let file = dir.file("f.txt", body.as_bytes());
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys(":%!cat\r:wq\r");
    assert_eq!(ed.exit_status(), 0, ":wq runs once the filter finishes");
    assert!(std::fs::read(&file).unwrap() == body.as_bytes());
}

/// #861: a failing filter's stderr is filtered in with its stdout, and the status line says it failed.
/// nvim v0.12.6 writes `out\nerr\n` and shows `shell returned 3` for the same command.
#[test]
fn failing_filter_keeps_its_stderr_and_reports_the_exit_code() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\ntwo\n");
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys_then_wait_for(":%!echo out; echo err >&2; exit 3\r", "shell returned 3");
    ed.keys(":wq\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"out\nerr\n");
}

/// #860: `:r` strips the read file's CRLF and BOM, so the save uses the target buffer's format only.
/// Expected bytes are nvim v0.12.6's for the same files and keys.
#[test]
fn read_file_takes_the_target_buffers_line_endings() {
    let cases: [(&[u8], &[u8], &[u8]); 2] = [
        (
            b"one\r\ntwo\r\n",
            b"X\r\nY\r\n",
            b"one\r\nX\r\nY\r\ntwo\r\n",
        ),
        (
            b"one\ntwo\n",
            b"\xEF\xBB\xBFX\r\nY\r\n",
            b"one\nX\nY\ntwo\n",
        ),
    ];
    for (start, ins, want) in cases {
        let dir = Scratch::new();
        let file = dir.file("f.txt", start);
        dir.file("ins.txt", ins);
        let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
        ed.keys(":r ins.txt\r:wq\r");
        assert_eq!(ed.exit_status(), 0);
        assert_eq!(
            std::fs::read(&file).unwrap(),
            want,
            "{ins:?} into {start:?}"
        );
    }
}

/// Open `start` bytes in a fresh editor, type `keys`, then `:wq`; returns the saved bytes.
fn edit_and_save(start: &[u8], keys: &str) -> Vec<u8> {
    let dir = Scratch::new();
    let file = dir.file("f.txt", start);
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(keys);
    ed.keys(&format!("{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0, "ruse exited cleanly after {keys:?}");
    std::fs::read(&file).unwrap()
}

/// #691: after `:g/x/d` the cursor is on the next line, not a stale offset inside a character.
#[test]
fn global_delete_leaves_the_cursor_on_a_character() {
    let ko = "a\nx\n한글한\n".as_bytes();
    assert_eq!(edit_and_save(ko, "Gl:g/x/d\rx"), "a\n글한\n".as_bytes());
    assert_eq!(edit_and_save(ko, "Gl:g/x/d\r~"), "a\n한글한\n".as_bytes());
    assert_eq!(
        edit_and_save(b"abc\nx\ndef\n", "G:g/x/d\riZ"),
        b"abc\nZdef\n"
    );
}

/// #691: a mark left past a shrinking Ex edit is snapped onto a character boundary.
#[test]
fn ex_delete_snaps_marks_onto_characters() {
    let ko = "a\n한글한\n".as_bytes();
    assert_eq!(edit_and_save(ko, "jlmagg:1d\rd`a"), "글한\n".as_bytes());
    let out = edit_and_save(ko, "jlmagg:1d\rg~`a");
    assert!(std::str::from_utf8(&out).is_ok(), "{out:?}");
}

/// #848: `:e` on a file that is already open focuses its buffer instead of loading a second Document that
/// a later `:w` would use to overwrite the first one's saved edits.
#[test]
fn reopening_an_open_file_reuses_its_buffer() {
    for spelling in ["f.txt", "./f.txt"] {
        let dir = Scratch::new();
        let file = dir.file("f.txt", b"orig\n");
        let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
        ed.keys(&format!(":e {spelling}\riA{ESC}:w\r:b 1\riB{ESC}:w\r:q\r"));
        assert_eq!(ed.exit_status(), 0, ":e {spelling}");
        assert_eq!(std::fs::read(&file).unwrap(), b"BAorig\n", ":e {spelling}");
    }
}

/// #859: a buffer opened with `:e` gets the startup recovery offer. `q` cancels the open and keeps the
/// journal; `r` loads the recovered text as an unsaved change, which `:w` then saves.
#[test]
fn edit_offers_recovery_for_a_journaled_file() {
    let dir = Scratch::new();
    dir.file("a.txt", b"a\n");
    let b = dir.file("b.txt", b"on disk\n");
    let journal = dir
        .journals()
        .write_crashed(Some(&b), b"UNSAVED B\n")
        .unwrap();
    let mut ed = Editor::start(&dir, &[std::path::Path::new("a.txt")]);
    ed.keys_then_wait_for(":e b.txt\r", "Recovery journal for");
    ed.keys_then_wait_for("q:e b.txt\r", "Recovery journal for");
    assert!(ed.is_running());
    assert!(journal.exists(), "q keeps the journal");
    assert_eq!(std::fs::read(&b).unwrap(), b"on disk\n");
    ed.keys_then_wait_for("r:q\r", "E37: No write since last change");
    assert!(ed.is_running(), "the recovered text is an unsaved change");
    ed.keys(":w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&b).unwrap(), b"UNSAVED B\n");
    assert!(!journal.exists(), "the save made the journal redundant");
}

/// #857: a fatal signal journals a short unsaved edit, restores the terminal, and exits as killed by it.
fn signal_rescues_the_edit(sig: libc::c_int) {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys(&format!("iabcde{ESC}"));
    ed.sync(); // the edit is applied before the signal arrives
    ed.signal(sig);
    assert_eq!(ed.exit_status(), -sig, "exits as killed by signal {sig}");
    ed.wait_for("\u{1b}[?1049l");
    assert_eq!(
        dir.journals().replay(Some(&file)).as_deref(),
        Some(&b"abcdeorig\n"[..]),
        "journal holds the edit after signal {sig}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
}

#[test]
fn sigterm_journals_the_edit_and_restores_the_terminal() {
    signal_rescues_the_edit(libc::SIGTERM);
}

#[test]
fn sighup_journals_the_edit_and_restores_the_terminal() {
    signal_rescues_the_edit(libc::SIGHUP);
}

/// #856: the rescue covers every modified buffer, not only the focused one.
#[test]
fn signal_journals_every_modified_buffer() {
    let dir = Scratch::new();
    let a = dir.file("a.txt", b"a\n");
    let b = dir.file("b.txt", b"b\n");
    let mut ed = Editor::start(&dir, &[std::path::Path::new("a.txt")]);
    ed.keys(&format!("iAA{ESC}:e b.txt\r"));
    ed.keys(&format!("iBB{ESC}:b 1\r"));
    ed.sync();
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    let j = dir.journals();
    assert_eq!(j.replay(Some(&a)).as_deref(), Some(&b"AAa\n"[..]));
    assert_eq!(j.replay(Some(&b)).as_deref(), Some(&b"BBb\n"[..]));
}

/// #1085: an emptied buffer saves as an empty file (nvim v0.12.6 writes 0 bytes for `ggdG:w`, its
/// `ML_EMPTY` buffer), while one empty line saves as `\n`. #1107: text typed into it keeps the file's
/// `'endofline'`, so `ia` gives `a\n`, as in nvim. Not pinned: `o` there gives `\n` where nvim writes
/// `\n\n`, because the bytes `\n` cannot tell one empty line from an emptied buffer plus `o`.
#[test]
fn an_emptied_buffer_saves_like_nvim() {
    assert_eq!(edit_and_save(b"a\nb\n", "ggdG"), b"");
    assert_eq!(edit_and_save(b"a\n", "S"), b"\n");
    assert_eq!(edit_and_save(b"a", "dd"), b"");
    assert_eq!(edit_and_save(b"a\nb\n", "ggdGia"), b"a\n");
    assert_eq!(edit_and_save(b"", "ihello"), b"hello\n");
}

/// #1107: a file opened without a final newline keeps it missing (F-008 byte-preserve), as nvim does with
/// `'nofixeol'`: `ob` gives `a\nb`, and emptied and retyped it still has none. `:set fixeol` adds it.
#[test]
fn a_file_without_a_final_newline_keeps_it_missing() {
    assert_eq!(edit_and_save(b"a", "ob"), b"a\nb");
    assert_eq!(edit_and_save(b"a", "ddix"), b"x");
    assert_eq!(edit_and_save(b"a\nb", "ggdGia"), b"a");
    assert_eq!(edit_and_save(b"a", ":set fixeol\r"), b"a\n");
}

/// The command line shows a `c_CTRL-V` literal `<CR>` as `^M` and puts the terminal caret on its own caret
/// (#612): after `<Left><Left>` it sits before `cd`, not at the end of the line.
#[test]
fn cmdline_shows_control_chars_and_its_caret() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(":ab");
    // The frame diff repaints only the changed cells: the two cells of `^M`.
    ed.keys_then_wait_for("\u{16}\r", "^M");
    ed.keys("cd");
    let before = ed.mark();
    // Two `<Left>` arrows, each in one read so they decode as arrows, not as `<Esc>` and text.
    ed.raw_chunks(&[b"\x1b[D", b"\x1b[D"]);
    // `:ab^M` is five cells, so the caret is in column 6 of the last row (1-based).
    ed.wait_for_after(before, "\u{1b}[24;6H");
    ed.keys(&format!("{ESC}:q\r"));
    assert_eq!(ed.exit_status(), 0);
}

/// #1198: a wait that gives up says what a flake report needs: how long it waited, how many bytes came,
/// whether the child is still running, and the screen as text. A child that exited fails the wait at once.
#[test]
fn a_failed_wait_reports_time_bytes_status_and_screen() {
    let dir = Scratch::new();
    dir.file("f.txt", b"visible\n");
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    let started = std::time::Instant::now();
    // On its own thread, so the wait's panic comes back as that thread's result.
    let err = std::thread::spawn(move || ed.wait_for("never drawn"))
        .join()
        .expect_err("the text never appears");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let msg = err.downcast_ref::<String>().expect("a formatted message");
    assert!(
        msg.contains("no \"never drawn\" on screen: after "),
        "{msg}"
    );
    assert!(
        msg.contains(" bytes received, child exited with status 0"),
        "{msg}"
    );
    assert!(msg.contains("visible"), "{msg}");
}
