//! ruse-core — the editor kernel.
//!
//! This crate is the reference implementation of ruse's kernel contracts. The first vertical slice
//! implements the mutation spine end-to-end, **built against the design docs, not invented**:
//!
//! - [`pos`] — typed coordinates + [`pos::Revision`] (INV-POS-TYPED, INV-TXN).
//! - [`edit`] — the canonical [`edit::Edit`] and the disjoint, sorted [`edit::EditList`] (RFC-0007).
//! - [`anchor`] — the long-lived-position store: bias, span-delete policy, the §3 update rule and the
//!   §4 batched `O(A+E)` sweep (anchor-store.md / D-023 / INV-ANCHOR).
//! - [`transaction`] — [`transaction::Transaction`] carrying `base_revision` + origin (INV-ORIGIN).
//! - [`undo`] — the branching undo tree with a chronological index (persistence §7 / INV-UNDO).
//! - [`document`] — [`document::Document`]: atomic transaction apply, undo/redo, anchors (INV-TXN).
//! - [`snapshot`] — the immutable, revision-stamped [`snapshot::DocumentSnapshot`] (INV-QUERY-SNAPSHOT).
//!
//! Out of scope for this slice (owned by F-008 persistence): the on-disk journal, atomic save, and
//! crash recovery. Those extend this in-memory core; the contracts here are their substrate.

// D-041: a non-test `.unwrap()` is an unjustified panic — use `.expect("<invariant>")` (the message IS the
// invariant) or a typed `Result`. Tests are exempt via `allow-unwrap-in-tests` in clippy.toml.
#![deny(clippy::unwrap_used, clippy::disallowed_methods)]

pub mod anchor;
pub mod command;
pub mod document;
pub mod edit;
pub mod editor;
pub mod effect;
pub mod keymap;
pub mod motion;
pub mod pattern;
pub mod pos;
pub mod register;
pub mod search;
pub mod snapshot;
pub mod trace;
pub mod transaction;
pub mod undo;
pub mod workspace;

pub use anchor::{AnchorId, AnchorPolicy, AnchorStore, Bias, Resolved};
pub use command::{
    BlockInsertKind, Command, CommandParseError, ForcedWise, OpKind, SearchOp, SelectKind,
};
pub use document::{Document, DocumentId, TxnError};
pub use edit::{Edit, EditError, EditList};
pub use editor::{apply_command, commit, plan, EditorState, IndentStyle, Mode, Plan, View};
pub use effect::Effect;
pub use motion::Motion;
pub use pattern::{Magic, Match, Options as RegexOptions, Regex, RegexError};
pub use pos::{BytePos, CellCol, CharPos, GraphemePos, Revision};
pub use register::{RegKind, Register, RegisterStore};
pub use snapshot::{AnchorIndex, DocumentSnapshot};
pub use trace::{doc_hash, Trace, TraceError, TRACE_FORMAT_VERSION};
pub use transaction::{GroupHint, Transaction, TransactionOrigin};
pub use undo::{MonotonicSeq, UndoHistory};
pub use workspace::{Pane, SplitDir, ViewId, Window, Workspace};

/// Reject a transaction whose base revision is stale (INV-TXN, ENG-TXN-001). Retained as the kernel's
/// smallest invariant check; [`Document::apply`] enforces it as part of atomic apply.
#[must_use]
pub fn is_stale_revision(base: Revision, current: Revision) -> bool {
    base < current
}
