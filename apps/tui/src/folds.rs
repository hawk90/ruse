//! Manual folds — pure, view-local state and the geometry that collapses a range of buffer lines into one
//! summary row. No dedicated PRD feature yet: folds are an F-007 (workspace) post-MVP carve-out, and their
//! census rows (`zf`/`zo`/`zc`/… on mode_key.normal) are unclassified pending the parity burn-down (D-043);
//! this ships ahead of that classification. (Earlier this header cited "F-003 slice 1" — wrong: F-003 is
//! the keymap-layer router, not folds.)
//! Frontend-only: a fold changes only what rows paint and how the cursor/scroll
//! skip lines, never the buffer bytes or motion spans (so the core stays view-free, INV-DOC-VIEW). These are
//! the inverse of the F-031 virtual lines — those ADD display rows, a closed fold REMOVES them. Everything
//! here is pure over `&[Fold]`, so it is unit-tested directly; the session owns the `Vec<Fold>` per view and
//! the render/viewport passes consult these helpers.

use ruse_core::Motion;

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

/// The START line of the next fold strictly BELOW `line` (`zj`), or `None` if there is none. A fold whose
/// start is at or above `line` is skipped — so a cursor already inside (or at the start of) a fold jumps to
/// the FOLLOWING fold, matching Vim. Non-overlapping folds mean the answer is the smallest such `start`.
#[must_use]
pub fn next_fold_start(folds: &[Fold], line: usize) -> Option<usize> {
    folds.iter().map(|f| f.start).filter(|&s| s > line).min()
}

/// The END line of the previous fold strictly ABOVE `line` (`zk`), or `None` if there is none. A fold whose
/// end is at or below `line` is skipped, so a cursor inside a fold jumps to the PRECEDING fold's end. The
/// answer is the largest such `end`.
#[must_use]
pub fn prev_fold_end(folds: &[Fold], line: usize) -> Option<usize> {
    folds.iter().map(|f| f.end).filter(|&e| e < line).max()
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

/// Insert a CLOSED fold over the inclusive 0-based line range `[start, end]`, keeping the slice-1
/// non-overlapping invariant: any existing fold that overlaps the new range is dropped first (nesting is
/// deferred — a `zf` over an existing fold REPLACES it rather than nesting). A degenerate `end < start`
/// range is ignored. Used by both Normal `zf{motion}` and Visual `zf`.
pub fn insert_closed_fold(folds: &mut Vec<Fold>, start: usize, end: usize) {
    if end < start {
        return;
    }
    // Drop every fold that overlaps [start, end] (two ranges overlap iff neither is fully before the other).
    folds.retain(|f| f.end < start || f.start > end);
    folds.push(Fold {
        start,
        end,
        closed: true,
    });
}

/// The parse result of feeding the keys typed after `zf` (Normal-mode fold-over-motion): the buffered
/// key string (e.g. `"3j"`, `"gg"`, `"ip"`) resolves to a linewise [`Motion`] + count for the
/// `reindent_range` seam, needs another key, or is not a supported fold motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldMotionParse {
    /// A complete `{count}{motion}` — feed to `Workspace::reindent_range` to get the fold's line range.
    Ready { motion: Motion, count: u32 },
    /// A prefix so far (a partial count, or a multi-key motion lead like `g`/`i`/`a`) — await more keys.
    More,
    /// Not a fold motion — abandon the pending `zf`.
    Cancel,
}

/// Parse the accumulated `zf{motion}` key string into a linewise motion + count for the reindent seam.
/// Recognizes the fold-relevant motions (`:help zf` over the common set): `j`/`k` (line down/up), `G`
/// (to EOF, or `{count}G` to a line), `gg` (to a line), `}`/`{` (paragraph fwd/back), `ip`/`ap`
/// (inner/around paragraph). A leading `1`-`9` run is the count; a bare/partial prefix returns `More`.
#[must_use]
pub fn parse_fold_motion(keys: &str) -> FoldMotionParse {
    // A leading `1`-`9`… run is the count (a leading `0` is NOT a count — it is the column-0 motion, which
    // is not a supported fold motion, so it falls through to `Cancel`).
    let digit_len = if keys.starts_with(|c: char| ('1'..='9').contains(&c)) {
        keys.chars().take_while(char::is_ascii_digit).count()
    } else {
        0
    };
    let count = keys[..digit_len].parse::<u32>().unwrap_or(1).max(1);
    let rest = &keys[digit_len..];
    if rest.is_empty() {
        return FoldMotionParse::More; // still typing the count
    }
    let motion = match rest {
        "j" => Motion::Down,
        "k" => Motion::Up,
        // `{count}G` targets a line (`GotoLine`); bare `G` is to the last line.
        "G" => {
            if digit_len > 0 {
                Motion::GotoLine
            } else {
                Motion::LastLine
            }
        }
        "}" => Motion::ParagraphFwd,
        "{" => Motion::ParagraphBack,
        // Multi-key leads: await the second key.
        "g" | "i" | "a" => return FoldMotionParse::More,
        "gg" => Motion::GotoLine,
        "ip" => Motion::InnerParagraph,
        "ap" => Motion::AParagraph,
        _ => return FoldMotionParse::Cancel,
    };
    FoldMotionParse::Ready { motion, count }
}

