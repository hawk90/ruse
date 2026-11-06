//! A trace saved by the real binary replays to the file it wrote, or is refused (#690, F-022).
//!
//! Each case is one session from #690 on a PTY: keys, `:trace save t.tr`, `:w`, then `ruse --replay t.tr`
//! on the original file. The replay must print exactly the bytes the session wrote, and exit 0. A session
//! the trace cannot follow (`:e!`, commands in another buffer) must refuse the save and write no trace.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::process::Command;

use support::{Editor, Scratch};

const ESC: &str = "\u{1b}";

/// Run `keys` on a file holding `text`, save the trace and write the file, then replay the trace onto
/// `text`: the replay prints what the session wrote and exits 0.
fn replays_to_the_written_file(text: &[u8], keys: &str) {
    let dir = Scratch::new();
    let file = dir.file("f.txt", text);
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(keys);
    ed.keys_then_wait_for(&format!("{ESC}:trace save t.tr\r"), "trace saved");
    ed.keys(":w\r:qa!\r");
    assert_eq!(ed.exit_status(), 0, "{keys:?}");
    let written = std::fs::read(&file).expect("the session wrote the file");
    let orig = dir.file("orig.txt", text);
    let out = Command::new(env!("CARGO_BIN_EXE_ruse"))
        .arg("--replay")
        .arg(dir.path("t.tr"))
        .arg(&orig)
        .output()
        .expect("spawn ruse --replay");
    assert!(
        out.status.success(),
        "{keys:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&written),
        "{keys:?}: the replay prints what the session wrote"
    );
}

/// Run `keys` on a file holding `text`, then `:trace save t.tr`: the save is refused naming `why`, and no
/// trace is written.
fn save_is_refused(text: &[u8], keys: &str, why: &str) {
    let dir = Scratch::new();
    dir.file("f.txt", text);
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(keys);
    ed.keys_then_wait_for(&format!("{ESC}:trace save t.tr\r"), why);
    ed.keys(&format!("\r{ESC}:qa!\r"));
    assert_eq!(ed.exit_status(), 0, "{keys:?}");
    assert!(!dir.path("t.tr").exists(), "{keys:?}: no trace is written");
}

#[test]
fn a_substitute_replays() {
    replays_to_the_written_file(b"aaa\nbbb\n", ":s/a/X/g\r");
}

#[test]
fn a_line_delete_replays() {
    replays_to_the_written_file(b"aaa\nbbb\nccc\n", ":2d\r");
}

#[test]
fn a_smartcase_search_replays_with_the_session_options() {
    // ruse ships `ignorecase`+`smartcase`; the trace starts with them, so `/foo` lands on `FOO` on replay
    // too (`EditorState::new` alone has both off).
    replays_to_the_written_file(b"xx FOO foo\n", "/foo\rx");
    replays_to_the_written_file(b"xx FOO foo\n", ":set noic\r/foo\rx");
}

#[test]
fn a_line_picker_jump_replays() {
    replays_to_the_written_file(b"l1\nl2\nl3\n", "\x0cl3\rx");
}

#[test]
fn a_crlf_file_replays() {
    replays_to_the_written_file(b"aaa\r\nbbb\r\n", "x:s/b/Z/\r");
}

#[test]
fn a_bom_file_replays() {
    replays_to_the_written_file(b"\xef\xbb\xbfaaa\nbbb\n", "xjdd");
}

#[test]
fn keys_mixed_with_ex_edits_replay() {
    replays_to_the_written_file(b"aaa bbb\nccc\n", "x:s/bbb/ZZZ/\r:2d\r");
    replays_to_the_written_file(b"abc abc\n", "x:s/b/Z/\r");
}

#[test]
fn substitute_repeats_replay() {
    replays_to_the_written_file(b"a a\na a\n", ":s/a/X/\rj&");
    replays_to_the_written_file(b"a a\na a\n", ":s/a/X/\rjg&");
}

#[test]
fn a_window_switch_replays() {
    // Each window has its own cursor: the trace records the one the switch brings.
    replays_to_the_written_file(b"abc\ndef\nghi\n", ":split\rjjx\x17wx");
}

#[test]
fn a_window_switch_replays_with_the_window_s_wanted_column() {
    // #1272: the window switched to wants column 0, the one left wants 1; `G` reads the wanted column.
    replays_to_the_written_file(b"\nbb\nb \n", ":split\rihi\x1b\x17wddGx");
    // Closing the focused first window focuses the other, at the same tile index.
    replays_to_the_written_file(b"a\n", ":split\rihi\x1b\x17wx\x17cp");
}

#[test]
fn a_re_read_file_refuses_the_save() {
    save_is_refused(b"aaa\n", "x:e!\rx", "re-read from disk");
}

#[test]
fn commands_in_another_buffer_refuse_the_save() {
    save_is_refused(
        b"aaa\n",
        &format!("x:w\r:e g.txt\rihello{ESC}:w\r:e #\rx"),
        "another buffer",
    );
}
