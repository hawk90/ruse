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
//!
//! A closed fold counts as one line for the commands that move by lines (#819): `j`/`k` step over it
//! ([`line_down`], [`line_up`], nvim's `cursor_down`/`cursor_up`) and an operator whose range starts or ends
//! in one takes the whole fold ([`closed_range`]); the session rewrites those commands before the core runs
//! them, so the core keeps counting buffer lines. The cursor may rest on any line of a closed fold, as in
//! Vim; it paints on the fold's summary row. Manual folds follow edits by line anchors ([`Anchored`]).

mod anchored;
mod indent;
mod motion_keys;

pub use anchored::Anchored;
pub use indent::{apply_foldlevel, compute_indent_folds, max_level};
pub use motion_keys::{parse_fold_motion, FoldMotionParse};

/// One fold: the inclusive buffer line range `[start, end]`, whether it is currently collapsed, and its
/// nesting `level` (1 = outermost). Manual (`zf`) folds are flat and always level 1; `foldmethod=indent`
/// folds (slice 3) can NEST, so an inner fold has a larger level and its range is contained in its parent's.
/// A fold with `level > foldlevel` is closed under the indent method (see [`apply_foldlevel`]); manual folds
/// ignore `foldlevel` and are driven purely by their `closed` flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
    pub closed: bool,
    /// Nesting depth, 1 = outermost. Manual folds use 1; indent folds carry the level the range acquires.
    pub level: usize,
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

/// Index of the innermost fold whose range contains `line`, or `None`: the deepest one on [`path`].
#[must_use]
pub fn fold_at(folds: &[Fold], line: usize) -> Option<usize> {
    path(folds, line).last().copied()
}

/// The indices of the folds holding `line`, outermost first (nvim walks a fold tree from the top). Folds
/// nest (a manual `zf` inside a fold, or `foldmethod=indent`), so each one on the path holds the next.
#[must_use]
pub fn path(folds: &[Fold], line: usize) -> Vec<usize> {
    let mut on: Vec<usize> = (0..folds.len())
        .filter(|&i| folds[i].contains(line))
        .collect();
    on.sort_by_key(|&i| {
        (
            folds[i].level,
            folds[i].start,
            std::cmp::Reverse(folds[i].end),
        )
    });
    on
}

/// Whether fold `j` lies inside fold `i` (deeper and within its lines): one of `i`'s nested folds.
fn nested_in(folds: &[Fold], i: usize, j: usize) -> bool {
    let (a, b) = (&folds[i], &folds[j]);
    i != j && b.level > a.level && a.start <= b.start && b.end <= a.end
}

/// Whether fold `j` is inside a closed fold, so it is not on the screen as a fold of its own: `zj`/`zk`
/// count a closed fold as one.
fn inside_closed(folds: &[Fold], j: usize) -> bool {
    (0..folds.len()).any(|i| shows_closed(&folds[i]) && nested_in(folds, i, j))
}

/// Whether a fold's lines show as one summary line when it is closed: one of at least two lines (nvim's
/// 'foldminlines' of 1). A one-line fold is kept, with its level, but never hides its line.
fn shows_closed(f: &Fold) -> bool {
    f.closed && f.end > f.start
}

/// `zo` (once) on `line`, nvim's `setManualFold` opening one level: open the outermost closed fold that
/// holds it. `None` when no fold holds `line` (`E490`), else whether a fold was opened.
pub fn open_one(folds: &mut [Fold], line: usize) -> Option<bool> {
    let on = path(folds, line);
    if on.is_empty() {
        return None;
    }
    Some(on.into_iter().find(|&i| folds[i].closed).is_some_and(|i| {
        folds[i].closed = false;
        true
    }))
}

/// `zO` on `line`: open the outermost closed fold that holds it and every fold nested in that one. `None`
/// when no fold holds `line`.
pub fn open_rec(folds: &mut [Fold], line: usize) -> Option<bool> {
    let on = path(folds, line);
    if on.is_empty() {
        return None;
    }
    let Some(i) = on.into_iter().find(|&i| folds[i].closed) else {
        return Some(false);
    };
    for j in 0..folds.len() {
        if j == i || nested_in(folds, i, j) {
            folds[j].closed = false;
        }
    }
    Some(true)
}

