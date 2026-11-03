//! A linewise delete through the last line of a newline-terminated buffer (#1167), through the headless
//! session. The oracle corpus opens its lines without a final newline, so it pins only the unterminated
//! case (`dollar_*_then_dd`); a file read from disk ends in one, and there ruse put the cursor at the end
//! of the new last line instead of at the wanted column. Expected cursors are nvim v0.12.6's on the same
//! lines (`nvim_win_get_cursor`: 1-based row, byte column).

use ruse_tui::headless::Headless;

fn cursor(text: &str, keys: &str) -> (String, (usize, usize)) {
    let mut h = Headless::vim(text);
    h.keys(keys);
    (h.text(), h.cursor_row_col())
}

const TWO: &str = "ab cd ef\ngh ij\n";
const THREE: &str = "ab cd ef\ngh ij\nkl\n";

#[test]
fn dd_of_the_last_line_lands_on_the_wanted_column_above() {
    let one = "ab cd ef\n".to_string();
    // The rows of #1167: an operator after `$` sets the wanted column where it leaves the cursor.
    assert_eq!(cursor(TWO, "$jyydd"), (one.clone(), (1, 4)));
    assert_eq!(cursor(TWO, "$jy$dd"), (one.clone(), (1, 4)));
    assert_eq!(cursor(TWO, "$jxdd"), (one.clone(), (1, 3)));
    assert_eq!(cursor(TWO, "$jDdd"), (one.clone(), (1, 3)));
    assert_eq!(cursor(TWO, "$j0Ddd"), (one.clone(), (1, 0)));
    assert_eq!(cursor(TWO, "yl$jpdd"), (one.clone(), (1, 5)));
    assert_eq!(cursor(TWO, "yl$jPdd"), (one.clone(), (1, 4)));
    // Without `$` in play: the column `j` kept.
    assert_eq!(cursor(TWO, "jdd"), (one.clone(), (1, 0)));
    assert_eq!(cursor(TWO, "wjdd"), (one.clone(), (1, 3)));
    // `$` itself still rides to the end of the line above.
    assert_eq!(cursor(TWO, "$jdd"), (one, (1, 7)));
}

#[test]
fn other_linewise_deletes_through_the_last_line_do_the_same() {
    let one = "ab cd ef\n".to_string();
    assert_eq!(cursor(THREE, "wjdj"), (one.clone(), (1, 3)));
    assert_eq!(cursor(THREE, "wjdG"), (one.clone(), (1, 3)));
    assert_eq!(cursor(THREE, "wjVjd"), (one.clone(), (1, 3)));
    assert_eq!(cursor(THREE, "wjVjx"), (one.clone(), (1, 3)));
    assert_eq!(cursor(THREE, "wjVjD"), (one, (1, 3)));
    // Not through the end: the line below moves up under the cursor.
    assert_eq!(
        cursor(THREE, "w2jdd"),
        ("ab cd ef\ngh ij\n".to_string(), (2, 3))
    );
}
