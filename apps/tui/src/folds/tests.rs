use super::*;

pub(super) fn f(start: usize, end: usize, closed: bool) -> Fold {
    Fold {
        start,
        end,
        closed,
        level: 1,
    }
}

/// A fold with an explicit level, for the indent-method tests.
pub(super) fn fl(start: usize, end: usize, closed: bool, level: usize) -> Fold {
    Fold {
        start,
        end,
        closed,
        level,
    }
}

#[test]
fn fold_at_and_snap_out() {
    let folds = [f(2, 5, true)];
    assert_eq!(fold_at(&folds, 3), Some(0));
    assert_eq!(fold_at(&folds, 6), None);
    // A line hidden inside the closed fold snaps to its start; the start and outside lines stay.
    assert_eq!(snap_out(&folds, 4), 2);
    assert_eq!(snap_out(&folds, 2), 2);
    assert_eq!(snap_out(&folds, 6), 6);
}

#[test]
fn hidden_and_closed_starting() {
    let folds = [f(2, 5, true)];
    assert!(!hidden(&folds, 2), "the start row is visible (the summary)");
    assert!(hidden(&folds, 3));
    assert!(hidden(&folds, 5));
    assert!(!hidden(&folds, 6));
    assert!(closed_starting_at(&folds, 2).is_some());
    assert!(closed_starting_at(&folds, 3).is_none());
    // An OPEN fold hides nothing.
    let open = [f(2, 5, false)];
    assert!(!hidden(&open, 3));
    assert!(closed_starting_at(&open, 2).is_none());
}

#[test]
fn visible_row_collapses_rows_above() {
    // Fold lines 2..=5 closed (4 lines → 1 summary row, hiding 3). Lines 0,1,2 unaffected.
    let folds = [f(2, 5, true)];
    assert_eq!(visible_row(&folds, 0), 0);
    assert_eq!(visible_row(&folds, 2), 2, "the fold's start keeps its row");
    // Line 6 is below the fold: 3 hidden rows above it (3,4,5) → display row 6-3=3.
    assert_eq!(visible_row(&folds, 6), 3);
    assert_eq!(visible_row(&folds, 7), 4);
}

/// The nvim answers for a fold over lines 2-4 (1-based) of six lines, 0-based here (`jzf2j`, then the
/// keys, `line('.')` after).
#[test]
fn line_down_and_up_step_over_a_closed_fold() {
    let folds = [f(1, 3, true)];
    assert_eq!(
        line_down(&folds, 0, 1, 5),
        Some(1),
        "j onto the fold lands on its first line"
    );
    assert_eq!(
        line_down(&folds, 1, 1, 5),
        Some(4),
        "j from the fold goes past it"
    );
    assert_eq!(line_down(&folds, 2, 1, 5), Some(4), "from any line of it");
    assert_eq!(line_down(&folds, 1, 2, 5), Some(5));
    assert_eq!(line_down(&folds, 0, 3, 5), Some(5), "gg3j");
    assert_eq!(line_up(&folds, 1, 1), Some(0), "k from the fold");
    assert_eq!(
        line_up(&folds, 3, 1),
        Some(0),
        "k from its last line steps from its first"
    );
    assert_eq!(
        line_up(&folds, 5, 2),
        Some(1),
        "G2k lands on the fold's first line"
    );
    assert_eq!(line_up(&folds, 4, 1), Some(1));
    assert_eq!(line_up(&folds, 0, 1), None, "k on the first line fails");
    // A fold reaching the last line: `j` from it fails (nvim beeps, the cursor stays).
    let folds = [f(3, 5, true)];
    assert_eq!(line_down(&folds, 3, 1, 5), None);
    assert_eq!(
        line_down(&folds, 0, 5, 5),
        Some(5),
        "a count past the end lands on the last line"
    );
    // A fold at the top: `k` from inside it lands on line 0.
    let folds = [f(0, 2, true)];
    assert_eq!(line_up(&folds, 1, 1), Some(0));
    // An open fold is plain lines.
    let folds = [f(1, 3, false)];
    assert_eq!(line_down(&folds, 1, 1, 5), Some(2));
    assert_eq!(line_up(&folds, 3, 1), Some(2));
}

