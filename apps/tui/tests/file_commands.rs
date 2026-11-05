//! The Ex file commands through the real binary (#610): the `:w` variants (`:w {file}`, `:w!`, `:[range]w`,
//! `:w >>`, `:w !cmd`), `:up`, `:sav`, `:wa`/`:wqa`, `:x`, and `:e` with `!`, `+cmd`, `++ff` and filename
//! modifiers. Messages and E-numbers follow nvim v0.12.6. Paths are relative to the scratch directory, the
//! editor's working directory, so the status line fits (#831).

#![cfg(unix)]

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

/// `name` once it holds `want`, or what it holds after a bounded wait: on a loaded runner a write can land
/// after the editor's output went quiet, so one read right after `:w` races (#1110).
fn read_written(dir: &Scratch, name: &str, want: &str) -> String {
    String::from_utf8_lossy(&support::wait_for_file(&dir.path(name), want.as_bytes())).into_owned()
}

/// `:w {file}` writes a copy and leaves the buffer on its own file; an existing target needs `!` (E13); a
/// partial range needs `!` on the buffer's own file (E140); `:w >>` appends; `:w !cmd` feeds a command;
/// `%:r` names the current file without its extension.
#[test]
fn write_variants_reach_the_right_files() {
    let dir = Scratch::new();
    dir.file("f.txt", b"a\nb\nc\n");
    dir.file("g.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":w g.txt\r");
    assert!(ed.screen_bytes().contains("E13: File exists"));
    assert_eq!(
        read_written(&dir, "g.txt", "x\n"),
        "x\n",
        "E13 leaves the file alone"
    );
    ed.keys(":w new.txt\r");
    assert!(ed
        .screen_bytes()
        .contains("\"new.txt\" [New] 3L, 6B written"));
    ed.keys(":w >> g.txt\r");
    assert!(ed.screen_bytes().contains("\"g.txt\" 3L, 6B appended"));
    assert_eq!(read_written(&dir, "g.txt", "x\na\nb\nc\n"), "x\na\nb\nc\n");
    ed.keys(":2,3w\r");
    assert!(ed
        .screen_bytes()
        .contains("E140: Use ! to write partial buffer"));
    ed.keys(":2,3w! part.txt\r");
    assert_eq!(read_written(&dir, "part.txt", "b\nc\n"), "b\nc\n");
    ed.keys(":2w !cat > piped.txt\r");
    assert_eq!(read_written(&dir, "piped.txt", "b\n"), "b\n");
    ed.keys(":w %:r.bak\r");
    assert_eq!(read_written(&dir, "f.bak", "a\nb\nc\n"), "a\nb\nc\n");
    ed.keys(":w >> nofile.txt\r");
    assert!(ed.screen_bytes().contains("E212"), "append needs the file");
    assert!(!dir.path("nofile.txt").exists());
    assert_eq!(
        read_written(&dir, "f.txt", "a\nb\nc\n"),
        "a\nb\nc\n",
        "the own file is untouched"
    );
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0, "no write made the buffer dirty");
}

