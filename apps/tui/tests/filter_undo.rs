//! Where `u` and `CTRL-R` leave the cursor after a filter (#1165). nvim's `do_filter` saves the change
//! with the cursor at the start of the range's last line, so undo and redo land there. `gq` under
//! `'formatprg'` and `=` under `'equalprg'` filter the same way. The oracle corpus covers `!` and
//! `:{range}!`; these cover the two option-driven filters, whose `:set` the comparator does not run.
//! Expected cursors are nvim v0.12.6's (`nvim --headless -u NONE`).

use ruse_tui::headless::Headless;

/// Run `keys` over `text` and return the text and the cursor as nvim reports it (1-based row, byte column).
fn run(text: &str, keys: &str) -> (String, (usize, usize)) {
    let mut h = Headless::vim(text);
    h.keys(keys);
    (h.text(), h.cursor_row_col())
}

#[test]
fn undo_after_gq_through_formatprg_lands_on_the_last_line() {
    // nvim: `:set fp=sort` then `gqju` over "a b|c d" leaves the cursor on line 2, column 0.
    let (text, at) = run("c d\na b\n", ":set fp=sort<CR>gqju");
    assert_eq!(text, "c d\na b\n");
    assert_eq!(at, (2, 0));
}

#[test]
fn undo_after_eq_through_equalprg_lands_on_the_last_line() {
    // nvim: `:set ep=sort` then `=ju` / `j=ku` over "b|a|c" leaves the cursor on line 2.
    let (text, at) = run("b\na\nc\n", ":set ep=sort<CR>=ju");
    assert_eq!(text, "b\na\nc\n");
    assert_eq!(at, (2, 0));
    let (_, at) = run("b\na\nc\n", ":set ep=sort<CR>j=ku");
    assert_eq!(at, (2, 0));
}

#[test]
fn redo_after_a_filter_lands_on_the_last_line_of_the_range() {
    // nvim: `:2,3!printf 'q\nr\ns\n'` then `u<C-r>` over "c|b|a|d" leaves the cursor on line 3.
    let (text, at) = run("c\nb\na\nd\n", ":2,3!printf 'q\\nr\\ns\\n'<CR>u<C-r>");
    assert_eq!(text, "c\nq\nr\ns\nd\n");
    assert_eq!(at, (3, 0));
}
