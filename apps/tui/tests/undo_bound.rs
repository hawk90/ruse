//! The undo bound (RFC-0026 §1, D-067, #1290): `'undolevels'` read and set as nvim v0.12.6 does
//! (`nvim --clean --headless`, `:set ul?`, errors from `v:errmsg`), the bound reaching every buffer, and
//! `u` stopping at the oldest kept state. The oracle fixtures `i1290_*` (tests/parity/vim) pin the undo
//! behaviour itself.

use ruse_core::{UndoLimits, Workspace};
use ruse_tui::headless::Headless;

fn status(keys: &str) -> String {
    let mut h = Headless::vim("abcdefgh\n");
    h.keys(keys);
    h.status().to_string()
}

fn text(keys: &str) -> String {
    let mut h = Headless::vim("abcdefgh\n");
    h.keys(keys);
    h.text()
}

#[test]
fn undolevels_reads_and_sets_as_nvim() {
    assert_eq!(status(":set ul?<CR>"), "  undolevels=1000");
    assert_eq!(status(":set undolevels?<CR>"), "  undolevels=1000");
    assert_eq!(status(":set ul=-5<CR>:set ul?<CR>"), "  undolevels=-5");
    assert_eq!(status(":set ul=0<CR>:set ul?<CR>"), "  undolevels=0");
    // nvim: past a 32-bit int is E474.
    assert_eq!(
        status(":set ul=99999999999<CR>"),
        "E474: Invalid argument: ul=99999999999"
    );
    assert_eq!(
        status(":set ul=7<CR>:set ul&<CR>:set ul?<CR>"),
        "  undolevels=1000"
    );
}

#[test]
fn u_stops_at_the_oldest_kept_state() {
    // nvim: `:set ul=3`, five `x`, five `u`: four undo, the fifth is `Already at oldest change`.
    let mut h = Headless::vim("abcdefgh\n");
    h.keys(":set ul=3<CR>xxxxxuuuuu");
    assert_eq!(h.text(), "bcdefgh\n");
    // ruse shows no undo messages yet (nvim: `Already at oldest change`); the fifth `u` only fails.
    assert!(!h.workspace().focused().doc.can_undo());
}

#[test]
fn the_bound_reaches_every_buffer() {
    let mut ws = Workspace::new("a\n");
    let limits = UndoLimits {
        levels: Some(2),
        bytes: None,
    };
    ws.set_undo_limits(limits);
    let later = ws.add_buffer("b\n", None);
    assert_eq!(ws.focused().doc.history().limits(), limits);
    assert_eq!(ws.document(later).unwrap().history().limits(), limits);
    ws.reload_focused("c\n");
    assert_eq!(ws.focused().doc.history().limits(), limits);
}

#[test]
fn undolevels_zero_and_negative() {
    // nvim: with `ul=0`, `u` takes its own undo back; with a negative value a change is not undoable.
    assert_eq!(text(":set ul=0<CR>xxuu"), "cdefgh\n");
    assert_eq!(text(":set ul=0<CR>xxuuu"), "bcdefgh\n");
    assert_eq!(text(":set ul=-1<CR>xxu"), "cdefgh\n");
    assert_eq!(text("xx:set ul=-1<CR>x:set ul=1000<CR>uu"), "defgh\n");
}
