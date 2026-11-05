//! A change that fails before nvim saves its redo buffer is no change for `.` (#1179), on a
//! newline-terminated buffer (a file read from disk). The oracle corpus opens its lines without a final
//! newline and pins that shape (`dot_fail_*`, `dot_kept_*`); the rows here repeat a sample of it with the
//! newline, on the last line where the final newline matters. Expected text is nvim v0.12.6's on the same
//! lines.

use ruse_tui::headless::Headless;

const TEXT: &str = "abcdef abc\ngh ij\n";

fn after(keys: &str) -> String {
    let mut h = Headless::vim(TEXT);
    h.keys(keys);
    h.text()
}

#[test]
fn a_failed_change_keeps_the_earlier_one() {
    // `jx` deletes the `g`; each failed change after it leaves `.` repeating the `x`.
    for keys in [
        "jxdj.",
        "jx2J.",
        "jxd/zzz<CR>.",
        "jxc?zzz<CR>.",
        "jxd'a.",
        "jx3dd.",
        "jxdi(.",
        "jxdfz.",
        "/zzz<CR>jxdn.",
    ] {
        assert_eq!(after(keys), "abcdef abc\n ij\n", "{keys}");
    }
    // `r` past the end of the line fails before its redo: `.` still deletes.
    assert_eq!(after("j$x5r-0."), "abcdef abc\nh i\n");
}

#[test]
fn a_failed_change_in_a_macro_keeps_the_earlier_one() {
    // The macro's failed `d/zzz` aborts it and is no change: `.` repeats the `dw` typed before.
    assert_eq!(after("jdwqqd/zzz<CR>q@q."), "abcdef abc\n\n");
    assert_eq!(after("jxqqdfzq@q."), "abcdef abc\n ij\n");
}

#[test]
fn a_change_saved_before_it_fails_stays_the_change() {
    // `"/dw` is refused by its register after nvim saved the redo: `.` repeats the refused `"/dw`.
    assert_eq!(after("jx\"/dw."), "abcdef abc\nh ij\n");
    // `CTRL-A` without a number is saved first too.
    assert_eq!(after("jx<C-a>."), "abcdef abc\nh ij\n");
}

#[test]
fn a_refused_register_with_a_failed_motion_keeps_the_earlier_change() {
    // `"/dfz` with no `z`: the motion fails before the operator could be refused, so `.` repeats the `x`
    // (#1184), where `"/dw` above repeats itself.
    assert_eq!(after("jx\"/dfz."), "abcdef abc\n ij\n");
    assert_eq!(after("jx\":dfz."), "abcdef abc\n ij\n");
}

#[test]
fn gn_with_no_previous_pattern_fails_with_e35() {
    // nvim v0.12.6: E35, no change, and `.` keeps the earlier change (#1184).
    for keys in ["gn", "dgn", "jxdgn", "jxcgnZ<Esc>"] {
        let mut h = Headless::vim(TEXT);
        h.keys(keys);
        assert_eq!(h.emsg(), "E35", "{keys}");
    }
    assert_eq!(after("jxdgn."), "abcdef abc\n ij\n");
}
