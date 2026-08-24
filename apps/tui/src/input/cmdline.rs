//! The command-line namespace's owned state (F-026). Split out of `input/mod.rs` as a self-contained
//! data definition; the `InputEngine` methods that drive it (`open_cmdline`, `open_minibuffer`,
//! `cmdline`, `feed_cmdline`, `submit_search`) stay in the engine core.

/// The command-line namespace's owned state (F-026 acceptance #2): a prefix, a line buffer, and a
/// cursor. `ex_mode` distinguishes the `gQ` Ex namespace (stays open, re-prompting after each `<CR>`)
/// from a one-shot `:`/`/` line. History index / wildmenu / incsearch UX are deferred (acceptance #3).
pub(crate) struct CmdLine {
    /// `:` (ex) or `/` (search) — also the glyph the status line shows.
    pub(crate) prefix: char,
    /// The text typed so far. Owned HERE, never on the frontend.
    pub(crate) buffer: String,
    /// Insertion point as a char index. MVP edits append/backspace at the end (mid-line editing is the
    /// deferred full line-editor); the field exists because the namespace owns the cursor (acceptance #2).
    pub(crate) cursor: usize,
    /// `gQ` Ex mode: `<CR>` executes AND re-opens the line; `:visual`/`:vi`/empty exits.
    pub(crate) ex_mode: bool,
    /// Emacs `M-x` minibuffer (F-012): the buffer holds a COMMAND NAME (not an ex line), resolved on `<CR>`
    /// against the command registry into a [`ruse_core::Command`]. `false` for the Vim `:`/`/` line.
    pub(crate) mx: bool,
    /// Expression-register prompt (`:help quote=` / `:help i_CTRL-R`): when `Some`, the buffer holds an
    /// EXPRESSION (not an ex line or search pattern), and `<CR>` hands it to the evaluator. The target says
    /// what to do with the result — arm the `"=` register for the next paste, or insert it at the caret.
    /// `None` for the ordinary `:`/`/` line and the `M-x` minibuffer.
    pub(crate) expr: Option<ExprTarget>,
}

/// What a completed expression-register prompt does with the evaluated result (`:help quote=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExprTarget {
    /// Normal-mode `"=<expr><CR>`: arm the `"=` register so the FOLLOWING `p`/`P` pastes the result
    /// ([`ruse_core::Command::SetExprRegister`]).
    Paste,
    /// Insert-mode `<C-r>=<expr><CR>`: splice the result at the caret right now
    /// ([`ruse_core::Command::InsertEval`]).
    Insert,
}
