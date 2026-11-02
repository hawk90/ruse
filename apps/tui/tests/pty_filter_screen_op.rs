//! The `!` filter operator over a frontend-resolved screen line (`!H` / `!L`), end to end on a real PTY:
//! the session must open the pre-seeded `:{range}!` line rather than drop it and type the shell command into
//! the buffer (#800). Expected bytes are nvim v0.12.6's for the same file and keys.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

#[test]
fn filter_over_screen_lines_runs_the_shell_command() {
    let cases: [(&str, &[u8]); 2] = [
        ("j!Htr a-z A-Z\r", b"ONE TWO THREE\nLINE2 A\nline3 b\n"),
        ("!Ltr a-z A-Z\r", b"ONE TWO THREE\nLINE2 A\nLINE3 B\n"),
    ];
    for (keys, want) in cases {
        let dir = Scratch::new();
        let file = dir.file("f.txt", b"one two three\nline2 a\nline3 b\n");
        let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
        ed.keys(keys);
        ed.keys(":wq\r");
        assert_eq!(ed.exit_status(), 0, "{keys:?}");
        assert_eq!(std::fs::read(&file).unwrap(), want, "{keys:?}");
    }
}
