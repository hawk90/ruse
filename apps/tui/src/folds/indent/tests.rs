use super::*;
use crate::folds::tests::fl;

// ── foldmethod=indent (slice 3) — every expectation is pinned against nvim v0.12.4. ──────────────────

#[test]
fn indent_level_maps_columns_over_shiftwidth() {
    // 4-space at sw=4 → 1, 8-space → 2 (nvim foldlevel()).
    assert_eq!(line_indent_level(b"    x", 4, 4), 1);
    assert_eq!(line_indent_level(b"        x", 4, 4), 2);
    // Floor: a 2-space and a 6-space indent at sw=4 → 0 and 1 (nvim-verified).
    assert_eq!(line_indent_level(b"  x", 4, 4), 0);
    assert_eq!(line_indent_level(b"      x", 4, 4), 1);
    // A tab expands to `tab_width` columns: at ts=sw=4 one tab = 4 cols = level 1; two tabs = level 2.
    assert_eq!(line_indent_level(b"\tx", 4, 4), 1);
    assert_eq!(line_indent_level(b"\t\tx", 4, 4), 2);
    // No indent → 0.
    assert_eq!(line_indent_level(b"x", 4, 4), 0);
    // `tabstop` and `shiftwidth` apart (#601): one tab at ts=8/sw=4 is 8 cols = level 2 (nvim).
    assert_eq!(line_indent_level(b"\tx", 8, 4), 2);
    assert_eq!(line_indent_level(b"    x", 8, 2), 2);
}

#[test]
fn compute_indent_folds_nested_matches_nvim() {
    // nvim table (sw=4): a0/·b1/·c2/··d3/··e4/·f5/g6  (· = one indent level of 4 spaces).
    // foldlevel(): 0,1,1,2,2,1,0. Fold ranges at fdl=0: outer [1,5] L1, inner [3,4] L2 (0-based).
    let text = b"a0\n    b1\n    c2\n        d3\n        e4\n    f5\ng6\n";
    let folds = compute_indent_folds(text, 4, 4);
    assert_eq!(
        folds,
        vec![fl(1, 5, false, 1), fl(3, 4, false, 2)],
        "outer level-1 fold [1,5] + inner level-2 fold [3,4], both open until foldlevel applied"
    );
}

#[test]
fn compute_indent_folds_blank_lines_follow_next_nonblank() {
    // (nvim ranges via foldclosed at fdl=0.)
    // Trailing blank before a dedent is EXCLUDED: a0/·b/·c/<blank>/dd → fold [1,2] (blank line 3 out).
    assert_eq!(
        compute_indent_folds(b"a0\n    b\n    c\n\ndd\n", 4, 4),
        vec![fl(1, 2, false, 1)]
    );
    // Interior blank between same-level lines is INCLUDED: a0/·b/<blank>/·c/dd → fold [1,3].
    assert_eq!(
        compute_indent_folds(b"a0\n    b\n\n    c\ndd\n", 4, 4),
        vec![fl(1, 3, false, 1)]
    );
    // Blank between a deeper line and a same-outer-level line stays in the OUTER fold: a0/·b/··c/<blank>/·e/ff
    // → outer [1,4] L1 (blank line 4 included); inner ··c is a lone line, no inner fold.
    assert_eq!(
        compute_indent_folds(b"a0\n    b\n        c\n\n    e\nff\n", 4, 4),
        vec![fl(1, 4, false, 1)]
    );
}

#[test]
fn compute_indent_folds_foldignore_hash_is_like_blank() {
    // nvim default foldignore="#": a `#`-leading line takes the surrounding level and is included, like a
    // blank. a0/·b/#c/·d → single fold [1,3] L1 (the `#c` line sits inside it).
    assert_eq!(
        compute_indent_folds(b"a0\n    b\n#c\n    d\n", 4, 4),
        vec![fl(1, 3, false, 1)]
    );
}

#[test]
fn compute_indent_folds_level_jump_is_single_deep_fold() {
    // A 0→2 indent jump makes ONE level-2 fold (nvim: foldclosed stays set at fdl=1, clears at fdl=2),
    // NOT a redundant level-1 wrapper. a0/··b/··c/d0 → fold [1,2] L2.
    assert_eq!(
        compute_indent_folds(b"a0\n        b\n        c\nd0\n", 4, 4),
        vec![fl(1, 2, false, 2)]
    );
}

#[test]
fn compute_indent_folds_single_indented_line_makes_no_fold() {
    // nvim: a lone indented line is not a closeable fold (foldclosed = -1 even at fdl=0).
    assert!(compute_indent_folds(b"a0\n    b\ncc\n", 4, 4).is_empty());
    // An empty buffer / all-flat buffer has no folds.
    assert!(compute_indent_folds(b"", 4, 4).is_empty());
    assert!(compute_indent_folds(b"a\nb\nc\n", 4, 4).is_empty());
}

#[test]
fn apply_foldlevel_closes_folds_deeper_than_level() {
    // From the nested nvim table: outer L1 [1,5], inner L2 [3,4].
    let mut folds = compute_indent_folds(
        b"a0\n    b1\n    c2\n        d3\n        e4\n    f5\ng6\n",
        4,
        4,
    );
    // fdl=0 → level>0 closed → BOTH closed (nvim foldclosed=1 for lines 2..6, inner subsumed).
    apply_foldlevel(&mut folds, 0);
    assert!(folds.iter().all(|f| f.closed), "fdl=0 closes every fold");
    // fdl=1 → only level>1 closed → inner [3,4] closed, outer open (nvim: only [4,5] closed at fdl=1).
    apply_foldlevel(&mut folds, 1);
    assert!(!folds[0].closed, "outer L1 open at fdl=1");
    assert!(folds[1].closed, "inner L2 closed at fdl=1");
    // fdl=2 ≥ max → nothing closed (nvim: all foldclosed=-1).
    apply_foldlevel(&mut folds, 2);
    assert!(folds.iter().all(|f| !f.closed), "fdl>=max opens all");
}

#[test]
fn max_level_reports_deepest_fold() {
    let folds = compute_indent_folds(b"a\n    b\n        c\n            d\n", 4, 4);
    // a/·b/··c/···d → runs at depths 1,2,3 but each deeper run is a single line, so only... check:
    // levels 1,2,3 each span [1,3],[2,3],[3,3]? d3 is lone. Ranges: [1,3]L1,[2,3]L2. max_level=2.
    assert_eq!(max_level(&folds), 2);
    assert_eq!(max_level(&[]), 0);
}
