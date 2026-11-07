//! The degraded large-file profile (#619, RFC-0008 §7, COM-12) and `ruse -b` on a real PTY: a file past
//! the 32 MiB large-file threshold opens without syntax highlighting and the status line says `[large]`;
//! `-b` reads and writes a file with `'binary'` (nvim's `-b`).

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{wait_for_file, Editor, Scratch};

#[test]
fn a_large_file_opens_in_the_degraded_profile() {
    let dir = Scratch::new();
    let line = b"fn main() { let x = 1; }\n";
    let big = line.repeat((33 << 20) / line.len() + 1);
    dir.file("big.txt", &big);
    dir.file("small.txt", line);
    let mut ed = Editor::start(&dir, &[Path::new("big.txt")]);
    ed.wait_for_row("[large]");
    ed.keys(":e small.txt\r");
    let rows = ed.wait_for_row("small.txt");
    assert!(
        !rows.iter().any(|r| r.contains("[large]")),
        "a small file is not degraded"
    );
    ed.keys(":qa!\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn dash_b_writes_the_bytes_unconverted() {
    let dir = Scratch::new();
    let file = dir.file("b.txt", b"a\r\nb");
    let mut ed = Editor::start(&dir, &[Path::new("-b"), Path::new("b.txt")]);
    ed.wait_for_row("b.txt");
    ed.keys(":set bin?\r");
    ed.wait_for_row("  binary");
    ed.keys("x:w\r");
    wait_for_file(&file, b"\r\nb");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
