//! The editable Document — atomic transaction apply, undo/redo, and the anchor store, tied together
//! (INV-TXN, INV-UNDO, INV-ANCHOR).
//!
//! Every mutation goes through [`Document::apply`], which enforces INV-TXN (a stale `base_revision` is
//! refused; a successful apply strictly increases the revision) and F-001 atomicity (bounds are checked
//! *before* any byte moves, so a rejected transaction leaves the document untouched). Undo/redo
//! *navigate* the undo tree, re-applying stored inverse/forward edits as new revisions without creating
//! new undo nodes (persistence §7).

use std::sync::Arc;

use std::collections::HashMap;

use crate::anchor::{
    AnchorId, AnchorPolicy, AnchorStore, Bias, LineGravity, LineMap, Resolved, Tracked,
};
use crate::change_log::{ChangeDrain, ChangeEntry, ChangeKind, ChangeLog, ChangeLogCap};
use crate::edit::{Edit, EditError, EditList};
use crate::line_undo::LineUndo;
use crate::pos::{BytePos, Revision};
use crate::snapshot::{AnchorIndex, DocumentSnapshot};
use crate::transaction::{GroupHint, Transaction, TransactionOrigin};
use crate::undo::{MonotonicSeq, UndoHistory, UndoStep};
use crate::undo_cursor::{self, UndoCursor};

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
    // `Arc<Vec<u8>>` (not `Arc<[u8]>`) so a committed edit MOVES the freshly-built buffer into the Arc
    // (`Arc::new`) instead of copying it a second time (`Arc::from(vec)` reallocates + memcpy). Reads still
    // yield `&[u8]` via deref; clones for snapshots stay a refcount bump. Saves one O(n) copy per edit.
    text: Arc<Vec<u8>>,
    revision: Revision,
    anchors: AnchorStore,
    undo: UndoHistory,
    anchor_index: std::sync::OnceLock<Arc<AnchorIndex>>,
    /// Where every line starts ([`Document::line_starts`]), made on first use after each change.
    line_starts: std::sync::OnceLock<Vec<usize>>,
    /// The line starts before the last change and its edits, when they had been made: the next
    /// [`Document::line_starts`] carries them through the edits instead of scanning the text.
    line_starts_before: Option<(Vec<usize>, EditList)>,
    saved_node: Option<MonotonicSeq>,
    /// The line Vim's `U` restores ([`Document::line_undo`]).
    line_undo: LineUndo,
    /// An open undo block ([`Document::begin_undo_block`]): `Some(joining)`, where `joining` turns true once
    /// the block's first change is recorded and every later change then joins its undo group.
    undo_block: Option<bool>,
    /// Per undo group, the restorable [`Tracked`] positions as they were when the group began (Vim's
    /// `uh_namedm`/`uh_visual`): undoing the group puts them back, and keeps the ones it replaced for the
    /// redo (#686).
    mark_undo: HashMap<u64, Vec<Option<usize>>>,
    /// Positions carried for Views at rest ([`Document::pin_tracked`]).
    pinned: Vec<Tracked>,
    /// The applied changes a recovery journal drains ([`Document::drain_changes`], RFC-0023 §1).
    changes: ChangeLog,
}

impl Document {
    /// Open a document with `initial` bytes at [`Revision::ZERO`]. It starts *unsaved*
    /// ([`Document::is_modified`] is true until [`Document::mark_saved`]); a document loaded from disk
    /// marks itself saved at open.
    pub fn new(id: DocumentId, initial: impl Into<Vec<u8>>) -> Document {
        Document::starting_at(id, initial, Revision::ZERO)
    }

    /// A document that REPLACES one whose revision was `last` under the same id (a reload from disk,
    /// `:e!`): its revisions continue past `last`, so one id never shows a revision twice (F-001 monotonic;
    /// a cache keyed on `(id, revision)` cannot mistake the new text for the old, #687).
    pub fn replacing(id: DocumentId, initial: impl Into<Vec<u8>>, last: Revision) -> Document {
        Document::starting_at(id, initial, last.next())
    }

