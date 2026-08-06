//! The canonical [`Edit`] and a normalized, non-overlapping [`EditList`].
//!
//! An `Edit` is one replacement `(pos, del, ins)` in the document's canonical byte unit (RFC-0007;
//! architecture §3.3; anchor-store "Edit"). A transaction's edits form an [`EditList`] that is
//! **disjoint and position-sorted** (INV-TXN) — the property that makes both apply and the batched
//! anchor sweep (anchor-store §4) total and order-free. Construction *rejects* overlaps, so a
//! transaction applies atomically or not at all (F-001).

use crate::pos::BytePos;

/// One normalized replacement: delete `del` bytes at `pos`, then insert `ins` there.
///
/// The affected interval is the closed range `[pos, pos + del]` (anchor-store §3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Edit {
    /// Byte gap the edit acts at.
    pub pos: BytePos,
    /// Number of bytes deleted at `pos`.
    pub del: usize,
    /// Bytes inserted at `pos` (after the deletion).
    pub ins: Vec<u8>,
}

impl Edit {
    /// Pure insertion of `bytes` at `pos` (no deletion).
    pub fn insert(pos: usize, bytes: impl Into<Vec<u8>>) -> Edit {
        Edit {
            pos: BytePos(pos),
            del: 0,
            ins: bytes.into(),
        }
    }

    /// Pure deletion of `del` bytes at `pos` (no insertion).
    #[must_use]
    pub fn delete(pos: usize, del: usize) -> Edit {
        Edit {
            pos: BytePos(pos),
            del,
            ins: Vec::new(),
        }
    }

    /// Replace `del` bytes at `pos` with `bytes`.
    pub fn replace(pos: usize, del: usize, bytes: impl Into<Vec<u8>>) -> Edit {
        Edit {
            pos: BytePos(pos),
            del,
            ins: bytes.into(),
        }
    }

    /// Signed length change this edit makes to the document: `ins.len() - del`.
    #[must_use]
    pub fn delta(&self) -> isize {
        self.ins.len() as isize - self.del as isize
    }

    /// End of the affected interval, `pos + del`.
    #[must_use]
    pub fn end(&self) -> usize {
        self.pos.0 + self.del
    }
}

/// Why a set of edits could not form a valid [`EditList`], or could not apply to a buffer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditError {
    /// Two edits share a byte or an insertion gap — the set is not disjoint (INV-TXN).
    Overlap { first_pos: usize, second_pos: usize },
    /// An edit reaches past the end of the buffer it is applied to.
    OutOfRange { pos: usize, del: usize, len: usize },
}

/// A transaction's edit set: **disjoint** and **position-sorted** (INV-TXN).
///
/// [`EditList::new`] normalizes (sorts) and validates disjointness, so downstream apply and the anchor
/// sweep never have to defend against overlaps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditList {
    edits: Vec<Edit>,
}