#[test]
fn closed_range_is_the_outermost_closed_fold() {
    let folds = [fl(1, 5, false, 1), fl(3, 4, true, 2)];
    assert_eq!(
        closed_range(&folds, 3),
        Some((3, 4)),
        "only the inner fold is closed"
    );
    assert_eq!(closed_range(&folds, 2), None);
    let folds = [fl(1, 5, true, 1), fl(3, 4, true, 2)];
    assert_eq!(closed_range(&folds, 3), Some((1, 5)));
}

#[test]
fn open_at_opens_every_closed_fold_holding_the_line() {
    let mut folds = vec![fl(1, 5, true, 1), fl(3, 4, true, 2), fl(7, 8, true, 1)];
    assert!(open_at(&mut folds, 3));
    assert!(!folds[0].closed && !folds[1].closed && folds[2].closed);
    assert!(!open_at(&mut folds, 3), "nothing left to open");
}

#[test]
fn assert_invariants_accepts_flat_and_nested_sets() {
    assert_invariants(&[f(0, 1, true), f(3, 4, false)], 5, true);
    assert_invariants(&[fl(0, 4, true, 1), fl(1, 2, true, 2)], 5, true);
    assert_invariants(&[fl(0, 4, true, 1), fl(1, 2, true, 3)], 5, false);
}

#[test]
#[should_panic(expected = "overlap")]
fn assert_invariants_rejects_overlapping_manual_folds() {
    assert_invariants(&[f(0, 2, true), f(2, 4, true)], 5, true);
}

#[test]
fn next_fold_start_moves_down_past_current() {
    // Two folds: A = [2,4], B = [6,8] (mirrors the nvim manual verification buffer).
    let folds = [f(2, 4, false), f(6, 8, false)];
    // From above every fold → first fold's start.
    assert_eq!(next_fold_start(&folds, 1), Some(2));
    // In the gap between the folds → next fold's start.
    assert_eq!(next_fold_start(&folds, 5), Some(6));
    // Inside fold A (line 3): its OWN start (2) is not below, so we skip to B's start.
    assert_eq!(next_fold_start(&folds, 3), Some(6));
    // Exactly on fold A's start (2): still skip to B (Vim does not re-land the current fold).
    assert_eq!(next_fold_start(&folds, 2), Some(6));
    // At/after the last fold → nothing below.
    assert_eq!(next_fold_start(&folds, 8), None);
    assert_eq!(next_fold_start(&folds, 9), None);
    // No folds at all.
    assert_eq!(next_fold_start(&[], 3), None);
}

#[test]
fn prev_fold_end_moves_up_past_current() {
    let folds = [f(2, 4, false), f(6, 8, false)];
    // From below every fold → last fold's end.
    assert_eq!(prev_fold_end(&folds, 9), Some(8));
    // In the gap → the fold above's end.
    assert_eq!(prev_fold_end(&folds, 5), Some(4));
    // Inside fold B (line 7): its OWN end (8) is not above, so skip to A's end (4).
    assert_eq!(prev_fold_end(&folds, 7), Some(4));
    // Exactly on fold B's end (8): skip to A's end.
    assert_eq!(prev_fold_end(&folds, 8), Some(4));
    // Inside fold A with no fold above → nothing.
    assert_eq!(prev_fold_end(&folds, 3), None);
    // At/above the first fold → nothing above.
    assert_eq!(prev_fold_end(&folds, 2), None);
    assert_eq!(prev_fold_end(&folds, 0), None);
    assert_eq!(prev_fold_end(&[], 3), None);
}

#[test]
fn fold_at_gives_bracket_z_endpoints() {
    // `[z` lands on fold_at(cursor).start; `]z` on .end. Outside any fold → None (no-op).
    let folds = [f(2, 4, false), f(6, 8, true)];
    let idx = fold_at(&folds, 3).unwrap();
    assert_eq!(folds[idx].start, 2, "[z from inside fold A");
    assert_eq!(folds[idx].end, 4, "]z from inside fold A");
    // On a boundary still resolves to that fold.
    assert_eq!(fold_at(&folds, 2), Some(0));
    assert_eq!(fold_at(&folds, 8), Some(1));
    // In the gap → no current fold (Vim beeps; ruse no-ops).
    assert_eq!(fold_at(&folds, 5), None);
}

