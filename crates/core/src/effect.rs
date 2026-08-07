//! Side effects a command requests but the pure core never performs (RFC-0012 plan/commit split).
//!
//! `editor-core` is IO-free: [`crate::editor::commit`] returns `Effect`s for the frontend to carry out, so
//! replaying a command sequence is deterministic and never touches the filesystem. This is the discipline
//! that captures most of what a Haskell rewrite would buy, enforced by an empty dependency set.

/// A side effect the frontend performs on the core's behalf.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Persist the current document (a `:w`). The frontend owns the file path; it writes the buffer bytes
    /// and, on success, calls [`crate::Document::mark_saved`]. Emitted only as a *request*.
    Save,
    /// Quit the editor.
    Quit,
    /// A transient status-line message (errors, `:w` confirmations).
    Status(String),
}
