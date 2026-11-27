//! What the search history records through the real key path. A Normal `*`/`#`/`g*`/`g#` adds the pattern
//! it searches for (#1617), as nvim v0.12.6's `nv_ident` does (probed with `nvim --clean` and
//! `:history search`): `\<foo\>` for the whole-word forms, `foo` for the `g` forms, a count changing
//! nothing. The `i1617_*` oracle fixtures pin the same through `/<Up>`.

use ruse_tui::headless::Headless;

/// The open command line's text, or `None` once it closed.
fn line(h: &Headless) -> Option<String> {
    h.engine().cmdline().map(|(_, t, _)| t.to_string())
}

/// The text `/<Up>` recalls after `keys` ran over `foo bar foo\nbar foo\n`.
fn recalled_after(keys: &str) -> Option<String> {
    let mut h = Headless::vim("foo bar foo\nbar foo\n");
    h.keys(keys);
    h.keys("/<Up>");
    line(&h)
}

#[test]
fn star_and_hash_record_the_whole_word_pattern() {
    for keys in ["*", "#", "2*", "3#"] {
        assert_eq!(recalled_after(keys).as_deref(), Some("\\<foo\\>"), "{keys}");
    }
}

#[test]
fn g_star_and_g_hash_record_the_bare_word() {
    for keys in ["g*", "g#", "2g*", "3g#"] {
        assert_eq!(recalled_after(keys).as_deref(), Some("foo"), "{keys}");
    }
}

#[test]
fn star_entry_sits_below_a_later_search() {
    let mut h = Headless::vim("foo bar foo\nbar foo\n");
    h.keys("*/bar<CR>/<Up>");
    assert_eq!(line(&h).as_deref(), Some("bar"));
    h.keys("<Up>");
    assert_eq!(line(&h).as_deref(), Some("\\<foo\\>"));
}

#[test]
fn star_with_no_word_records_nothing() {
    // E348: no keyword on the line, so nothing is searched and the history keeps only `/x`.
    let mut h = Headless::vim("x\n  \n");
    h.keys("/x<CR>j*/<Up><Up>");
    assert_eq!(line(&h).as_deref(), Some("x"));
}
