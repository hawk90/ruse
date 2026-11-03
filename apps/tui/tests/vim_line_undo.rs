//! `U` line-undo and the small Normal keys of #605, plus Insert `CTRL-U` (#607), through the headless
//! session (`Session::on_key`). The oracle corpus (`tests/parity/vim/fixtures/corpus.yaml`, `U_*`,
//! `i_ctrl_u_*`, `z_plus*`, `z_caret*`, `count_colon_*`, `ctrl_bslash_*`) pins the text and cursor against
//! nvim v0.12.6. What it cannot observe is pinned here: undo after `U` (the oracle's feedkeys are one undo
//! block), messages (`g8`, `:set top`), the seeded command line, macro registers (nvim records only typed
//! keys), and the cases where ruse deliberately differs from nvim.

use ruse_tui::headless::Headless;

fn run(text: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text);
    h.keys(keys);
    assert!(!h.quit(), "keys {keys:?} quit the session");
    h
}

fn text(text: &str, keys: &str) -> String {
    run(text, keys).text()
}

const THREE: &str = "abc def\nghi jkl\nmno pqr\n";

#[test]
fn u_undoes_a_line_undo_and_a_second_line_undo_redoes() {
    assert_eq!(text(THREE, "xU"), THREE);
    assert_eq!(text(THREE, "xUu"), "bc def\nghi jkl\nmno pqr\n");
    assert_eq!(text(THREE, "xUU"), "bc def\nghi jkl\nmno pqr\n");
    // After `u` the saved copy is the text `U` replaced, so `U` puts the same text back (nvim: `xxUuU`).
    assert_eq!(text(THREE, "xUuU"), "bc def\nghi jkl\nmno pqr\n");
    // Nothing changed yet: `U` does nothing and leaves the buffer unmodified.
    let h = run(THREE, "U");
    assert_eq!(h.text(), THREE);
    assert!(!h.workspace().focused().doc.is_modified());
}

#[test]
fn undo_leaves_the_saved_line_alone() {
    // nvim v0.12.6 (typed keys): `xxjxuU` keeps line 2 as the `U` line after `u`, so `U` lands there and
    // puts back the text it already has.
    let h = run(THREE, "xjxuU");
    assert_eq!(h.text(), "bc def\nghi jkl\nmno pqr\n");
    assert_eq!(h.cursor_row_col(), (2, 0));
}

#[test]
fn the_saved_line_follows_lines_added_above_it() {
    // DIVERGENCE (deliberate): nvim keeps the saved line's NUMBER, so after `yyP` above it `U` overwrites
    // the line that moved into that slot (nvim: `abc def / abc def / mno pqr / o pqr`). ruse moves the
    // saved line with its text and restores that line.
    let h = run(THREE, "jjxxggyyPU");
    assert_eq!(h.text(), "abc def\nabc def\nghi jkl\nmno pqr\n");
    assert_eq!(h.cursor_row_col(), (4, 0));
}

#[test]
fn joining_the_saved_line_drops_it() {
    // DIVERGENCE (deliberate): nvim's `U` after `J` puts back the old first line and so drops the joined
    // text (`xxJU` gives `abc def / mno pqr`); ruse treats the join as the end of that line's changes.
    assert_eq!(text(THREE, "xxJU"), "c def ghi jkl\nmno pqr\n");
    // `jxxkJU`: nvim overwrites line 2 (now `mno pqr`) with `ghi jkl`; ruse leaves it.
    assert_eq!(text(THREE, "jxxkJU"), "abc def i jkl\nmno pqr\n");
}

#[test]
fn insert_ctrl_u_is_part_of_the_insert() {
    // One undo step takes back the whole insert, CTRL-U included (nvim: `jA<C-u>Z<Esc>u`).
    assert_eq!(text("abc\n    def\n", "jA<C-u>Z<Esc>u"), "abc\n    def\n");
    assert_eq!(text("abc\n    def\n", "jA<C-u>Z<Esc>"), "abc\n    Z\n");
}

#[test]
fn g8_prints_the_utf8_bytes_under_the_cursor() {
    assert_eq!(run("abc\n", "g8").status(), "61 ");
    assert_eq!(run("€x\n", "g8").status(), "e2 82 ac ");
    // A composing character follows its base after a `+`.
    assert_eq!(run("e\u{301}x\n", "g8").status(), "65 + cc 81 ");
    assert_eq!(run("\nx\n", "g8").status(), "NUL");
    // Under an operator `g8` is not a motion: it aborts.
    assert_eq!(text("abc\n", "dg8"), "abc\n");
}

#[test]
fn tildeop_makes_tilde_an_operator() {
    assert_eq!(text(THREE, ":set top<CR>~w"), "ABC def\nghi jkl\nmno pqr\n");
    assert_eq!(text(THREE, ":set top<CR>~~"), "ABC DEF\nghi jkl\nmno pqr\n");
    assert_eq!(
        text(THREE, ":set top<CR>2~~"),
        "ABC DEF\nGHI JKL\nmno pqr\n"
    );
    assert_eq!(
        text(THREE, ":set top<CR>:set notop<CR>~"),
        "Abc def\nghi jkl\nmno pqr\n"
    );
    assert_eq!(
        run(THREE, ":set tildeop<CR>:set top?<CR>").status(),
        "  tildeop"
    );
}

#[test]
fn count_colon_seeds_the_count_lines() {
    let h = run(THREE, "j2:");
    assert_eq!(h.engine().cmdline(), Some((':', "2,3", 3)));
    let h = run(THREE, "j1:");
    assert_eq!(h.engine().cmdline(), Some((':', "2", 1)));
    let h = run(THREE, ":");
    assert_eq!(h.engine().cmdline(), Some((':', "", 0)));
}

#[test]
fn macros_record_into_numbered_and_unnamed_registers() {
    // nvim v0.12.6 (typed keys): `q1xq@1` deletes twice and `"1P` puts the recording back.
    assert_eq!(text("abcdefgh\n", "q1xq@1"), "cdefgh\n");
    assert_eq!(text("abcdefgh\n", "q1xq\"1P"), "xbcdefgh\n");
    assert_eq!(text("abcdefgh\n", "q0xq@0"), "cdefgh\n");
    // `q"` records into `"0`, which `P` then puts.
    assert_eq!(text("abcdefgh\n", "q\"lqP"), "albcdefgh\n");
    let h = run("abcdefgh\n", "q\"lq@\"");
    assert_eq!(h.cursor_row_col(), (1, 2));
}
