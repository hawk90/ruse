//! Manual folds pinned to the document by line anchors, so they follow edits.

use ruse_core::{AnchorId, Document, LineGravity};

use super::{renest, Fold};

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
    /// next line, as nvim deletes the joined line. Should two manual folds now overlap without one holding
    /// the other, the later one goes, so the set still nests ([`renest`]).
    pub fn follow(&self, doc: &Document, folds: &mut Vec<Fold>, line_of: impl Fn(usize) -> usize) {
        if self.rev.is_none() || self.rev == Some(doc.revision()) {
            return;
        }
        let b = doc.bytes();
        // The slot after a final newline is no line (`dispatch::line_count`), though an anchor can land
        // there: `x` on the only char of a last line with no newline (`\nh` to `\n`) leaves the fold's
        // line nowhere (#1296). nvim keeps that line, empty; the buffer here cannot hold an empty last
        // line without a newline, so the fold goes with its line, and one that ran onto it ends a line up.
        let last =
            (ruse_core::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'))).max(1) - 1;
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
            })
            .min(last);
            if (s.invalidated && e.invalidated) || end < start {
                return false;
            }
            f.start = start;
            f.end = end;
            true
        });
        renest(folds);
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

#[cfg(test)]
mod tests;
