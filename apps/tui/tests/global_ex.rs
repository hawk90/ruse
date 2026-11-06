//! `:g/pat/{cmd}` runs any Ex command once per marked line (#1245), through the real key path.
//!
//! The behaviour is pinned against nvim v0.12.6 by the `gex_*` oracle fixtures; these tests hold what the
//! oracle cannot observe or what a regression would hide in a tally: one undo step for the whole `:g`, the
//! status a failure leaves, and the nested `:g` rules.

use ruse_tui::headless::Headless;

fn run(text: &str, keys: &str) -> Headless {
    let mut h = Headless::vim(text).vanilla_search_case();
    h.keys(keys);
    h
}

/// One `u` undoes every command the `:g` ran, and one `CTRL-R` redoes them all (Vim does not sync undo
/// between the commands of a `:g`).
#[test]
fn a_per_line_global_is_one_undo_step() {
    let text = "a1\nb1\na2\nb2\na3";
    for keys in [
        ":g/^/m0<CR>",
        ":g/a/t0<CR>",
        ":g/a/d 2<CR>",
        ":g/b/j<CR>",
        ":g/a/s/1/X/|d<CR>",
        ":g/a/>|d<CR>",
        ":g/a/normal :.d<C-v><CR><CR>",
    ] {
        let mut h = run(text, keys);
        let after = h.text();
        assert_ne!(after, text, "{keys}: the :g changed the buffer");
        h.keys("u");
        assert_eq!(h.text(), text, "{keys}: one u undoes the whole :g");
        h.keys("<C-r>");
        assert_eq!(h.text(), after, "{keys}: one CTRL-R redoes it");
        h.assert_invariants();
    }
}

/// `:g/^/m0` reverses the buffer; `:g/b/d 2` deletes from each mark that is still there.
#[test]
fn per_line_commands_follow_their_marks() {
    assert_eq!(
        run("a1\nb1\na2\nb2\na3", ":g/^/m0<CR>").text(),
        "a3\nb2\na2\nb1\na1"
    );
    assert_eq!(run("a1\nb\na2\nb\na3\nb", ":g/b/d 2<CR>").text(), "a1");
    let mut h = run("a1\nb\na2\nb\na3", ":g/b/d a<CR>");
    assert_eq!(h.text(), "a1\na2\na3");
    assert_eq!(h.register('a').0, "b\n");
}

/// The first command that fails ends the `:g` and leaves its error; a `:silent!` one does not.
#[test]
fn a_failing_command_stops_the_global() {
    let h = run("a1\nb1\na2\nb2\na3", ":g/./m-2<CR>");
    assert_eq!(h.text(), "a1\nb1\na2\nb2\na3");
    assert_eq!(h.emsg(), "E16");
    let h = run("a1\nb1\na2\nb2\na3", ":g/a/foo<CR>");
    assert_eq!(h.emsg(), "E492");
    assert_eq!(h.cursor_row_col(), (1, 0));
    // `.+1d` is past the end on the last mark only, so the earlier marks ran.
    let h = run("a1\nb1\na2\nb2\na3", ":g/a/.+1d<CR>");
    assert_eq!(h.text(), "a1\na2\na3");
    assert_eq!(h.emsg(), "E16");
    let h = run("a1\nb1\na2\nb2\na3", ":g/a/silent! .+1d<CR>");
    assert_eq!(h.text(), "a1\na2\na3");
    assert!(!h.status().starts_with('E'), "{}", h.status());
    // A `:s` that misses a line is no failure inside a `:g`: the `|` after it still runs.
    assert_eq!(
        run("a1\nb1\na2\nb2\na3", ":g/a/s/zzz/X/|d<CR>").text(),
        "b1\nb2"
    );
}

/// A `:g` inside a `:g` runs on the current line when it matches, takes no range but the whole file
/// (E147), and says nothing when the line does not match.
#[test]
fn a_nested_global_acts_on_the_current_line() {
    assert_eq!(
        run("a1\nb1\na2\nb2\na3", ":g/a/g/1/d<CR>").text(),
        "b1\na2\nb2\na3"
    );
    assert_eq!(
        run("a1\nb1\na2\nb2\na3", ":g/a/v/1/d<CR>").text(),
        "a1\nb1\nb2"
    );
    let h = run("a1\nb1\na2\nb2\na3", ":g/a/g/b/d<CR>");
    assert_eq!(h.text(), "a1\nb1\na2\nb2\na3");
    assert_eq!(h.status(), "");
    let h = run("a1\nb1\na2\nb2\na3", ":g/a/.g/b/d<CR>");
    assert_eq!(h.emsg(), "E147");
    // After the `:g` ends, a `:g` is top-level again.
    assert_eq!(run("a1\nb1\na2", ":g/a/j<CR>:g/b/d<CR>").text(), "a2");
}
