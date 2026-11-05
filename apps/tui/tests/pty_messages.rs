//! A message too long for the status line on a real 80x24 PTY (#1222). nvim v0.12.6, probed in a PTY at
//! 80x24, wraps it across the rows above `Press ENTER or type command to continue`; `<CR>` and `<Space>`
//! are used up by the prompt, and any other key clears it and then runs (`:` opens the command line).

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

const HIT_ENTER: &str = "Press ENTER or type command to continue";

/// An unknown command 103 cells long: its E492 is wider than the screen.
fn long_command() -> String {
    format!("Xnosuch{}", "z".repeat(96))
}

#[test]
fn a_long_error_is_shown_whole_and_enter_is_used_up() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\ntwo\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    let cmd = long_command();
    ed.keys(&format!(":{cmd}\r"));
    let rows = ed.wait_for_row(HIT_ENTER);
    // The message wraps at column 80 into the two rows above the prompt on the last row.
    let full = format!("E492: Not an editor command: {cmd}");
    let (first, rest) = full.split_at(80);
    assert_eq!(rows[21..], [first, rest, HIT_ENTER], "{rows:#?}");
    // `<CR>` answers the prompt and does not move the cursor down: `x` deletes on line 1.
    ed.keys("\r");
    ed.keys("x:w\r");
    support::wait_for_file(&file, b"ne\ntwo\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

#[test]
fn another_key_clears_the_prompt_and_runs() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\ntwo\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(&format!(":{}\r", long_command()));
    ed.wait_for_row(HIT_ENTER);
    // `j` runs: the cursor goes to line 2, where `x` deletes.
    ed.keys("jx:w\r");
    support::wait_for_file(&file, b"one\nwo\n");
    // `:` opens the command line straight from the prompt, and the bar is whole again after it.
    ed.keys(&format!(":{}\r", long_command()));
    ed.wait_for_row(HIT_ENTER);
    ed.keys_then_wait_for(":set ts?\r", "tabstop=4");
    let rows = ed.wait_for_row("tabstop=4");
    assert_eq!(rows[23], "NORMAL  f.txt    tabstop=4", "{rows:#?}");
    assert_eq!(rows[22], "~", "the message rows are gone");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// A file message too long for the bar is cut at its start with `<`, as nvim's default `'shortmess'` `t`
/// does, and asks for no `<CR>`.
#[test]
fn a_long_file_message_is_cut_at_its_start() {
    let dir = Scratch::new();
    // A 31-cell name: the bar's prefix leaves the 50-cell message 39 cells.
    let deep = format!("{}/{}", "d".repeat(12), "e".repeat(12));
    std::fs::create_dir_all(dir.path(&deep)).unwrap();
    let rel = format!("{deep}/f.txt");
    dir.file(&rel, b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new(&rel)]);
    ed.keys(":w\r");
    let rows = ed.wait_for_row("1L, 4B written");
    let bar = &rows[23];
    assert_eq!(bar.chars().count(), 80, "{bar:?}");
    assert!(
        bar.starts_with(&format!("NORMAL  {rel}  <")),
        "cut at its start: {bar:?}"
    );
    assert!(bar.ends_with("/f.txt\" 1L, 4B written"), "{bar:?}");
    assert!(!rows.iter().any(|r| r.contains(HIT_ENTER)), "no prompt");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
