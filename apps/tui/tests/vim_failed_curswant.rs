//! A failed command after `$` keeps or resets the wanted column as nvim's handler for it does (#1171), on a
//! newline-terminated buffer (a file read from disk). The oracle corpus opens its lines without a final
//! newline and pins that shape (`fail_*_curswant`); the rows here repeat a sample of it with the newline.
//! Expected cursors are nvim v0.12.6's on the same lines (1-based row, byte column).

use ruse_tui::headless::Headless;

const TEXT: &str = "ab cd ef ghijklmnop qrstuvwxyz\ngh (ij kl\n";

fn cursor(keys: &str) -> (usize, usize) {
    let mut h = Headless::vim(TEXT);
    h.keys(keys);
    assert_eq!(h.text(), TEXT, "{keys}: a failed command changes nothing");
    h.cursor_row_col()
}

#[test]
fn failures_that_keep_the_end_of_line_column() {
    for keys in [
        "j$<C-a>k", "j$<C-x>k", "j$fzk", "j$tzk", "j$lk", "j$dfzk", "j$%k", "j$])k", "j$Jk",
        "j$3rxk", "j$<C-i>k",
    ] {
        assert_eq!(cursor(keys), (1, 29), "{keys}");
    }
}

#[test]
fn failures_that_reset_the_column_to_the_cursor() {
    for keys in [
        "j$/zzz<CR>k",
        "j$d/zzz<CR>k",
        "j$di(k",
        "j$`ak",
        "j$'ak",
        "j$d`ak",
        "j$wk",
    ] {
        assert_eq!(cursor(keys), (1, 8), "{keys}");
    }
}

#[test]
fn a_failed_mark_jump_is_not_a_jump() {
    // `` `a `` with `a` unset records nothing on the jumplist: `CTRL-O` goes back to where `G` left.
    let mut h = Headless::vim(TEXT);
    h.keys("wG$`a<C-o>");
    assert_eq!(h.cursor_row_col(), (1, 3));
}