#[test]
fn next_prev_repeat_composes_for_counts() {
    // `{count}zj` / `{count}zk` iterate the single-step helper; verify a 2-hop chain.
    let folds = [f(2, 4, false), f(6, 8, false)];
    let a = next_fold_start(&folds, 1).unwrap(); // 1 -> 2
    let b = next_fold_start(&folds, a).unwrap(); // 2 -> 6
    assert_eq!((a, b), (2, 6), "2zj from line 1 lands on fold B's start");
    let c = prev_fold_end(&folds, 9).unwrap(); // 9 -> 8
    let d = prev_fold_end(&folds, c).unwrap(); // 8 -> 4
    assert_eq!((c, d), (8, 4), "2zk from line 9 lands on fold A's end");
    // Over-count: the second hop past the last fold returns None, so the caller keeps the last landing.
    assert_eq!(next_fold_start(&folds, b), None);
}

/// The folds as `(start, end, level)`, in order.
fn shape(folds: &[Fold]) -> Vec<(usize, usize, usize)> {
    let mut v: Vec<_> = folds.iter().map(|f| (f.start, f.end, f.level)).collect();
    v.sort_unstable();
    v
}

#[test]
fn insert_closed_fold_nests_as_nvim_fold_create() {
    // A fresh fold is inserted closed over the inclusive range; a disjoint one beside it.
    let mut folds = Vec::new();
    insert_closed_fold(&mut folds, 2, 5);
    assert_eq!(folds, vec![f(2, 5, true)]);
    insert_closed_fold(&mut folds, 7, 9);
    assert_eq!(shape(&folds), [(2, 5, 1), (7, 9, 1)]);
    // Inside a fold: one level deeper (nvim `jzf4jzojzfj`).
    insert_closed_fold(&mut folds, 3, 4);
    assert_eq!(shape(&folds), [(2, 5, 1), (3, 4, 2), (7, 9, 1)]);
    // Partly over a fold: the new fold takes it in whole and starts at its start (`kzf3j`).
    let mut folds = vec![f(1, 5, false)];
    insert_closed_fold(&mut folds, 0, 3);
    assert_eq!(shape(&folds), [(0, 5, 1), (1, 5, 2)]);
    // Starting inside a fold and ending past it: the new fold starts at that fold's start.
    let mut folds = vec![f(1, 5, false)];
    insert_closed_fold(&mut folds, 3, 6);
    assert_eq!(shape(&folds), [(1, 5, 2), (1, 6, 1)]);
    // Around two folds and the fold inside one of them.
    let mut folds = vec![f(1, 2, false), f(4, 5, false)];
    insert_closed_fold(&mut folds, 4, 4);
    insert_closed_fold(&mut folds, 0, 5);
    assert_eq!(shape(&folds), [(0, 5, 1), (1, 2, 2), (4, 4, 3), (4, 5, 2)]);
    // The same lines as a fold: inside it.
    let mut folds = vec![f(2, 3, false)];
    insert_closed_fold(&mut folds, 2, 3);
    assert_eq!(shape(&folds), [(2, 3, 1), (2, 3, 2)]);
    assert_invariants(&folds, 9, true);
    // A reversed range is ignored.
    let before = folds.clone();
    insert_closed_fold(&mut folds, 5, 4);
    assert_eq!(folds, before);
}

