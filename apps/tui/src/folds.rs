//! Manual folds (F-003 slice 1) — pure, view-local state and the geometry that collapses a range of buffer
//! lines into one summary row. Frontend-only: a fold changes only what rows paint and how the cursor/scroll
//! skip lines, never the buffer bytes or motion spans (so the core stays view-free, INV-DOC-VIEW). These are
//! the inverse of the F-031 virtual lines — those ADD display rows, a closed fold REMOVES them. Everything
//! here is pure over `&[Fold]`, so it is unit-tested directly; the session owns the `Vec<Fold>` per view and
//! the render/viewport passes consult these helpers.

/// One manual fold: the inclusive buffer line range `[start, end]` and whether it is currently collapsed.
/// Slice 1 keeps folds non-overlapping (nesting is deferred).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
    pub closed: bool,
}

impl Fold {
    /// Whether buffer `line` lies within this fold's range (inclusive).
    #[must_use]
    pub fn contains(&self, line: usize) -> bool {
        self.start <= line && line <= self.end
    }
    /// The number of buffer lines this fold spans (≥ 1).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.end - self.start + 1
    }
}

/// Index of the fold whose range contains `line`, or `None`. (Slice 1 folds are non-overlapping, so at most
/// one matches.) Used by `za`/`zo`/`zc`/`zd` to act on the fold at the cursor.
#[must_use]
pub fn fold_at(folds: &[Fold], line: usize) -> Option<usize> {
    folds.iter().position(|f| f.contains(line))
}

/// The CLOSED fold that starts at buffer `line` (the row whose summary the renderer paints), or `None`.
#[must_use]
pub fn closed_starting_at(folds: &[Fold], line: usize) -> Option<&Fold> {
    folds.iter().find(|f| f.closed && f.start == line)
}

/// Whether buffer `line` is HIDDEN by a closed fold — inside a closed fold but NOT its start row (the start
/// row stays visible as the summary). The renderer skips hidden lines; the cursor snaps out of them.
#[must_use]
pub fn hidden(folds: &[Fold], line: usize) -> bool {
    folds
        .iter()
        .any(|f| f.closed && line > f.start && line <= f.end)
}

/// If `line` is hidden by a closed fold, the fold's START line (where the cursor/render belongs); else `line`.
#[must_use]
pub fn snap_out(folds: &[Fold], line: usize) -> usize {
    folds
        .iter()
        .find(|f| f.closed && f.contains(line))
        .map_or(line, |f| f.start)
}

/// The count of buffer lines hidden by closed folds strictly ABOVE `line` — the number of display rows the
/// folds remove before `line`. `visible_row = line − hidden_before(line)` maps a buffer row to its display
/// row (a hidden line maps to its fold's start's display row).
#[must_use]
pub fn hidden_before(folds: &[Fold], line: usize) -> usize {
    folds
        .iter()
        .filter(|f| f.closed)
        .map(|f| {
            // Hidden rows of this fold that sit strictly below its start AND strictly above `line`.
            let lo = f.start + 1;
            let hi = f.end.min(line.saturating_sub(1));
            if hi >= lo {
                hi - lo + 1
            } else {
                0
            }
        })
        .sum()
}

/// Buffer row → display (visible) row, subtracting the rows collapsed by closed folds above it.
#[must_use]
pub fn visible_row(folds: &[Fold], line: usize) -> usize {
    line - hidden_before(folds, line)
}

/// Adjust folds after an edit at buffer line `at_line` that changed the line count by `delta`: folds fully
/// BELOW the edit shift by `delta`; a fold that STRADDLES the edit line is dropped (its range is no longer
/// trustworthy); folds fully above are untouched. A coarse-but-correct rule for slice 1.
pub fn shift(folds: &mut Vec<Fold>, at_line: usize, delta: isize) {
    if delta == 0 {
        return;
    }
    folds.retain(|f| !(f.start <= at_line && at_line <= f.end && delta < 0));
    for f in folds.iter_mut() {
        if f.start > at_line {
            f.start = (f.start as isize + delta).max(0) as usize;
            f.end = (f.end as isize + delta).max(0) as usize;
        }
    }
}

/// The one-line summary a closed fold paints on its start row: `▸ {N} lines: {first-line text}` (trimmed).
#[must_use]
pub fn summary(fold: &Fold, first_line: &str) -> String {
    let text = first_line.trim();
    if text.is_empty() {
        format!("\u{25b8} {} lines", fold.line_count())
    } else {
        format!("\u{25b8} {} lines: {text}", fold.line_count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(start: usize, end: usize, closed: bool) -> Fold {
        Fold { start, end, closed }
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

    #[test]
    fn shift_moves_below_and_drops_straddled() {
        // Insert 2 lines at line 1: a fold at [4,6] moves to [6,8].
        let mut folds = vec![f(4, 6, true)];
        shift(&mut folds, 1, 2);
        assert_eq!(folds, vec![f(6, 8, true)]);
        // Deleting a line inside a fold's range drops it.
        let mut folds = vec![f(4, 6, true)];
        shift(&mut folds, 5, -1);
        assert!(folds.is_empty(), "a fold straddling a deletion is dropped");
        // A fold above the edit is untouched.
        let mut folds = vec![f(1, 2, true)];
        shift(&mut folds, 8, 3);
        assert_eq!(folds, vec![f(1, 2, true)]);
    }

    #[test]
    fn summary_renders_count_and_text() {
        assert_eq!(
            summary(&f(2, 5, true), "  fn main() {"),
            "\u{25b8} 4 lines: fn main() {"
        );
        assert_eq!(summary(&f(0, 0, true), "   "), "\u{25b8} 1 lines");
    }
}
