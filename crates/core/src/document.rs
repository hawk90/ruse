//! The editable Document — atomic transaction apply, undo/redo, and the anchor store, tied together
//! (INV-TXN, INV-UNDO, INV-ANCHOR).
//!
//! Every mutation goes through [`Document::apply`], which enforces INV-TXN (a stale `base_revision` is
//! refused; a successful apply strictly increases the revision) and F-001 atomicity (bounds are checked
//! *before* any byte moves, so a rejected transaction leaves the document untouched). Undo/redo
//! *navigate* the undo tree, re-applying stored inverse/forward edits as new revisions without creating
//! new undo nodes (persistence §7).

use std::sync::Arc;

use crate::anchor::{AnchorId, AnchorPolicy, AnchorStore, Bias, Resolved};
use crate::edit::{EditError, EditList};
use crate::pos::Revision;
use crate::snapshot::{AnchorIndex, DocumentSnapshot};
use crate::transaction::Transaction;
use crate::undo::{MonotonicSeq, UndoHistory};

/// A typed, generation-free document handle (INV-HANDLE). Process-unique id per open document.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DocumentId(pub u64);

/// Why a transaction could not be applied — a typed, expected failure (INV-ERR-CLASS), never a panic.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TxnError {
    /// The edit set was computed against a revision the document has already moved past (INV-TXN).
    StaleBaseRevision { base: Revision, current: Revision },
    /// An edit reaches past the end of the document; the document is left untouched (F-001 atomicity).
    OutOfRange { pos: usize, del: usize, len: usize },
}

/// An in-memory editable document: bytes + revision + anchors + undo history.
pub struct Document {
    id: DocumentId,
    text: Arc<[u8]>,
    revision: Revision,
    anchors: AnchorStore,
    undo: UndoHistory,
    anchor_index: Arc<AnchorIndex>,
    saved_node: Option<MonotonicSeq>,
}

impl Document {
    /// Open a document with `initial` bytes at [`Revision::ZERO`]. It starts *unsaved*
    /// ([`Document::is_modified`] is true until [`Document::mark_saved`]); a document loaded from disk
    /// marks itself saved at open.
    pub fn new(id: DocumentId, initial: impl Into<Vec<u8>>) -> Document {
        let text: Arc<[u8]> = Arc::from(initial.into());
        let mut d = Document {
            id,
            text,
            revision: Revision::ZERO,
            anchors: AnchorStore::new(),
            undo: UndoHistory::new(Revision::ZERO),
            anchor_index: Arc::new(AnchorIndex::default()),
            saved_node: None,
        };
        d.rebuild_index();
        d
    }

    /// This document's handle.
    #[must_use]
    pub fn id(&self) -> DocumentId {
        self.id
    }

