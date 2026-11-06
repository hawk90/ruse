//! A byte that is not UTF-8 in the real binary (#1251): nvim draws it as `<ff>`, four cells, and the
//! cursor after it stands where the core counts its virtual column.

#![cfg(unix)]

mod support;

use std::path::Path;

use support::{Editor, Scratch};

#[test]
fn an_invalid_byte_shows_as_hex_and_the_cursor_counts_its_four_cells() {
    let dir = Scratch::new();
    dir.file("b.txt", b"\xffabc\nx\x80\xea\xb0yz\nabcdefghij\n");
    let mut ed = Editor::start(&dir, &[Path::new("b.txt")]);
    let rows = ed.wait_for_row("<ff>abc");
    assert_eq!(rows[0], "<ff>abc");
    // A stray continuation byte and a cut 3-byte char: each byte is its own `<xx>`.
    assert_eq!(rows[1], "x<80><ea><b0>yz");
    // On the invalid byte the cursor shows on its first cell; after it, on the next char's.
    assert_eq!(ed.cursor(), (0, 0));
    ed.keys("l");
    assert_eq!(ed.cursor(), (0, 4));
    ed.keys("$");
    assert_eq!(ed.cursor(), (0, 6));
    // `b` is at virtual column 5; below it, cells 5-8 are `<ea>`, and below that `f`.
    ed.keys("0llj");
    assert_eq!(ed.cursor(), (1, 5));
    ed.keys("j");
    assert_eq!(ed.cursor(), (2, 5));
    // `10|` on the second line lands on `<b0>` (cells 9-12).
    ed.keys("k10|");
    assert_eq!(ed.cursor(), (1, 9));
    ed.keys(":q!\r");
    assert_eq!(ed.exit_status(), 0);
}
