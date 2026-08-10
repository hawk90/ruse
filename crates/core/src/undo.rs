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

use std::collections::HashSet;

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

    /// Move `current` to any node, returning the edits that transform the buffer from the current
    /// state into `target`'s: the inverses from `current` up to the lowest common ancestor, then the
    /// forwards from the LCA down to `target`. This is what lets `g-`/`g+` cross into a branch the
    /// parent/child walk of `undo`/`redo` cannot reach. Empty when already at `target`.
    fn navigate_to(&mut self, target: UndoNodeId) -> Vec<EditList> {
        if target == self.current {
            return Vec::new();
        }
        let current_ancestry: HashSet<UndoNodeId> =
            self.ancestry(self.current).into_iter().collect();
        // Walk target->root until we meet an ancestor of current: that node is the LCA. Collect the
        // strictly-below-LCA path (target first) to replay forward afterwards.
        let mut down = Vec::new();
        let mut node = target;
        let lca = loop {
            if current_ancestry.contains(&node) {
                break node;
            }
            down.push(node);
            match self.nodes[node.0].parent {
                Some(p) => node = p,
                None => break node, // reached the root without meeting: the root is the LCA
            }
        };
        let mut out = Vec::new();
        // Inverses from current up to (not including) the LCA.
        let mut c = self.current;
        while c != lca {
            out.push(
                self.nodes[c.0]
                    .inverse
                    .clone()
                    .expect("non-root node carries an inverse"),
            );
            c = self.nodes[c.0]
                .parent
                .expect("a node above the LCA has a parent");
        }
        // Forwards from just below the LCA down to the target (down is target-first, so reverse).
        for &n in down.iter().rev() {
            out.push(
                self.nodes[n.0]
                    .forward
                    .clone()
                    .expect("non-root node carries a forward"),
            );
        }
        self.current = target;
        self.last_was_nav = true;
        out
    }

    /// `current` and every ancestor up to the root, nearest first.
    fn ancestry(&self, mut n: UndoNodeId) -> Vec<UndoNodeId> {
        let mut v = vec![n];
        while let Some(p) = self.nodes[n.0].parent {
            v.push(p);
            n = p;
        }
        v
    }

    /// The tip node of each undo GROUP in creation order — the state after each logical change. A
    /// group's nodes are created consecutively (coalescing only merges adjacent edits) and groups
    /// never recur, so each maximal same-group run in `chronological` contributes one tip.
    fn group_tips(&self) -> Vec<UndoNodeId> {
        let mut tips = Vec::new();
        for (i, &id) in self.chronological.iter().enumerate() {
            let last_of_run = match self.chronological.get(i + 1) {
                Some(&next) => self.nodes[next.0].group != self.nodes[id.0].group,
                None => true,
            };
            if last_of_run {
                tips.push(id);
            }
        }
        tips
    }

    /// `g-` (`older = true`) / `g+` (`older = false`): step one logical change along CHRONOLOGICAL
    /// creation order — across branches, unlike `undo`/`redo` which follow the tree — and return the
    /// edits to reach that state. One step crosses a whole group (e.g. an insert session), matching
    /// Vim's undo-block granularity. Empty at either end of history.
    pub fn to_chronological(&mut self, older: bool) -> Vec<EditList> {
        let tips = self.group_tips();
        let Some(pos) = tips.iter().position(|&id| id == self.current) else {
            return Vec::new();
        };
        let target = if older {
            pos.checked_sub(1)
        } else {
            (pos + 1 < tips.len()).then_some(pos + 1)
        };
        match target {
            Some(tp) => self.navigate_to(tips[tp]),
            None => Vec::new(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_edits() -> EditList {
        EditList::new(Vec::new()).expect("empty edit list is valid")
    }

    /// Record a change with an explicit origin/hint; edit CONTENT is irrelevant to grouping.
    fn record(h: &mut UndoHistory, origin: TransactionOrigin, hint: GroupHint) -> u64 {
        h.record(empty_edits(), empty_edits(), origin, hint, Revision::ZERO);
        h.current_group()
    }

    #[test]
    fn same_origin_continue_coalesces_but_a_different_origin_does_not() {
        // F-008/F-005 #2: a formatter/LSP edit must NOT coalesce into the user's edit group.
        let mut h = UndoHistory::new(Revision::ZERO);
        let g1 = record(&mut h, TransactionOrigin::UserInput, GroupHint::Continue);
        let g2 = record(&mut h, TransactionOrigin::UserInput, GroupHint::Continue);
        assert_eq!(g1, g2, "consecutive same-origin edits share one undo group");
        let g3 = record(&mut h, TransactionOrigin::Lsp, GroupHint::Continue);
        assert_ne!(
            g2, g3,
            "an LSP edit starts its own undo unit, even with a Continue hint"
        );
    }

    #[test]
    fn g_minus_then_g_plus_round_trips_across_a_branch() {
        // root -> A (userinput), undo to root, root -> B (branch). Chronological: root, A, B.
        let mut h = UndoHistory::new(Revision::ZERO);
        h.record(
            empty_edits(),
            empty_edits(),
            TransactionOrigin::UserInput,
            GroupHint::BreakBefore,
            Revision::ZERO,
        );
        let a_seq = h.current_seq();
        h.undo();
        h.record(
            empty_edits(),
            empty_edits(),
            TransactionOrigin::UserInput,
            GroupHint::BreakBefore,
            Revision::ZERO,
        );
        let b_seq = h.current_seq();
        // g- lands on A (the branched-away state), g+ returns to B.
        h.to_chronological(true);
        assert_eq!(
            h.current_seq(),
            a_seq,
            "g- reaches the abandoned branch by creation order"
        );
        h.to_chronological(false);
        assert_eq!(h.current_seq(), b_seq, "g+ returns to the newest state");
        // g+ at the tip is a no-op (empty), not a panic.
        assert!(h.to_chronological(false).is_empty());
    }
}
