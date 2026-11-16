//! The display tests: `:set opt?` and `:set all` against nvim. Split from `set_cmd.rs` (#1388).

use super::*;

#[test]
fn show_and_listing_match_nvim() {
    assert_eq!(
        show_one(opt_def(Opt::TabStop), OptVal::Num(8)),
        "  tabstop=8"
    );
    assert_eq!(
        show_one(opt_def(Opt::IgnoreCase), OptVal::Bool(true)),
        "  ignorecase"
    );
    assert_eq!(
        show_one(opt_def(Opt::IgnoreCase), OptVal::Bool(false)),
        "noignorecase"
    );
    // nvim's `:set` with ic, so=100, ts=4, sw=4, tw=2147483647 (and its own `scroll=11`) on 80 columns:
    // "  ignorecase          scrolloff=100       tabstop=4" / "  scroll=11           shiftwidth=4" /
    // "  textwidth=2147483647": short entries fill 20-cell columns top to bottom, a long one gets its
    // own row. Without `scroll` the four short ones fit one row.
    let mut items = vec![
        (opt_def(Opt::IgnoreCase), OptVal::Bool(true)),
        (opt_def(Opt::ScrollOff), OptVal::Num(100)),
        (opt_def(Opt::ShiftWidth), OptVal::Num(4)),
        (opt_def(Opt::TabStop), OptVal::Num(4)),
        (opt_def(Opt::TextWidth), OptVal::Num(2_147_483_647)),
    ];
    assert_eq!(
        listing("--- Options ---", &items, 80),
        [
            "--- Options ---",
            "  ignorecase          scrolloff=100       shiftwidth=4        tabstop=4",
            "  textwidth=2147483647",
        ]
    );
    items.insert(2, (opt_def(Opt::FoldLevel), OptVal::Num(1)));
    assert_eq!(
        listing("--- Options ---", &items, 80)[1..3],
        [
            "  ignorecase          foldlevel=1         tabstop=4",
            "  scrolloff=100       shiftwidth=4",
        ]
    );
}
