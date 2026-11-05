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

/// `:w` of a writable file in a read-only directory writes it in place, as Neovim v0.12.6 does (#864;
/// before, the save failed because the temp could not be created). A NEW file there still fails visibly
/// and keeps the edit: the buffer stays modified (`:q` refuses).
#[test]
fn write_into_a_read_only_directory() {
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
    let written = ed.screen_bytes();
    ed.keys(&format!(":e ro/g.txt\rinew {ESC}:w\r"));
    let failed = ed.screen_bytes();
    ed.keys(":q\r");
    let running = ed.is_running();
    drop(ed);
    chmod(&locked, 0o755);
    assert!(
        written.contains("\"ro/f.txt\" 1L, 9B written"),
        "{written:?}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"new orig\n");
    assert!(failed.contains("write failed"), "visible error: {failed:?}");
    assert!(running, ":q must refuse the unsaved edit");
    assert!(!locked.join("g.txt").exists());
}

/// #864: `:w` on one name of a hard-linked file updates every name, as Neovim v0.12.6 does (before, the
/// rename split the link: `f` had the edit, `g` kept the old bytes).
#[test]
fn write_keeps_a_hard_link() {
    let dir = Scratch::new();
    let f = dir.file("f.txt", b"v1\n");
    std::fs::hard_link(&f, dir.path("g.txt")).unwrap();
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&format!("Ax{ESC}:wq\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(dir.path("g.txt")).unwrap(), b"v1x\n");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(&f).unwrap().nlink(), 2);
}

/// #864: a mode-0444 file is not silently overwritten: `:w` is refused with nvim's E505 and the buffer
/// stays modified; `:w!` writes it and keeps the mode, as Neovim v0.12.6 does (nvim says E45 here because it
/// sets 'readonly' at load; ruse has no 'readonly' yet, #619, so the write-time check reports E505).
#[test]
fn read_only_file_needs_a_bang() {
    if is_root() {
        return;
    }
    let dir = Scratch::new();
    let file = dir.file("r.txt", b"ro\n");
    chmod(&file, 0o444);
    let mut ed = Editor::start(&dir, &[Path::new("r.txt")]);
    ed.keys(&format!("Ax{ESC}:w\r"));
    let refused = ed.screen_bytes();
    assert!(
        refused.contains("E505: \"r.txt\" is read-only (add ! to override)"),
        "{refused:?}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"ro\n");
    ed.keys(":q\r");
    assert!(ed.is_running(), ":q must refuse the unsaved edit");
    ed.keys(":w!\r");
    assert!(
        ed.screen_bytes().contains("\"r.txt\" 1L, 4B written"),
        "{:?}",
        ed.screen_bytes()
    );
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"rox\n");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    chmod(&file, 0o644);
    assert_eq!(mode, 0o444);
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

/// A journal write that fails (the journal directory is read-only) is shown, as Neovim's E303 is, instead
/// of leaving the work silently unprotected. A read-only PROJECT directory no longer matters: journals live
/// in the state directory (RFC-0023 §3).
#[test]
fn journal_append_failure_is_visible() {
    if is_root() {
        return;
    }
    // 200 changes (one per typed character) trigger a sync without waiting for the idle timer.
    let burst = format!("i{}{ESC}", "x".repeat(200));
    let dir = Scratch::new();
    let locked = dir.path("ro");
    std::fs::create_dir(&locked).unwrap();
    let file = locked.join("f.txt");
    std::fs::write(&file, b"orig\n").unwrap();
    chmod(&locked, 0o555);
    let mut ed = Editor::start(&dir, &[Path::new("ro/f.txt")]);
    ed.keys(&burst);
    let journal = dir.journals().path_for(&file);
    let journaled = support::wait_until(Duration::from_secs(5), || journal.exists());
    let shown = ed.screen_bytes();
    drop(ed);
    chmod(&locked, 0o755);
    assert!(journaled, "a file in a read-only directory is journaled");
    assert!(!shown.contains("E303"), "{shown:?}");

    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let jdir = dir.journals().dir().to_path_buf();
    std::fs::create_dir_all(&jdir).unwrap();
    chmod(&jdir, 0o555);
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&burst);
    let shown = ed.screen_bytes();
    drop(ed);
    chmod(&jdir, 0o755);
    assert!(shown.contains("E303"), "journal failure shown: {shown:?}");
    assert_eq!(std::fs::read(&file).unwrap(), b"orig\n");
}

/// RFC-0023 §8 (#938): while the journal cannot be written, the status line shows `[journal!]` after
/// `[+]`; the first failure names the journal; `:checkhealth` reports it; a sync that works again (`:!`
/// syncs first) takes the flag away.
#[test]
fn a_failing_journal_shows_journal_bang_until_a_sync_works() {
    if is_root() {
        return;
    }
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"orig\n");
    let journal = dir.journals().path_for(&file);
    let jdir = dir.journals().dir().to_path_buf();
    std::fs::create_dir_all(&jdir).unwrap();
    chmod(&jdir, 0o555);
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    // 200 changes trigger a sync; the first failure is shown (the journal path follows the error, past
    // the 80 columns here; the unit test `journal_outcome_names_the_journal` reads it).
    let before = ed.screen_bytes().len();
    ed.keys(&format!("i{}{ESC}", "x".repeat(200)));
    let shown = ed.screen_bytes()[before..].to_string();
    let named = shown.contains("E303: unable to write the recovery journal for");
    // A command line gives the status line back whole.
    let before = ed.screen_bytes().len();
    ed.keys(":set ts?\r");
    let bar = ed.screen_bytes()[before..].to_string();
    let flagged = bar.contains("f.txt [+][journal!]    tabstop=4");
    let before = ed.screen_bytes().len();
    ed.keys(":checkhealth\r");
    let health = ed.screen_bytes()[before..].to_string();
    ed.keys(ESC);
    chmod(&jdir, 0o755);
    assert!(named, "{shown:?}");
    assert!(flagged, "{bar:?}");
    assert!(
        health.contains("WARN journal: f.txt: last write failed"),
        "{health:?}"
    );
    let before = ed.screen_bytes().len();
    ed.keys(":!true\r\r");
    assert!(support::wait_until(Duration::from_secs(5), || journal.exists()));
    ed.keys(":checkhealth\r");
    let after = ed.screen_bytes()[before..].to_string();
    assert!(
        after.contains("OK   journal: f.txt: ") && after.contains("synced 0s ago"),
        "{:?}",
        &after[after.len().saturating_sub(1500)..]
    );
    ed.keys(ESC);
    let before = ed.screen_bytes().len();
    ed.keys(":set ts?\r");
    let bar = ed.screen_bytes()[before..].to_string();
    assert!(bar.contains("f.txt [+]    tabstop=4"), "{bar:?}");
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
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
/// journal is written for it under a name derived from the odd one.
fn round_trips(dir: &Scratch, name: &Path) {
    let file = dir.0.join(name);
    // `--` first: a leading-dash name is otherwise an unknown option (#896), as in nvim.
    let mut ed = Editor::start(dir, &[Path::new("--"), name]);
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
        dir.journals().replay(Some(&file)).as_deref(),
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
