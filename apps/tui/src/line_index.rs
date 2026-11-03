//! A revision-cached line index (D-042 win D): the byte offset of each line start, rebuilt only when the
//! buffer changes. Turns the per-frame line math from O(buffer) scans into O(log n) lookups.
//!
//! `render` asks "what row is the cursor on" and "where does line N start" once per pane per frame; the
//! shared `ruse_core::pos` helpers answer each by scanning newlines from byte 0, which the `line_math`
//! bench measured at 0.6–2.1 ms on a 100k-line buffer — several ms/frame once the viewport range needs it
//! twice per pane. Here the newline scan happens ONCE per edit (keyed on [`Revision`], sound per INV-TXN),
//! and every lookup is a binary search over the cached starts. Idle frames (cursor motion, scroll) reuse
//! the cache and pay only the O(log n) lookup.

use ruse_core::{DocumentId, Revision};

/// Keyed on `(buffer, revision)`: a revision is counted per document, so two buffers (and, before #687,
/// a reloaded one) share revision numbers; the buffer id tells them apart.
#[derive(Default)]
pub struct LineIndex {
    rev: Option<(DocumentId, Revision)>,
    /// Byte offset of every line start; `starts[0]` is always 0. Length = line count.
    starts: Vec<usize>,
    /// The buffer length, so `nth_line_start` past the last line clamps to the end (as the viewport math
    /// expects) rather than to the last line's start.
    len: usize,
}

impl LineIndex {
    /// [`LineIndex::refresh_for`] for a single-buffer caller (benches, tests).
    pub fn refresh(&mut self, rev: Revision, bytes: &[u8]) {
        self.refresh_for(DocumentId(0), rev, bytes);
    }

    /// Rebuild the index for buffer `doc`'s `bytes` at `rev` unless it already holds that buffer at
    /// that revision.
    pub fn refresh_for(&mut self, doc: DocumentId, rev: Revision, bytes: &[u8]) {
        let rev = (doc, rev);
        if self.rev == Some(rev) {
            return;
        }
        self.starts.clear();
        self.starts.push(0);
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                self.starts.push(i + 1);
            }
        }
        self.len = bytes.len();
        self.rev = Some(rev);
    }

    /// The 0-based row of `byte` (the line containing it) — O(log n).
    #[must_use]
    pub fn line_of(&self, byte: usize) -> usize {
        // Count line-starts at or before `byte`; the containing line is the last such start.
        self.starts
            .partition_point(|&s| s <= byte)
            .saturating_sub(1)
    }

    /// The byte offset where 0-based `line` starts, clamped to the buffer end past the last line — O(1).
    #[must_use]
    pub fn nth_line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruse_core::pos;

    #[test]
    fn matches_the_shared_pos_helpers() {
        // The index must agree with the O(n) `pos` primitives at every byte / line — it is a cache of the
        // same facts, so a differential check over a multi-line buffer pins the equivalence.
        let bytes = b"fn a() {}\n\nlet x = 1;\nend\n";
        let mut idx = LineIndex::default();
        idx.refresh(Revision(0), bytes);
        for byte in 0..=bytes.len() {
            assert_eq!(
                idx.line_of(byte),
                pos::line_of(bytes, byte),
                "line_of @ {byte}"
            );
        }
        let lines = bytes.iter().filter(|&&b| b == b'\n').count();
        for line in 0..=lines + 2 {
            assert_eq!(
                idx.nth_line_start(line),
                pos::nth_line_start(bytes, line),
                "nth_line_start @ {line}"
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// A shadow consumer (#906): the index, refreshed by `(buffer, revision)` only, equals the `pos`
        /// scan of the current text after any run of edits, undos and redos — the revision key is never
        /// reused for other text.
        #[test]
        fn the_index_follows_any_edit_undo_redo_sequence(
            init in proptest::collection::vec(
                proptest::sample::select(b"ab\n".to_vec()), 0..16),
            ops in proptest::collection::vec(
                (0u8..4, proptest::prelude::any::<usize>(), 0usize..4,
                 proptest::collection::vec(proptest::sample::select(b"x\n".to_vec()), 0..3)),
                0..30),
        ) {
            use ruse_core::{Document, Edit, EditList, Transaction, TransactionOrigin};
            let mut doc = Document::new(DocumentId(1), init);
            let mut idx = LineIndex::default();
            for (kind, p, d, ins) in ops {
                match kind {
                    0 => { doc.undo(); }
                    1 => { doc.redo(); }
                    _ => {
                        let len = doc.len();
                        let p = p % (len + 1);
                        let list = EditList::new(vec![Edit::replace(p, d.min(len - p), ins)])
                            .expect("one edit");
                        doc.sync_undo();
                        doc.apply(Transaction::new(doc.revision(), list, TransactionOrigin::UserInput))
                            .expect("in range");
                    }
                }
                let bytes = doc.bytes();
                idx.refresh_for(doc.id(), doc.revision(), bytes);
                for b in 0..=bytes.len() {
                    proptest::prop_assert_eq!(idx.line_of(b), pos::line_of(bytes, b));
                }
                let lines = bytes.iter().filter(|&&b| b == b'\n').count();
                for l in 0..=lines + 1 {
                    proptest::prop_assert_eq!(idx.nth_line_start(l), pos::nth_line_start(bytes, l));
                }
            }
        }
    }

    #[test]
    fn reuses_the_cache_on_unchanged_revision() {
        let mut idx = LineIndex::default();
        idx.refresh(Revision(0), b"a\nb\nc");
        // Same revision, different bytes → stale cache retained (proves no rebuild).
        idx.refresh(Revision(0), b"totally different and much longer\n\n\n");
        assert_eq!(
            idx.nth_line_start(1),
            2,
            "unchanged revision keeps the old index"
        );
    }
}
