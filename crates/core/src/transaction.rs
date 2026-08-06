//! The Transaction — the only sanctioned way to mutate an editable Document (INV-TXN).

use crate::edit::EditList;
use crate::pos::Revision;

/// Who caused a mutation (**INV-ORIGIN**). Every transaction records exactly one; undo grouping and
/// post-hoc audit ("what did the AI change") key off it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransactionOrigin {
    /// A direct user gesture (typing, an operator).
    UserInput,
    /// Macro playback.
    Macro,
    /// A plugin-issued edit.
    Plugin,
    /// A language-server edit (format-on-save, code action, rename).
    Lsp,
    /// An AI-agent proposal (always reviewed before apply, INV-TRUST-1).
    AiAgent,
    /// An edit replayed from a remote peer.
    RemotePeer,
}

/// A proposed mutation: a disjoint edit set (INV-TXN) applied onto a specific `base_revision`.
///
/// Applying it either fully succeeds — advancing the document revision — or leaves the document byte-
/// for-byte untouched (F-001 atomicity). It is a *request*; the document validates the base revision
/// and bounds before it commits.
#[derive(Clone, Debug)]
pub struct Transaction {
    /// The revision this edit set was computed against; apply is refused if the document has moved on.
    pub base_revision: Revision,
    /// The normalized, disjoint edits to apply.
    pub edits: EditList,
    /// The cause of the mutation (INV-ORIGIN).
    pub origin: TransactionOrigin,
}

impl Transaction {
    /// Build a transaction over `base_revision`.
    #[must_use]
    pub fn new(base_revision: Revision, edits: EditList, origin: TransactionOrigin) -> Transaction {
        Transaction {
            base_revision,
            edits,
            origin,
        }
    }
}
