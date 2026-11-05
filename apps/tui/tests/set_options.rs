//! What the `:set` grammar (#614) shows and which copy of an option it writes, through the real key path.
//! The buffer effects and the E-numbers are pinned by the oracle corpus (`set_grammar_*`, `setlocal_*`,
//! `setglobal_*`); this covers the messages, ruse's defaults, the local/global split and the options the
//! corpus does not observe. Message texts are nvim v0.12.6's (probed headless with `execute()`).

use ruse_tui::headless::{Headless, Profile};

fn status_after(keys: &str) -> String {
    let mut h = Headless::vim("x");
    h.keys(keys);
    h.status().to_string()
}

#[test]
fn show_forms_print_like_nvim() {
    // ruse's defaults: ts=4, ic on (the shipped smartcase pair), so=3.
    assert_eq!(status_after(":set ts?<CR>"), "  tabstop=4");
    assert_eq!(status_after(":set ts<CR>"), "  tabstop=4");
    assert_eq!(status_after(":set ic?<CR>"), "  ignorecase");
    assert_eq!(status_after(":set noic<CR>:set ic?<CR>"), "noignorecase");
    assert_eq!(status_after(":set noic?<CR>"), "  ignorecase");
    assert_eq!(status_after(":set so?<CR>"), "  scrolloff=3");
    assert_eq!(status_after(":set fixeol?<CR>"), "nofixendofline");
    assert_eq!(status_after(":set fdm?<CR>"), "  foldmethod=manual");
    // Several shows: the listing opens, the last row stays on the status line.
    assert_eq!(status_after(":set sw? ts?<CR>"), "  tabstop=4");
    // A plain set prints nothing.
    assert_eq!(status_after(":set ts=8 sw=2<CR>"), "");
}

#[test]
fn operators_and_defaults() {
    assert_eq!(
        status_after(":set ts=4<CR>:set ts+=2<CR>:set ts?<CR>"),
        "  tabstop=6"
    );
    assert_eq!(
        status_after(":set ts=6 ts-=3<CR>:set ts?<CR>"),
        "  tabstop=3"
    );
    assert_eq!(
        status_after(":set ts=5 ts^=3<CR>:set ts?<CR>"),
        "  tabstop=15"
    );
    assert_eq!(status_after(":set ts:7<CR>:set ts?<CR>"), "  tabstop=7");
    // `&` is ruse's default, `&vim` Vim's.
    assert_eq!(
        status_after(":set ts=7<CR>:set ts&<CR>:set ts?<CR>"),
        "  tabstop=4"
    );
    assert_eq!(status_after(":set ts&vim<CR>:set ts?<CR>"), "  tabstop=8");
    assert_eq!(
        status_after(":set noic<CR>:set ic&<CR>:set ic?<CR>"),
        "  ignorecase"
    );
    assert_eq!(status_after(":set ic&vim<CR>:set ic?<CR>"), "noignorecase");
    assert_eq!(
        status_after(":set fixeol&vim<CR>:set fixeol?<CR>"),
        "  fixendofline"
    );
    assert_eq!(status_after(":set ic!<CR>:set ic?<CR>"), "noignorecase");
    assert_eq!(status_after(":set invic<CR>:set ic?<CR>"), "noignorecase");
    assert_eq!(
        status_after(":set ts=8 sw=2 all&<CR>:set sw? ts?<CR>"),
        "  tabstop=4"
    );
    assert_eq!(status_after(":set so=-2<CR>:set so?<CR>"), "  scrolloff=-2");
}

#[test]
fn errors_quote_the_argument() {
    assert_eq!(
        status_after(":set bogus<CR>"),
        "E518: Unknown option: bogus"
    );
    assert_eq!(
        status_after(":set ts=4 bogus sw=2<CR>"),
        "E518: Unknown option: bogus "
    );
    assert_eq!(
        status_after(":set ts=0<CR>"),
        "E487: Argument must be positive: ts=0"
    );
    assert_eq!(
        status_after(":set sw=-1<CR>"),
        "E487: Argument must be positive: sw=-1"
    );
    assert_eq!(
        status_after(":set sw=0 sw-=1<CR>"),
        "E487: Argument must be positive: sw-=1"
    );
    assert_eq!(
        status_after(":set ts=10000<CR>"),
        "E474: Invalid argument: ts=10000"
    );
    assert_eq!(
        status_after(":set ic=1<CR>"),
        "E474: Invalid argument: ic=1"
    );
    assert_eq!(
        status_after(":set ts=abc<CR>"),
        "E521: Number required after =: ts=abc"
    );
    assert_eq!(
        status_after(":set ts!<CR>"),
        "E488: Trailing characters: ts!"
    );
    assert_eq!(
        status_after(":set fdm=bogus<CR>"),
        "E474: Invalid argument: fdm=bogus"
    );
    assert_eq!(
        status_after(":set fdm+=x<CR>"),
        "E474: Invalid argument: fdm+=x"
    );
    // The arguments before the bad one still apply, the ones after it do not.
    assert_eq!(
        status_after(":set sw=2 bogus sw=6<CR>:set sw?<CR>"),
        "  shiftwidth=2"
    );
    // An option nvim knows but ruse does not implement only says so; the rest of the line still runs.
    assert_eq!(
        status_after(":set nospell<CR>"),
        "option not supported: nospell"
    );
    assert_eq!(
        status_after(":set nospell ts=6<CR>:set ts?<CR>"),
        "  tabstop=6"
    );
    assert_eq!(
        status_after(":set fdm=marker<CR>"),
        "option not supported: fdm=marker"
    );
    // nvim accepts and ignores terminal key codes.
    assert_eq!(status_after(":set t_xx=1 <F1><CR>"), "");
}