#[test]
fn nested_open_close_follow_set_manual_fold() {
    // Outer 1-5 open, inner 2-3 closed (nvim `jzf4jzojzfj`, cursor on line 2).
    let fresh = || vec![fl(1, 5, false, 1), fl(2, 3, true, 2)];
    let closed = |v: &[Fold]| v.iter().map(|f| f.closed).collect::<Vec<_>>();
    // `zc` closes the innermost open fold above the closed one: the outer.
    let mut v = fresh();
    assert_eq!(close_one(&mut v, 2), Some(true));
    assert_eq!(closed(&v), [true, true]);
    // ... and with every fold there closed does nothing, without an error.
    assert_eq!(close_one(&mut v, 2), Some(false));
    // `zo` opens the outermost closed one only.
    assert_eq!(open_one(&mut v, 2), Some(true));
    assert_eq!(closed(&v), [false, true]);
    // `zO` opens the outermost closed one and everything inside it.
    let mut v = fresh();
    v[0].closed = true;
    assert_eq!(open_rec(&mut v, 2), Some(true));
    assert_eq!(closed(&v), [false, false]);
    // `zC` closes every fold holding the line.
    assert_eq!(close_rec(&mut v, 2), Some(true));
    assert_eq!(closed(&v), [true, true]);
    // `zc` with every fold open closes the innermost.
    let mut v = fresh();
    v[1].closed = false;
    assert_eq!(close_one(&mut v, 2), Some(true));
    assert_eq!(closed(&v), [false, true]);
    // Outside the inner fold, the outer is the innermost.
    let mut v = fresh();
    v[1].closed = false;
    assert_eq!(close_one(&mut v, 5), Some(true));
    assert_eq!(closed(&v), [true, false]);
    // No fold at the line: `E490` for all of them.
    let mut v = fresh();
    assert_eq!(open_one(&mut v, 7), None);
    assert_eq!(close_one(&mut v, 7), None);
    assert_eq!(open_rec(&mut v, 7), None);
    assert_eq!(close_rec(&mut v, 7), None);
    // `zv` opens every fold holding the line.
    let mut v = fresh();
    v[0].closed = true;
    assert!(open_cursor_fold(&mut v, 3));
    assert_eq!(closed(&v), [false, false]);
}

#[test]
fn delete_at_moves_nested_folds_up_or_drops_them() {
    let fresh = || vec![fl(1, 5, false, 1), fl(2, 3, true, 2), fl(2, 2, false, 3)];
    // `zd` on the closed inner fold deletes it; the fold inside it goes up a level.
    let mut v = fresh();
    assert!(delete_at(&mut v, 2, false));
    assert_eq!(shape(&v), [(1, 5, 1), (2, 2, 2)]);
    // `zD` there deletes it and the fold inside it.
    let mut v = fresh();
    assert!(delete_at(&mut v, 2, true));
    assert_eq!(shape(&v), [(1, 5, 1)]);
    // `zD` on the open outer fold (a line outside the inner one) deletes all three.
    let mut v = fresh();
    assert!(delete_at(&mut v, 4, true));
    assert!(v.is_empty());
    // With every fold open, `zd` deletes the innermost.
    let mut v = fresh();
    v[1].closed = false;
    assert!(delete_at(&mut v, 2, false));
    assert_eq!(shape(&v), [(1, 5, 1), (2, 3, 2)]);
    // A closed outer fold is the one deleted, whatever is inside it.
    let mut v = fresh();
    v[0].closed = true;
    assert!(delete_at(&mut v, 2, false));
    assert_eq!(shape(&v), [(2, 2, 2), (2, 3, 1)]);
    assert!(!delete_at(&mut v, 7, false));
}

#[test]
fn open_fold_edge_steps_out_of_a_nested_fold() {
    // Outer 1-5 and inner 2-3, both open (nvim `zR[z` on line 2 goes to line 1).
    let v = vec![fl(1, 5, false, 1), fl(2, 3, false, 2)];
    assert_eq!(open_fold_edge(&v, 2, true), Some(1));
    assert_eq!(open_fold_edge(&v, 2, false), Some(3));
    assert_eq!(open_fold_edge(&v, 3, true), Some(2));
    assert_eq!(open_fold_edge(&v, 3, false), Some(5));
    assert_eq!(open_fold_edge(&v, 1, true), None);
    // A closed fold stops the walk: only the open folds around it count.
    let v = vec![fl(1, 5, false, 1), fl(2, 3, true, 2)];
    assert_eq!(open_fold_edge(&v, 3, false), Some(5));
    let v = vec![fl(1, 5, true, 1)];
    assert_eq!(open_fold_edge(&v, 3, true), None);
}

