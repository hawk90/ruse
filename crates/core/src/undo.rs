//! The branching undo history with logical grouping and a chronological index (persistence §6/§7, INV-UNDO).
//!
//! Undoing then making a new change **branches** — branching loses no state ([COM-8], Vim `undo.txt`).
//! Two structures sit over the same nodes: parent/child edges express document *lineage* (what `u`/`C-r`
//! walk), and creation order by `seq` (the order of the node slots) is the key `g-`/`g+` traverse.
//!
//! Undo is recorded by **logical unit, not per keystroke** (INV-UNDO, persistence §6): consecutive changes
//! of the same origin in one session coalesce into a **group**, and `undo`/`redo` move a whole group at a
//! time. Grouping keys off [`TransactionOrigin`] plus explicit break/join ([`GroupHint`]); a different
//! origin (e.g. a formatter edit landing while you type) never silently merges into your typing.
//!
//! Undo/redo *navigate* existing nodes (moving `current`); only a fresh edit appends a node. The revision
//! counter still advances on undo (INV-TXN §2), which is why dirty-tracking compares node identity (`seq`),
//! not revision magnitude (persistence §1).
//!
//! The history is **bounded** ([`UndoLimits`], RFC-0026 §1): past `'undolevels'` + 1 groups or the byte
//! budget, [`UndoHistory::trim`] drops the oldest level as nvim does: the oldest alternate branch first
//! (`u_freebranch`), else the oldest group, by re-rooting the tree at the oldest kept state
//! (`u_freeheader`). The current group is never dropped and `seq` is never renumbered, so no state
//! within the bound is lost.

use std::collections::{HashMap, HashSet};
use std::ops::{Index, IndexMut};

use crate::edit::EditList;
use crate::pos::{BytePos, Revision};
use crate::transaction::{GroupHint, TransactionOrigin};
use crate::undo_cursor::UndoEntry;

/// How much undo history a document keeps (RFC-0026 §1). The frontend sets it from its options; the core
/// reads no config. It depends on option values and edit payloads only, never on a clock or on free
/// memory, so a replayed trace rebuilds the same tree (F-022).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UndoLimits {
    /// `'undolevels'`: `Some(n)` keeps at most `n + 1` undo groups, as nvim does (`u_savecommon` frees the
    /// oldest header while there are more than `'undolevels'`, then adds the new one). `Some(0)` is Vi's
    /// undo: `u` undoes the last change or the undo itself, and `CTRL-R` has nothing to redo. `None` is a
    /// negative `'undolevels'`: a change is not undoable and frees the history before it.
    pub levels: Option<u32>,
    /// The edit payload the tree may hold, in bytes ([`EditList::payload_bytes`] of every kept node's
    /// forward and inverse lists); `None` = no byte bound.
    pub bytes: Option<usize>,
}

impl UndoLimits {
    /// nvim's default `'undolevels'`.
    pub const DEFAULT_LEVELS: u32 = 1000;
    /// ruse's default undo payload budget: 256 MiB.
    pub const DEFAULT_BYTES: usize = 256 << 20;

    /// No bound: every state is kept.
    pub const UNBOUNDED: UndoLimits = UndoLimits {
        levels: Some(u32::MAX),
        bytes: None,
    };

    /// The limits for an `'undolevels'` value and a byte budget as the options give them: a negative
    /// `undolevels` is no undo, a `max_bytes` of 0 or less no byte bound.
    #[must_use]
    pub fn from_options(undolevels: i64, max_bytes: i64) -> UndoLimits {
        UndoLimits {
            levels: (undolevels >= 0).then(|| u32::try_from(undolevels).unwrap_or(u32::MAX)),
            bytes: (max_bytes > 0).then(|| usize::try_from(max_bytes).unwrap_or(usize::MAX)),
        }
    }

    /// The most undo groups the tree keeps (the current one always stays).
    fn max_groups(self) -> usize {
        match self.levels {
            Some(n) => usize::try_from(n).map_or(usize::MAX, |n| n.saturating_add(1)),
            None => 0,
        }
    }
}

impl Default for UndoLimits {
    /// 1000 levels (nvim's `'undolevels'`) and 256 MiB.
    fn default() -> UndoLimits {
        UndoLimits {
            levels: Some(UndoLimits::DEFAULT_LEVELS),
            bytes: Some(UndoLimits::DEFAULT_BYTES),
        }
    }
}

/// Monotonic creation-order key over undo nodes — the chronological index key, and the identity behind
/// `saved_node` dirty-tracking (persistence §1). Never renumbered, so history is immutable under new
/// branches and under dropping the oldest states.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MonotonicSeq(pub u64);

/// A node in the undo tree: a slot index, private to this module and the document. The root is the
/// oldest kept state (the opened document until states are dropped); every other node is the result of
/// one transaction applied to its parent.
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
    /// The child `C-r` redoes into: the one most recently left or entered (Vim's `uh_alt_next` order puts
    /// the branch just visited first), else the newest. Set whenever `current` crosses the parent/child edge.
    redo_child: Option<UndoNodeId>,
    /// Vim's `uh_cursor`: where the cursor stood when this change began, as a byte offset into the
    /// PARENT's text. Read off the first node of a group when `u`/`C-r` place the cursor.
    cursor: usize,
    /// How this node's edits map onto Vim undo entries (#1612); only `u`/`C-r` placement reads it.
    undo_entry: UndoEntry,
}