/// `zc` (once) on `line`: close the innermost open fold above the first closed one that holds it (nvim
/// closes "one more level"; with every fold there closed it does nothing). `None` when no fold holds
/// `line`.
pub fn close_one(folds: &mut [Fold], line: usize) -> Option<bool> {
    let on = path(folds, line);
    if on.is_empty() {
        return None;
    }
    let deepest_open = on.into_iter().take_while(|&i| !folds[i].closed).last();
    Some(deepest_open.is_some_and(|i| {
        folds[i].closed = true;
        true
    }))
}

/// `zC` on `line`: close every fold that holds it. `None` when none does.
pub fn close_rec(folds: &mut [Fold], line: usize) -> Option<bool> {
    let on = path(folds, line);
    if on.is_empty() {
        return None;
    }
    let mut done = false;
    for i in on {
        done |= !folds[i].closed;
        folds[i].closed = true;
    }
    Some(done)
}

/// `:{range}foldopen[!]` / `:{range}foldclose[!]` over the 0-based lines `first..=last`, nvim's
/// `opFoldRange`: a `zo`/`zc` (`zO`/`zC` when `recursive`) on each line in turn, skipping past the closed
/// fold a level-one open leaves (or a close made). Whether any fold held a line of the range (else
/// `E490`).
pub fn op_range(
    folds: &mut [Fold],
    first: usize,
    last: usize,
    opening: bool,
    recursive: bool,
) -> bool {
    let mut found = false;
    let mut line = first;
    while line <= last {
        let mut next = line;
        if opening && !recursive {
            next = closed_range(folds, line).map_or(line, |r| r.1);
        }
        let hit = match (opening, recursive) {
            (true, false) => open_one(folds, line),
            (true, true) => open_rec(folds, line),
            (false, false) => close_one(folds, line),
            (false, true) => close_rec(folds, line),
        };
        found |= hit.is_some();
        if !opening && !recursive {
            next = closed_range(folds, line).map_or(line, |r| r.1);
        }
        line = next + 1;
    }
    found
}

/// Whether a closed fold holds `line` (nvim's `foldClosed` ≥ 0 for `za`/`zA`: closed with its lines
/// hidden, so a closed one-line fold does not count).
#[must_use]
pub fn is_closed_at(folds: &[Fold], line: usize) -> bool {
    closed_range(folds, line).is_some()
}

/// The fold `zd`/`zD` act on at `line`: the innermost fold holding it, or the outermost closed one when a
/// closed fold holds it (nvim's `deleteFold` does not look inside a closed fold).
#[must_use]
pub fn delete_target(folds: &[Fold], line: usize) -> Option<usize> {
    let on = path(folds, line);
    let first_closed = on.iter().position(|&i| shows_closed(&folds[i]));
    match first_closed {
        Some(k) => Some(on[k]),
        None => on.last().copied(),
    }
}

/// `zd` (`recursive` false) or `zD` on `line`: delete the fold [`delete_target`] picks. `zd` moves its
/// nested folds one level up; `zD` deletes them too. Whether a fold held `line`.
pub fn delete_at(folds: &mut Vec<Fold>, line: usize, recursive: bool) -> bool {
    let Some(i) = delete_target(folds, line) else {
        return false;
    };
    let inner: Vec<bool> = (0..folds.len()).map(|j| nested_in(folds, i, j)).collect();
    let mut k = 0;
    folds.retain_mut(|f| {
        let j = k;
        k += 1;
        if j == i || (recursive && inner[j]) {
            return false;
        }
        if inner[j] {
            f.level -= 1;
        }
        true
    });
    true
}

