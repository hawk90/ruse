//! The drainable change log a recovery journal is written from (RFC-0023 §1, D-064).
//!
//! A [`crate::Document`] that records changes ([`crate::Document::record_changes`]) appends one
//! [`ChangeEntry`] per revision bump, whatever path made it: a fresh transaction, `U`, an empty
//! transaction, or each edit list an undo, redo or chronological move re-applies. The frontend drains the
//! log after each command ([`crate::Document::drain_changes`]) and writes the entries; the core does no
//! I/O and never depends on the journal.
//!
//! The log is bounded (INV-FAIL-BOUNDED). When it would grow past its [`ChangeLogCap`] it drops every
//! entry and stops recording until the next drain, which answers [`ChangeDrain::Checkpoint`]: the writer
//! must write the whole current text instead of deltas. Recording is off by default, so a frontend that
//! does not journal pays nothing; a drain while it is off also answers `Checkpoint`, so a writer that
//! forgot to turn it on cannot mistake "not recorded" for "nothing changed".

use std::sync::Arc;

use crate::edit::EditList;
use crate::pos::Revision;
use crate::transaction::{GroupHint, TransactionOrigin};
use crate::undo::MonotonicSeq;

/// How an entry moved the undo tree, with nodes named by their [`MonotonicSeq`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    /// A change that added undo node `node` as a child of `parent` (typing, an operator, `U`).
    Fresh {
        node: MonotonicSeq,
        parent: MonotonicSeq,
    },
    /// A move through existing nodes that leaves `to_node` current (`u`, `C-r`, `g-`, `g+`). One `u` that
    /// undoes a group of several nodes is one entry per node, each naming the node it lands on. An empty
    /// transaction, which advances the revision but adds no node (#765), is a `Navigate` to the node that
    /// is already current, with empty edit lists.
    Navigate { to_node: MonotonicSeq },
}

/// One applied change: `forward` took the text at `base_revision` to the text at `result_revision`, and
/// `inverse` takes it back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChangeEntry {
    pub base_revision: Revision,
    pub result_revision: Revision,
    /// For [`ChangeKind::Fresh`], the transaction's origin; for a navigation step, the origin of the
    /// undo node whose edits it re-applies (the root's, an empty transaction's own).
    pub origin: TransactionOrigin,
    /// The transaction's grouping hint; [`GroupHint::Continue`] on a navigation step.
    pub group_hint: GroupHint,
    pub forward: EditList,
    pub inverse: EditList,
    pub kind: ChangeKind,
}

impl ChangeEntry {
    /// The bytes this entry holds against the cap: every inserted byte of both lists, plus a fixed cost
    /// per edit for its position and length.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        list_bytes(&self.forward) + list_bytes(&self.inverse)
    }
}

/// The per-edit cost counted on top of its inserted bytes: a position and a deleted length.
const EDIT_OVERHEAD: usize = 16;

fn list_bytes(list: &EditList) -> usize {
    list.edits()
        .iter()
        .map(|e| e.ins.len() + EDIT_OVERHEAD)
        .sum()
}

/// The log's bound: past either limit it overflows (RFC-0023 §1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChangeLogCap {
    pub entries: usize,
    pub bytes: usize,
}

impl Default for ChangeLogCap {
    /// 4096 entries or 8 MiB of edit payload.
    fn default() -> ChangeLogCap {
        ChangeLogCap {
            entries: 4096,
            bytes: 8 << 20,
        }
    }
}

/// Why a drain has no deltas to give.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckpointReason {
    /// Recording is off ([`crate::Document::record_changes`]); nothing was recorded.
    NotRecording,
    /// Recording was just turned on: there is no base the entries would apply to yet.
    Started,
    /// The log passed its [`ChangeLogCap`] and dropped its entries.
    Overflow,
}

/// What [`crate::Document::drain_changes`] hands the journal writer. `#[must_use]` and an enum, so the
/// overflow cannot be read past as an empty list.
#[must_use = "a Checkpoint drain means the deltas are gone and the whole text must be written"]
#[derive(Clone, Debug)]
pub enum ChangeDrain {
    /// Every change since the last drain, oldest first; each entry's `base_revision` is the previous
    /// one's `result_revision`. Empty when nothing changed.
    Changes(Vec<ChangeEntry>),
    /// The deltas since the last drain are not available. Write `text` (the document as it is now, at
    /// `revision`, on undo node `node`) as a new base; later drains continue from it.
    Checkpoint {
        reason: CheckpointReason,
        text: Arc<Vec<u8>>,
        revision: Revision,
        node: MonotonicSeq,
    },
}

/// The log itself, owned by the document.
#[derive(Debug, Default)]
pub(crate) struct ChangeLog {
    recording: bool,
    entries: Vec<ChangeEntry>,
    bytes: usize,
    /// Set when the entries since the last drain are unusable; recording pauses until the drain.
    lost: Option<CheckpointReason>,
    cap: ChangeLogCap,
}

impl ChangeLog {
    /// Whether a change should be offered to [`ChangeLog::push`]. False while off or after an overflow,
    /// so the caller skips building the entry (and cloning its edits).
    pub(crate) fn wants(&self) -> bool {
        self.recording && self.lost.is_none()
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.recording
    }

    /// Turn recording on or off. Turning it on starts with a `Started` checkpoint; turning it off drops
    /// what was recorded.
    pub(crate) fn set_recording(&mut self, on: bool) {
        if on == self.recording {
            return;
        }
        self.recording = on;
        self.clear();
        if on {
            self.lost = Some(CheckpointReason::Started);
        }
    }

    pub(crate) fn set_cap(&mut self, cap: ChangeLogCap) {
        self.cap = cap;
    }

    fn clear(&mut self) {
        self.entries = Vec::new();
        self.bytes = 0;
        self.lost = None;
    }

    /// Append `entry`, or overflow if it would pass the cap.
    pub(crate) fn push(&mut self, entry: ChangeEntry) {
        if !self.wants() {
            return;
        }
        debug_assert!(
            self.entries
                .last()
                .is_none_or(|p| p.result_revision == entry.base_revision),
            "change log entries must chain revision to revision"
        );
        let bytes = self.bytes.saturating_add(entry.payload_bytes());
        if self.entries.len() >= self.cap.entries || bytes > self.cap.bytes {
            self.clear();
            self.lost = Some(CheckpointReason::Overflow);
            return;
        }
        self.bytes = bytes;
        self.entries.push(entry);
    }

    /// Hand over what was recorded and start again. `text`, `revision` and `node` are the document's
    /// current state, given to the writer when it must checkpoint.
    pub(crate) fn drain(
        &mut self,
        text: &Arc<Vec<u8>>,
        revision: Revision,
        node: MonotonicSeq,
    ) -> ChangeDrain {
        let reason = if self.recording {
            self.lost.take()
        } else {
            Some(CheckpointReason::NotRecording)
        };
        if let Some(reason) = reason {
            self.clear();
            return ChangeDrain::Checkpoint {
                reason,
                text: Arc::clone(text),
                revision,
                node,
            };
        }
        self.bytes = 0;
        ChangeDrain::Changes(std::mem::take(&mut self.entries))
    }
}