    fn starting_at(id: DocumentId, initial: impl Into<Vec<u8>>, revision: Revision) -> Document {
        let text: Arc<Vec<u8>> = Arc::new(initial.into());
        let mut d = Document {
            id,
            text,
            revision,
            anchors: AnchorStore::new(),
            undo: UndoHistory::new(revision),
            anchor_index: std::sync::OnceLock::new(),
            line_starts: std::sync::OnceLock::new(),
            line_starts_before: None,
            saved_node: None,
            line_undo: LineUndo::default(),
            undo_block: None,
            mark_undo: HashMap::new(),
            pinned: Vec::new(),
            changes: ChangeLog::default(),
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

    /// A cheap, owned handle to the current buffer: an `Arc` clone (refcount bump, no copy). Since a
    /// committed edit MOVES a freshly-built `Vec` into a new `Arc` (see [`Document::text`]), a held snapshot
    /// keeps pointing at the bytes it captured — so callers that need the buffer to outlive a later
    /// `&mut Document` borrow (e.g. the per-frame render snapshot) can hold this instead of copying with
    /// `bytes().to_vec()`. Deref-coerces to `&[u8]` at every use site.
    #[must_use]
    pub fn text_arc(&self) -> Arc<Vec<u8>> {
        Arc::clone(&self.text)
    }

    /// The byte offset where each line starts: `0`, then one past every `\n` (so a final newline is
    /// followed by the start of an empty last line). Built in one pass on the first call after a change
    /// and kept until the next, so a command that looks lines up many times scans the text once (#1257).
    #[must_use]
    pub fn line_starts(&self) -> &[usize] {
        self.line_starts
            .get_or_init(|| match &self.line_starts_before {
                Some((starts, edits)) => carried_line_starts(starts, edits),
                None => line_starts_of(&self.text),
            })
    }

    /// The text just changed by `edits`: keep the line starts made before it, if any, for the next
    /// [`Document::line_starts`] to carry through the edits (`O(lines)` with no text scan). A change
    /// with no lookup since the last one drops them, so a run of edits pays nothing.
    fn carry_line_starts(&mut self, edits: &EditList) {
        self.line_starts_before = std::mem::take(&mut self.line_starts)
            .into_inner()
            .map(|starts| (starts, edits.clone()));
    }

    /// The 0-based row of `at` — the number of `\n` before it ([`crate::pos::line_of`]), in `O(log n)`.
    #[must_use]
    pub fn line_of(&self, at: BytePos) -> usize {
        let at = at.0.min(self.text.len());
        self.line_starts().partition_point(|&s| s <= at) - 1
    }

    /// Where the 0-based `line`-th line starts, clamped to the end ([`crate::pos::nth_line_start`]).
    #[must_use]
    pub fn nth_line_start(&self, line: usize) -> BytePos {
        BytePos(
            self.line_starts()
                .get(line)
                .copied()
                .unwrap_or(self.text.len()),
        )
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

    /// Create a LINE anchor at `offset` that follows line inserts and deletes as a Vim mark does
    /// (anchor-store §3.1).
    pub fn create_line_anchor(&mut self, offset: BytePos, gravity: LineGravity) -> AnchorId {
        let id = self.anchors.insert_line(offset, gravity);
        self.rebuild_index();
        id
    }

    /// Create a long-lived anchor at `offset` with the given [`Bias`] and span-delete [`AnchorPolicy`].
    pub fn create_anchor(&mut self, offset: BytePos, bias: Bias, policy: AnchorPolicy) -> AnchorId {
        let id = self.anchors.insert(offset, bias, policy);
        self.rebuild_index();
        id
    }

    /// Create many anchors at once, rebuilding the anchor index once instead of once per anchor (a `:g`
    /// over many lines marks two per line; one by one that was quadratic).
    pub fn create_anchors(
        &mut self,
        specs: impl IntoIterator<Item = (BytePos, Bias, AnchorPolicy)>,
    ) -> Vec<AnchorId> {
        let ids = specs
            .into_iter()
            .map(|(offset, bias, policy)| self.anchors.insert(offset, bias, policy))
            .collect();
        self.rebuild_index();
        ids
    }

    /// Remove many anchors at once (see [`Document::create_anchors`]). Returns how many were live.
    pub fn remove_anchors(&mut self, ids: impl IntoIterator<Item = AnchorId>) -> usize {
        let removed = ids
            .into_iter()
            .filter(|&id| self.anchors.remove(id))
            .count();
        if removed > 0 {
            self.rebuild_index();
        }
        removed
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
        self.apply_inner(txn, true, &mut [])
    }

    /// [`Document::apply`] that also carries `tracked` — positions the caller keeps outside the store —
    /// through the change by the anchor rules (#686). When the change begins a new undo group, the
    /// restorable ones are saved for its undo.
    pub fn apply_tracked(
        &mut self,
        txn: Transaction,
        tracked: &mut [Tracked],
    ) -> Result<Revision, TxnError> {
        self.apply_inner(txn, true, tracked)
    }

    /// Carry `tracked` with every change from now until [`Document::take_pinned`] — the marks and cursor
    /// of a View at rest while another View edits this document (#686). Returns where they start in
    /// the pinned list. Undo does not restore pinned positions. They stay out of the anchor store, so
    /// pinning costs nothing until an edit, which moves each by the same rules in `O(log E)`.
    pub fn pin_tracked(&mut self, tracked: &[Tracked]) -> usize {
        let at = self.pinned.len();
        self.pinned.extend(tracked.iter().map(|t| Tracked {
            restore: false,
            ..*t
        }));
        at
    }

    /// Whether no position is pinned ([`Document::pin_tracked`]) — the state between commands.
    pub(crate) fn pinned_is_empty(&self) -> bool {
        self.pinned.is_empty()
    }

    /// Take back every position [`Document::pin_tracked`] holds, as the changes since left them.
    pub fn take_pinned(&mut self) -> Vec<Tracked> {
        std::mem::take(&mut self.pinned)
    }

    /// Carry the caller's `tracked` and the pinned positions through one change from `old` to the
    /// current text.
    fn track_change(&mut self, old: &[u8], edits: &EditList, tracked: &mut [Tracked]) {
        if tracked.is_empty() && self.pinned.is_empty() {
            return;
        }
        let map = LineMap::new(old, &self.text, edits);
        for t in tracked.iter_mut().chain(self.pinned.iter_mut()) {
            t.apply(edits, &map);
        }
    }

    fn apply_inner(
        &mut self,
        txn: Transaction,
        track: bool,
        tracked: &mut [Tracked],
    ) -> Result<Revision, TxnError> {
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
        // An empty edit list changes nothing, so it is no undo step and leaves the modified state alone
        // (INV-UNDO, #765); the revision still advances (INV-TXN).
        if txn.edits.is_empty() {
            let base = self.revision;
            self.revision = self.revision.next();
            if self.changes.wants() {
                self.changes.push(ChangeEntry {
                    base_revision: base,
                    result_revision: self.revision,
                    origin: txn.origin,
                    group_hint: txn.group_hint,
                    forward: EditList::default(),
                    inverse: EditList::default(),
                    kind: ChangeKind::Navigate {
                        to_node: self.undo.current_seq(),
                    },
                });
            }
            return Ok(self.revision);
        }
        // Committed path — nothing above mutated state, so failure above is atomic-no-op.
        let inverse = txn.edits.inverse(&self.text);
        let new_text = txn.edits.apply_to(&self.text);
        if track {
            let fresh = txn.group_hint == GroupHint::BreakBefore;
            self.line_undo.on_edit(&self.text, &txn.edits, fresh);
        }
        let saved: Vec<Option<usize>> = tracked
            .iter()
            .filter(|t| t.restore)
            .map(|t| t.pos.map(|p| p.0))
            .collect();
        let old = std::mem::replace(&mut self.text, Arc::new(new_text));
        self.carry_line_starts(&txn.edits);
        self.track_change(&old, &txn.edits, tracked);
        let prev = self.revision;
        self.revision = self.revision.next();
        // Internal invariant (INV-TXN §1): a successful apply strictly advances the revision. A debug_assert,
        // not a Result — a violation here is a bug in Revision, not an expected input failure (stability §1).
        debug_assert!(
            self.revision > prev,
            "apply must strictly advance revision ({prev:?} -> {:?})",
            self.revision
        );
        self.anchors.apply_change(&old, &self.text, &txn.edits);
        let (origin, mut hint) = (txn.origin, txn.group_hint);
        match self.undo_block {
            Some(true) => hint = GroupHint::JoinPrev,
            Some(false) => self.undo_block = Some(true),
            None => {}
        }
        // Without an explicit start cursor, the change began at the start of its first edited line.
        let cursor = txn.cursor.unwrap_or_else(|| {
            txn.edits
                .edits()
                .first()
                .map_or(BytePos(0), |e| crate::pos::line_start(&self.text, e.pos))
        });
        let group_before = self.undo.current_group();
        let parent = self.undo.current_seq();
        let logged = self
            .changes
            .wants()
            .then(|| (txn.edits.clone(), inverse.clone()));
        self.undo
            .record(txn.edits, inverse, origin, hint, self.revision, cursor);
        if let Some((forward, inverse)) = logged {
            self.changes.push(ChangeEntry {
                base_revision: prev,
                result_revision: self.revision,
                origin,
                group_hint: txn.group_hint,
                forward,
                inverse,
                kind: ChangeKind::Fresh {
                    node: self.undo.current_seq(),
                    parent,
                },
            });
        }
        let group = self.undo.current_group();
        if group != group_before && !saved.is_empty() {
            // A new undo step: remember the marks as they were before it (Vim's `u_savecommon`).
            self.mark_undo.insert(group, saved);
        }
        self.rebuild_index();
        Ok(self.revision)
    }

    /// Open an undo block: every change recorded until [`Document::end_undo_block`] joins the group of the
    /// block's first change, so one `u` undoes them all (INV-UNDO). This is how Vim treats keys that were
    /// not typed (`:normal`, `:g/pat/normal`): no undo sync happens between them. A block already open is
    /// left as it is, so nested blocks fold into the outermost.
    pub fn begin_undo_block(&mut self) {
        self.undo_block.get_or_insert(false);
    }

    /// Close the undo block [`Document::begin_undo_block`] opened; the next change starts a group as usual.
    pub fn end_undo_block(&mut self) {
        self.undo_block = None;
    }

    /// Whether an undo block is open.
    #[must_use]
    pub fn in_undo_block(&self) -> bool {
        self.undo_block.is_some()
    }

    /// Undo the last change **group** on the current lineage, returning the new revision, or `None` at the
    /// root. Re-applies each node's inverse (a whole insert session / operator undoes in one step) and moves
    /// `current` to the node below the group — the revision advances (INV-TXN §2) but no new node is created.
    pub fn undo(&mut self) -> Option<Revision> {
        self.undo_from(BytePos(0)).map(|_| self.revision)
    }

    /// Redo the next change **group** along the newest branch, or `None` if there is nothing to redo.
    pub fn redo(&mut self) -> Option<Revision> {
        self.redo_from(BytePos(0)).map(|_| self.revision)
    }

    /// [`Document::undo_from`] carrying `tracked` (see [`Document::apply_tracked`]): the restorable ones go
    /// back to where they were when the undone change began.
    pub fn undo_tracked(&mut self, cursor: BytePos, tracked: &mut [Tracked]) -> Option<UndoCursor> {
        let step = self.undo.undo()?;
        Some(self.replay(step, cursor.0, tracked))
    }

    /// [`Document::redo_from`] carrying `tracked`.
    pub fn redo_tracked(&mut self, cursor: BytePos, tracked: &mut [Tracked]) -> Option<UndoCursor> {
        let step = self.undo.redo()?;
        Some(self.replay(step, cursor.0, tracked))
    }

    /// [`Document::chronological_from`] carrying `tracked`.
    pub fn chronological_tracked(
        &mut self,
        older: bool,
        cursor: BytePos,
        tracked: &mut [Tracked],
    ) -> Option<UndoCursor> {
        let step = self.undo.to_chronological(older)?;
        Some(self.replay(step, cursor.0, tracked))
    }

    /// Move one logical change along CHRONOLOGICAL creation order — `g-` (`older = true`) to the
    /// previously-created state, `g+` (`older = false`) to the next — crossing branches so a state
    /// abandoned by a new edit is still reachable (Vim `g-`/`g+`). Returns the new revision, or
    /// `None` at either end. Re-applies the transforming edits; no new node is created.
    pub fn undo_chronological(&mut self, older: bool) -> Option<Revision> {
        self.chronological_from(older, BytePos(0))
            .map(|_| self.revision)
    }

    /// [`Document::undo`] that also says where Vim leaves the cursor; `cursor` is the caret before it.
    pub fn undo_from(&mut self, cursor: BytePos) -> Option<UndoCursor> {
        self.undo_tracked(cursor, &mut [])
    }

    /// [`Document::redo`] that also says where Vim leaves the cursor; `cursor` is the caret before it.
    pub fn redo_from(&mut self, cursor: BytePos) -> Option<UndoCursor> {
        self.redo_tracked(cursor, &mut [])
    }

    /// [`Document::undo_chronological`] that also says where Vim leaves the cursor.
    pub fn chronological_from(&mut self, older: bool, cursor: BytePos) -> Option<UndoCursor> {
        self.chronological_tracked(older, cursor, &mut [])
    }

    /// Apply an undo step and place the cursor from its last group, as Vim's `u_undoredo` does (#1063).
    /// After each group's last list, the restorable `tracked` positions take the values that group saved,
    /// and the group keeps the ones they replace for the way back (Vim swaps `uh_namedm`, #686).
    fn replay(&mut self, step: UndoStep, cursor: usize, tracked: &mut [Tracked]) -> UndoCursor {
        let cur_line = crate::pos::raw::line_of(&self.text, cursor);
        let mut entries = Vec::new();
        let mut uh = (0, 0);
        let mut lines_before = undo_cursor::line_count(&self.text);
        let mut before: Vec<Option<usize>> = Vec::new();
        for (i, edits) in step.edits.iter().enumerate() {
            let group = step.groups.get(i).copied();
            if i == 0 || step.groups.get(i - 1).copied() != group {
                before = tracked
                    .iter()
                    .filter(|t| t.restore)
                    .map(|t| t.pos.map(|p| p.0))
                    .collect();
            }
            if i == step.tail {
                lines_before = undo_cursor::line_count(&self.text);
                if step.forward {
                    // A redo's `uh_cursor` is in the text before the change, which is the text right now.
                    uh = undo_cursor::line_col(&self.text, step.cursor.0);
                }
            }
            let old = Arc::clone(&self.text);
            self.reapply(edits, step.nodes[i], tracked);
            if i >= step.tail {
                undo_cursor::entries(&old, &self.text, edits, &mut entries);
            }
            if let Some(g) = group {
                if step.groups.get(i + 1).copied() != Some(g) {
                    self.restore_marks(g, std::mem::take(&mut before), tracked);
                }
            }
        }
        if !step.forward {
            // An undo's `uh_cursor` is in the text the undo restores.
            uh = undo_cursor::line_col(&self.text, step.cursor.0);
        }
        let lines = undo_cursor::line_count(&self.text);
        let mut placed = undo_cursor::place(&entries, uh, cur_line, lines);
        placed.marks = undo_cursor::change_marks(&entries, lines_before, lines);
        placed
    }

    /// Put the restorable `tracked` positions back as undo group `group` saved them, keeping `before` (the
    /// values they had before this undo/redo) for the way back. A position the group saw unset is left
    /// as it is, and one that is unset now is saved unset (Vim's `u_undoredo`).
    fn restore_marks(&mut self, group: u64, before: Vec<Option<usize>>, tracked: &mut [Tracked]) {
        let Some(saved) = self.mark_undo.get_mut(&group) else {
            return;
        };
        let len = self.text.len();
        for (t, s) in tracked.iter_mut().filter(|t| t.restore).zip(saved.iter()) {
            if let Some(p) = *s {
                t.pos = Some(BytePos(crate::motion::snap(&self.text, p.min(len))));
            }
        }
        *saved = before;
    }

    /// Before a change: the caret it starts from (the column `U` returns to) and whether it begins a new
    /// command, which lets it save its line for `U`. See [`crate::line_undo`].
    pub(crate) fn prime_line_undo(&mut self, cursor: usize, new_command: bool) {
        self.line_undo.prime(cursor, new_command);
    }

    /// Drop the line `U` would restore (`o`, `O`, `<CR>`); `disarm` keeps the rest of the command from
    /// saving another.
    pub(crate) fn clear_line_undo(&mut self, disarm: bool) {
        self.line_undo.clear(disarm);
    }

    /// Whether `U` has a line to restore.
    #[must_use]
    pub fn can_line_undo(&self) -> bool {
        self.line_undo.available(&self.text)
    }

    /// Vim `U`: put back the latest changed line as it was before its run of changes, as one undoable
    /// change (so `u` undoes it), keeping the current text so the next `U` redoes. Returns the caret
    /// (the column the line was saved with), or `None` when there is nothing to restore.
    pub fn line_undo(&mut self, cursor: BytePos) -> Option<BytePos> {
        self.line_undo_tracked(cursor, TransactionOrigin::UserInput, &mut [])
    }

    /// [`Document::line_undo`] made by `origin`, carrying `tracked` through the change.
    pub fn line_undo_tracked(
        &mut self,
        cursor: BytePos,
        origin: TransactionOrigin,
        tracked: &mut [Tracked],
    ) -> Option<BytePos> {
        let cursor = cursor.0;
        let (s, e, text, col) = self.line_undo.swap(&self.text, cursor)?;
        let len = text.len();
        let list = EditList::new(vec![Edit::replace(BytePos(s), e - s, text)])
            .expect("one edit is disjoint");
        let txn = Transaction::new(self.revision, list, origin)
            .with_hint(GroupHint::BreakBefore)
            .with_cursor(BytePos(cursor));
        self.apply_inner(txn, false, tracked)
            .expect("the saved line's span is in range");
        Some(BytePos(s + col.min(len)))
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

    /// Mark the current state as the one on disk (called after a save, or at open for a loaded file). It
    /// also closes the undo group, so a change after the save is a step of its own and undoing it lands
    /// back on the saved state (#766).
    pub fn mark_saved(&mut self) {
        self.saved_node = Some(self.undo.current_seq());
        self.undo.sync();
    }

    /// Close the current undo group (Vim's `u_sync`): the next change starts a new undo step even when its
    /// transaction asks to continue the group. The frontend syncs before every typed command outside text
    /// entry, so each Normal or Ex command undoes on its own while an Insert session stays one step (#678).
    pub fn sync_undo(&mut self) {
        self.undo.sync();
    }

    /// Turn the change log on or off (RFC-0023 §1). Off by default, so a frontend that does not journal
    /// pays nothing. Turning it on makes the next [`Document::drain_changes`] a
    /// [`ChangeDrain::Checkpoint`] (the base the following entries apply to); turning it off drops what
    /// was recorded.
    pub fn record_changes(&mut self, on: bool) {
        self.changes.set_recording(on);
    }

    /// Whether the change log is on ([`Document::record_changes`]).
    #[must_use]
    pub fn is_recording_changes(&self) -> bool {
        self.changes.is_recording()
    }

    /// Set the change log's bound (default [`ChangeLogCap::default`]: 4096 entries or 8 MiB of edits).
    pub fn set_change_log_cap(&mut self, cap: ChangeLogCap) {
        self.changes.set_cap(cap);
    }

    /// Hand the journal writer every change applied since the last drain, and clear the log: one
    /// [`ChangeEntry`] per revision bump, oldest first. When the log is off, was just turned on, or
    /// overflowed its cap since the last drain, the answer is [`ChangeDrain::Checkpoint`] with the current
    /// text instead, and the writer must write that whole text as its new base.
    pub fn drain_changes(&mut self) -> ChangeDrain {
        self.changes
            .drain(&self.text, self.revision, self.undo.current_seq())
    }

    /// Access to the undo history (for chronological inspection / navigation state).
    #[must_use]
    pub fn history(&self) -> &UndoHistory {
        &self.undo
    }

    /// Take an immutable, revision-stamped snapshot. O(1): clones the text `Arc` and the frozen anchor
    /// index `Arc` (INV-QUERY-SNAPSHOT, query-and-snapshot §2). The index is frozen on the first snapshot
    /// after a change, not on every change, so a run of edits with no reader between pays for none.
    #[must_use]
    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot::new(
            self.revision,
            Arc::clone(&self.text),
            Arc::clone(
                self.anchor_index
                    .get_or_init(|| Arc::new(AnchorIndex::from_entries(self.anchors.resolved()))),
            ),
        )
    }

    /// Re-apply an edit list as a navigation step (undo/redo): advances the revision and moves anchors,
    /// but records no new undo node.
    /// `node` is the undo node current after it and the origin of the node whose edits these are, for the
    /// change log.
    fn reapply(
        &mut self,
        edits: &EditList,
        (to_node, origin): (MonotonicSeq, Option<TransactionOrigin>),
        tracked: &mut [Tracked],
    ) {
        let new_text = edits.apply_to(&self.text);
        self.line_undo.on_reapply(&self.text, edits);
        if self.changes.wants() {
            self.changes.push(ChangeEntry {
                base_revision: self.revision,
                result_revision: self.revision.next(),
                // Only the root has no origin, and its edits are never re-applied.
                origin: origin.unwrap_or(TransactionOrigin::UserInput),
                group_hint: GroupHint::Continue,
                forward: edits.clone(),
                inverse: edits.inverse(&self.text),
                kind: ChangeKind::Navigate { to_node },
            });
        }
        let old = std::mem::replace(&mut self.text, Arc::new(new_text));
        self.carry_line_starts(edits);
        self.revision = self.revision.next();
        self.anchors.apply_change(&old, &self.text, edits);
        self.track_change(&old, edits, tracked);
        self.rebuild_index();
    }

    /// The invariant walker for one buffer (#906): the undo tree's structure, the anchor store against
    /// the text, the saved state naming a node of the tree, the mark groups naming issued groups, and
    /// a frozen anchor index (if any) agreeing with the store. Panics on the first broken rule; property
    /// and fuzz tests call it after every step, so an inconsistency is caught where it starts rather than
    /// when a later read trips over it.
    pub fn assert_invariants(&self) {
        let len = self.text.len();
        self.undo.assert_invariants();
        self.anchors.assert_invariants(len);
        if let Some(seq) = self.saved_node {
            assert!(
                self.undo.has_seq(seq),
                "the saved state {seq:?} is no undo node"
            );
        }
        for g in self.mark_undo.keys() {
            assert!(
                self.undo.issued_group(*g),
                "marks saved for unissued undo group {g}"
            );
        }
        for t in &self.pinned {
            if let Some(BytePos(p)) = t.pos {
                assert!(p <= len, "pinned position {p} past the end {len}");
            }
        }
        if let Some(ix) = self.anchor_index.get() {
            let frozen: HashMap<_, _> = ix.all().iter().copied().collect();
            let live: HashMap<_, _> = self.anchors.resolved().into_iter().collect();
            assert_eq!(frozen, live, "the frozen anchor index is stale");
        }
    }

    /// Drop the frozen anchor index after any change; the next snapshot freezes a fresh one.
    fn rebuild_index(&mut self) {
        self.anchor_index = std::sync::OnceLock::new();
    }
}

/// [`Document::line_starts`] of `text`: 64 bytes at a time, a mask of the newlines (which the compiler
/// vectorises) and then one push per set bit.
fn line_starts_of(text: &[u8]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(text.len() / 32 + 1);
    starts.push(0);
    for (k, chunk) in text.chunks(64).enumerate() {
        let mut mask = chunk
            .iter()
            .enumerate()
            .fold(0u64, |m, (i, &c)| m | (u64::from(c == b'\n') << i));
        while mask != 0 {
            starts.push(k * 64 + mask.trailing_zeros() as usize + 1);
            mask &= mask - 1;
        }
    }
    starts
}

/// The line starts of the text `edits` made, from the `starts` of the text before them: a start whose
/// newline the edits deleted goes, one per inserted newline comes, and every other moves by the edits
/// before it.
fn carried_line_starts(starts: &[usize], edits: &EditList) -> Vec<usize> {
    let mut out = Vec::with_capacity(starts.len() + 16);
    out.push(0);
    let mut delta: isize = 0;
    let mut i = 1;
    let shifted = |s: usize, d: isize| s.wrapping_add_signed(d);
    for e in edits.edits() {
        let (pos, end) = (e.pos.0, e.pos.0 + e.del);
        // A newline before the edit (start `<= pos`) stays; one inside the deleted span goes.
        let keep = i + starts[i..].partition_point(|&s| s <= pos);
        out.extend(starts[i..keep].iter().map(|&s| shifted(s, delta)));
        i = keep + starts[keep..].partition_point(|&s| s <= end);
        let at = shifted(pos, delta);
        out.extend(
            e.ins
                .iter()
                .enumerate()
                .filter(|&(_, &c)| c == b'\n')
                .map(|(j, _)| at + j + 1),
        );
        delta += e.delta();
    }
    out.extend(starts[i..].iter().map(|&s| shifted(s, delta)));
    out
}

#[cfg(test)]
mod text_arc_tests {
    use super::*;
    use crate::edit::{Edit, EditList};
    use crate::transaction::{Transaction, TransactionOrigin};

    /// The line index answers what the scanning [`crate::pos`] helpers answer, before and after an edit
    /// and an undo.
    #[test]
    fn line_index_matches_the_scans() {
        let check = |d: &Document| {
            let b = d.bytes();
            for at in 0..=b.len() + 1 {
                assert_eq!(d.line_of(BytePos(at)), crate::pos::line_of(b, BytePos(at)));
            }
            for line in 0..=crate::pos::count_newlines(b) + 2 {
                assert_eq!(d.nth_line_start(line), crate::pos::nth_line_start(b, line));
            }
        };
        let long: Vec<u8> = (0..300u32)
            .flat_map(|i| {
                let mut l = vec![b'x'; (i % 70) as usize];
                l.push(b'\n');
                l
            })
            .collect();
        for text in [&b""[..], b"\n", b"a", b"a\n", b"\n\nb", &long] {
            let mut d = Document::new(DocumentId(1), text.to_vec());
            check(&d);
            let edit = EditList::new(vec![Edit::insert(BytePos(0), "x\ny\n")]).unwrap();
            d.apply(Transaction::new(
                d.revision(),
                edit,
                TransactionOrigin::UserInput,
            ))
            .unwrap();
            check(&d);
            d.undo();
            check(&d);
        }
        // Carried through edits that delete and insert newlines, against a scan of the result.
        for len in [0usize, 1, 2, 3, 7, 40, 200] {
            let text: Vec<u8> = (0..len)
                .map(|i| if i % 3 == 1 { b'\n' } else { b'a' })
                .collect();
            for a in 0..=len {
                for b in a..=len {
                    let mut d = Document::new(DocumentId(1), text.clone());
                    let _ = d.line_starts();
                    let mut es = vec![Edit::replace(BytePos(a), b - a, "\nq\n")];
                    if b + 2 <= len {
                        es.push(Edit::delete(BytePos(b + 1), 1));
                    }
                    let list = EditList::new(es).unwrap();
                    d.apply(Transaction::new(
                        d.revision(),
                        list,
                        TransactionOrigin::UserInput,
                    ))
                    .unwrap();
                    assert_eq!(d.line_starts(), line_starts_of(d.bytes()), "{len} {a} {b}");
                    d.undo();
                    assert_eq!(
                        d.line_starts(),
                        line_starts_of(d.bytes()),
                        "{len} {a} {b} u"
                    );
                }
            }
        }
    }

    #[test]
    fn text_arc_is_a_cheap_shared_handle() {
        let d = Document::new(DocumentId(1), b"abc".to_vec());
        // Two handles point at the SAME allocation (a refcount bump, not a copy).
        let a = d.text_arc();
        let b = d.text_arc();
        assert!(Arc::ptr_eq(&a, &b), "text_arc clones share the buffer");
        assert_eq!(&**a, b"abc");
    }

    #[test]
    fn a_held_snapshot_survives_a_later_edit() {
        // The whole point: a snapshot taken before an edit keeps the OLD bytes, because a committed edit
        // builds a fresh Arc rather than mutating the shared one in place.
        let mut d = Document::new(DocumentId(1), b"abc".to_vec());
        let snapshot = d.text_arc();
        d.apply(Transaction::new(
            d.revision(),
            EditList::new(vec![Edit::insert(BytePos(3), b"d".to_vec())]).unwrap(),
            TransactionOrigin::UserInput,
        ))
        .unwrap();
        assert_eq!(&**snapshot, b"abc", "the held snapshot is unchanged");
        assert_eq!(d.bytes(), b"abcd", "the live document advanced");
    }

    /// #852: every change inside an undo block undoes as one step, even ones that ask for a fresh group;
    /// after the block the next change starts its own group again.
    #[test]
    fn an_undo_block_undoes_as_one_step() {
        let mut d = Document::new(DocumentId(1), b"abc".to_vec());
        let put = |d: &mut Document, at: usize, s: &[u8]| {
            let list = EditList::new(vec![Edit::insert(BytePos(at), s.to_vec())]).unwrap();
            let txn = Transaction::new(d.revision(), list, TransactionOrigin::UserInput)
                .with_hint(GroupHint::BreakBefore);
            d.apply(txn).unwrap();
        };
        d.begin_undo_block();
        d.begin_undo_block(); // nested: folds into the outer block
        put(&mut d, 0, b"1");
        put(&mut d, 0, b"2");
        d.end_undo_block();
        assert!(!d.in_undo_block());
        put(&mut d, 0, b"3");
        assert_eq!(d.bytes(), b"321abc");
        d.undo();
        assert_eq!(
            d.bytes(),
            b"21abc",
            "the change after the block is its own step"
        );
        d.undo();
        assert_eq!(d.bytes(), b"abc", "the block's changes undo together");
    }

    fn put_continue(d: &mut Document, at: usize, s: &[u8]) {
        let list = EditList::new(vec![Edit::insert(BytePos(at), s.to_vec())]).unwrap();
        d.apply(Transaction::new(
            d.revision(),
            list,
            TransactionOrigin::UserInput,
        ))
        .unwrap();
    }

    /// #765: an empty edit list is no undo step and does not make the buffer modified.
    #[test]
    fn an_empty_transaction_is_no_undo_step() {
        let mut d = Document::new(DocumentId(1), b"abc".to_vec());
        d.mark_saved();
        let rev = d.revision();
        let empty = EditList::new(Vec::new()).unwrap();
        let after = d
            .apply(Transaction::new(rev, empty, TransactionOrigin::UserInput))
            .unwrap();
        assert!(after > rev, "the revision still advances (INV-TXN)");
        assert!(!d.is_modified());
        assert!(!d.can_undo());
        assert_eq!(d.history().node_count(), 1);
    }

    /// #766: a save closes the undo group, so undoing the change made after it lands on the saved state.
    #[test]
    fn a_save_breaks_the_undo_group() {
        let mut d = Document::new(DocumentId(1), b"abc".to_vec());
        put_continue(&mut d, 0, b"1");
        d.mark_saved();
        put_continue(&mut d, 0, b"2");
        assert!(d.is_modified());
        d.undo();
        assert_eq!(d.bytes(), b"1abc");
        assert!(!d.is_modified(), "back on the saved state");
    }

    /// #678: an undo sync makes the next change its own step even when it asks to continue the group.
    #[test]
    fn an_undo_sync_starts_a_new_step() {
        let mut d = Document::new(DocumentId(1), b"abc".to_vec());
        put_continue(&mut d, 0, b"1");
        put_continue(&mut d, 0, b"2");
        d.sync_undo();
        put_continue(&mut d, 0, b"3");
        d.undo();
        assert_eq!(d.bytes(), b"21abc");
        d.undo();
        assert_eq!(
            d.bytes(),
            b"abc",
            "the changes before the sync stay one step"
        );
    }
}