impl EditList {
    /// Sort by position and reject any overlap. Two sorted edits `a`, `b` are disjoint iff `b` starts
    /// strictly after `a`'s affected interval ends (`b.pos > a.pos + a.del`); sharing a start position
    /// (including two inserts at the same gap) is an [`EditError::Overlap`] because their order would be
    /// ambiguous.
    pub fn new(mut edits: Vec<Edit>) -> Result<EditList, EditError> {
        edits.sort_by_key(|e| e.pos.0);
        for w in edits.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if b.pos.0 == a.pos.0 || b.pos.0 < a.end() {
                return Err(EditError::Overlap {
                    first_pos: a.pos.0,
                    second_pos: b.pos.0,
                });
            }
        }
        Ok(EditList { edits })
    }

    /// Build from edits already known to be sorted and disjoint (e.g. a computed inverse). Debug-asserts
    /// the invariant rather than re-sorting.
    fn from_sorted(edits: Vec<Edit>) -> EditList {
        debug_assert!(
            edits
                .windows(2)
                .all(|w| w[1].pos.0 > w[0].end() && w[1].pos.0 != w[0].pos.0),
            "from_sorted given overlapping or unsorted edits"
        );
        EditList { edits }
    }

    /// The edits, ascending by position.
    #[must_use]
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// Whether the list has no edits (a no-op transaction).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Check every edit lies within a buffer of `len` bytes. Called before apply so a range error is
    /// reported *before* any mutation (atomicity, F-001).
    pub fn check_bounds(&self, len: usize) -> Result<(), EditError> {
        for e in &self.edits {
            if e.pos.0 > len || e.end() > len {
                return Err(EditError::OutOfRange {
                    pos: e.pos.0,
                    del: e.del,
                    len,
                });
            }
        }
        Ok(())
    }

    /// Apply the edits to `buf`, returning the new buffer. Assumes [`EditList::check_bounds`] passed.
    /// Walks edits ascending, splicing each in — O(n + total insert bytes).
    #[must_use]
    pub fn apply_to(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len());
        let mut cur = 0;
        for e in &self.edits {
            out.extend_from_slice(&buf[cur..e.pos.0]);
            out.extend_from_slice(&e.ins);
            cur = e.end();
        }
        out.extend_from_slice(&buf[cur..]);
        out
    }

    /// The exact inverse of applying `self` to `buf`: an `EditList` that, applied to
    /// `self.apply_to(buf)`, restores `buf`. Storing it makes undo O(record) (persistence §2).
    ///
    /// For each forward edit the inverse deletes the bytes that were inserted and re-inserts the bytes
    /// that were deleted, at the position the insertion occupies in the *new* buffer.
    #[must_use]
    pub fn inverse(&self, buf: &[u8]) -> EditList {
        let mut inv = Vec::with_capacity(self.edits.len());
        let mut delta: isize = 0;
        for e in &self.edits {
            let new_pos = (e.pos.0 as isize + delta) as usize;
            let removed = buf[e.pos.0..e.end()].to_vec();
            inv.push(Edit {
                pos: BytePos(new_pos),
                del: e.ins.len(),
                ins: removed,
            });
            delta += e.delta();
        }
        // `new_pos` is strictly increasing with a gap of at least `e.ins.len()`, so the inverse is
        // already sorted and disjoint.
        EditList::from_sorted(inv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_edits_are_rejected() {
        // deleting [2,5) and inserting at 3 (inside the deleted span) is not disjoint.
        let err = EditList::new(vec![Edit::delete(2, 3), Edit::insert(3, b"x".to_vec())]);
        assert!(matches!(err, Err(EditError::Overlap { .. })));
        // two inserts at the same gap are ambiguous → rejected.
        assert!(matches!(
            EditList::new(vec![
                Edit::insert(4, b"a".to_vec()),
                Edit::insert(4, b"b".to_vec())
            ]),
            Err(EditError::Overlap { .. })
        ));
    }

    #[test]
    fn adjacent_edits_are_disjoint() {
        // delete [2,4) then insert at 4 (touching at a point, not overlapping) is allowed.
        let list = EditList::new(vec![Edit::insert(4, b"x".to_vec()), Edit::delete(2, 2)]).unwrap();
        assert_eq!(list.edits().len(), 2);
        assert_eq!(list.edits()[0].pos.0, 2); // sorted ascending
    }

    #[test]
    fn apply_and_inverse_round_trip() {
        let buf = b"hello world".to_vec();
        // replace "hello" with "HI", delete " world" tail's "world" -> two disjoint edits.
        let fwd = EditList::new(vec![
            Edit::replace(0, 5, b"HI".to_vec()),
            Edit::delete(6, 5),
        ])
        .unwrap();
        fwd.check_bounds(buf.len()).unwrap();
        let inv = fwd.inverse(&buf);
        let after = fwd.apply_to(&buf);
        assert_eq!(after, b"HI ");
        // applying the inverse to the new buffer restores the original exactly (INV-UNDO substrate).
        let restored = inv.apply_to(&after);
        assert_eq!(restored, buf);
    }

    #[test]
    fn out_of_range_is_reported_before_apply() {
        let list = EditList::new(vec![Edit::delete(3, 10)]).unwrap();
        assert_eq!(
            list.check_bounds(5),
            Err(EditError::OutOfRange {
                pos: 3,
                del: 10,
                len: 5
            })
        );
    }
}