/// The START line of the next fold strictly BELOW `line` (`zj`), or `None` if there is none. A fold whose
/// start is at or above `line` is skipped — so a cursor already inside (or at the start of) a fold jumps to
/// the FOLLOWING fold, nested ones included, matching Vim. A fold inside a closed fold is not one of its
/// own (a closed fold counts as one).
#[must_use]
pub fn next_fold_start(folds: &[Fold], line: usize) -> Option<usize> {
    (0..folds.len())
        .filter(|&i| !inside_closed(folds, i))
        .map(|i| folds[i].start)
        .filter(|&s| s > line)
        .min()
}

/// The END line of the previous fold strictly ABOVE `line` (`zk`), or `None` if there is none. A fold whose
/// end is at or below `line` is skipped, so a cursor inside a fold jumps to the PRECEDING fold's end. The
/// answer is the largest such `end`, skipping the folds inside a closed fold.
#[must_use]
pub fn prev_fold_end(folds: &[Fold], line: usize) -> Option<usize> {
    (0..folds.len())
        .filter(|&i| !inside_closed(folds, i))
        .map(|i| folds[i].end)
        .filter(|&e| e < line)
        .max()
}

/// `[z` (`start`) or `]z` on `line`: the line of the start (or end) of the innermost open fold holding
/// `line` that is not already at it, looking only at the folds above the first closed one (nvim's
/// `foldMoveTo`). At the start of a nested fold `[z` goes to the start of the fold around it.
#[must_use]
pub fn open_fold_edge(folds: &[Fold], line: usize, start: bool) -> Option<usize> {
    let mut found = None;
    for i in path(folds, line) {
        let f = &folds[i];
        if shows_closed(f) {
            break;
        }
        if start && f.start < line {
            found = Some(f.start);
        } else if !start && f.end > line {
            found = Some(f.end);
        }
    }
    found
}

/// The CLOSED fold that starts at buffer `line` (the row whose summary the renderer paints), or `None`.
/// When nested folds share a start row (an outer and an inner both beginning here), the OUTERMOST — the one
/// with the widest range (largest `end`) — wins, so the summary reflects the largest collapsed span, matching
/// Vim (a closed parent hides the whole subtree behind one summary line).
#[must_use]
pub fn closed_starting_at(folds: &[Fold], line: usize) -> Option<&Fold> {
    folds
        .iter()
        .filter(|f| shows_closed(f) && f.start == line)
        .max_by_key(|f| f.end)
}

/// Whether buffer `line` is HIDDEN by a closed fold — inside a closed fold but NOT its start row (the start
/// row stays visible as the summary). The renderer skips hidden lines; the cursor snaps out of them.
#[must_use]
pub fn hidden(folds: &[Fold], line: usize) -> bool {
    folds
        .iter()
        .any(|f| f.closed && line > f.start && line <= f.end)
}

/// The line range `(start, end)` of the closed fold holding `line`, or `None` when no closed fold holds it.
/// With nested folds it is the OUTERMOST closed one (the widest range), which is what hides `line`: nvim's
/// `hasFolding`.
#[must_use]
pub fn closed_range(folds: &[Fold], line: usize) -> Option<(usize, usize)> {
    folds
        .iter()
        .filter(|f| shows_closed(f) && f.contains(line))
        .fold(None, |acc: Option<(usize, usize)>, f| {
            Some(acc.map_or((f.start, f.end), |(s, e)| (s.min(f.start), e.max(f.end))))
        })
}

/// The line `n` lines below `line`, a closed fold counting as one line, or `None` when the move fails
/// because `line` (or the end of its closed fold) is the `last` line. nvim's `cursor_down`: from inside a
/// closed fold it steps from the fold's end, and a count that reaches past the end lands on `last`.
#[must_use]
pub fn line_down(folds: &[Fold], line: usize, n: usize, last: usize) -> Option<usize> {
    let mut l = closed_range(folds, line).map_or(line, |r| r.1);
    if n > 0 && l >= last {
        return None;
    }
    if l + n >= last {
        return Some(last);
    }
    for _ in 0..n {
        l = closed_range(folds, l).map_or(l, |r| r.1) + 1;
        if l >= last {
            break;
        }
    }
    Some(l.min(last))
}

