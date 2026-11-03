//! Bracketed paste (#600) through the headless session: the same `Session::on_paste` the binary runs for a
//! terminal's `CSI 200 ~ … CSI 201 ~`. Expected text, cursor (1-based row, 0-based byte column) and mode
//! are nvim v0.12.6's, captured with `nvim_paste(data, true, -1)` (what its TUI calls) over the buffer
//! `abc def / xyz / "  ind"`. Divergences are marked where they occur.

use ruse_core::Mode;
use ruse_tui::headless::Headless;

const TEXT: &str = "abc def\nxyz\n  ind\n";

/// A Vim session on [`TEXT`] with the cursor on line 1, byte column `col`, after typing `pre`.
fn at(col: usize, pre: &str) -> Headless {
    let mut h = Headless::vim(TEXT);
    h.set_cursor(col);
    h.keys(pre);
    h
}

/// Paste `data` after `pre` (cursor on line 1 at `col`), then type `post`.
fn paste(col: usize, pre: &str, data: &str, post: &str) -> Headless {
    let mut h = at(col, pre);
    assert!(h.paste(data));
    h.keys(post);
    h
}

fn check(h: &Headless, text: &str, cursor: (usize, usize), mode: Mode) {
    assert_eq!(h.text(), text);
    assert_eq!(h.cursor_row_col(), cursor, "cursor");
    assert_eq!(h.mode(), mode, "mode");
}

#[test]
fn normal_mode_paste_is_a_text_esc() {
    check(
        &paste(4, "", "PP", ""),
        "abc dPPef\nxyz\n  ind\n",
        (1, 6),
        Mode::Normal,
    );
    check(
        &paste(4, "", "P1\nP2", ""),
        "abc dP1\nP2ef\nxyz\n  ind\n",
        (2, 1),
        Mode::Normal,
    );
    check(
        &paste(4, "", "P1\n", ""),
        "abc dP1\nef\nxyz\n  ind\n",
        (2, 0),
        Mode::Normal,
    );
    // At the end of the line the text is appended.
    check(
        &paste(6, "", "PP", ""),
        "abc defPP\nxyz\n  ind\n",
        (1, 8),
        Mode::Normal,
    );
}

#[test]
fn crlf_and_cr_become_newlines() {
    check(
        &paste(4, "", "P1\r\nP2\rP3", ""),
        "abc dP1\nP2\nP3ef\nxyz\n  ind\n",
        (3, 1),
        Mode::Normal,
    );
}

#[test]
fn normal_paste_is_one_undo_step_and_dot_repeats_it() {
    // Undo puts the cursor back where the paste began, and redo too (#1063). DIVERGENCE: nvim says (1, 4),
    // since its Normal-mode paste is a put that leaves the cursor where it was; ruse pastes as `a` does,
    // so the change begins one column right, where `a` put the caret.
    check(&paste(4, "", "P1\nP2", "u"), TEXT, (1, 5), Mode::Normal);
    check(
        &paste(4, "", "P1\nP2", "u<C-r>"),
        "abc dP1\nP2ef\nxyz\n  ind\n",
        (1, 5),
        Mode::Normal,
    );
    // The same after a typed multi-line insert (nvim: `4laP1<CR>P2<Esc>u` -> (1, 5)).
    check(&at(4, "aP1<CR>P2<Esc>u"), TEXT, (1, 5), Mode::Normal);
    check(
        &paste(4, "", "PP", "j0."),
        "abc dPPef\nxPPyz\n  ind\n",
        (2, 2),
        Mode::Normal,
    );
}

#[test]
fn a_pending_count_or_operator_does_not_apply_to_the_paste() {
    // The paste's bytes are never keys: `dd` in the payload deletes nothing, and the `3`/`d` typed before
    // it are dropped.
    check(
        &paste(4, "3", "dd", "x"),
        "abc ddef\nxyz\n  ind\n",
        (1, 6),
        Mode::Normal,
    );
    check(
        &paste(4, "d", "dd", ""),
        "abc dddef\nxyz\n  ind\n",
        (1, 6),
        Mode::Normal,
    );
}

#[test]
fn insert_mode_paste_is_literal_and_part_of_the_insert() {
    check(
        &paste(4, "i", "PP", ""),
        "abc PPdef\nxyz\n  ind\n",
        (1, 6),
        Mode::Insert,
    );
    check(
        &paste(4, "i", "PP", "<Esc>"),
        "abc PPdef\nxyz\n  ind\n",
        (1, 5),
        Mode::Normal,
    );
    check(
        &paste(4, "i", "P1\nP2", ""),
        "abc P1\nP2def\nxyz\n  ind\n",
        (2, 2),
        Mode::Insert,
    );
    // An ESC inside the payload is never a key. Divergence (TERM-PASTE-2): nvim inserts it literally;
    // ruse drops ESC and the C1 controls, so a paste never carries a control sequence.
    check(
        &paste(4, "i", "a\u{1b}[31m\u{9d}b", ""),
        "abc a[31mbdef\nxyz\n  ind\n",
        (1, 10),
        Mode::Insert,
    );
}