/// `zE` — ELIMINATE every fold in the window (Vim: "Eliminate all folds"). Non-nesting slice-1 folds are a
/// flat per-view vector, so this simply empties it (nvim removes the folds outright, not merely opens them —
/// verified: after `zE` `foldclosed()`/`foldlevel()` report no folds). Cursor and buffer bytes are untouched.
pub fn eliminate_all(folds: &mut Vec<Fold>) {
    folds.clear();
}

/// `zv` — "view cursor line": OPEN just enough folds to make `line` visible, without deleting any. For the
/// non-nesting slice-1 model that means open the (single) fold containing `line` if it is closed; other
/// folds keep their state (verified against nvim: `zv` on a line inside fold A opens A but leaves fold B
/// closed). Returns whether a closed fold was opened (so the caller can skip a redundant repaint/no-op).
pub fn open_cursor_fold(folds: &mut [Fold], line: usize) -> bool {
    match fold_at(folds, line) {
        Some(idx) if folds[idx].closed => {
            folds[idx].closed = false;
            true
        }
        _ => false,
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

    #[test]
    fn insert_closed_fold_is_closed_and_replaces_overlaps() {
        // A fresh fold is inserted closed over the inclusive range.
        let mut folds = Vec::new();
        insert_closed_fold(&mut folds, 2, 5);
        assert_eq!(folds, vec![f(2, 5, true)]);

        // A non-overlapping fold coexists (slice-1 non-overlapping set).
        insert_closed_fold(&mut folds, 7, 9);
        assert_eq!(folds, vec![f(2, 5, true), f(7, 9, true)]);

        // A new fold overlapping the FIRST replaces it (nesting deferred); the disjoint one survives.
        insert_closed_fold(&mut folds, 1, 3);
        assert_eq!(folds, vec![f(7, 9, true), f(1, 3, true)]);

        // A new fold spanning BOTH remaining folds replaces both.
        insert_closed_fold(&mut folds, 0, 10);
        assert_eq!(folds, vec![f(0, 10, true)]);

        // A degenerate reversed range is ignored (the caller also guards `end > start`).
        let before = folds.clone();
        insert_closed_fold(&mut folds, 5, 4);
        assert_eq!(folds, before);
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
    fn parse_fold_motion_resolves_the_common_motions() {
        use FoldMotionParse::Ready;
        // Single-key linewise motions, no count.
        assert_eq!(
            parse_fold_motion("j"),
            Ready {
                motion: Motion::Down,
                count: 1
            }
        );
        assert_eq!(
            parse_fold_motion("k"),
            Ready {
                motion: Motion::Up,
                count: 1
            }
        );
        // Bare `G` → last line; `{count}G` → GotoLine with the count.
        assert_eq!(
            parse_fold_motion("G"),
            Ready {
                motion: Motion::LastLine,
                count: 1
            }
        );
        assert_eq!(
            parse_fold_motion("5G"),
            Ready {
                motion: Motion::GotoLine,
                count: 5
            }
        );
        // A count then a line motion (`zf3j`).
        assert_eq!(
            parse_fold_motion("3j"),
            Ready {
                motion: Motion::Down,
                count: 3
            }
        );
        // Paragraph motions.
        assert_eq!(
            parse_fold_motion("}"),
            Ready {
                motion: Motion::ParagraphFwd,
                count: 1
            }
        );
        assert_eq!(
            parse_fold_motion("{"),
            Ready {
                motion: Motion::ParagraphBack,
                count: 1
            }
        );
    }

    #[test]
    fn parse_fold_motion_awaits_multi_key_and_counts() {
        use FoldMotionParse::{More, Ready};
        // Multi-key leads await the second key.
        assert_eq!(parse_fold_motion("g"), More);
        assert_eq!(
            parse_fold_motion("gg"),
            Ready {
                motion: Motion::GotoLine,
                count: 1
            }
        );
        assert_eq!(parse_fold_motion("i"), More);
        assert_eq!(
            parse_fold_motion("ip"),
            Ready {
                motion: Motion::InnerParagraph,
                count: 1
            }
        );
        assert_eq!(parse_fold_motion("a"), More);
        assert_eq!(
            parse_fold_motion("ap"),
            Ready {
                motion: Motion::AParagraph,
                count: 1
            }
        );
        // A partial count alone awaits its motion.
        assert_eq!(parse_fold_motion("3"), More);
        assert_eq!(parse_fold_motion("12"), More);
    }

    #[test]
    fn parse_fold_motion_cancels_unsupported() {
        use FoldMotionParse::Cancel;
        // A leading `0` is the column-0 motion, not a count — unsupported for folds.
        assert_eq!(parse_fold_motion("0"), Cancel);
        // Unknown / non-fold motions abandon the pending zf.
        assert_eq!(parse_fold_motion("x"), Cancel);
        assert_eq!(parse_fold_motion("w"), Cancel);
        assert_eq!(parse_fold_motion("gx"), Cancel);
        assert_eq!(parse_fold_motion("ix"), Cancel);
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
