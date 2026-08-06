//! The branching undo history with a chronological index (persistence-and-recovery §7, INV-UNDO).
//!
//! Undoing then making a new change **branches** — no state is ever lost ([COM-8], Vim `undo.txt`).
//! Two structures sit over the same nodes: parent/child edges express document *lineage* (what `u`/`C-r`
//! walk), and a separate append-only `chronological` order by creation `seq` is the key `g-`/`g+` and
//! `:earlier/:later` traverse. Undo/redo *navigate* existing nodes (moving `current`); only a fresh edit
//! appends a node. The revision counter still advances on undo (INV-TXN §2) — which is exactly why
//! dirty-tracking compares node identity (`seq`), not revision magnitude (persistence §1).

use crate::edit::EditList;
use crate::pos::Revision;
use crate::transaction::TransactionOrigin;

/// Monotonic creation-order key over undo nodes — the chronological index key, and the identity behind
/// `saved_node` dirty-tracking (persistence §1). Never renumbered, so history is immutable under new
/// branches.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MonotonicSeq(pub u64);

/// A node in the undo tree. The root (seq 0) is the empty/opened document; every other node is the
/// result of one transaction applied to its parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UndoNodeId(usize);

struct UndoNode {
    parent: Option<UndoNodeId>,
    children: Vec<UndoNodeId>,
    seq: MonotonicSeq,
    /// The transaction (forward + its exact inverse) that produced this node from `parent`. `None` on
    /// the root. `forward` is applied on redo *into* this node; `inverse` is applied on undo *out* of it.
    forward: Option<EditList>,
    inverse: Option<EditList>,
    origin: Option<TransactionOrigin>,
    result_revision: Revision,
}

/// The in-memory undo history for one document.
pub struct UndoHistory {
    nodes: Vec<UndoNode>,
    current: UndoNodeId,
    chronological: Vec<UndoNodeId>,
    next_seq: u64,
}

impl Default for UndoHistory {
    fn default() -> UndoHistory {
        UndoHistory::new(Revision::ZERO)
    }
}

impl UndoHistory {
    /// A history rooted at `base_revision` (the document's revision before any edit).
    #[must_use]
    pub fn new(base_revision: Revision) -> UndoHistory {
        let root = UndoNode {
            parent: None,
            children: Vec::new(),
            seq: MonotonicSeq(0),
            forward: None,
            inverse: None,
            origin: None,
            result_revision: base_revision,
        };
        UndoHistory {
            nodes: vec![root],
            current: UndoNodeId(0),
            chronological: vec![UndoNodeId(0)],
            next_seq: 1,
        }
    }

    /// Record a freshly-applied transaction as a new child of `current`, and move `current` onto it.
    /// If `current` already had children (we had undone), this simply adds another branch — the earlier
    /// children (and everything under them) are retained (INV-UNDO: no orphaned/lost history).
    pub fn record(
        &mut self,
        forward: EditList,
        inverse: EditList,
        origin: TransactionOrigin,
        result_revision: Revision,
    ) -> UndoNodeId {
        let id = UndoNodeId(self.nodes.len());
        let seq = MonotonicSeq(self.next_seq);
        self.next_seq += 1;
        self.nodes.push(UndoNode {
            parent: Some(self.current),
            children: Vec::new(),
            seq,
            forward: Some(forward),
            inverse: Some(inverse),
            origin: Some(origin),
            result_revision,
        });
        self.nodes[self.current.0].children.push(id);
        self.current = id;
        self.chronological.push(id);
        id
    }

    /// Whether `u` has somewhere to go (the current node is not the root).
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.nodes[self.current.0].parent.is_some()
    }

    /// Whether `C-r` has somewhere to go (the current node has at least one redo branch).
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.nodes[self.current.0].children.is_empty()
    }

    /// Move `current` to its parent, returning the inverse edits to apply (undo). `None` at the root.
    pub fn undo(&mut self) -> Option<EditList> {
        let node = &self.nodes[self.current.0];
        let parent = node.parent?;
        let inverse = node
            .inverse
            .clone()
            .expect("non-root node carries an inverse");
        self.current = parent;
        Some(inverse)
    }

    /// Move `current` to its most-recently-created child, returning that child's forward edits to apply
    /// (redo). `None` if there is nothing to redo. "Newest child is the default redo" (persistence §7).
    pub fn redo(&mut self) -> Option<EditList> {
        let child = *self.nodes[self.current.0].children.last()?;
        let forward = self.nodes[child.0]
            .forward
            .clone()
            .expect("non-root node carries a forward");
        self.current = child;
        Some(forward)
    }

    /// The `seq` of the current node — the identity dirty-tracking compares against `saved_node`.
    #[must_use]
    pub fn current_seq(&self) -> MonotonicSeq {
        self.nodes[self.current.0].seq
    }

    /// The document revision the current node was produced at — the value a journal Edit record
    /// persists as `result_revision` (persistence §2). For the root this is the open revision.
    #[must_use]
    pub fn current_result_revision(&self) -> Revision {
        self.nodes[self.current.0].result_revision
    }

    /// Total number of nodes ever created (including the root and any branched-away states).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Nodes in creation (`seq`) order as `(seq, origin)` — the chronological index the `g-`/`g+`
    /// traversal is built on. Present here so a test can assert branched-away states are retained.
    #[must_use]
    pub fn creation_order(&self) -> Vec<(MonotonicSeq, Option<TransactionOrigin>)> {
        self.chronological
            .iter()
            .map(|&id| {
                let n = &self.nodes[id.0];
                (n.seq, n.origin)
            })
            .collect()
    }
}
