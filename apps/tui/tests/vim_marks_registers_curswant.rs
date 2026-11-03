//! #1173-#1176 on a newline-terminated buffer (a file read from disk), which the oracle corpus cannot open:
//! `n` with no previous pattern, a linewise operator refused by its register (on the last line its delete
//! span starts at the newline before it), `` g` ``/`g'` and the cursor after a shift. Expected cursors are
//! nvim v0.12.6's on the same lines (1-based row, byte column); each session's trace replays to the live
//! state (RFC-0020), so the failures and the jumplist skips reach the core as recorded commands.

use ruse_core::Trace;
use ruse_tui::headless::Headless;

const WORDS: &str = "one two three\nfour five six\nseven eight\n";
const WORDS4: &str = "one two three\nfour five six\nseven eight\nnine ten\n";
const FAIL: &str = "ab cd ef ghijklmnop qrstuvwxyz\ngh (ij kl\n";

/// Run `keys` on `text`, check that the trace replays to the live document, cursor and mode, and return
/// the session.
fn run(text: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text).vanilla_search_case();
    h.keys(keys);
    let trace = h.trace().to_text();
    let st = Trace::from_text(&trace)
        .unwrap_or_else(|e| panic!("{keys:?}: the trace parses: {e:?}\n{trace}"))
        .replay(text.as_bytes())
        .expect("replay");
    assert_eq!(
        String::from_utf8_lossy(st.bytes()),
        h.text(),
        "{keys:?}: replayed document\n{trace}"
    );
    assert_eq!(
        st.cursor().0,
        h.cursor(),
        "{keys:?}: replayed cursor\n{trace}"
    );
    assert_eq!(st.mode(), h.mode(), "{keys:?}: replayed mode\n{trace}");
    h
}

#[test]
fn n_with_no_previous_pattern_fails_with_e35_and_resets_the_column() {
    for keys in ["j$n", "j$N", "j$dn", "j$/<CR>", "j$?<CR>"] {
        let h = run(FAIL, keys);
        assert_eq!(h.emsg(), "E35", "{keys}");
        assert_eq!(h.text(), FAIL, "{keys}");
        assert_eq!(h.cursor_row_col(), (2, 8), "{keys}");
        let h = run(FAIL, &format!("{keys}k"));
        assert_eq!(h.cursor_row_col(), (1, 8), "{keys}k");
    }
    // A failed `dn` is no change for `.`: it repeats the `x` before it.
    let h = run("abcdef abc\n", "xdn.");
    assert_eq!(h.text(), "cdef abc\n");
}

#[test]
fn a_refused_linewise_operator_stays_at_the_start_of_its_range() {
    for (keys, want) in [
        ("jw\"/dd", (2, 5)),
        ("jw\"/dk", (1, 5)),
        ("jw\"/3dd", (2, 5)),
        ("Gw\"/dd", (3, 6)),
        ("Gw\"/dk", (2, 6)),
        ("Gw\"/yy", (3, 6)),
        ("Gw\"/dgg", (1, 6)),
        ("Gw\"/ddk", (2, 6)),
        ("jwV\"/d", (2, 5)),
        ("jwVj\"/y", (2, 0)),
    ] {
        let h = run(WORDS, keys);
        assert_eq!(h.text(), WORDS, "{keys}: nothing is deleted");
        assert_eq!(h.cursor_row_col(), want, "{keys}");
    }
}

#[test]
fn g_backtick_and_g_quote_jump_without_the_jumplist() {
    for (keys, want) in [
        ("jwmajGg`a", (2, 5)),
        ("jwmajGg'a", (2, 0)),
        ("jwmajGg`a<C-o>", (3, 5)),
        ("jwmajGg'a<C-o>", (3, 5)),
        ("jwmajG`a<C-o>", (4, 5)),
        ("jwmaGg`a``", (2, 5)),
    ] {
        let h = run(WORDS4, keys);
        assert_eq!(h.cursor_row_col(), want, "{keys}");
    }
    let h = run(WORDS4, "Gg`a");
    assert_eq!(h.emsg(), "E20");
    assert_eq!(h.cursor_row_col(), (4, 0));
    // `:keepjumps` around it stays on for the rest of the `:normal`.
    let six = "l1\nl2\nl3\nl4\nl5\nl6\n";
    let h = run(six, "jmaG:keepjumps normal g`ajjG<CR><C-o>");
    assert_eq!(h.cursor_row_col(), (2, 0));
    let h = run(six, "jmaG:normal g`ajjG<CR><C-o>");
    assert_eq!(h.cursor_row_col(), (4, 0));
}

#[test]
fn a_shift_lands_at_the_wanted_column() {
    for (keys, want) in [
        ("j$>>", (2, 12)),
        ("j$>>k", (1, 12)),
        ("jw>>k", (1, 3)),
        ("j$V>k", (1, 12)),
        ("j$>jk", (1, 29)),
        ("j$:><CR>k", (1, 29)),
    ] {
        let h = run(FAIL, keys);
        assert_eq!(h.cursor_row_col(), want, "{keys}");
    }
}