impl UndoNode {
    /// What the node holds against the byte budget: its forward and inverse payloads (RFC-0026 §1).
    fn cost(&self) -> usize {
        self.forward.as_ref().map_or(0, EditList::payload_bytes)
            + self.inverse.as_ref().map_or(0, EditList::payload_bytes)
    }
}

/// The node slots in creation order. A dropped node leaves an empty slot until [`UndoHistory::compact`]
/// closes the gaps, so an [`UndoNodeId`] stays valid while its node lives.
struct Slots(Vec<Option<UndoNode>>);

impl Slots {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn get(&self, id: UndoNodeId) -> Option<&UndoNode> {
        self.0.get(id.0).and_then(Option::as_ref)
    }

    /// The live nodes with their ids, in creation order.
    fn live(&self) -> impl Iterator<Item = (UndoNodeId, &UndoNode)> {
        self.0
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.as_ref().map(|n| (UndoNodeId(i), n)))
    }
}

impl Index<UndoNodeId> for Slots {
    type Output = UndoNode;
    fn index(&self, id: UndoNodeId) -> &UndoNode {
        self.0[id.0].as_ref().expect("a live undo node")
    }
}

impl IndexMut<UndoNodeId> for Slots {
    fn index_mut(&mut self, id: UndoNodeId) -> &mut UndoNode {
        self.0[id.0].as_mut().expect("a live undo node")
    }
}

/// What one `u`/`C-r`/`g-`/`g+` step re-applies, and what the cursor placement needs to know about it.
pub struct UndoStep {
    /// The edit lists to apply, in order.
    pub edits: Vec<EditList>,
    /// How each of `edits` maps onto Vim undo entries (see `UndoNode::undo_entry`).
    pub(crate) undo_entry: Vec<UndoEntry>,
    /// The undo group each of `edits` belongs to — a step can cross several (`g-`/`g+`), and the marks a
    /// group saved are restored after its last list (Vim's `uh_namedm`, #686).
    pub groups: Vec<u64>,
    /// The node current after each of `edits` is applied, and the origin of the node whose edits it is —
    /// what the change log names a navigation step by (RFC-0023 §1).
    pub nodes: Vec<(MonotonicSeq, Option<TransactionOrigin>)>,
    /// `edits[tail..]` belong to the LAST group the step crossed — the one Vim's `u_undoredo` places the
    /// cursor from (each group it crosses overrides the previous placement).
    pub tail: usize,
    /// Whether that last group was re-applied forwards (redo) rather than undone.
    pub forward: bool,
    /// The last group's `uh_cursor`: a byte offset into the text from BEFORE the group's change, which
    /// is the text after an undo and the text before a redo.
    pub cursor: BytePos,
}

/// The in-memory undo history for one document.
pub struct UndoHistory {
    nodes: Slots,
    /// The oldest kept state: slot 0 until states are dropped, then the node the tree was re-rooted at.
    root: UndoNodeId,
    current: UndoNodeId,
    /// Empty slots in `nodes` (dropped nodes not yet compacted away).
    free: usize,
    next_seq: u64,
    next_group: u64,
    /// True right after an undo/redo, so the next fresh edit starts a new group (a change after an undo is
    /// a new logical unit, matching Vim), never coalescing into the group we just navigated away from.
    last_was_nav: bool,
    /// The last move was a `u` (not a redo, a `g-`/`g+` or a change): with `'undolevels'` 0 the next `u`
    /// takes it back.
    undid_last: bool,
    /// An undo sync since the last change (Vim's `u_sync`, [`UndoHistory::sync`]): the next change starts a
    /// new group even with a [`GroupHint::Continue`] hint. An explicit [`GroupHint::JoinPrev`] still joins.
    synced: bool,
    /// What the newest [`UndoHistory::record`] replaced, for [`UndoHistory::retract`]: the node it made,
    /// its parent's redo child and the history's `last_was_nav`/`synced` before it.
    last_record: Option<(UndoNodeId, Option<UndoNodeId>, bool, bool)>,
    /// The bound [`UndoHistory::trim`] keeps the tree within.
    limits: UndoLimits,
    /// Per undo group, how many kept non-root nodes it has: its length is the number of undo levels held.
    groups: HashMap<u64, usize>,
    /// The kept nodes' payload ([`UndoNode::cost`]), kept current so the bound costs O(1) per change.
    bytes: usize,
}

impl Default for UndoHistory {
    fn default() -> UndoHistory {
        UndoHistory::new(Revision::ZERO)
    }
}

