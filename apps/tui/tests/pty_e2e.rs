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
    assert_eq!(std::fs::read(&file).unwrap(), b"new orig\n");
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
    assert!(std::fs::read(&file).unwrap().starts_with(b"hello"));
}

#[test]
fn q_refuses_a_dirty_buffer_and_q_bang_discards_it() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("ix{ESC}:q\r"));
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
    ed.settle();
    assert!(
        ed.screen_bytes().contains("\u{1b}[?1049l"),
        "leaves the alt screen"
    );
}

/// #842: a failed write must keep the editor, and the unsaved edit, alive.
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
    std::fs::write(&file, b"orig\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    let mut ed = Editor::start(&dir, &[&file]);
    ed.keys(&format!("inew {ESC}:wq\r"));
    let running = ed.is_running();
    drop(ed);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(running, ":wq must not exit after a failed write");
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
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
    ruse_tui::persist::journal::append(Some(&file), b"unsaved work\n").unwrap();
    let journal = ruse_tui::persist::journal::journal_path(Some(&file));
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

/// #846: other keys are ignored, `d` deletes, and `r` recovers.
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
    ed.keys("d:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert!(!journal.exists());
    assert_eq!(std::fs::read(&file).unwrap(), b"on disk\n");
}

/// #847: the palette's Quit must not discard unsaved changes.
#[test]
fn palette_quit_refuses_a_dirty_buffer() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    // A relative path keeps the status message on screen (#831 pushes it off after a long name).
    let mut ed = Editor::start(&dir, &[std::path::Path::new("f.txt")]);
    ed.keys(&format!("iUNSAVED{ESC}\u{10}quit\r"));
    assert!(
        ed.is_running(),
        "palette Quit must refuse while a buffer is dirty"
    );
    let screen = ed.screen_bytes();
    let tail: String = screen
        .chars()
        .rev()
        .take(600)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    assert!(screen.contains("E162"), "{tail:?}");
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
    ed.keys(":split\r:e b.txt\rx:close\r:q\r");
    assert!(
        ed.is_running(),
        ":q must refuse while the hidden b.txt is modified"
    );
    assert!(ed.screen_bytes().contains("E162"));
    ed.keys(":b 2\r:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&b).unwrap(), b"wo\n");
}