    /// The current revision.
    #[must_use]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// The current document bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.text
    }

    /// The document as UTF-8 text, or `None` if not valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.text).ok()
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Whether the document is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Create a long-lived anchor at `offset` with the given [`Bias`] and span-delete [`AnchorPolicy`].
    pub fn create_anchor(&mut self, offset: usize, bias: Bias, policy: AnchorPolicy) -> AnchorId {
        let id = self.anchors.insert(offset, bias, policy);
        self.rebuild_index();
        id
    }

    /// Remove an anchor. Returns whether it was live.
    pub fn remove_anchor(&mut self, id: AnchorId) -> bool {
        let removed = self.anchors.remove(id);
        if removed {
            self.rebuild_index();
        }
        removed
    }

    /// Resolve an anchor to its current position, or `None` if it is not live.
    #[must_use]
    pub fn resolve_anchor(&self, id: AnchorId) -> Option<Resolved> {
        self.anchors.try_resolve(id)
    }

    /// Number of live anchors.
    #[must_use]
    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    /// Apply a transaction. Fails (leaving the document untouched) if `base_revision` is stale or an
    /// edit is out of range; on success the revision strictly increases and the change is recorded as a
    /// new undo node (INV-TXN, INV-UNDO, F-001).
    pub fn apply(&mut self, txn: Transaction) -> Result<Revision, TxnError> {
        if txn.base_revision != self.revision {
            return Err(TxnError::StaleBaseRevision {
                base: txn.base_revision,
                current: self.revision,
            });
        }
        match txn.edits.check_bounds(self.text.len()) {
            Ok(()) => {}
            Err(EditError::OutOfRange { pos, del, len }) => {
                return Err(TxnError::OutOfRange { pos, del, len });
            }
            Err(EditError::Overlap { .. }) => {
                unreachable!("EditList is validated disjoint at construction");
            }
        }
        // Committed path — nothing above mutated state, so failure above is atomic-no-op.
        let inverse = txn.edits.inverse(&self.text);
        let new_text = txn.edits.apply_to(&self.text);
        self.text = Arc::from(new_text);
        let prev = self.revision;
        self.revision = self.revision.next();
        // Internal invariant (INV-TXN §1): a successful apply strictly advances the revision. A debug_assert,
        // not a Result — a violation here is a bug in Revision, not an expected input failure (stability §1).
        debug_assert!(
            self.revision > prev,
            "apply must strictly advance revision ({prev:?} -> {:?})",
            self.revision
        );
        self.anchors.apply_edits(&txn.edits);
        let (origin, hint) = (txn.origin, txn.group_hint);
        self.undo
            .record(txn.edits, inverse, origin, hint, self.revision);
        self.rebuild_index();
        Ok(self.revision)
    }

    /// Undo the last change **group** on the current lineage, returning the new revision, or `None` at the
    /// root. Re-applies each node's inverse (a whole insert session / operator undoes in one step) and moves
    /// `current` to the node below the group — the revision advances (INV-TXN §2) but no new node is created.
    pub fn undo(&mut self) -> Option<Revision> {
        let inverses = self.undo.undo();
        if inverses.is_empty() {
            return None;
        }
        for edits in &inverses {
            self.reapply(edits);
        }
        Some(self.revision)
    }

    /// Redo the next change **group** along the newest branch, or `None` if there is nothing to redo.
    pub fn redo(&mut self) -> Option<Revision> {
        let forwards = self.undo.redo();
        if forwards.is_empty() {
            return None;
        }
        for edits in &forwards {
            self.reapply(edits);
        }
        Some(self.revision)
    }

    /// Move one logical change along CHRONOLOGICAL creation order — `g-` (`older = true`) to the
    /// previously-created state, `g+` (`older = false`) to the next — crossing branches so a state
    /// abandoned by a new edit is still reachable (Vim `g-`/`g+`). Returns the new revision, or
    /// `None` at either end. Re-applies the transforming edits; no new node is created.
    pub fn undo_chronological(&mut self, older: bool) -> Option<Revision> {
        let edits = self.undo.to_chronological(older);
        if edits.is_empty() {
            return None;
        }
        for e in &edits {
            self.reapply(e);
        }
        Some(self.revision)
    }

    /// Whether undo/redo can move.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    /// Whether redo can move.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Whether the buffer differs from what is on disk — compared by undo-node **identity**, not
    /// revision magnitude (persistence §1): undoing back to the saved node makes this false again even
    /// though the revision counter has climbed.
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.saved_node != Some(self.undo.current_seq())
    }

    /// Mark the current state as the one on disk (called after a save, or at open for a loaded file).
    pub fn mark_saved(&mut self) {
        self.saved_node = Some(self.undo.current_seq());
    }

    /// Access to the undo history (for chronological inspection / navigation state).
    #[must_use]
    pub fn history(&self) -> &UndoHistory {
        &self.undo
    }

    /// Take an immutable, revision-stamped snapshot. O(1): clones the text `Arc` and the frozen anchor
    /// index `Arc` (INV-QUERY-SNAPSHOT, query-and-snapshot §2).
    #[must_use]
    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot::new(
            self.revision,
            Arc::clone(&self.text),
            Arc::clone(&self.anchor_index),
        )
    }

    /// Re-apply an edit list as a navigation step (undo/redo): advances the revision and moves anchors,
    /// but records no new undo node.
    fn reapply(&mut self, edits: &EditList) {
        let new_text = edits.apply_to(&self.text);
        self.text = Arc::from(new_text);
        self.revision = self.revision.next();
        self.anchors.apply_edits(edits);
        self.rebuild_index();
    }

    /// Refresh the frozen anchor index after any change (rebuild-on-commit, query-and-snapshot §2).
    fn rebuild_index(&mut self) {
        self.anchor_index = Arc::new(AnchorIndex::from_entries(self.anchors.resolved()));
    }
}