#[test]
fn setlocal_and_setglobal_write_their_copy() {
    // `:setlocal` changes the focused window only; a new buffer takes the global value.
    assert_eq!(
        status_after(":setlocal ts=3<CR>:setglobal ts?<CR>"),
        "  tabstop=4"
    );
    assert_eq!(
        status_after(":setlocal ts=3<CR>:setlocal ts?<CR>"),
        "  tabstop=3"
    );
    assert_eq!(
        status_after(":setlocal ts=3<CR>:enew<CR>:set ts?<CR>"),
        "  tabstop=4"
    );
    // `:setglobal` changes what new buffers take, not the current window.
    assert_eq!(
        status_after(":setglobal ts=6<CR>:set ts?<CR>"),
        "  tabstop=4"
    );
    assert_eq!(
        status_after(":setglobal ts=6<CR>:enew<CR>:set ts?<CR>"),
        "  tabstop=6"
    );
    assert_eq!(
        status_after(":setglobal ts=6<CR>:setlocal ts<<CR>:set ts?<CR>"),
        "  tabstop=6"
    );
    assert_eq!(
        status_after(":setglobal ts=6<CR>:set ts<<CR>:set ts?<CR>"),
        "  tabstop=6"
    );
    // A Vim-global option has one value: `:setlocal` reaches every window.
    assert_eq!(
        status_after(":split<CR>:setlocal noic<CR><C-w>w:set ic?<CR>"),
        "noignorecase"
    );
    assert_eq!(
        status_after(":setglobal noic<CR>:setlocal ic?<CR>"),
        "noignorecase"
    );
}

#[test]
fn bare_set_lists_what_differs_from_the_default() {
    let mut h = Headless::vim("x");
    h.keys(":set ts=2 noic<CR>:set<CR>");
    // The listing opens (title + rows); its last row is on the status line.
    assert_eq!(h.status(), "noignorecase          tabstop=2");
    let mut h = Headless::vim("x");
    h.keys(":set<CR>");
    assert_eq!(h.status(), "--- Options ---");
    let mut h = Headless::vim("x");
    h.keys(":setlocal ts=2<CR>:setglobal<CR>");
    assert_eq!(h.status(), "--- Global option values ---");
}

#[test]
fn scrolloff_is_settable() {
    let text: String = (1..=50).map(|i| format!("{i}\n")).collect();
    // 11 text rows (the status line is extra): `L` keeps 'scrolloff' rows of context above the window's bottom.
    let mut h = Headless::with_size(Profile::Vim, &text, 80, 11);
    h.keys("L");
    assert_eq!(h.cursor_row_col().0, 7);
    let mut h = Headless::with_size(Profile::Vim, &text, 80, 11);
    h.keys(":set so=0<CR>L");
    assert_eq!(h.cursor_row_col().0, 10);
}

#[test]
fn hlsearch_tildeop_and_folds_keep_working() {
    assert_eq!(status_after(":set nohls<CR>:set hls?<CR>"), "nohlsearch");
    assert_eq!(status_after(":set top<CR>:set top?<CR>"), "  tildeop");
    assert_eq!(
        status_after(":set fdm=indent fdl=2<CR>:set fdm? fdl?<CR>"),
        "  foldlevel=2"
    );
    assert_eq!(status_after(":set nofen<CR>:set fen?<CR>"), "nofoldenable");
}

/// #1211: `'readonly'` (nvim v0.12.6, probed headless). The first change to an unmodified read-only
/// buffer gives W10, and so does starting Insert mode before a character is typed. It warns once until
/// `'readonly'` is set again, not after an undo back to the unmodified text; a buffer already modified
/// when `'readonly'` is set does not warn. The text still changes.
#[test]
fn readonly_warns_once_on_the_first_change() {
    const W10: &str = "W10: Warning: Changing a readonly file";
    assert_eq!(status_after(":set ro?<CR>"), "noreadonly");
    assert_eq!(status_after(":set ro<CR>:set ro?<CR>"), "  readonly");
    assert_eq!(
        status_after(":set ro<CR>:set noro<CR>:set ro?<CR>"),
        "noreadonly"
    );
    let mut h = Headless::vim("abc");
    h.keys(":set ro<CR>x");
    assert_eq!((h.status(), h.text().as_str()), (W10, "bc"));
    // `:set ts?` puts something else on the status line first.
    h.keys(":set ts?<CR>x");
    assert_eq!(
        (h.status(), h.text().as_str()),
        ("  tabstop=4", "c"),
        "once"
    );
    h.keys("uu:set ts?<CR>x");
    assert_eq!(
        h.status(),
        "  tabstop=4",
        "not again after undo to the unmodified text"
    );
    h.keys("u:set ro<CR>x");
    assert_eq!(h.status(), W10, "setting 'readonly' again warns again");
    // Insert mode warns as it starts.
    let mut h = Headless::vim("abc");
    h.keys(":set ro<CR>i");
    assert_eq!(h.status(), W10);
    // `:set noro` stops it; a modified buffer does not warn.
    assert_eq!(status_after(":set ro<CR>:set noro<CR>x"), "");
    let mut h = Headless::vim("abc");
    h.keys("x:set ro<CR>:set ts?<CR>x");
    assert_eq!((h.status(), h.text().as_str()), ("  tabstop=4", "c"));
    // A command-line change warns too.
    assert_eq!(status_after(":set ro<CR>:s/x/y/<CR>"), W10);
}
