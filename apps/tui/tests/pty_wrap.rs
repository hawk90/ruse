//! Soft wrap and the line-number gutter in the real binary (#615, #616), on the harness's 80x24
//! terminal: the tail of a line longer than the window is painted (it used to be cut at the edge), and
//! `:set nu` paints the number column.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

#[test]
fn a_long_line_wraps_and_number_shows_the_gutter() {
    let dir = Scratch::new();
    let line = format!("{}TAILMARK\nsecond\n", "a".repeat(100));
    dir.file("w.txt", line.as_bytes());
    let mut ed = Editor::start(&dir, &[Path::new("w.txt")]);
    let shown = ed.screen_bytes();
    assert!(
        shown.contains("TAILMARK"),
        "the wrapped tail is drawn: {shown}"
    );
    ed.keys(":set nu\r");
    let shown = ed.screen_bytes();
    assert!(shown.contains("  2 "), "the number column: {shown}");
    // `gj` moves to the wrapped row, so `x` deletes there.
    ed.keys("gggjx:wq\r");
    assert_eq!(ed.exit_status(), 0);
    let text = std::fs::read_to_string(dir.path("w.txt")).unwrap();
    // 80 columns less the 4-cell gutter: the second row starts at byte 76.
    assert_eq!(
        text,
        format!("{}{}TAILMARK\nsecond\n", "a".repeat(76), "a".repeat(23))
    );
}
