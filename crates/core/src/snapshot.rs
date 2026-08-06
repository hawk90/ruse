//! Immutable, revision-stamped document snapshots (C-QUERY, INV-QUERY-SNAPSHOT).
//!
//! A [`DocumentSnapshot`] is an immutable view of one document at one revision. It exposes only `&self`
//! readers and hands out no `&mut` and no owned inner buffer, so INV-QUERY-SNAPSHOT is *structural*: a
//! holder can, at worst, read a slightly old document. Cloning is O(1) — a handful of `Arc` bumps —
//! because the text and the frozen anchor index are rebuilt once per commit and shared, never copied
//! per snapshot (query-and-snapshot §2).

use std::sync::Arc;

use crate::anchor::{AnchorId, Resolved};
use crate::pos::Revision;

/// Anchor positions frozen at one revision (query-and-snapshot §2 `AnchorIndex`). Built once at commit
/// and shared by `Arc`; a snapshot borrows it rather than re-resolving the store.
#[derive(Debug, Default)]
pub struct AnchorIndex {
    entries: Vec<(AnchorId, Resolved)>,
}

impl AnchorIndex {
    pub(crate) fn from_entries(entries: Vec<(AnchorId, Resolved)>) -> AnchorIndex {
        AnchorIndex { entries }
    }

    /// Resolve `id` as of this index's revision, or `None` if it was not live then.
    #[must_use]
    pub fn resolve(&self, id: AnchorId) -> Option<Resolved> {
        self.entries.iter().find(|(k, _)| *k == id).map(|(_, r)| *r)
    }

    /// Every frozen anchor, ascending by offset.
    #[must_use]
    pub fn all(&self) -> &[(AnchorId, Resolved)] {
        &self.entries
    }
}

/// An immutable view of a document at a single revision.
#[derive(Clone)]
pub struct DocumentSnapshot {
    revision: Revision,
    text: Arc<[u8]>,
    anchors: Arc<AnchorIndex>,
}

impl DocumentSnapshot {
    pub(crate) fn new(
        revision: Revision,
        text: Arc<[u8]>,
        anchors: Arc<AnchorIndex>,
    ) -> DocumentSnapshot {
        DocumentSnapshot {
            revision,
            text,
            anchors,
        }
    }

    /// The revision this snapshot is a view of.
    #[must_use]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// The document bytes at this revision.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.text
    }

    /// The document as UTF-8 text, or `None` if the bytes are not valid UTF-8.
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

    /// Resolve an anchor as of this snapshot's revision (INV-QUERY-SNAPSHOT: a stable, frozen answer,
    /// unaffected by edits made after the snapshot was taken).
    #[must_use]
    pub fn resolve(&self, id: AnchorId) -> Option<Resolved> {
        self.anchors.resolve(id)
    }

    /// Every anchor frozen in this snapshot, ascending by offset.
    #[must_use]
    pub fn anchors(&self) -> &[(AnchorId, Resolved)] {
        self.anchors.all()
    }
}