impl UndoHistory {
    /// A history rooted at `base_revision` (the document's revision before any edit), bounded by the
    /// default [`UndoLimits`].
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
            redo_child: None,
            cursor: 0,
            undo_entry: UndoEntry::PerEdit,
        };
        UndoHistory {
            nodes: Slots(vec![Some(root)]),
            root: UndoNodeId(0),
            current: UndoNodeId(0),
            free: 0,
            next_seq: 1,
            next_group: 1,
            last_was_nav: false,
            undid_last: false,
            synced: false,
            last_record: None,
            limits: UndoLimits::default(),
            groups: HashMap::new(),
            bytes: 0,
        }
    }

    /// The bound the history keeps to.
    #[must_use]
    pub fn limits(&self) -> UndoLimits {
        self.limits
    }

    /// Set the bound. Nothing is dropped until the next change ([`UndoHistory::trim`]), as nvim frees
    /// undo headers only when it saves a new one.
    pub fn set_limits(&mut self, limits: UndoLimits) {
        self.limits = limits;
    }

    /// The edit payload the kept states hold, in bytes (what the byte budget bounds).
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.bytes
    }

    /// How many undo groups (levels) the tree holds.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Close the current group (Vim's `u_sync`): the next change starts a new undo step unless it asks to
    /// join ([`GroupHint::JoinPrev`]). The frontend syncs before each typed command outside text entry, so
    /// `xx` is two steps while an Insert session stays one; a save syncs too (#766).
    pub fn sync(&mut self) {
        self.synced = true;
    }

    /// Whether the history is synced ([`UndoHistory::sync`]) since its last change, so another sync would
    /// change nothing. The frontend records a sync in the trace only when it is not (RFC-0020).
    #[must_use]
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Decide the group id for a new change appended to `current`, per persistence §6.
    fn group_for(&mut self, origin: TransactionOrigin, hint: GroupHint) -> u64 {
        let cur = &self.nodes[self.current];
        let mergeable = cur.parent.is_some() && !self.last_was_nav;
        match hint {
            GroupHint::BreakBefore => self.fresh_group(),
            // JoinPrev merges into the current group even across the normal boundary (`:undojoin`); if the
            // prior change was undone (nav) or we are at the root there is nothing to join → a new group.
            GroupHint::JoinPrev if mergeable => cur.group,
            // Continue coalesces only within a same-origin session.
            GroupHint::Continue if mergeable && !self.synced && cur.origin == Some(origin) => {
                cur.group
            }
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
    /// retained (branching loses nothing). `cursor` is where the change began, in the text before it.
    /// Nothing is dropped here: the caller runs [`UndoHistory::trim`] once the node is complete.
    pub fn record(
        &mut self,
        forward: EditList,
        inverse: EditList,
        origin: TransactionOrigin,
        hint: GroupHint,
        result_revision: Revision,
        cursor: BytePos,
    ) -> UndoNodeId {
        let cursor = cursor.0;
        // Stability §1: an undo node's parent must exist.
        debug_assert!(
            self.nodes.get(self.current).is_some(),
            "the new node's parent exists"
        );
        let group = self.group_for(origin, hint);
        let id = UndoNodeId(self.nodes.len());
        let seq = MonotonicSeq(self.next_seq);
        self.next_seq += 1;
        let node = UndoNode {
            parent: Some(self.current),
            children: Vec::new(),
            seq,
            group,
            forward: Some(forward),
            inverse: Some(inverse),
            origin: Some(origin),
            result_revision,
            redo_child: None,
            cursor,
            undo_entry: UndoEntry::PerEdit,
        };
        self.bytes += node.cost();
        *self.groups.entry(group).or_insert(0) += 1;
        self.nodes.0.push(Some(node));
        let parent = &mut self.nodes[self.current];
        self.last_record = Some((id, parent.redo_child, self.last_was_nav, self.synced));
        parent.children.push(id);
        parent.redo_child = Some(id);
        self.current = id;
        self.last_was_nav = false;
        self.undid_last = false;
        self.synced = false;
        id
    }

    /// Map node `id`'s edits onto Vim undo entries as `shape` says (#1612), for where `u`/`C-r` put the
    /// cursor and the `'.` mark.
    pub(crate) fn set_undo_entry(&mut self, id: UndoNodeId, shape: UndoEntry) {
        self.nodes[id].undo_entry = shape;
    }

    /// Take back the newest recorded change as though it was never made: `current` returns to its parent,
    /// the node leaves the tree (its parent's redo child, `seq` and grouping state are restored), and its
    /// inverse is returned to apply, with the node's group when the node was that group's only one. Only the
    /// change just recorded, with nothing done since, is taken back; `None` otherwise (also when a negative
    /// `'undolevels'` made the change the root, so there is no parent to return to). Unlike
    /// [`UndoHistory::undo`] it leaves no redo branch and no undo step. States the bound dropped when the
    /// change was recorded stay dropped.
    pub(crate) fn retract(&mut self) -> Option<(EditList, Option<u64>)> {
        let (id, redo_child, last_was_nav, synced) = self.last_record.take()?;
        if self.current != id || id.0 + 1 != self.nodes.len() || id == self.root {
            return None;
        }
        let node = self.nodes.0.pop()??;
        let parent = node.parent?;
        self.bytes -= node.cost();
        self.leave_group(node.group);
        let root = self.root;
        let p = &mut self.nodes[parent];
        let own_group = (parent == root || p.group != node.group).then_some(node.group);
        p.children.retain(|c| *c != id);
        // The redo child it replaced, unless the bound dropped that branch since.
        p.redo_child = redo_child.filter(|r| p.children.contains(r));
        self.next_seq = node.seq.0;
        self.current = parent;
        self.last_was_nav = last_was_nav;
        self.undid_last = false;
        self.synced = synced;
        node.inverse.map(|inv| (inv, own_group))
    }

    /// One kept node of group `g` is gone; whether it was the group's last.
    fn leave_group(&mut self, g: u64) -> bool {
        let Some(n) = self.groups.get_mut(&g) else {
            return false;
        };
        *n -= 1;
        if *n == 0 {
            self.groups.remove(&g);
            return true;
        }
        false
    }

    /// Whether the tree is past its bound.
    fn over_limits(&self) -> bool {
        self.groups.len() > self.limits.max_groups()
            || self.limits.bytes.is_some_and(|b| self.bytes > b)
    }

    /// Keep the tree within its [`UndoLimits`] after a change (RFC-0026 §1): while it is past either limit,
    /// drop the oldest level ([`UndoHistory::drop_oldest`]): a branch that left the history before the
    /// oldest group, else that group by re-rooting the tree. The current group always stays, except under
    /// a negative `'undolevels'`, where the change just made becomes the root (it cannot be undone, and
    /// nothing before it can be reached).
    /// Returns the groups that left the tree, whose saved marks the document drops. Navigation never
    /// calls it: it creates no node, so it cannot cross a limit.
    pub(crate) fn trim(&mut self) -> Vec<u64> {
        let mut gone = Vec::new();
        if self.limits.levels.is_none() {
            if self.current != self.root {
                self.reroot(self.current, &mut gone);
            }
        } else {
            while self.over_limits() && self.drop_oldest(&mut gone) {}
        }
        if self.free > 0 && self.free >= self.nodes.len() / 2 {
            self.compact();
        }
        gone
    }

    /// Drop the oldest undo level, as nvim's `u_savecommon` loop does: while the oldest header on the
    /// current lineage (the root's child on the path to `current`) has alternate branches (the root's other
    /// children), the oldest of them goes, a whole branch at a time (`u_freebranch`); once it has none, its
    /// group goes by re-rooting the tree at the group's last node (`u_freeheader`), unless that group is
    /// the current one. Whether anything went. Measured in nvim v0.12.6 `--clean`: with `ul=1`, `x` `u`
    /// `$x` `x` keeps the two changes on the lineage and drops only the undone first `x`.
    fn drop_oldest(&mut self, gone: &mut Vec<u64>) -> bool {
        // The path from the root's child to `current`, root side first.
        let mut path = self.ancestry(self.current);
        path.pop(); // the root
        path.reverse();
        let on_path = path.first().copied();
        let root = self.root;
        let oldest_branch = self.nodes[root]
            .children
            .iter()
            .copied()
            .filter(|&c| Some(c) != on_path)
            .min_by_key(|&c| self.nodes[c].seq);
        if let Some(branch) = oldest_branch {
            let r = &mut self.nodes[root];
            r.children.retain(|&c| c != branch);
            if r.redo_child == Some(branch) {
                r.redo_child = None;
            }
            self.free_subtree(branch, None, gone);
            return true;
        }
        let Some(c) = on_path else {
            return false;
        };
        let g = self.nodes[c].group;
        if g == self.nodes[self.current].group {
            return false;
        }
        let last = path
            .iter()
            .take_while(|&&n| self.nodes[n].group == g)
            .last()
            .copied()
            .unwrap_or(c);
        self.reroot(last, gone);
        true
    }

    /// Free `n` and every node under it, except the subtree under `keep`.
    fn free_subtree(&mut self, n: UndoNodeId, keep: Option<UndoNodeId>, gone: &mut Vec<u64>) {
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            if Some(x) == keep {
                continue;
            }
            let node = self.nodes.0[x.0].take().expect("a live undo node");
            self.free += 1;
            if node.parent.is_some() {
                self.bytes -= node.cost();
                if self.leave_group(node.group) {
                    gone.push(node.group);
                }
            }
            stack.extend(node.children);
        }
    }

    /// Make `n` the root: free every node that is not under it (the old root, the states before `n` and
    /// the branches that left them), and drop `n`'s own edits, since nothing can be undone past it. `n`
    /// keeps its `seq` and its revision.
    fn reroot(&mut self, n: UndoNodeId, gone: &mut Vec<u64>) {
        let old = self.root;
        self.free_subtree(old, Some(n), gone);
        let node = &mut self.nodes[n];
        let cost = node.cost();
        node.parent = None;
        node.forward = None;
        node.inverse = None;
        node.origin = None;
        let g = node.group;
        self.bytes -= cost;
        if self.leave_group(g) {
            gone.push(g);
        }
        self.root = n;
    }

    /// Close the gaps dropped nodes left, keeping creation order (so the root becomes slot 0 again),
    /// and renumber every id that names a slot. Amortised: run once half the slots are empty.
    fn compact(&mut self) {
        let mut map = vec![usize::MAX; self.nodes.len()];
        let mut next = 0;
        for (i, slot) in self.nodes.0.iter().enumerate() {
            if slot.is_some() {
                map[i] = next;
                next += 1;
            }
        }
        let re = |id: UndoNodeId| UndoNodeId(map[id.0]);
        let slots = std::mem::take(&mut self.nodes.0);
        self.nodes.0 = slots
            .into_iter()
            .flatten()
            .map(|mut n| {
                n.parent = n.parent.map(re);
                n.children.iter_mut().for_each(|c| *c = re(*c));
                n.redo_child = n.redo_child.map(re);
                Some(n)
            })
            .collect();
        self.root = re(self.root);
        self.current = re(self.current);
        self.last_record = self.last_record.and_then(|(id, redo, nav, synced)| {
            (map[id.0] != usize::MAX).then(|| {
                let redo = redo.filter(|r| map[r.0] != usize::MAX).map(re);
                (re(id), redo, nav, synced)
            })
        });
        self.free = 0;
    }

    /// Whether `u` has somewhere to go: a parent, or with `'undolevels'` 0 an undo to take back.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.toggles_back() || self.nodes[self.current].parent.is_some()
    }

    /// Whether `u` takes back the undo just made instead of undoing further: `'undolevels'` 0, as Vi.
    /// nvim's `u_doit` does not step to an older header there, so `u` re-applies the one it just undid
    /// (measured, v0.12.6 `--clean`: `xxx`, `:set ul=0`, `uuu` leaves one `x` undone).
    fn toggles_back(&self) -> bool {
        self.limits.levels == Some(0) && self.undid_last && self.redo_target(self.current).is_some()
    }

    /// Whether `C-r` has somewhere to go. With `'undolevels'` 0 never: `u` is the redo (Vi).
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.limits.levels != Some(0) && !self.nodes[self.current].children.is_empty()
    }

    /// Undo the whole current group: the inverse edits of every node from `current` back through the
    /// contiguous same-group chain, in application order (current first). Moves `current` to the node
    /// below the group. `None` if already at the root. With `'undolevels'` 0 a `u` right after a `u` takes
    /// that undo back instead (Vi's undo, nvim measured: `x` `u` `u` toggles).
    pub fn undo(&mut self) -> Option<UndoStep> {
        if self.toggles_back() {
            return self.redo_group();
        }
        self.nodes[self.current].parent?;
        let mut out = Vec::new();
        let mut undo_entry = Vec::new();
        let mut groups = Vec::new();
        let mut nodes = Vec::new();
        let group = self.nodes[self.current].group;
        let mut cursor = 0;
        loop {
            let node = &self.nodes[self.current];
            let Some(parent) = node.parent else { break };
            if node.group != group {
                break;
            }
            out.push(
                node.inverse
                    .clone()
                    .expect("non-root node carries an inverse"),
            );
            undo_entry.push(node.undo_entry);
            groups.push(group);
            nodes.push((self.nodes[parent].seq, node.origin));
            cursor = node.cursor;
            let child = self.current;
            self.nodes[parent].redo_child = Some(child);
            self.current = parent;
        }
        self.last_was_nav = true;
        self.undid_last = true;
        Some(UndoStep {
            edits: out,
            undo_entry,
            groups,
            nodes,
            tail: 0,
            forward: false,
            cursor: BytePos(cursor),
        })
    }

    /// The child `C-r` moves into from `n`: the branch last visited, else the newest (#750).
    fn redo_target(&self, n: UndoNodeId) -> Option<UndoNodeId> {
        let node = &self.nodes[n];
        node.redo_child.or_else(|| node.children.last().copied())
    }

    /// Redo the whole next group along the branch last visited (Vim: after `g-` into an older branch and
    /// `u`, `C-r` goes back down that branch, #750), else the newest: the forward edits of the group's
    /// node chain, in application order. Moves `current` to the group's tip. `None` if nothing to redo,
    /// and always with `'undolevels'` 0 (nvim: `CTRL-R` is `Already at newest change` there).
    pub fn redo(&mut self) -> Option<UndoStep> {
        if self.limits.levels == Some(0) {
            return None;
        }
        self.redo_group()
    }

    fn redo_group(&mut self) -> Option<UndoStep> {
        let mut out = Vec::new();
        let mut undo_entry = Vec::new();
        let first = self.redo_target(self.current)?;
        let group = self.nodes[first].group;
        let mut groups = Vec::new();
        let mut nodes = Vec::new();
        let mut node = first;
        loop {
            out.push(
                self.nodes[node]
                    .forward
                    .clone()
                    .expect("non-root node carries a forward"),
            );
            undo_entry.push(self.nodes[node].undo_entry);
            groups.push(group);
            nodes.push((self.nodes[node].seq, self.nodes[node].origin));
            self.current = node;
            match self.redo_target(node) {
                Some(child) if self.nodes[child].group == group => node = child,
                _ => break,
            }
        }
        self.last_was_nav = true;
        self.undid_last = false;
        Some(UndoStep {
            edits: out,
            undo_entry,
            groups,
            nodes,
            tail: 0,
            forward: true,
            cursor: BytePos(self.nodes[first].cursor),
        })
    }

    /// Move `current` to any node, returning the edits that transform the buffer from the current
    /// state into `target`'s: the inverses from `current` up to the lowest common ancestor, then the
    /// forwards from the LCA down to `target`. This is what lets `g-`/`g+` cross into a branch the
    /// parent/child walk of `undo`/`redo` cannot reach. `None` when already at `target`.
    fn navigate_to(&mut self, target: UndoNodeId) -> Option<UndoStep> {
        if target == self.current {
            return None;
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
            match self.nodes[node].parent {
                Some(p) => node = p,
                None => break node, // reached the root without meeting: the root is the LCA
            }
        };
        let mut out = Vec::new();
        let mut undo_entry = Vec::new();
        let mut groups = Vec::new();
        let mut nodes = Vec::new();
        // The last group crossed: the tail of the forwards when the walk ends going down, else the tail of
        // the inverses. `tail` indexes its first list, `cursor` is its first node's `uh_cursor`.
        let mut tail = 0;
        let mut cursor = 0;
        // Inverses from current up to (not including) the LCA.
        let mut c = self.current;
        let mut group = None;
        while c != lca {
            let node = &self.nodes[c];
            if group != Some(node.group) {
                group = Some(node.group);
                tail = out.len();
            }
            cursor = node.cursor;
            undo_entry.push(node.undo_entry);
            groups.push(node.group);
            out.push(
                self.nodes[c]
                    .inverse
                    .clone()
                    .expect("non-root node carries an inverse"),
            );
            let parent = self.nodes[c]
                .parent
                .expect("a node above the LCA has a parent");
            nodes.push((self.nodes[parent].seq, self.nodes[c].origin));
            self.nodes[parent].redo_child = Some(c);
            c = parent;
        }
        // Forwards from just below the LCA down to the target (down is target-first, so reverse).
        let forward = !down.is_empty();
        let mut group = None;
        for &n in down.iter().rev() {
            if let Some(p) = self.nodes[n].parent {
                self.nodes[p].redo_child = Some(n);
            }
            let node = &self.nodes[n];
            if group != Some(node.group) {
                group = Some(node.group);
                tail = out.len();
                cursor = node.cursor;
            }
            undo_entry.push(node.undo_entry);
            groups.push(node.group);
            nodes.push((node.seq, node.origin));
            out.push(
                self.nodes[n]
                    .forward
                    .clone()
                    .expect("non-root node carries a forward"),
            );
        }
        self.current = target;
        self.last_was_nav = true;
        self.undid_last = false;
        Some(UndoStep {
            edits: out,
            undo_entry,
            groups,
            nodes,
            tail,
            forward,
            cursor: BytePos(cursor),
        })
    }

    /// `current` and every ancestor up to the root, nearest first.
    fn ancestry(&self, mut n: UndoNodeId) -> Vec<UndoNodeId> {
        let mut v = vec![n];
        while let Some(p) = self.nodes[n].parent {
            v.push(p);
            n = p;
        }
        v
    }

    /// The tip node of each undo GROUP in creation order — the state after each logical change. A
    /// group's nodes are created consecutively (coalescing only merges adjacent edits) and groups
    /// never recur, so each maximal same-group run of the kept nodes contributes one tip. The root is
    /// always one: it is the oldest kept state, whatever group it was made in.
    fn group_tips(&self) -> Vec<UndoNodeId> {
        let live: Vec<(UndoNodeId, u64)> = self.nodes.live().map(|(id, n)| (id, n.group)).collect();
        let mut tips = Vec::new();
        for (i, &(id, group)) in live.iter().enumerate() {
            let last_of_run = id == self.root
                || match live.get(i + 1) {
                    Some(&(_, next)) => next != group,
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
    /// Vim's undo-block granularity. `None` at either end of history (the oldest end is the root, the
    /// oldest kept state).
    pub fn to_chronological(&mut self, older: bool) -> Option<UndoStep> {
        let tips = self.group_tips();
        let pos = tips.iter().position(|&id| id == self.current)?;
        let target = if older {
            pos.checked_sub(1)
        } else {
            (pos + 1 < tips.len()).then_some(pos + 1)
        };
        self.navigate_to(tips[target?])
    }

    /// The `seq` of the current node — the identity dirty-tracking compares against `saved_node`.
    #[must_use]
    pub fn current_seq(&self) -> MonotonicSeq {
        self.nodes[self.current].seq
    }

    /// The `seq` of the root: the oldest state undo can reach.
    #[must_use]
    pub fn root_seq(&self) -> MonotonicSeq {
        self.nodes[self.root].seq
    }

    /// The document revision the current node was produced at — the value a journal Edit record persists
    /// as `result_revision` (persistence §2). For the root this is the open revision.
    #[must_use]
    pub fn current_result_revision(&self) -> Revision {
        self.nodes[self.current].result_revision
    }

    /// The undo group id of the current node — nodes sharing it undo/redo as one step.
    #[must_use]
    pub fn current_group(&self) -> u64 {
        self.nodes[self.current].group
    }

    /// Number of nodes the tree holds (the root, the states on the current lineage and the branched-away
    /// ones), after the bound dropped the oldest.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len() - self.free
    }

    /// Check the tree's structure and panic on the first broken rule (#906, stability §1 "an undo node's
    /// parent does not exist"): one root (the one node without a parent, carrying no edits), every other
    /// node's parent exists, was created before it and lists it as a child, `seq` follows creation order,
    /// a node's redo target is one of its children, every non-root node carries its forward and inverse
    /// edits, and the level and byte counts the bound reads agree with the nodes. `O(nodes)`; for
    /// property and fuzz tests.
    pub fn assert_invariants(&self) {
        let n = self.nodes.len();
        assert!(
            self.nodes.get(self.current).is_some(),
            "current node {} of {n} is not live",
            self.current.0
        );
        let root = self.nodes.get(self.root).expect("the root is live");
        assert!(root.parent.is_none(), "the root has a parent");
        assert!(
            root.forward.is_none() && root.inverse.is_none(),
            "the root carries edits"
        );
        let mut free = 0;
        let mut prev_seq = None;
        let mut groups: HashMap<u64, usize> = HashMap::new();
        let mut bytes = 0;
        for (i, slot) in self.nodes.0.iter().enumerate() {
            let Some(node) = slot else {
                free += 1;
                continue;
            };
            assert!(
                node.group < self.next_group,
                "node {i} has an unissued group"
            );
            assert!(node.seq.0 < self.next_seq, "node {i} has an unissued seq");
            if let Some(prev) = prev_seq {
                assert!(node.seq > prev, "seq does not increase at node {i}");
            }
            prev_seq = Some(node.seq);
            for c in &node.children {
                assert_eq!(
                    self.nodes.get(*c).and_then(|c| c.parent),
                    Some(UndoNodeId(i)),
                    "child {} of node {i} names another parent",
                    c.0
                );
            }
            if let Some(r) = node.redo_child {
                assert!(
                    node.children.contains(&r),
                    "node {i} redoes into a stranger"
                );
            }
            if UndoNodeId(i) == self.root {
                continue;
            }
            let p = node
                .parent
                .expect("a node other than the root has a parent");
            assert!(p.0 < i, "node {i}'s parent {} is not older than it", p.0);
            assert!(
                self.nodes
                    .get(p)
                    .is_some_and(|p| p.children.contains(&UndoNodeId(i))),
                "node {i} is missing from its parent's children"
            );
            assert!(
                node.forward.is_some() && node.inverse.is_some(),
                "node {i} lost its edits"
            );
            *groups.entry(node.group).or_insert(0) += 1;
            bytes += node.cost();
        }
        assert_eq!(free, self.free, "the empty slot count is off");
        assert_eq!(groups, self.groups, "the undo level count is off");
        assert_eq!(bytes, self.bytes, "the undo payload count is off");
    }

    /// Whether `g` is a group id this history has handed out.
    pub(crate) fn issued_group(&self, g: u64) -> bool {
        g < self.next_group
    }

    /// Whether `seq` was handed out by this history (the node may have been dropped since).
    pub(crate) fn issued_seq(&self, seq: MonotonicSeq) -> bool {
        seq.0 < self.next_seq
    }

    /// Kept nodes in creation (`seq`) order as `(seq, origin, group)` — the chronological index the
    /// `g-`/`g+` traversal is built on. Present here so a test can assert branched-away states are retained.
    #[must_use]
    pub fn creation_order(&self) -> Vec<(MonotonicSeq, Option<TransactionOrigin>, u64)> {
        self.nodes
            .live()
            .map(|(_, n)| (n.seq, n.origin, n.group))
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
        h.record(
            empty_edits(),
            empty_edits(),
            origin,
            hint,
            Revision::ZERO,
            BytePos(0),
        );
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
            BytePos(0),
        );
        let a_seq = h.current_seq();
        h.undo();
        h.record(
            empty_edits(),
            empty_edits(),
            TransactionOrigin::UserInput,
            GroupHint::BreakBefore,
            Revision::ZERO,
            BytePos(0),
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
        assert!(h.to_chronological(false).is_none());
    }

    #[test]
    fn redo_follows_the_branch_last_visited() {
        // #750: root -> A, undo, root -> B; g- to A, undo to root: C-r goes back to A, not to the newest B.
        let mut h = UndoHistory::new(Revision::ZERO);
        record(&mut h, TransactionOrigin::UserInput, GroupHint::BreakBefore);
        let a_seq = h.current_seq();
        h.undo();
        record(&mut h, TransactionOrigin::UserInput, GroupHint::BreakBefore);
        let b_seq = h.current_seq();
        h.to_chronological(true);
        h.undo();
        h.redo();
        assert_eq!(
            h.current_seq(),
            a_seq,
            "redo re-enters the branch just left"
        );
        h.undo();
        h.to_chronological(false); // root -> A by creation order
        h.to_chronological(false); // A -> B
        h.undo();
        h.redo();
        assert_eq!(h.current_seq(), b_seq, "and B once B was the one visited");
    }

    /// A change of its own group with `n` inserted bytes, through `trim` as the document does.
    fn change(h: &mut UndoHistory, n: usize) -> MonotonicSeq {
        let ins = EditList::new(vec![crate::edit::Edit::insert(BytePos(0), vec![b'a'; n])])
            .expect("one edit");
        let del = EditList::new(vec![crate::edit::Edit::delete(BytePos(0), n)]).expect("one edit");
        h.record(
            ins,
            del,
            TransactionOrigin::UserInput,
            GroupHint::BreakBefore,
            Revision::ZERO,
            BytePos(0),
        );
        h.trim();
        h.assert_invariants();
        h.current_seq()
    }

    fn undo_steps(h: &mut UndoHistory) -> usize {
        let mut n = 0;
        while h.undo().is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn undolevels_keeps_one_more_group_than_its_value() {
        // nvim v0.12.6 `--clean`, `:set ul=3` then five `x`: four undo, the fifth is `Already at oldest change`.
        let mut h = UndoHistory::new(Revision::ZERO);
        h.set_limits(UndoLimits {
            levels: Some(3),
            bytes: None,
        });
        let seqs: Vec<_> = (0..5).map(|_| change(&mut h, 1)).collect();
        assert_eq!(h.group_count(), 4);
        assert_eq!(
            seqs,
            (1..=5).map(MonotonicSeq).collect::<Vec<_>>(),
            "seq is never renumbered"
        );
        assert_eq!(undo_steps(&mut h), 4);
        assert_eq!(
            h.current_seq(),
            MonotonicSeq(1),
            "the oldest kept state is the root"
        );
        assert_eq!(h.root_seq(), MonotonicSeq(1));
        assert!(
            h.to_chronological(true).is_none(),
            "g- stops at the root too"
        );
    }

    #[test]
    fn the_byte_budget_drops_the_oldest_but_never_the_current_group() {
        let edit = std::mem::size_of::<crate::edit::Edit>();
        let mut h = UndoHistory::new(Revision::ZERO);
        // Two one-byte changes fit, a third does not.
        h.set_limits(UndoLimits {
            levels: Some(1000),
            bytes: Some(2 * (2 * edit + 1)),
        });
        for _ in 0..3 {
            change(&mut h, 1);
        }
        assert_eq!(h.group_count(), 2);
        // One change past the whole budget stays undoable: it is the current group.
        change(&mut h, 10_000);
        assert_eq!(h.group_count(), 1);
        assert!(h.payload_bytes() > 2 * (2 * edit + 1));
        assert_eq!(undo_steps(&mut h), 1);
    }

    #[test]
    fn the_oldest_alternate_branch_goes_before_the_oldest_level() {
        // nvim v0.12.6 `--clean`, `ul=1`, `x` `u` `$x` `x`: root -> A, undo, root -> B -> C. C drops A, the
        // alternate branch of the oldest header B, and keeps B.
        let mut h = UndoHistory::new(Revision::ZERO);
        h.set_limits(UndoLimits {
            levels: Some(1),
            bytes: None,
        });
        change(&mut h, 1);
        h.undo();
        let b = change(&mut h, 1);
        change(&mut h, 1);
        assert_eq!(h.group_count(), 2, "only the branch A went");
        assert_eq!(h.node_count(), 3);
        h.undo();
        h.undo();
        assert_eq!(
            h.current_seq(),
            MonotonicSeq(0),
            "B's parent, the open state, is still the root"
        );
        assert!(
            h.creation_order().iter().all(|n| n.0 != MonotonicSeq(1)),
            "A is gone"
        );
        h.redo();
        assert_eq!(h.current_seq(), b);
        // A third change re-roots the tree at B.
        h.redo();
        change(&mut h, 1);
        assert_eq!(h.root_seq(), b);
    }

    #[test]
    fn undolevels_zero_toggles_and_has_no_redo() {
        // nvim v0.12.6 `--clean`, `:set ul=0`, `xx`: `u` undoes the second `x`, `u` again takes the undo
        // back, and `CTRL-R` is `Already at newest change`.
        let mut h = UndoHistory::new(Revision::ZERO);
        h.set_limits(UndoLimits {
            levels: Some(0),
            bytes: None,
        });
        change(&mut h, 1);
        let second = change(&mut h, 1);
        assert_eq!(h.group_count(), 1);
        assert!(!h.can_redo());
        assert!(h.undo().is_some_and(|s| !s.forward));
        let root = h.current_seq();
        assert!(h.can_undo() && !h.can_redo());
        assert!(h.redo().is_none());
        assert!(h.undo().is_some_and(|s| s.forward), "u takes the undo back");
        assert_eq!(h.current_seq(), second);
        assert!(h.undo().is_some());
        assert_eq!(h.current_seq(), root);
    }

    #[test]
    fn a_negative_undolevels_makes_each_change_the_root() {
        // nvim v0.12.6 `--clean`: `xx`, `:set ul=-1`, `x`, `:set ul=1000`, `u` is `Already at oldest
        // change`; with no change under `ul=-1` the history stays.
        let mut h = UndoHistory::new(Revision::ZERO);
        change(&mut h, 1);
        change(&mut h, 1);
        h.set_limits(UndoLimits::from_options(-1, 0));
        assert!(h.can_undo(), "setting it drops nothing");
        let c = change(&mut h, 1);
        h.set_limits(UndoLimits::default());
        assert!(!h.can_undo());
        assert_eq!(h.root_seq(), c);
        assert_eq!(h.group_count(), 0);
        assert!(h.retract().is_none(), "the root cannot be taken back");
        h.assert_invariants();
    }

    #[test]
    fn retract_after_a_drop_takes_back_only_the_change() {
        let mut h = UndoHistory::new(Revision::ZERO);
        h.set_limits(UndoLimits {
            levels: Some(0),
            bytes: None,
        });
        let first = change(&mut h, 1);
        change(&mut h, 2);
        assert!(h.retract().is_some());
        h.assert_invariants();
        assert_eq!(h.current_seq(), first, "back on the new root");
        assert_eq!(h.group_count(), 0);
    }

    #[test]
    fn compaction_keeps_ids_and_order() {
        let mut h = UndoHistory::new(Revision::ZERO);
        h.set_limits(UndoLimits {
            levels: Some(2),
            bytes: None,
        });
        for _ in 0..50 {
            change(&mut h, 1);
        }
        assert!(h.nodes.len() < 10, "the dropped slots were compacted away");
        assert_eq!(
            h.nodes.live().next().map(|(id, _)| id),
            Some(h.root),
            "the root is the oldest slot"
        );
        assert_eq!(undo_steps(&mut h), 3);
        assert!(h.retract().is_none());
    }

    #[test]
    fn undolevels_zero_toggles_the_newest_group_even_with_older_ones_kept() {
        // nvim v0.12.6 `--clean`: `xxx`, `:set ul=0` (nothing dropped yet), `uuu` leaves one `x` undone.
        let mut h = UndoHistory::new(Revision::ZERO);
        let seqs: Vec<_> = (0..3).map(|_| change(&mut h, 1)).collect();
        h.set_limits(UndoLimits::from_options(0, 0));
        for want in [seqs[1], seqs[2], seqs[1]] {
            assert!(h.undo().is_some());
            assert_eq!(h.current_seq(), want);
        }
    }

    #[test]
    fn limits_from_options() {
        assert_eq!(UndoLimits::from_options(-5, 0).levels, None);
        assert_eq!(UndoLimits::from_options(1000, 0).bytes, None);
        assert_eq!(
            UndoLimits::from_options(7, 9),
            UndoLimits {
                levels: Some(7),
                bytes: Some(9)
            }
        );
    }
}