/// `:up` writes only a modified buffer; `:sav` moves the buffer to a new file, so the next `:w` goes there;
/// `:x` writes a modified buffer and quits.
#[test]
fn update_saveas_and_exit_write() {
    let dir = Scratch::new();
    dir.file("f.txt", b"abc\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys("x:up\r");
    assert_eq!(read_written(&dir, "f.txt", "bc\n"), "bc\n");
    ed.keys(":sav\r");
    assert!(ed.screen_bytes().contains("E471: Argument required"));
    ed.keys(":sav moved.txt\r");
    // The durable save's fsyncs can keep the screen quiet past the harness's settle time: wait for it.
    assert!(support::wait_until(Duration::from_secs(10), || ed
        .screen_bytes()
        .contains("\"moved.txt\" [New] 1L, 3B written")));
    ed.keys("x:x\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(read_written(&dir, "moved.txt", "c\n"), "c\n");
    assert_eq!(
        read_written(&dir, "f.txt", "bc\n"),
        "bc\n",
        ":sav left the old file behind"
    );
}

/// `:e +2 {file}` opens on line 2; a bare `:e` refuses unsaved changes (E37) and `:e!` drops them; a file
/// that does not exist opens empty and `:w` creates it.
#[test]
fn edit_variants() {
    let dir = Scratch::new();
    dir.file("f.txt", b"one\n");
    dir.file("g.txt", b"a\nb\nc\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":e +2 g.txt\r");
    ed.keys("dd:w\r");
    assert_eq!(
        read_written(&dir, "g.txt", "a\nc\n"),
        "a\nc\n",
        "+2 put the cursor on line 2"
    );
    ed.keys("dd:e\r");
    assert!(ed
        .screen_bytes()
        .contains("E37: No write since last change"));
    ed.keys(":e!\r:w\r");
    assert_eq!(
        read_written(&dir, "g.txt", "a\nc\n"),
        "a\nc\n",
        ":e! dropped the change"
    );
    ed.keys(":e new.txt\r");
    assert!(ed.screen_bytes().contains("\"new.txt\" [New]"));
    ed.keys(&format!("ihello{ESC}:w\r"));
    // A new file has 'endofline', so it gets a final newline, as in nvim (#1107).
    assert_eq!(read_written(&dir, "new.txt", "hello\n"), "hello\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// `:wa` writes every modified buffer that has a file; `:wqa` then quits.
#[test]
fn write_all_writes_every_modified_buffer() {
    let dir = Scratch::new();
    dir.file("f.txt", b"f\n");
    dir.file("g.txt", b"g\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&format!("A1{ESC}:e g.txt\rA2{ESC}:wa\r"));
    assert_eq!(read_written(&dir, "f.txt", "f1\n"), "f1\n");
    assert_eq!(read_written(&dir, "g.txt", "g2\n"), "g2\n");
    ed.keys(&format!("A3{ESC}:wqa\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(read_written(&dir, "g.txt", "g23\n"), "g23\n");
}

/// A buffer with no file takes the name it is first written to, so it is saved and `:q` quits; `:wa` on a
/// modified buffer with no file is E141.
#[test]
fn unnamed_buffer_takes_the_name_it_is_written_to() {
    let dir = Scratch::new();
    let mut ed = Editor::start(&dir, &[]);
    ed.keys(&format!("ihi{ESC}:wa\r"));
    assert!(ed.screen_bytes().contains("E141: No file name for buffer"));
    ed.keys(":w out.txt\r");
    assert_eq!(
        read_written(&dir, "out.txt", "hi\n"),
        "hi\n",
        "a new buffer has 'endofline', as in nvim (#1107)"
    );
    ed.keys("x:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(
        read_written(&dir, "out.txt", "h\n"),
        "h\n",
        ":w now writes the adopted file"
    );
}

/// `:e ++ff=dos` reads CRLF line endings off a file whose bare `\n` would otherwise make it LF, and the
/// CRLF style is written back.
#[test]
fn edit_with_forced_file_format() {
    let dir = Scratch::new();
    dir.file("f.txt", b"x\n");
    dir.file("d.txt", b"a\r\nb\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":e ++ff=dos d.txt\r");
    ed.keys(&format!("A!{ESC}:w\r:q\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(fs::read(dir.path("d.txt")).unwrap(), b"a!\r\nb\r\n");
}

/// `:w {file}` from a buffer with a file, `:sav` and `:file {name}` leave the other name as the alternate
/// file (`"#`, `:e #`), as nvim v0.12.6 does; `:0file` unnames the buffer, so `:w` is E32.
#[test]
fn writes_and_renames_set_the_alternate_file() {
    let dir = Scratch::new();
    dir.file("f.txt", b"x\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":w other.txt\r\"#p:w\r");
    assert_eq!(
        read_written(&dir, "f.txt", "xother.txt\n"),
        "xother.txt\n",
        "`#` is the name written to"
    );
    ed.keys(":e #\rx:w\r");
    assert_eq!(
        read_written(&dir, "other.txt", "\n"),
        "\n",
        ":e # opened the written copy"
    );
    ed.keys(":sav moved.txt\r0\"#P:w\r");
    assert_eq!(
        read_written(&dir, "moved.txt", "other.txt\n"),
        "other.txt\n",
        ":sav left the old name as `#`"
    );
    ed.keys(":file renamed.txt\r0\"#P:w\r");
    assert_eq!(
        read_written(&dir, "renamed.txt", "moved.txtother.txt\n"),
        "moved.txtother.txt\n",
        ":file renames the buffer and keeps the old name as `#`"
    );
    ed.keys(":0file\r:w\r");
    assert!(ed.screen_bytes().contains("E32: No file name"));
    ed.keys(":qa!\r");
    assert_eq!(ed.exit_status(), 0);
}

/// `:find` looks in the current file's directory, then the working directory (`'path'` is `.,,`), and
/// fails with E345; `:sf` splits first; `:drop` goes to the window that shows the file; `:vi` is `:e`.
#[test]
fn find_drop_and_visual_open_files() {
    let dir = Scratch::new();
    fs::create_dir(dir.path("sub")).unwrap();
    dir.file("f.txt", b"f\n");
    dir.file("sub/g.txt", b"g\n");
    dir.file("sub/h.txt", b"h\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":find g.txt\r");
    assert!(ed
        .screen_bytes()
        .contains("E345: Can't find file \"g.txt\" in path"));
    ed.keys(&format!(":find sub/g.txt\rA1{ESC}:w\r"));
    assert_eq!(read_written(&dir, "sub/g.txt", "g1\n"), "g1\n");
    ed.keys(&format!(":find h.txt\rA2{ESC}:w\r"));
    assert_eq!(
        read_written(&dir, "sub/h.txt", "h2\n"),
        "h2\n",
        "found next to the current file"
    );
    ed.keys(&format!(":sf f.txt\rA3{ESC}:w\r"));
    assert_eq!(
        read_written(&dir, "f.txt", "f3\n"),
        "f3\n",
        "found in the working directory"
    );
    // Two windows now, one on f.txt: from the other one, `:drop` goes back to it.
    ed.keys(&format!("\u{17}w:drop f.txt\rA4{ESC}:w\r"));
    assert_eq!(read_written(&dir, "f.txt", "f34\n"), "f34\n");
    ed.keys(&format!(":vi sub/g.txt\rA5{ESC}:w\r"));
    assert_eq!(read_written(&dir, "sub/g.txt", "g15\n"), "g15\n");
    ed.keys(":qa!\r");
    assert_eq!(ed.exit_status(), 0);
}

/// `:cd` changes the working directory and keeps the buffer on its file; `:cd -` goes back; a missing
/// directory is E344.
#[test]
fn cd_keeps_buffers_on_their_files() {
    let dir = Scratch::new();
    fs::create_dir(dir.path("sub")).unwrap();
    dir.file("f.txt", b"f\n");
    dir.file("sub/g.txt", b"g\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&format!(":cd sub\rA1{ESC}:w\r"));
    assert_eq!(
        read_written(&dir, "f.txt", "f1\n"),
        "f1\n",
        "the buffer still writes its own file"
    );
    assert!(!dir.path("sub/f.txt").exists());
    ed.keys(&format!(":e g.txt\rA2{ESC}:w\r"));
    assert_eq!(
        read_written(&dir, "sub/g.txt", "g2\n"),
        "g2\n",
        "a relative name is under the new directory"
    );
    ed.keys(&format!(":cd -\r:e f.txt\rA3{ESC}:w\r"));
    assert_eq!(
        read_written(&dir, "f.txt", "f13\n"),
        "f13\n",
        "`:cd -` went back"
    );
    ed.keys(":cd nosuch\r");
    assert!(ed
        .screen_bytes()
        .contains("E344: Can't find directory \"nosuch\" in cdpath"));
    ed.keys(":qa!\r");
    assert_eq!(ed.exit_status(), 0);
}

/// `:keepalt` leaves the alternate file as it was (#609).
#[test]
fn keepalt_keeps_the_alternate_file() {
    let dir = Scratch::new();
    dir.file("f.txt", b"f\n");
    dir.file("g.txt", b"g\n");
    dir.file("h.txt", b"\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":e g.txt\r:keepalt e h.txt\r\"#P:w\r");
    assert_eq!(
        read_written(&dir, "h.txt", "f.txt\n"),
        "f.txt\n",
        "`#` is still f.txt"
    );
    ed.keys(":qa!\r");
    assert_eq!(ed.exit_status(), 0);
}
