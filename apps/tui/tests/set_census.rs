//! The `:set` census options of #1151, against nvim v0.12.6 (`nvim --headless -u NONE`, `:set opt?`
//! read with `nvim_exec2`). ruse's own defaults differ from Vim's for `'scrolloff'` (3, not 0); every
//! other value here is nvim's. Where ruse honours only part of an option, the test pins that part.

use ruse_tui::headless::Headless;

/// The status line after `keys` on a fresh Vim-profile session.
fn status(keys: &str) -> String {
    let mut h = Headless::vim("abc\n");
    h.keys(keys);
    h.status().to_string()
}

#[test]
fn history_reads_sets_and_checks_its_range() {
    assert_eq!(status(":set hi?<CR>"), "  history=10000");
    assert_eq!(status(":set hi=0<CR>:set history?<CR>"), "  history=0");
    assert_eq!(
        status(":set hi=10001<CR>"),
        "E474: Invalid argument: hi=10001"
    );
    assert_eq!(
        status(":set hi=-1<CR>"),
        "E487: Argument must be positive: hi=-1"
    );
}

#[test]
fn history_zero_keeps_no_line() {
    // `:set hi=0` empties both rings, so `CTRL-P` cannot reach the `:s` and `:<C-p><C-p><CR>` runs an
    // empty line.
    let mut h = Headless::vim("a\n");
    h.keys(":s/a/b/<CR>u:set hi=0<CR>:<C-p><C-p><CR>");
    assert_eq!(h.text(), "a\n");
    let mut h = Headless::vim("a\n");
    h.keys(":s/a/b/<CR>u:set hi=5<CR>:<C-p><C-p><CR>");
    assert_eq!(h.text(), "b\n");
}

#[test]
fn showmode_and_verbose_are_options() {
    assert_eq!(status(":set smd?<CR>"), "  showmode");
    assert_eq!(status(":set nosmd<CR>:set showmode?<CR>"), "noshowmode");
    assert_eq!(status(":set vbs?<CR>"), "  verbose=0");
    assert_eq!(status(":set vbs=2<CR>:set verbose?<CR>"), "  verbose=2");
}

#[test]
fn cpoptions_reads_nvims_default_and_refuses_another_value() {
    assert_eq!(status(":set cpo?<CR>"), "  cpoptions=aABceFs_");
    assert_eq!(status(":set cpo+=Z<CR>"), "option not supported: cpo+=Z");
    assert_eq!(
        status(":set cpo+=Z<CR>:set cpo?<CR>"),
        "  cpoptions=aABceFs_"
    );
    // Setting the default itself is fine.
    assert_eq!(
        status(":set cpo=aABceFs_<CR>:set cpo?<CR>"),
        "  cpoptions=aABceFs_"
    );
}

#[test]
fn verbosefile_reads_empty_and_refuses_a_file() {
    assert_eq!(status(":set vfile?<CR>"), "  verbosefile=");
    assert_eq!(
        status(":set vfile=log.txt<CR>"),
        "option not supported: vfile=log.txt"
    );
}

#[test]
fn scrollback_stays_unsupported() {
    assert_eq!(status(":set scbk=5<CR>"), "option not supported: scbk=5");
}

#[test]
fn scrolloff_is_global_local() {
    // nvim: `:setlocal so?` is -1 until set; `:set` drops the window's value.
    assert_eq!(status(":setlocal so?<CR>"), "  scrolloff=-1");
    assert_eq!(status(":setlocal so=5<CR>:set so?<CR>"), "  scrolloff=5");
    assert_eq!(
        status(":setlocal so=5<CR>:setglobal so?<CR>"),
        "  scrolloff=3"
    );
    assert_eq!(
        status(":setlocal so=5<CR>:set so=7<CR>:setlocal so?<CR>"),
        "  scrolloff=-1"
    );
    assert_eq!(
        status(":setlocal so=5<CR>:setlocal so<<CR>:setlocal so?<CR>"),
        "  scrolloff=3"
    );
}

#[test]
fn a_split_keeps_the_windows_own_scrolloff() {
    // nvim copies a window's local options to the window a split makes.
    let mut h = Headless::vim("abc\n");
    h.keys(":setlocal so=5<CR><C-w>s:setlocal so?<CR>");
    assert_eq!(h.status(), "  scrolloff=5");
    assert_eq!(h.workspace().window_count(), 2);
}

#[test]
fn options_lists_every_option() {
    // nvim opens its option window; ruse lists the options it honours, as `:set all` does.
    let mut h = Headless::vim("abc\n");
    h.keys(":options<CR>");
    assert_eq!(h.emsg(), "");
    let mut h = Headless::vim("abc\n");
    h.keys(":opt<CR>");
    assert_eq!(h.emsg(), "");
    assert_eq!(status(":options x<CR>"), "E488: Trailing characters: x");
}
