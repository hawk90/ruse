//! Filesystem failure modes, hostile paths and odd file names through the real binary (#907). Each case
//! asserts the two things a user needs: no data is lost, and the failure is visible. Every wait is bounded
//! by the PTY harness; paths are passed relative to the scratch directory so the status line fits (#831).
//!
//! Not covered: a full disk cannot be simulated portably without root or a loop-mounted filesystem.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions. Root ignores permission bits, so those cases cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mkfifo(p: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
}

/// A mode-0200 (write-only) file is refused at startup, as #1039 made every unreadable file: it must not
/// open as an empty buffer that `:w` then truncates. (Neovim opens an empty `[Permission Denied]` buffer
/// and refuses `:w` with E45; ruse refuses the open.)
#[test]
fn write_only_file_refuses_to_open() {
    if is_root() {
        return;
    }
    let dir = Scratch::new();
    let file = dir.file("wo.txt", b"secret\n");
    chmod(&file, 0o200);
    let mut ed = Editor::start(&dir, &[Path::new("wo.txt")]);
    let status = ed.exit_status();
    let shown = ed.screen_bytes();
    chmod(&file, 0o644);
    assert_eq!(status, 1);
    assert!(
        shown.contains("ruse: wo.txt: "),
        "names the failure: {shown:?}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"secret\n");
}

/// `:w` into a read-only directory fails visibly and keeps the edit: the buffer stays modified (`:q`
/// refuses) and the file on disk is untouched.
#[test]
fn write_into_a_read_only_directory_fails_visibly() {
    if is_root() {
        return;
    }
    let dir = Scratch::new();
    let locked = dir.path("ro");
    std::fs::create_dir(&locked).unwrap();
    let file = locked.join("f.txt");
    std::fs::write(&file, b"orig\n").unwrap();
    chmod(&locked, 0o555);
    let mut ed = Editor::start(&dir, &[Path::new("ro/f.txt")]);
    ed.keys(&format!("inew {ESC}:w\r"));
    let shown = ed.screen_bytes();
    ed.keys(":q\r");
    let running = ed.is_running();
    drop(ed);
    chmod(&locked, 0o755);
    assert!(shown.contains("write failed"), "visible error: {shown:?}");
    assert!(running, ":q must refuse the unsaved edit");
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
}

/// The file's directory disappears while it is open: `:w` fails visibly, the edit survives in the buffer,
/// and once the directory is back the same `:w` writes it (nothing was lost in between).
#[test]
fn write_after_the_directory_disappears_keeps_the_edit() {
    let dir = Scratch::new();
    let sub = dir.path("gone");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("f.txt"), b"orig\n").unwrap();
    let mut ed = Editor::start(&dir, &[Path::new("gone/f.txt")]);
    ed.keys(&format!("inew {ESC}"));
    std::fs::remove_dir_all(&sub).unwrap();
    ed.keys(":w\r");
    assert!(
        ed.screen_bytes().contains("write failed"),
        "visible error: {:?}",
        ed.screen_bytes()
    );
    ed.keys(":q\r");
    assert!(ed.is_running(), ":q must refuse the unsaved edit");
    std::fs::create_dir(&sub).unwrap();
    ed.keys(":wq\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(sub.join("f.txt")).unwrap(), b"new orig\n");
}

/// A recovery-journal append that fails (the directory is read-only, so `f.txt.ruse-journal` cannot be
/// created) is shown, as Neovim's E303 is, instead of leaving the work silently unprotected.
#[test]
fn journal_append_failure_is_visible() {
    if is_root() {
        return;
    }
    let dir = Scratch::new();
    let locked = dir.path("ro");
    std::fs::create_dir(&locked).unwrap();
    let file = locked.join("f.txt");
    std::fs::write(&file, b"orig\n").unwrap();
    chmod(&locked, 0o555);
    let mut ed = Editor::start(&dir, &[Path::new("ro/f.txt")]);
    ed.keys("i");
    // The journal is throttled to every 8th change; one key per frame makes each a change.
    for c in "abcdefghijkl".chars() {
        ed.keys(&c.to_string());
    }
    ed.keys(ESC);
    let shown = ed.screen_bytes();
    drop(ed);
    chmod(&locked, 0o755);
    assert!(shown.contains("E303"), "journal failure shown: {shown:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
}

/// Start on a hostile path and expect a prompt refusal naming `why`.
fn refuses_quickly(dir: &Scratch, arg: &str, why: &str) {
    let t = Instant::now();
    let mut ed = Editor::start(dir, &[Path::new(arg)]);
    assert_eq!(ed.exit_status(), 1, "{arg}");
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "{arg} took {:?}",
        t.elapsed()
    );
    let shown = ed.screen_bytes();
    assert!(shown.contains(why), "{arg}: {shown:?}");
}

/// A FIFO with no writer is refused at once. Neovim v0.12.6 blocks in open() before any UI (and ignores
/// SIGTERM there); ruse opens it without blocking and reports that nothing is writing.
#[test]
fn fifo_without_a_writer_does_not_hang() {
    let dir = Scratch::new();
    mkfifo(&dir.path("pipe"));
    refuses_quickly(&dir, "pipe", "is a FIFO with no writer");
}

/// A character device is not read without end (`/dev/zero` would exhaust memory); Neovim says the same
/// `is not a file`.
#[test]
fn device_is_not_a_file() {
    let dir = Scratch::new();
    refuses_quickly(&dir, "/dev/zero", "is not a file");
}

/// The same paths through `:e` keep the session alive and report the failure in the status line.
#[test]
fn edit_of_hostile_paths_reports_and_keeps_the_session() {
    let dir = Scratch::new();
    mkfifo(&dir.path("pipe"));
    std::fs::create_dir(dir.path("sub")).unwrap();
    let file = dir.file("f.txt", b"orig\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    for (arg, why) in [
        ("pipe", "is a FIFO with no writer"),
        ("/dev/zero", "is not a file"),
        ("sub", "is a directory"),
    ] {
        let t = Instant::now();
        ed.keys(&format!(":e {arg}\r"));
        assert!(t.elapsed() < Duration::from_secs(5), ":e {arg} hung");
        let shown = ed.screen_bytes();
        assert!(
            shown.contains(&format!("E484: can't open {arg}: {why}")),
            "{arg}: {shown:?}"
        );
        assert!(ed.is_running());
    }
    ed.keys(&format!("ix{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"xorig\n");
}

/// Open, edit, save and journal a file under `name`: the bytes land in that exact file, and the crash
/// journal is written next to it under the same odd name.
fn round_trips(dir: &Scratch, name: &Path) {
    let file = dir.0.join(name);
    let mut ed = Editor::start(dir, &[name]);
    ed.settle();
    ed.keys(&format!("ix{ESC}:w\r"));
    // Bounded poll: on a loaded CI runner the write can land after the output went quiet.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut on_disk = std::fs::read(&file).unwrap();
    while on_disk != b"xorig\n" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        on_disk = std::fs::read(&file).unwrap();
    }
    if on_disk != b"xorig\n" {
        let shown = ed.screen_bytes();
        let tail: String = shown
            .chars()
            .rev()
            .take(1500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let listing: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        panic!(
            "{name:?}: disk {on_disk:?}, running {}, files {listing:?}, screen tail {tail:?}",
            ed.is_running()
        );
    }
    ed.keys(&format!("iy{ESC}"));
    ed.signal(libc::SIGTERM);
    assert_eq!(ed.exit_status(), -libc::SIGTERM);
    assert_eq!(
        ruse_tui::persist::journal::replay(Some(&file)).as_deref(),
        Some(&b"yxorig\n"[..]),
        "{name:?}"
    );
}

#[test]
fn leading_dash_name_round_trips() {
    let dir = Scratch::new();
    dir.file("-dash.txt", b"orig\n");
    round_trips(&dir, Path::new("-dash.txt"));
}

#[test]
fn newline_in_name_round_trips() {
    let dir = Scratch::new();
    dir.file("a\nb.txt", b"orig\n");
    round_trips(&dir, Path::new("a\nb.txt"));
}

/// A non-UTF-8 name: `std::env::args` used to panic on it before anything opened. Skipped where the
/// filesystem rejects such names (APFS on macOS does, with EILSEQ).
#[test]
fn non_utf8_name_round_trips() {
    use std::os::unix::ffi::OsStrExt;
    let dir = Scratch::new();
    let name = Path::new(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
    if std::fs::write(dir.0.join(name), b"orig\n").is_err() {
        return;
    }
    round_trips(&dir, name);
}
