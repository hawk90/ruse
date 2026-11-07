//! CTRL-C interrupts a long-running command through the real binary on a PTY (#898, #682, #683, #787).
//!
//! Raw mode makes CTRL-C a key, so only the terminal shows whether the editor reads it while it is busy.
//! Each test starts a command that would run for a long time (a self-invoking macro, a huge count-insert,
//! a `:g` over many lines), sends CTRL-C while it runs, then `:wq`. Expected, as in nvim (`got_int`): the
//! command stops within moments, the editor takes the next keys, and the file shows the work stopped early.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::time::{Duration, Instant};
use support::{Editor, Scratch};

const CTRL_C: &[u8] = b"\x03";

/// How long the editor may take, from CTRL-C to the end of `:wq`, to count as responsive. A debug build
/// under a 16-process CPU load takes at most about 0.4 s for each test here (#1294: the count insert took
/// up to 5 s before a window scrolled into its 1 MB line stopped laying out the rows it skips).
const RESPONSIVE: Duration = Duration::from_secs(5);

/// Let the long command get going, interrupt it, then save and quit. Returns how long the editor took from
/// CTRL-C to exiting, failing the test if it did not exit within [`RESPONSIVE`].
fn interrupt_then_wq(ed: &mut Editor) -> Duration {
    std::thread::sleep(Duration::from_millis(300));
    let t0 = Instant::now();
    ed.send(CTRL_C);
    ed.send(b":wq\r");
    let status = ed.exit_within(RESPONSIVE);
    let took = t0.elapsed();
    assert_eq!(status, Some(0), "no exit {took:?} after CTRL-C");
    took
}

#[test]
fn ctrl_c_stops_a_self_invoking_macro() {
    let dir = Scratch::new();
    dir.file("f.txt", b"\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    // `qaq` empties a; `qaix<Esc>@aq` records a macro that inserts an `x` and calls itself. It never
    // fails, so only the 100,000-key cap (20,000 runs of its 5 keys) or CTRL-C ends it.
    ed.keys("qaqqaix\x1b@aq");
    ed.send(b"@a");
    interrupt_then_wq(&mut ed);
    let text = std::fs::read(dir.path("f.txt")).unwrap();
    let xs = text.iter().filter(|&&c| c == b'x').count();
    assert!(xs > 1, "the macro ran ({xs} x)");
    assert!(xs < 20_000, "CTRL-C did not stop the macro: {xs} x");
}

#[test]
fn ctrl_c_stops_a_huge_count_insert_and_leaves_insert() {
    let dir = Scratch::new();
    dir.file("f.txt", b"\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    // 999999999 copies would be a gigabyte; the repeat is lazy and stops on CTRL-C (#683), and the
    // `<Esc>` that ends it still runs, so `:wq` is read in Normal mode.
    ed.send(b"999999999ix\x1b");
    interrupt_then_wq(&mut ed);
    let text = std::fs::read(dir.path("f.txt")).unwrap();
    assert!(text.len() < 100_000_000, "{} bytes", text.len());
    assert!(text.starts_with(b"xx"), "the repeat ran");
}

#[test]
fn ctrl_c_stops_a_global_normal_over_many_lines() {
    let dir = Scratch::new();
    let lines = 200_000;
    dir.file("f.txt", "a\n".repeat(lines).as_bytes());
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    ed.send(b":g/a/normal Ab\r");
    interrupt_then_wq(&mut ed);
    let text = String::from_utf8(std::fs::read(dir.path("f.txt")).unwrap()).unwrap();
    assert_eq!(text.lines().count(), lines);
    let done = text.lines().filter(|l| *l == "ab").count();
    assert!(
        done < lines,
        "CTRL-C did not stop the :g ({done} lines changed)"
    );
}

#[test]
fn a_huge_search_count_returns_at_once() {
    let dir = Scratch::new();
    dir.file("f.txt", b"foo\nbar\nfoo\n");
    let mut ed = Editor::start(&dir, &[Path::new("f.txt")]);
    // The walk over the two matches is periodic, so 99999999 steps cost two (#682); nvim lands on line 3.
    let t0 = Instant::now();
    ed.send(b"99999999/foo\rD:wq\r");
    assert_eq!(ed.exit_within(RESPONSIVE), Some(0), "{:?}", t0.elapsed());
    assert_eq!(std::fs::read(dir.path("f.txt")).unwrap(), b"foo\nbar\n\n");
}