/// The line `n` lines above `line`, a closed fold counting as one line and landing on its first line, or
/// `None` when `line` is the first line. nvim's `cursor_up`: a count at least the line number goes to the
/// first line.
#[must_use]
pub fn line_up(folds: &[Fold], line: usize, n: usize) -> Option<usize> {
    if n > 0 && line == 0 {
        return None;
    }
    if n > line {
        return Some(0);
    }
    let start = |l: usize| closed_range(folds, l).map_or(l, |r| r.0);
    let mut l = start(line);
    for _ in 0..n {
        l = l.saturating_sub(1);
        if l == 0 {
            break;
        }
        l = start(l);
    }
    Some(l)
}

/// Open every closed fold that holds `line` (nvim's `foldOpenCursor`). Returns whether one was opened.
pub fn open_at(folds: &mut [Fold], line: usize) -> bool {
    let mut opened = false;
    for f in folds.iter_mut().filter(|f| f.closed && f.contains(line)) {
        f.closed = false;
        opened = true;
    }
    opened
}

/// If `line` is inside a closed fold, the fold's START line (the summary row a cursor there paints on);
/// else `line`.
#[must_use]
pub fn snap_out(folds: &[Fold], line: usize) -> usize {
    closed_range(folds, line).map_or(line, |r| r.0)
}

/// The count of buffer lines hidden by closed folds strictly ABOVE `line` — the number of display rows the
/// folds remove before `line`. `visible_row = line − hidden_before(line)` maps a buffer row to its display
/// row (a hidden line maps to its fold's start's display row). Counts DISTINCT hidden lines, so overlapping
/// (nested) closed folds — where an inner fold's rows are a subset of its parent's — are not double-counted.
#[must_use]
pub fn hidden_before(folds: &[Fold], line: usize) -> usize {
    (0..line).filter(|&l| hidden(folds, l)).count()
}

/// The count of lines in `[a, b)` hidden by closed folds (nested folds counted once), without walking the
/// lines: what `'relativenumber'` skips between two lines (#616).
#[must_use]
pub fn hidden_between(folds: &[Fold], a: usize, b: usize) -> usize {
    let mut spans: Vec<(usize, usize)> = folds
        .iter()
        .filter(|f| f.closed && f.end > f.start)
        .map(|f| ((f.start + 1).max(a), (f.end + 1).min(b)))
        .filter(|&(s, e)| s < e)
        .collect();
    spans.sort_unstable();
    let (mut n, mut reach) = (0, a);
    for (s, e) in spans {
        let s = s.max(reach);
        if e > s {
            n += e - s;
            reach = e;
        }
    }
    n
}

/// Buffer row → display (visible) row, subtracting the rows collapsed by closed folds above it.
#[must_use]
pub fn visible_row(folds: &[Fold], line: usize) -> usize {
    line - hidden_before(folds, line)
}

/// Panic unless `folds` is a well-formed fold set over a buffer of `lines` lines (the invariant walker,
/// #906): every range is inside the buffer and ordered, and two folds are disjoint or one holds the other.
/// A `manual` set's levels are also its depths, as [`renest`] gives them, so nested folds of the same lines
/// keep a strict order.
pub fn assert_invariants(folds: &[Fold], lines: usize, manual: bool) {
    for f in folds {
        assert!(
            f.start <= f.end && f.end < lines.max(1),
            "fold {f:?} outside a {lines}-line buffer"
        );
        assert!(f.level >= 1, "fold {f:?} has no level");
    }
    for (i, a) in folds.iter().enumerate() {
        for b in &folds[i + 1..] {
            let disjoint = a.end < b.start || b.end < a.start;
            let nests =
                (a.start <= b.start && b.end <= a.end) || (b.start <= a.start && a.end <= b.end);
            assert!(disjoint || nests, "folds {a:?} and {b:?} overlap");
        }
    }
    if manual {
        let key = |f: &Fold| (f.start, std::cmp::Reverse(f.end), f.level);
        let mut sorted = folds.to_vec();
        sorted.sort_by_key(key);
        let mut renested = sorted.clone();
        renest(&mut renested);
        assert_eq!(sorted, renested, "manual fold levels are not their depths");
    }
}

