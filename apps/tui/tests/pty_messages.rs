//! A message too long for the status line on a real 80x24 PTY (#1222). nvim v0.12.6, probed in a PTY at
//! 80x24, wraps it across the rows above `Press ENTER or type command to continue`; `<CR>` and `<Space>`
//! are used up by the prompt, and any other key clears it and then runs (`:` opens the command line).
//! `:messages` shows the history, and output taller than the screen is paged with `-- More --` (#1235).

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

const MORE: &str = "-- More --";

/// `:messages` shows the errors and reports kept so far, not an option's reply, above the hit-enter
/// prompt; `:messages clear` empties it (#1235).
#[test]
fn messages_shows_the_history_and_clear_empties_it() {
    let dir = Scratch::new();
    dir.file("f.txt", b"one\ntwo\nthree\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.keys(":nosuch1\r");
    ed.wait_for_row("E492: Not an editor command: nosuch1");
    ed.keys(":set ts?\r");
    ed.wait_for_row("tabstop=4");
    ed.keys(":1,3y\r");
    ed.wait_for_row("3 lines yanked");
    ed.keys(":messages\r");
    let rows = ed.wait_for_row(HIT_ENTER);
    assert_eq!(
        rows[21..],
        [
            "E492: Not an editor command: nosuch1",
            "3 lines yanked",
            HIT_ENTER
        ],
        "{rows:#?}"
    );
    ed.keys("\r");
    ed.keys(":messages clear\r");
    ed.keys(":messages\r");
    ed.keys(":set ts?\r");
    let rows = ed.wait_for_row("tabstop=4");
    assert!(!rows.iter().any(|r| r.contains(HIT_ENTER)), "{rows:#?}");
    // Nothing to show: `:messages` left the bar alone before the reply.
    ed.keys(":messages\r");
    let rows = ed.wait_for_row("NORMAL  f.txt");
    assert_eq!(rows[23], "NORMAL  f.txt", "{rows:#?}");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// Output taller than the screen stops at `-- More --` (`:help more-prompt`, nvim v0.12.6 in a PTY at
/// 80x24): `<Space>` a screen on, `k` a row back, a key it does not take shows its help, `q` ends it.
#[test]
fn a_tall_message_is_paged_with_more() {
    let dir = Scratch::new();
    let file = dir.file("f.txt", b"one\ntwo\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    for i in 1..=30 {
        ed.keys(&format!(":x{i}\r"));
    }
    ed.wait_for_row("E492: Not an editor command: x30");
    let line = |i: usize| format!("E492: Not an editor command: x{i}");
    ed.keys(":messages\r");
    let rows = ed.wait_for_row(MORE);
    assert_eq!(rows[0], line(1), "the first page: {rows:#?}");
    assert_eq!(rows[22], line(23));
    assert_eq!(rows[23], MORE);
    // `<Space>`: the next screen reaches the end, so the hit-enter prompt follows the last row.
    ed.keys(" ");
    let rows = ed.wait_for_row(HIT_ENTER);
    assert_eq!(
        (rows[0].as_str(), rows[22].as_str()),
        (&*line(8), &*line(30))
    );
    // `k` at the hit-enter prompt of a paged message goes back a row, to `-- More --`.
    ed.keys("k");
    let rows = ed.wait_for_row(MORE);
    assert_eq!(
        (rows[0].as_str(), rows[22].as_str()),
        (&*line(7), &*line(29))
    );
    // `b` back a screen, `j` a row on.
    ed.keys("bj");
    ed.keys("x");
    let rows = ed.wait_for_row("SPACE/d/j");
    assert_eq!(
        rows[23],
        "-- More -- SPACE/d/j: screen/page/line down, b/u/k: up, q: quit"
    );
    assert_eq!(rows[0], line(2), "b then j: {rows:#?}");
    // `q` ends it and is used up: the file shows again, and `x` then deletes on line 1.
    ed.keys("q");
    let rows = ed.wait_for_row("NORMAL  f.txt");
    assert_eq!(rows[0], "one", "{rows:#?}");
    ed.keys("x:w\r");
    support::wait_for_file(&file, b"ne\ntwo\n");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}

/// A file message when the bar's prefix is wider than the screen is cut to the whole row with `<`, as
/// nvim's message row is, and asks for no `<CR>` (#1235).
#[test]
fn a_file_message_after_a_too_wide_prefix_is_cut_to_the_row() {
    let dir = Scratch::new();
    // An 87-cell name: `NORMAL  {name}  ` alone is wider than the 80-cell screen.
    let deep = format!("{}/{}", "d".repeat(40), "e".repeat(40));
    std::fs::create_dir_all(dir.path(&deep)).unwrap();
    let rel = format!("{deep}/f.txt");
    dir.file(&rel, b"one\n");
    let mut ed = Editor::start(&dir, &[Path::new(&rel)]);
    ed.keys(":w\r");
    let rows = ed.wait_for_row("1L, 4B written");
    let bar = &rows[23];
    assert_eq!(bar.chars().count(), 80, "{bar:?}");
    assert!(bar.starts_with("<ddd"), "cut at its start: {bar:?}");
    assert!(bar.ends_with("/f.txt\" 1L, 4B written"), "{bar:?}");
    assert!(!rows.iter().any(|r| r.contains(HIT_ENTER)), "no prompt");
    ed.keys(":q\r");
    assert_eq!(ed.exit_status(), 0);
}
