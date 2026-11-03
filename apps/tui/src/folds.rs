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

use ruse_core::{AnchorId, Document, LineGravity, Motion};

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
/// When nested folds share a start row (an outer and an inner both beginning here), the OUTERMOST — the one
/// with the widest range (largest `end`) — wins, so the summary reflects the largest collapsed span, matching
/// Vim (a closed parent hides the whole subtree behind one summary line).
#[must_use]
pub fn closed_starting_at(folds: &[Fold], line: usize) -> Option<&Fold> {
    folds
        .iter()
        .filter(|f| f.closed && f.start == line)
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
    folds.iter().filter(|f| f.closed && f.contains(line)).fold(
        None,
        |acc: Option<(usize, usize)>, f| {
            Some(acc.map_or((f.start, f.end), |(s, e)| (s.min(f.start), e.max(f.end))))
        },
    )
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

/// Buffer row → display (visible) row, subtracting the rows collapsed by closed folds above it.
#[must_use]
pub fn visible_row(folds: &[Fold], line: usize) -> usize {
    line - hidden_before(folds, line)
}

/// The manual folds of one buffer pinned to its [`Document`] by line anchors, so each fold's range follows
/// the edits made since, as nvim's `foldMarkAdjust` moves a manual fold (#819): lines inserted inside a
/// fold grow it, lines inserted above or below it (`O` on its first line, `o` on its last) shift it or
/// leave it alone, deleted lines shrink it, and a fold whose lines are all deleted is gone. Undo is one
/// more change: a fold its lines' deletion removed does not come back (nvim).
///
/// Each fold holds three anchors: its first line (dropped with its line, landing on the line after), its
/// last line (dropped likewise) and its last line again with [`LineGravity::Cursor`], which lands on the
/// line before a deleted block — where a fold whose last lines were deleted now ends.
#[derive(Debug, Default)]
pub struct Anchored {
    rev: Option<ruse_core::Revision>,
    folds: Vec<(Fold, [AnchorId; 3])>,
}

impl Anchored {
    /// Whether `folds` are the fold ranges anchored at revision `rev`, so nothing needs re-anchoring.
    #[must_use]
    pub fn current(&self, rev: ruse_core::Revision, folds: &[Fold]) -> bool {
        self.rev == Some(rev)
            && self.folds.len() == folds.len()
            && self
                .folds
                .iter()
                .zip(folds)
                .all(|((a, _), b)| same_range(a, b))
    }

    /// Move each fold of `folds` that was anchored to where its anchors went in `doc` since, dropping the
    /// ones whose lines are gone. A fold made after the anchoring (not among the anchored ones) is kept as
    /// it is. `line_of` maps a byte offset of `doc` to its line.
    ///
    /// A first line joined onto the line above (`J` there) is gone from the fold, which then starts on the
    /// next line, as nvim deletes the joined line. Should two manual folds still meet, the later one goes,
    /// so the set stays flat.
    pub fn follow(&self, doc: &Document, folds: &mut Vec<Fold>, line_of: impl Fn(usize) -> usize) {
        if self.rev.is_none() || self.rev == Some(doc.revision()) {
            return;
        }
        let b = doc.bytes();
        folds.retain_mut(|f| {
            let Some((_, ids)) = self.folds.iter().find(|(a, _)| same_range(a, f)) else {
                return true;
            };
            let [Some(s), Some(e), Some(e_up)] = ids.map(|id| doc.resolve_anchor(id)) else {
                return true;
            };
            let s_at = s.offset.0.min(b.len());
            let joined = s_at > 0 && b[s_at - 1] != b'\n';
            let start = line_of(s_at) + usize::from(joined);
            let end = line_of(if e.invalidated {
                e_up.offset.0
            } else {
                e.offset.0
            });
            if (s.invalidated && e.invalidated) || end < start {
                return false;
            }
            f.start = start;
            f.end = end;
            true
        });
        if !folds.iter().any(|f| f.level > 1) {
            let mut kept: Vec<Fold> = Vec::with_capacity(folds.len());
            for f in folds.drain(..) {
                if kept.iter().all(|k| k.end < f.start || f.end < k.start) {
                    kept.push(f);
                }
            }
            *folds = kept;
        }
    }

    /// Re-anchor `folds` in `doc` at its current revision, releasing the old anchors. `line_start` maps a
    /// line to its first byte.
    pub fn anchor(
        &mut self,
        doc: &mut Document,
        folds: &[Fold],
        line_start: impl Fn(usize) -> usize,
    ) {
        self.release(doc);
        self.rev = Some(doc.revision());
        for f in folds {
            let mut at = |l: usize, g: LineGravity| {
                doc.create_line_anchor(ruse_core::BytePos(line_start(l)), g)
            };
            let ids = [
                at(f.start, LineGravity::Drop),
                at(f.end, LineGravity::Drop),
                at(f.end, LineGravity::Cursor),
            ];
            self.folds.push((*f, ids));
        }
    }

    /// Remove every anchor this holds from `doc`.
    pub fn release(&mut self, doc: &mut Document) {
        let ids: Vec<AnchorId> = self.folds.drain(..).flat_map(|(_, ids)| ids).collect();
        doc.remove_anchors(ids);
        self.rev = None;
    }

    /// The anchors this holds, for the invariant walker: each must still be live in its document.
    pub fn anchor_ids(&self) -> impl Iterator<Item = AnchorId> + '_ {
        self.folds.iter().flat_map(|(_, ids)| ids.iter().copied())
    }
}

