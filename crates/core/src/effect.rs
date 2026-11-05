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
    /// The command failed (Vim beeps: a motion that cannot move, `wx` on the last char). Vim then flushes
    /// the keys that were not typed — the rest of a macro, a `:normal` payload, fed keys — so `@a` stops at
    /// the failure (#1085). The workspace records it ([`crate::Workspace::take_beep`]); nothing else to do.
    Beep,
    /// The operator ran but its register cannot be written (`"/dw`, `":yy`, #1022), so it was refused; it
    /// always comes with a [`Effect::Beep`]. Apart from a failed motion (`"/dfz`, no `z`), which beeps
    /// before the operator runs: nvim saves the redo buffer for a refused operator, not for a failed motion,
    /// so the frontend keeps the refused one as the `.` change and drops the other (#1184). The workspace
    /// records it ([`crate::Workspace::take_refused`]).
    RegisterRefused,
}