/// Create a CLOSED manual fold over the inclusive 0-based line range `[start, end]` (`zf`, `:fold`), as
/// nvim's `foldCreate` nests it: inside the deepest fold that holds the whole range, and around the folds
/// at that level that it meets. A fold it only partly covers is taken in whole, so the new fold starts at
/// that fold's start or ends at its end. The folds it takes in go one level deeper. A reversed range is
/// ignored.
pub fn insert_closed_fold(folds: &mut Vec<Fold>, start: usize, end: usize) {
    if end < start {
        return;
    }
    // The deepest fold that holds the whole range (nvim goes one level deeper while the fold holding
    // `start` reaches `end`). Its level + 1 is the new fold's.
    let mut parent: Option<usize> = None;
    loop {
        let level = parent.map_or(1, |p| folds[p].level + 1);
        let within =
            |f: &Fold| parent.is_none_or(|p| folds[p].start <= f.start && f.end <= folds[p].end);
        let holder = (0..folds.len())
            .find(|&i| folds[i].level == level && within(&folds[i]) && folds[i].contains(start));
        match holder {
            Some(h) if folds[h].end >= end => parent = Some(h),
            _ => break,
        }
    }
    let level = parent.map_or(1, |p| folds[p].level + 1);
    let (lo, hi) = parent.map_or((0, usize::MAX), |p| (folds[p].start, folds[p].end));
    // The folds at that level the new one meets: the one holding `start`, and those starting up to `end`.
    let (mut s, mut e) = (start, end);
    for f in folds.iter() {
        if f.level == level && lo <= f.start && f.end <= hi && f.end >= start && f.start <= end {
            s = s.min(f.start);
            e = e.max(f.end);
        }
    }
    for f in folds.iter_mut() {
        if f.level >= level && lo <= f.start && f.end <= hi && s <= f.start && f.end <= e {
            f.level += 1;
        }
    }
    folds.push(Fold {
        start: s,
        end: e,
        closed: true,
        level,
    });
}

/// Restore the nesting of a manual fold set after its ranges moved with an edit: a fold that now only
/// partly overlaps an earlier one is dropped, and each level is set again to the fold's depth. Folds that
/// cover the same lines keep their order.
pub fn renest(folds: &mut Vec<Fold>) {
    folds.sort_by_key(|f| (f.start, std::cmp::Reverse(f.end), f.level));
    let mut stack: Vec<usize> = Vec::new();
    let mut kept: Vec<Fold> = Vec::with_capacity(folds.len());
    for mut f in folds.drain(..) {
        while stack.last().is_some_and(|&t| kept[t].end < f.start) {
            stack.pop();
        }
        if stack.last().is_some_and(|&t| f.end > kept[t].end) {
            continue;
        }
        f.level = stack.len() + 1;
        stack.push(kept.len());
        kept.push(f);
    }
    *folds = kept;
}

/// `zE` — ELIMINATE every fold in the window (Vim: "Eliminate all folds"). Non-nesting slice-1 folds are a
/// flat per-view vector, so this simply empties it (nvim removes the folds outright, not merely opens them —
/// verified: after `zE` `foldclosed()`/`foldlevel()` report no folds). Cursor and buffer bytes are untouched.
pub fn eliminate_all(folds: &mut Vec<Fold>) {
    folds.clear();
}

/// `zv` — "view cursor line": open just enough folds to make `line` visible (nvim's `foldOpenCursor`
/// repeats a `zo` until no closed fold holds it), without deleting any. Folds not holding `line` keep
/// their state. Returns whether a fold was opened.
pub fn open_cursor_fold(folds: &mut [Fold], line: usize) -> bool {
    let mut opened = false;
    while open_one(folds, line) == Some(true) {
        opened = true;
    }
    opened
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
mod tests;
