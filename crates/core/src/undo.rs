//! The branching undo history with logical grouping and a chronological index (persistence §6/§7, INV-UNDO).
//!
//! Undoing then making a new change **branches** — no state is ever lost ([COM-8], Vim `undo.txt`).
//! Two structures sit over the same nodes: parent/child edges express document *lineage* (what `u`/`C-r`
//! walk), and a separate append-only `chronological` order by creation `seq` is the key `g-`/`g+` traverse.
//!
//! Undo is recorded by **logical unit, not per keystroke** (INV-UNDO, persistence §6): consecutive changes
//! of the same origin in one session coalesce into a **group**, and `undo`/`redo` move a whole group at a
//! time. Grouping keys off [`TransactionOrigin`] plus explicit break/join ([`GroupHint`]); a different
//! origin (e.g. a formatter edit landing while you type) never silently merges into your typing.
//!
//! Undo/redo *navigate* existing nodes (moving `current`); only a fresh edit appends a node. The revision
//! counter still advances on undo (INV-TXN §2), which is why dirty-tracking compares node identity (`seq`),
//! not revision magnitude (persistence §1).

use crate::edit::EditList;
use crate::pos::Revision;
use crate::transaction::{GroupHint, TransactionOrigin};

/// Monotonic creation-order key over undo nodes — the chronological index key, and the identity behind
/// `saved_node` dirty-tracking (persistence §1). Never renumbered, so history is immutable under new branches.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MonotonicSeq(pub u64);

/// A node in the undo tree. The root (seq 0) is the empty/opened document; every other node is the result
/// of one transaction applied to its parent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UndoNodeId(usize);

struct UndoNode {
    parent: Option<UndoNodeId>,
    children: Vec<UndoNodeId>,
    seq: MonotonicSeq,
    /// The undo group this node belongs to; nodes sharing a group id undo/redo together (persistence §6).
    group: u64,
    /// The transaction (forward + its exact inverse) that produced this node from `parent`. `None` on the
    /// root. `forward` is applied on redo *into* this node; `inverse` is applied on undo *out* of it.
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
    next_group: u64,
    /// True right after an undo/redo, so the next fresh edit starts a new group (a change after an undo is
    /// a new logical unit, matching Vim), never coalescing into the group we just navigated away from.
    last_was_nav: bool,
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
            group: 0,
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
            next_group: 1,
            last_was_nav: false,
        }
    }

    /// Decide the group id for a new change appended to `current`, per persistence §6.
    fn group_for(&mut self, origin: TransactionOrigin, hint: GroupHint) -> u64 {
        let cur = &self.nodes[self.current.0];
        let mergeable = cur.parent.is_some() && !self.last_was_nav;
        match hint {
            GroupHint::BreakBefore => self.fresh_group(),
            // JoinPrev merges into the current group even across the normal boundary (`:undojoin`); if the
            // prior change was undone (nav) or we are at the root there is nothing to join → a new group.
            GroupHint::JoinPrev if mergeable => cur.group,
            // Continue coalesces only within a same-origin session.
            GroupHint::Continue if mergeable && cur.origin == Some(origin) => cur.group,
            _ => self.fresh_group(),
        }
    }

    fn fresh_group(&mut self) -> u64 {
        let g = self.next_group;
        self.next_group += 1;
        g
    }

    /// Record a freshly-applied transaction as a new child of `current`, and move `current` onto it. If
    /// `current` already had children (we had undone), this adds another branch — earlier children are
    /// retained (INV-UNDO: no lost history).
    pub fn record(
        &mut self,
        forward: EditList,
        inverse: EditList,
        origin: TransactionOrigin,
        hint: GroupHint,
        result_revision: Revision,
    ) -> UndoNodeId {
        let group = self.group_for(origin, hint);
        let id = UndoNodeId(self.nodes.len());
        let seq = MonotonicSeq(self.next_seq);
        self.next_seq += 1;
        self.nodes.push(UndoNode {
            parent: Some(self.current),
            children: Vec::new(),
            seq,
            group,
            forward: Some(forward),
            inverse: Some(inverse),
            origin: Some(origin),
            result_revision,
        });
        self.nodes[self.current.0].children.push(id);
        self.current = id;
        self.chronological.push(id);
        self.last_was_nav = false;
        id
    }

    /// Whether `u` has somewhere to go.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.nodes[self.current.0].parent.is_some()
    }

    /// Whether `C-r` has somewhere to go.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.nodes[self.current.0].children.is_empty()
    }

    /// Undo the whole current group: the inverse edits of every node from `current` back through the
    /// contiguous same-group chain, in application order (current first). Moves `current` to the node
    /// below the group. Empty if already at the root.
    pub fn undo(&mut self) -> Vec<EditList> {
        let mut out = Vec::new();
        if self.nodes[self.current.0].parent.is_none() {
            return out;
        }
        let group = self.nodes[self.current.0].group;
        loop {
            let node = &self.nodes[self.current.0];
            let Some(parent) = node.parent else { break };
            if node.group != group {
                break;
            }
            out.push(
                node.inverse
                    .clone()
                    .expect("non-root node carries an inverse"),
            );
            self.current = parent;
        }
        self.last_was_nav = true;
        out
    }

    /// Redo the whole next group along the newest branch: the forward edits of the group's node chain, in
    /// application order. Moves `current` to the group's tip. Empty if there is nothing to redo.
    pub fn redo(&mut self) -> Vec<EditList> {
        let mut out = Vec::new();
        let Some(&first) = self.nodes[self.current.0].children.last() else {
            return out;
        };
        let group = self.nodes[first.0].group;
        let mut node = first;
        loop {
            out.push(
                self.nodes[node.0]
                    .forward
                    .clone()
                    .expect("non-root node carries a forward"),
            );
            self.current = node;
            match self.nodes[node.0].children.last() {
                Some(&child) if self.nodes[child.0].group == group => node = child,
                _ => break,
            }
        }
        self.last_was_nav = true;
        out
    }

    /// The `seq` of the current node — the identity dirty-tracking compares against `saved_node`.
    #[must_use]
    pub fn current_seq(&self) -> MonotonicSeq {
        self.nodes[self.current.0].seq
    }

    /// The document revision the current node was produced at — the value a journal Edit record persists
    /// as `result_revision` (persistence §2). For the root this is the open revision.
    #[must_use]
    pub fn current_result_revision(&self) -> Revision {
        self.nodes[self.current.0].result_revision
    }

    /// The undo group id of the current node — nodes sharing it undo/redo as one step.
    #[must_use]
    pub fn current_group(&self) -> u64 {
        self.nodes[self.current.0].group
    }

    /// Total number of nodes ever created (including the root and any branched-away states).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Nodes in creation (`seq`) order as `(seq, origin, group)` — the chronological index the `g-`/`g+`
    /// traversal is built on. Present here so a test can assert branched-away states are retained.
    #[must_use]
    pub fn creation_order(&self) -> Vec<(MonotonicSeq, Option<TransactionOrigin>, u64)> {
        self.chronological
            .iter()
            .map(|&id| {
                let n = &self.nodes[id.0];
                (n.seq, n.origin, n.group)
            })
            .collect()
    }
}
