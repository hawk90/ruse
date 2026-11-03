//! The Ex file commands through the real binary (#610): the `:w` variants (`:w {file}`, `:w!`, `:[range]w`,
//! `:w >>`, `:w !cmd`), `:up`, `:sav`, `:wa`/`:wqa`, `:x`, and `:e` with `!`, `+cmd`, `++ff` and filename
//! modifiers. Messages and E-numbers follow nvim v0.12.6. Paths are relative to the scratch directory, the
//! editor's working directory, so the status line fits (#831).

#![cfg(unix)]

mod support;

use std::fs;
use std::path::Path;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

fn read(dir: &Scratch, name: &str) -> String {
    fs::read_to_string(dir.path(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
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
    assert_eq!(read(&dir, "g.txt"), "x\n", "E13 leaves the file alone");
    ed.keys(":w new.txt\r");
    assert!(ed
        .screen_bytes()
        .contains("\"new.txt\" [New] 3L, 6B written"));
    ed.keys(":w >> g.txt\r");
    assert!(ed.screen_bytes().contains("\"g.txt\" 3L, 6B appended"));
    assert_eq!(read(&dir, "g.txt"), "x\na\nb\nc\n");
    ed.keys(":2,3w\r");
    assert!(ed
        .screen_bytes()
        .contains("E140: Use ! to write partial buffer"));
    ed.keys(":2,3w! part.txt\r");
    assert_eq!(read(&dir, "part.txt"), "b\nc\n");
    ed.keys(":2w !cat > piped.txt\r");
    assert_eq!(read(&dir, "piped.txt"), "b\n");
    ed.keys(":w %:r.bak\r");
    assert_eq!(read(&dir, "f.bak"), "a\nb\nc\n");
    ed.keys(":w >> nofile.txt\r");
    assert!(ed.screen_bytes().contains("E212"), "append needs the file");
    assert!(!dir.path("nofile.txt").exists());
    assert_eq!(
        read(&dir, "f.txt"),
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
    assert_eq!(read(&dir, "f.txt"), "bc\n");
    ed.keys(":sav\r");
    assert!(ed.screen_bytes().contains("E471: Argument required"));
    ed.keys(":sav moved.txt\r");
    assert!(ed
        .screen_bytes()
        .contains("\"moved.txt\" [New] 1L, 3B written"));
    ed.keys("x:x\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(read(&dir, "moved.txt"), "c\n");
    assert_eq!(read(&dir, "f.txt"), "bc\n", ":sav left the old file behind");
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
    assert_eq!(read(&dir, "g.txt"), "a\nc\n", "+2 put the cursor on line 2");
    ed.keys("dd:e\r");
    assert!(ed
        .screen_bytes()
        .contains("E37: No write since last change"));
    ed.keys(":e!\r:w\r");
    assert_eq!(read(&dir, "g.txt"), "a\nc\n", ":e! dropped the change");
    ed.keys(":e new.txt\r");
    assert!(ed.screen_bytes().contains("\"new.txt\" [New]"));
    ed.keys(&format!("ihello{ESC}:w\r"));
    // ruse keeps a missing final newline unless `:set fixeol` (nvim adds one).
    assert_eq!(read(&dir, "new.txt"), "hello");
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
    assert_eq!(read(&dir, "f.txt"), "f1\n");
    assert_eq!(read(&dir, "g.txt"), "g2\n");
    ed.keys(&format!("A3{ESC}:wqa\r"));
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(read(&dir, "g.txt"), "g23\n");
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
        read(&dir, "out.txt"),
        "hi",
        "no final newline without fixeol"
    );
    ed.keys("x:w\r:q\r");
    assert_eq!(ed.exit_status(), 0);
    assert_eq!(read(&dir, "out.txt"), "h", ":w now writes the adopted file");
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