#[test]
fn insert_paste_gets_no_autoindent() {
    // After `  ind`, a typed newline would indent the next line; a pasted one does not.
    let h = paste(0, "3GA", "P1\nP2\n  P3", "<Esc>");
    assert_eq!(h.text(), "abc def\nxyz\n  indP1\nP2\n  P3\n");
}

#[test]
fn insert_paste_is_one_undo_step_with_the_insert_and_dot_repeats() {
    check(
        &paste(4, "iZ", "P1\nP2", "<Esc>u"),
        TEXT,
        (1, 4),
        Mode::Normal,
    );
    check(
        &paste(4, "i", "PP", "<Esc>j0."),
        "abc PPdef\nPPxyz\n  ind\n",
        (2, 1),
        Mode::Normal,
    );
    // A count on the insert repeats the pasted text too.
    check(
        &paste(4, "3i", "PP", "<Esc>"),
        "abc PPPPPPdef\nxyz\n  ind\n",
        (1, 9),
        Mode::Normal,
    );
    let mut h = paste(4, "i", "PP", "<BS><Esc>");
    assert_eq!(h.text(), "abc Pdef\nxyz\n  ind\n");
    // `".` holds the net inserted text.
    h.keys("j0\".p");
    assert_eq!(h.text(), "abc Pdef\nxPyz\n  ind\n");
}

#[test]
fn insert_paste_drops_a_pending_ctrl_r() {
    check(
        &paste(4, "i<C-r>", "PP", "<Esc>"),
        "abc PPdef\nxyz\n  ind\n",
        (1, 5),
        Mode::Normal,
    );
}

#[test]
fn replace_mode_paste_overwrites() {
    // Divergence: nvim leaves the cursor where the paste began (col 4) until `<Esc>`; ruse puts it after
    // the text, as typing does.
    check(
        &paste(4, "R", "PPP", ""),
        "abc PPP\nxyz\n  ind\n",
        (1, 7),
        Mode::Replace,
    );
    let h = paste(4, "R", "P1\nP2", "<Esc>");
    assert_eq!(h.text(), "abc P1\nP2\nxyz\n  ind\n");
    let h = paste(4, "R", "PPP", "<Esc>u");
    assert_eq!(h.text(), TEXT);
}

#[test]
fn visual_paste_replaces_the_selection() {
    let mut h = paste(4, "vl", "PP", "");
    check(&h, "abc PPf\nxyz\n  ind\n", (1, 5), Mode::Normal);
    assert_eq!(h.register('"').0, "de");
    // `.` repeats the paste as the Normal-mode one.
    h.keys("j0.");
    assert_eq!(h.text(), "abc PPf\nxPPyz\n  ind\n");
    // A selection to the end of its line: the text lands where the selection was.
    check(
        &paste(5, "vl", "PP", ""),
        "abc dPP\nxyz\n  ind\n",
        (1, 6),
        Mode::Normal,
    );
    // Select mode takes the same replace path as a typed Select key.
    let h = paste(4, "gh", "PP", "");
    check(&h, "abc PPef\nxyz\n  ind\n", (1, 5), Mode::Normal);
    check(
        &paste(0, "<C-v>jl", "PP", ""),
        "PPc def\nz\n  ind\n",
        (1, 1),
        Mode::Normal,
    );
    // Linewise: the lines go, then the text is inserted at the caret. nvim gives `xyPPz`: its `Vd` keeps
    // the column (`nostartofline`), ruse's `Vd` lands on column 0, so the paste follows it.
    let h = paste(4, "V", "PP", "");
    assert_eq!(h.text(), "PPxyz\n  ind\n");
    assert_eq!(h.mode(), Mode::Normal);
}

#[test]
fn command_line_paste_inserts_the_first_line_literally() {
    let mut h = at(0, ":");
    h.paste("s/a/\tb\nrest");
    assert_eq!(h.engine().cmdline().map(|c| c.1), Some("s/a/\tb"));
    h.keys("<CR>");
    assert_eq!(h.text(), "\tbbc def\nxyz\n  ind\n");
    // On the search line too.
    let mut h = at(0, "/");
    h.paste("xy");
    h.keys("<CR>");
    assert_eq!(h.cursor_row_col(), (2, 0));
}

#[test]
fn emacs_paste_yanks_at_point() {
    // Emacs 31.1 `xterm-paste` with `xterm-store-paste-on-kill-ring`: kill-new, then yank.
    let mut h = Headless::emacs("abc\n");
    h.set_cursor(1);
    h.paste("X\r\nY");
    assert_eq!(h.text(), "aX\nYbc\n");
    assert_eq!(h.cursor(), 4);
    assert_eq!(h.mark(), Some(1));
    assert_eq!(h.register('"').0, "X\nY");
    // A pending `C-x` prefix does not swallow it, and the prefix is gone after it.
    h.keys("<C-x>");
    h.paste("Z");
    h.keys("u");
    assert_eq!(h.text(), "aX\nYZubc\n");
}

#[test]
fn a_large_paste_is_linear() {
    // 4 MiB in 64 Ki lines: one edit, not one per char or line.
    let line = "x".repeat(63) + "\n";
    let data = line.repeat(64 * 1024);
    let started = std::time::Instant::now();
    let mut h = at(0, "i");
    h.paste(&data);
    h.keys("<Esc>");
    assert_eq!(h.text().len(), TEXT.len() + data.len());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
}