#[test]
fn zj_zk_skip_the_folds_inside_a_closed_fold() {
    let mut v = vec![fl(1, 5, false, 1), fl(2, 3, false, 2), fl(7, 8, false, 1)];
    assert_eq!(next_fold_start(&v, 1), Some(2));
    assert_eq!(prev_fold_end(&v, 7), Some(5));
    assert_eq!(prev_fold_end(&v, 5), Some(3));
    v[0].closed = true;
    assert_eq!(next_fold_start(&v, 1), Some(7));
    assert_eq!(prev_fold_end(&v, 5), None);
}

#[test]
fn a_one_line_fold_never_shows_closed() {
    let v = vec![f(2, 2, true)];
    assert_eq!(closed_range(&v, 2), None);
    assert!(closed_starting_at(&v, 2).is_none());
    assert!(!is_closed_at(&v, 2));
    assert_eq!(fold_at(&v, 2), Some(0));
}

#[test]
fn renest_drops_partial_overlaps_and_resets_levels() {
    let mut v = vec![fl(0, 3, true, 1), fl(2, 5, true, 1), fl(1, 2, false, 1)];
    renest(&mut v);
    assert_eq!(shape(&v), [(0, 3, 1), (1, 2, 2)]);
}

#[test]
fn eliminate_all_clears_every_fold() {
    // `zE` removes all folds outright (regardless of open/closed), matching nvim's "eliminate".
    let mut folds = vec![f(2, 4, true), f(6, 8, false)];
    eliminate_all(&mut folds);
    assert!(folds.is_empty(), "zE eliminates every fold");
    // No-op on an already-empty set.
    eliminate_all(&mut folds);
    assert!(folds.is_empty());
}

#[test]
fn open_cursor_fold_opens_only_the_containing_fold() {
    // `zv` opens the fold under the cursor and leaves the rest closed (nvim-verified).
    let mut folds = vec![f(2, 4, true), f(6, 8, true)];
    assert!(open_cursor_fold(&mut folds, 3), "opened fold A");
    assert!(!folds[0].closed, "fold A now open");
    assert!(folds[1].closed, "fold B stays closed");
    // The fold is OPENED, never removed — zv keeps the fold, unlike zE/zd.
    assert_eq!(folds.len(), 2);
    // A cursor on the fold's start row still opens it.
    let mut folds = vec![f(2, 4, true)];
    assert!(open_cursor_fold(&mut folds, 2));
    // Already-open fold → no change, returns false.
    assert!(!open_cursor_fold(&mut folds, 3));
    // No fold at the cursor → no-op, returns false.
    let mut folds = vec![f(2, 4, true)];
    assert!(!open_cursor_fold(&mut folds, 6));
    assert!(folds[0].closed, "an unrelated fold is untouched");
}

#[test]
fn summary_renders_count_and_text() {
    assert_eq!(
        summary(&f(2, 5, true), "  fn main() {"),
        "\u{25b8} 4 lines: fn main() {"
    );
    assert_eq!(summary(&f(0, 0, true), "   "), "\u{25b8} 1 lines");
}

#[test]
fn hidden_before_counts_nested_closed_folds_once() {
    // Nested closed folds overlap; the inner's rows are a subset of the outer's. hidden_before must count
    // DISTINCT hidden lines, not sum per-fold (which would double-count and mis-place the display row).
    let folds = vec![fl(1, 5, true, 1), fl(3, 4, true, 2)];
    // Hidden lines (inside a closed fold, not a start row): 2,3,4,5. So below line 6 → 4 hidden rows.
    assert_eq!(hidden_before(&folds, 6), 4);
    assert_eq!(visible_row(&folds, 6), 2);
    // Line 1 (outer start) keeps its row.
    assert_eq!(visible_row(&folds, 1), 1);
}

#[test]
fn closed_starting_at_prefers_the_outermost_fold() {
    // If an outer and inner fold shared a start row, the outer (widest) wins the summary.
    let folds = vec![fl(2, 8, true, 1), fl(2, 4, true, 2)];
    assert_eq!(closed_starting_at(&folds, 2).map(|f| f.end), Some(8));
}