/// Whether two folds cover the same lines (open or closed).
fn same_range(a: &Fold, b: &Fold) -> bool {
    a.start == b.start && a.end == b.end
}

/// Panic unless `folds` is a well-formed fold set over a buffer of `lines` lines (the invariant walker,
/// #906): every range is inside the buffer and ordered, a manual set is flat (no two folds overlap) and an
/// indent set nests (two folds are disjoint or one holds the other).
pub fn assert_invariants(folds: &[Fold], lines: usize, nested: bool) {
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
            assert!(
                disjoint || (nested && nests),
                "folds {a:?} and {b:?} overlap in a {} set",
                if nested { "nested" } else { "manual" }
            );
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
        level: 1, // manual folds are flat (level 1); nesting is an indent-method concept.
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

// ── `foldmethod=indent` (slice 3): folds are DERIVED from indentation, not created by hand. ───────────────
// These pure functions compute the nested fold set from the buffer bytes + shiftwidth, and translate a
// `foldlevel` value into per-fold closed state. The session recomputes on edit and re-applies `foldlevel`.

/// The indent LEVEL of a single line: its indentation in columns (tabs expanded to the `tabstop`) divided by
/// the `shiftwidth`, rounded DOWN (the caller resolves Vim's `shiftwidth=0` → `tabstop` fallback first).
/// nvim-verified: 4-space indent at sw=4 → 1, 8-space → 2, a 2-space indent at sw=4 → 0 (floor); a single
/// tab at ts=8/sw=4 → 8 cols → level 2.
#[must_use]
fn line_indent_level(line: &[u8], tabstop: usize, shiftwidth: usize) -> usize {
    let tw = tabstop.max(1);
    let mut cols = 0usize;
    for &c in line {
        match c {
            b' ' => cols += 1,
            b'\t' => cols += tw - (cols % tw),
            _ => break,
        }
    }
    cols / shiftwidth.max(1)
}

/// Whether a line is IGNORED for indent folding: it is blank (empty or all whitespace) or a `foldignore`
/// line. Vim's default `foldignore` is `#`, so a line whose first non-blank byte is `#` is ignored too
/// (nvim-verified: a `#`-leading line takes the surrounding fold level, exactly like a blank). An ignored
/// line's effective level is the level of the next NON-ignored line below (Vim's classic blank-line rule):
/// a blank/comment between two indented lines stays inside the fold, but a trailing blank before a dedent
/// (or at end of buffer) drops to level 0 and is excluded from the fold above.
fn is_ignored_line(line: &[u8]) -> bool {
    let first = line.iter().find(|&&c| c != b' ' && c != b'\t');
    matches!(first, None | Some(b'#'))
}

/// Compute the nested `foldmethod=indent` folds for `text` at the given `tabstop` and `shiftwidth`. Pure and
/// unit-tested against nvim v0.12.4. Every returned fold is OPEN (`closed: false`); the caller applies the
/// foldlevel rule via [`apply_foldlevel`].
///
/// The algorithm assigns each line an EFFECTIVE level (a non-ignored line's own indent level; an ignored
/// blank/`#` line takes the level of the next non-ignored line below, or 0 at trailing blanks / EOF). Then,
/// for each depth `d ≥ 1`, every MAXIMAL run of consecutive lines with effective level `≥ d` spanning `≥ 2`
/// lines becomes a fold whose `level` is the MINIMUM effective level within the run. So a 0→2 indent jump
/// yields a single level-2 fold (matching nvim), not a redundant level-1 wrapper, and a lone indented line
/// forms no fold (nvim: `foldclosed` is -1 for it). Identical `[start, end]` ranges are de-duplicated (the
/// jump case emits the same run at several depths). Line indices are 0-based and match the frontend's line
/// numbering (split on `\n`; a trailing `\n` does not add a phantom final line).
#[must_use]
pub fn compute_indent_folds(text: &[u8], tabstop: usize, shiftwidth: usize) -> Vec<Fold> {
    let mut lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    // A trailing newline is the last real line's terminator, not a new empty line ("a\nb\n" = 2 lines).
    if lines.len() > 1 && lines.last() == Some(&&b""[..]) {
        lines.pop();
    }
    let n = lines.len();
    // Own indent level for non-ignored lines; `None` marks an ignored (blank/`#`) line, resolved next.
    let own: Vec<Option<usize>> = lines
        .iter()
        .map(|l| (!is_ignored_line(l)).then(|| line_indent_level(l, tabstop, shiftwidth)))
        .collect();
    // Effective level: scan bottom-up so an ignored line inherits the next non-ignored line's level below it.
    let mut eff = vec![0usize; n];
    let mut next_lvl = 0usize;
    for i in (0..n).rev() {
        match own[i] {
            Some(l) => {
                eff[i] = l;
                next_lvl = l;
            }
            None => eff[i] = next_lvl,
        }
    }
    let max_eff = eff.iter().copied().max().unwrap_or(0);
    let mut folds: Vec<Fold> = Vec::new();
    for d in 1..=max_eff {
        let mut i = 0;
        while i < n {
            if eff[i] < d {
                i += 1;
                continue;
            }
            let s = i;
            while i < n && eff[i] >= d {
                i += 1;
            }
            let e = i - 1;
            if e > s {
                let level = eff[s..=e].iter().copied().min().unwrap_or(d);
                if !folds.iter().any(|f| f.start == s && f.end == e) {
                    folds.push(Fold {
                        start: s,
                        end: e,
                        closed: false,
                        level,
                    });
                }
            }
        }
    }
    folds
}

/// Apply Vim's `foldlevel` rule to a set of (indent) folds: a fold is CLOSED iff its `level` EXCEEDS
/// `foldlevel`. `foldlevel = 0` closes every outermost fold; a value ≥ the deepest level opens all.
/// nvim-verified against `foldclosed()` at several `foldlevel` values on a nested buffer.
pub fn apply_foldlevel(folds: &mut [Fold], foldlevel: usize) {
    for f in folds.iter_mut() {
        f.closed = f.level > foldlevel;
    }
}

/// The deepest fold `level` present (0 if there are no folds) — the `foldlevel` value `zR` sets to open all.
#[must_use]
pub fn max_level(folds: &[Fold]) -> usize {
    folds.iter().map(|f| f.level).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(start: usize, end: usize, closed: bool) -> Fold {
        Fold {
            start,
            end,
            closed,
            level: 1,
        }
    }

    /// A fold with an explicit level, for the indent-method tests.
    fn fl(start: usize, end: usize, closed: bool, level: usize) -> Fold {
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

    fn doc(text: &str) -> Document {
        Document::new(ruse_core::DocumentId(1), text.as_bytes().to_vec())
    }

    /// Apply one edit at `pos` deleting `del` bytes and inserting `ins`.
    fn edit(doc: &mut Document, pos: usize, del: usize, ins: &str) {
        let edits = ruse_core::EditList::new(vec![ruse_core::Edit::replace(
            pos,
            del,
            ins.as_bytes().to_vec(),
        )])
        .unwrap();
        let txn = ruse_core::Transaction::new(
            doc.revision(),
            edits,
            ruse_core::TransactionOrigin::UserInput,
        );
        doc.apply(txn).unwrap();
    }

    fn line_starts(b: &[u8]) -> Vec<usize> {
        std::iter::once(0)
            .chain(
                b.iter()
                    .enumerate()
                    .filter(|(_, &c)| c == b'\n')
                    .map(|(i, _)| i + 1),
            )
            .collect()
    }

    fn line_of(b: &[u8], o: usize) -> usize {
        b[..o.min(b.len())].iter().filter(|&&c| c == b'\n').count()
    }

    /// The folds after `change` edits `text` with `folds` anchored over it.
    fn followed_from(
        text: &str,
        folds: &[Fold],
        change: impl FnOnce(&mut Document),
    ) -> Vec<(usize, usize)> {
        let mut d = doc(text);
        let mut folds = folds.to_vec();
        let mut a = Anchored::default();
        let ls = line_starts(d.bytes());
        a.anchor(&mut d, &folds, |l| ls[l]);
        change(&mut d);
        let b = d.bytes().to_vec();
        a.follow(&d, &mut folds, |o| line_of(&b, o));
        a.release(&mut d);
        assert_eq!(d.anchor_count(), 0, "release frees every anchor");
        folds.iter().map(|f| (f.start, f.end)).collect()
    }

    /// The folds after `change` edits six lines `L1`…`L6` with a fold over lines 1-3 (0-based).
    fn followed(change: impl FnOnce(&mut Document)) -> Vec<(usize, usize)> {
        followed_from("L1\nL2\nL3\nL4\nL5\nL6\n", &[f(1, 3, true)], change)
    }

    /// Each expectation is nvim's `foldclosed()`/`foldclosedend()` after the same change (0-based).
    #[test]
    fn manual_folds_follow_edits_like_nvim() {
        // `o` on the fold's last line (`L4` + `\nNEW`): outside the fold.
        assert_eq!(followed(|d| edit(d, 11, 0, "\nNEW")), vec![(1, 3)]);
        // `O` on its first line: the fold moves down.
        assert_eq!(followed(|d| edit(d, 3, 0, "NEW\n")), vec![(2, 4)]);
        // A line put inside it grows it.
        assert_eq!(followed(|d| edit(d, 6, 0, "X\n")), vec![(1, 4)]);
        // `dd` of all its lines removes it; so does a delete running past it.
        assert_eq!(followed(|d| edit(d, 3, 9, "")), vec![]);
        assert_eq!(followed(|d| edit(d, 3, 12, "")), vec![]);
        // Deleting its first line or its last line shrinks it.
        assert_eq!(followed(|d| edit(d, 3, 3, "")), vec![(1, 2)]);
        assert_eq!(followed(|d| edit(d, 9, 3, "")), vec![(1, 2)]);
        // Deleting lines above shifts it up.
        assert_eq!(followed(|d| edit(d, 0, 3, "")), vec![(0, 2)]);
        // `cc` over the whole fold leaves a one-line fold (nvim `foldlevel` 1 on the new line).
        assert_eq!(followed(|d| edit(d, 3, 8, "!")), vec![(1, 1)]);
        // `J` on its first line joins two of its lines.
        assert_eq!(followed(|d| edit(d, 5, 1, " ")), vec![(1, 2)]);
        // `J` on the line above joins its first line out of it.
        assert_eq!(followed(|d| edit(d, 2, 1, " ")), vec![(1, 2)]);
    }

    /// Two folds that meet: `J` on the first one's last line joins the second's first line into it, which
    /// leaves the second fold starting a line later (nvim), so the two stay apart.
    #[test]
    fn a_join_between_two_folds_keeps_them_apart() {
        let text = "L1\nL2\nL3\nL4\nL5\nL6\n";
        let folds = [f(1, 2, true), f(3, 4, true)];
        assert_eq!(
            followed_from(text, &folds, |d| edit(d, 8, 1, " ")),
            vec![(1, 2), (3, 3)]
        );
    }

    /// A fold at the top whose lines are all deleted is gone too (its `Cursor` anchor stays on line 0).
    #[test]
    fn a_deleted_fold_at_the_top_is_gone() {
        assert_eq!(
            followed_from("L1\nL2\nL3\nL4\n", &[f(0, 1, true)], |d| edit(d, 0, 6, "")),
            vec![]
        );
        // Its first lines deleted, the rest of it stays.
        assert_eq!(
            followed_from("L1\nL2\nL3\nL4\n", &[f(0, 2, true)], |d| edit(d, 0, 6, "")),
            vec![(0, 0)]
        );
    }

    #[test]
    fn assert_invariants_accepts_flat_and_nested_sets() {
        assert_invariants(&[f(0, 1, true), f(3, 4, false)], 5, false);
        assert_invariants(&[fl(0, 4, true, 1), fl(1, 2, true, 2)], 5, true);
    }

    #[test]
    #[should_panic(expected = "overlap")]
    fn assert_invariants_rejects_overlapping_manual_folds() {
        assert_invariants(&[f(0, 2, true), f(2, 4, true)], 5, false);
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
}
