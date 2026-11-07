//! The write options' `:set` grammar (#619), against nvim v0.12.6 (`nvim --clean --headless`, `:set
//! opt?`, the errors read from `v:errmsg`): defaults, the `'backupcopy'` and `'backupext'` checks, and
//! E589 when `'backupext'` and `'patchmode'` are equal.

use ruse_tui::headless::Headless;

fn status(keys: &str) -> String {
    let mut h = Headless::vim("abc\n");
    h.keys(keys);
    h.status().to_string()
}

#[test]
fn defaults_are_nvims() {
    assert_eq!(status(":set bk?<CR>"), "nobackup");
    assert_eq!(status(":set wb?<CR>"), "  writebackup");
    assert_eq!(status(":set bkc?<CR>"), "  backupcopy=auto");
    assert_eq!(status(":set bex?<CR>"), "  backupext=~");
    assert_eq!(status(":set pm?<CR>"), "  patchmode=");
    assert_eq!(status(":set fs?<CR>"), "  fsync");
    assert_eq!(status(":set write?<CR>"), "  write");
    assert_eq!(status(":set wa?<CR>"), "nowriteany");
    assert_eq!(status(":set eol?<CR>"), "  endofline");
    assert_eq!(status(":set eof?<CR>"), "noendoffile");
    assert_eq!(status(":set bomb?<CR>"), "nobomb");
    assert!(status(":set bdir?<CR>").starts_with("  backupdir=.,"));
    assert!(status(":set bsk?<CR>").starts_with("  backupskip=/"));
}

#[test]
fn backupcopy_takes_one_method_and_the_break_flags() {
    assert_eq!(
        status(":set bkc=yes,breakhardlink<CR>:set bkc?<CR>"),
        "  backupcopy=yes,breakhardlink"
    );
    for bad in ["foo", "yes,no", "breakhardlink"] {
        assert_eq!(
            status(&format!(":set bkc={bad}<CR>")),
            format!("E474: Invalid argument: bkc={bad}")
        );
    }
}

#[test]
fn backupext_and_patchmode_checks() {
    assert_eq!(
        status(":set backupext=a/b<CR>"),
        "E474: Invalid argument: backupext=a/b"
    );
    assert_eq!(
        status(":set patchmode=~<CR>"),
        "E589: 'backupext' and 'patchmode' are equal: patchmode=~"
    );
    assert_eq!(
        status(":set patchmode=.orig backupext=.orig<CR>"),
        "E589: 'backupext' and 'patchmode' are equal: backupext=.orig"
    );
    assert_eq!(status(":set pm=.orig<CR>:set pm?<CR>"), "  patchmode=.orig");
    // A refused value leaves the option alone.
    assert_eq!(status(":set bex=a/b<CR>:set bex?<CR>"), "  backupext=~");
}

#[test]
fn a_reset_restores_the_environment_defaults() {
    let shown = status(":set bdir?<CR>");
    assert_eq!(
        status(":set bdir=/x<CR>:set bdir&<CR>:set bdir?<CR>"),
        shown
    );
}

/// The journal as nvim's swap file (#619): `'swapfile'` and `'updatecount'` read and set; `'directory'`
/// shows the journal directory and refuses another (the next session looks for unsaved work there).
#[test]
fn swap_options_map_onto_the_journal() {
    assert_eq!(status(":set swf?<CR>"), "  swapfile");
    assert_eq!(status(":set noswf<CR>:set swf?<CR>"), "noswapfile");
    assert_eq!(status(":set uc?<CR>"), "  updatecount=200");
    assert_eq!(status(":set uc=0<CR>:set uc?<CR>"), "  updatecount=0");
    let dir = status(":set dir?<CR>");
    assert!(
        dir.starts_with("  directory=") && dir.ends_with("//"),
        "{dir}"
    );
    assert_eq!(status(":set dir=/x<CR>"), "option not supported: dir=/x");
}
