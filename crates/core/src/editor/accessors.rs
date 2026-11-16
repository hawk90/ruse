//! The `EditorState` API around the plan / commit pipeline: the option setters, the register access the
//! Workspace lends through, and the read-only queries the frontend paints from (cursor, mode, selection,
//! marks, jumps, changes). Moved out of `editor/mod.rs` (#1495); a child module, so it reads the private
//! `View` fields as before.

use super::{planner, Want, WantKind};
use crate::motion::snap;
use crate::{
    BytePos, CaretGravity, Command, EditorOption, EditorState, IndentStyle, Mode, Register,
    RegisterStore,
};

impl EditorState {
    /// Set the indentation config the shift operators (`>>`/`<<`) use. Runtime config loading is deferred;
    /// until it lands this is the seam a loader (or a test) uses to install `editor.tab_width` /
    /// `editor.indent_style`. Sets the tabstop AND the shiftwidth to `tab_width` (the common `ts=sw` case);
    /// `:set shiftwidth`/`:set tabstop` ([`EditorState::set_option`]) set them apart.
    pub fn set_indent(&mut self, tab_width: usize, indent_style: IndentStyle) {
        self.view
            .update_options(|o| o.set_indent(tab_width, indent_style));
    }

    /// Set the search case config (`'ignorecase'` / `'smartcase'`, F-009 #1). The seam a config loader
    /// (or a test) uses until runtime config lands; the default is Vim's factory case-sensitive search.
    pub fn set_search_case(&mut self, ignore_case: bool, smart_case: bool) {
        self.view
            .update_options(|o| o.set_search_case(ignore_case, smart_case));
    }

    /// Set one editor option (`:set`) on the focused view, leaving the others untouched. Maps the
    /// runtime `:set` surface onto the existing indent/search-case config (no new schema keys).
    pub fn set_option(&mut self, opt: EditorOption) {
        self.view.update_options(|o| o.set(opt));
    }

    /// The leading whitespace for an indent of `level` shiftwidths, in the configured style (spaces, or
    /// tabs at the tabstop plus a spaces remainder) — Vim's `set_indent(level * sw)`.
    pub(super) fn indent_for_level(&self, level: usize) -> Vec<u8> {
        self.view
            .indent
            .whitespace(level * self.view.indent.shift_width())
    }

    /// The effective shiftwidth in columns (see [`View::shift_width`]).
    pub(super) fn shift_width(&self) -> usize {
        self.view.indent.shift_width()
    }

    /// The unnamed register's current contents (for tests / a future `:registers`).
    #[must_use]
    pub fn register(&self) -> &Register {
        self.view.registers.unnamed()
    }

    /// The whole register store (for tests / a future `:registers`), giving access to the named slots.
    #[must_use]
    pub fn registers(&self) -> &RegisterStore {
        &self.view.registers
    }

    /// The 0-based inclusive line range the operator span of `cmd` covers — a search, `n`/`N`, `*`/`#`, a
    /// `gn` match or a mark — or `None` when it has no target or is not a span command. Read-only; the
    /// command's operator is ignored. The `!` filter numbers its `:{range}!` line from this (#800).
    #[must_use]
    pub fn span_lines(&self, cmd: &Command) -> Option<(usize, usize)> {
        planner::span_lines(self, cmd)
    }

    /// The one-shot register selected by the most recent `"x` and not yet consumed. The
    /// [`Workspace`](crate::Workspace) reads this before applying a command to decide whether that command
    /// touches the system clipboard (`"+`/`"*`).
    #[must_use]
    pub fn pending_register(&self) -> Option<char> {
        self.view.pending_register
    }

    /// Refresh the `"+`/`"*` clipboard mirror slot from external OS-clipboard bytes ahead of a paste, so
    /// `"+p` reflects whatever another application copied. Preserves the in-session paste geometry when the
    /// bytes are unchanged (see [`RegisterStore::set_clipboard_from_external`]).
    pub fn sync_clipboard_in(&mut self, bytes: Vec<u8>) {
        self.view.registers.set_clipboard_from_external(bytes);
    }

    /// Sync the four read-only special registers `"/ ": ". "%` from frontend state ahead of a
    /// register-reading command, so `"/p` / `C-r :` / `".p` / `"%p` resolve to the live values (see
    /// [`RegisterStore::set_special`](crate::register::RegisterStore::set_special)).
    pub fn set_special_registers(
        &mut self,
        search: Option<String>,
        last_ex: Option<String>,
        inserted: Option<String>,
        file: Option<String>,
    ) {
        self.view
            .registers
            .set_special(search, last_ex, inserted, file);
    }

    /// Write raw bytes into a named register as a CHARWISE entry (D-055 macro recording): the frontend
    /// stores a recorded keystroke stream into `"{name}`, sharing the same a-z slots as yank/paste so a
    /// macro pastes as text and yanked text runs as a macro. `name` is `Some('a'..='z')`; the byte content
    /// is opaque to the core (it is a key stream, not display text).
    pub fn set_register_raw(&mut self, name: Option<char>, bytes: Vec<u8>) {
        self.view
            .registers
            .set_macro(name, crate::register::Register::charwise(bytes));
    }

    /// The highlighted byte range `[start, end)` of the current charwise/linewise Visual selection, or
    /// `None` in Normal/Insert **and for a blockwise selection** (a rectangle is not one contiguous range —
    /// use [`EditorState::block_spans`] to paint that). Charwise includes the character under the active
    /// end; linewise spans whole lines. For the frontend to paint the selection.
    #[must_use]
    pub fn selection_span(&self) -> Option<(BytePos, BytePos)> {
        // Visual and Select paint the same selection (they share the anchor and toggle via CTRL-G).
        self.view.selection_span(self.bytes())
    }

    /// The per-row highlighted byte ranges of the current BLOCKWISE selection (one `[start, end)` per line
    /// the rectangle crosses; short lines contribute an empty range at their end), or `None` when the
    /// selection is not blockwise. For the frontend to paint a column-aligned block.
    #[must_use]
    pub fn block_spans(&self) -> Option<Vec<(BytePos, BytePos)>> {
        self.view.block_spans(self.bytes())
    }

    /// The document bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.doc.bytes()
    }

    /// The document as UTF-8, or `None` if not valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        self.doc.as_str()
    }

    /// The cursor's byte offset (on a char boundary).
    #[must_use]
    pub fn cursor(&self) -> BytePos {
        BytePos(self.view.cursor)
    }

    /// The jumplist positions (oldest → newest) for `:jumps`.
    #[must_use]
    pub fn jumps_snapshot(&self) -> Vec<BytePos> {
        self.view.jumps.iter().copied().map(BytePos).collect()
    }

    /// The change-list positions (oldest → newest) for `:changes`.
    #[must_use]
    pub fn changes_snapshot(&self) -> Vec<BytePos> {
        self.view.changes.iter().copied().map(BytePos).collect()
    }

    /// The SET marks for `:marks` — the named marks `a`–`z`, the `.` last-change mark, and the `^`
    /// last-insert mark — as `(name, byte offset)`, in that order (unset marks omitted).
    #[must_use]
    pub fn marks_snapshot(&self) -> Vec<(char, BytePos)> {
        let mut out = Vec::new();
        for c in ('a'..='z').chain('A'..='Z').chain('0'..='9') {
            if let Some(p) = self.view.named_mark(c) {
                out.push((c, p));
            }
        }
        if let Some(p) = self.view.last_change() {
            out.push(('.', p));
        }
        if let Some(p) = self.view.last_insert() {
            out.push(('^', BytePos(p)));
        }
        out
    }

    /// The mark's byte offset, or `None` when no mark is set. The mark is the Emacs profile's other end of
    /// the region (`set-mark-command` and the region ops); one-caret degenerate is `None`. View-local. For
    /// the Emacs parity comparator and a future `:marks`.
    #[must_use]
    pub fn mark(&self) -> Option<BytePos> {
        self.view.mark.map(BytePos)
    }

    /// Place the cursor at byte offset `pos` (on a char boundary). View-local; does not touch the document.
    /// Used to home a fixture's starting point in the parity comparator (mirrors the frontend seam).
    ///
    /// Placing the caret also SEEDS the sticky desired column (`curswant`) to `pos`'s column, exactly as a
    /// committed motion would: a subsequent `j`/`k` (`next-line`/`previous-line`) aims at the column the
    /// caret was placed on, not column 0. Without this a teleport (goto, mouse, a fixture's start point)
    /// would leave `curswant` stale at 0 and vertical moves would snap to the line start.
    pub fn set_cursor(&mut self, pos: BytePos) {
        let pos = pos.0;
        let b = self.doc.bytes();
        // Clamp to the buffer and snap to a char boundary: `set_cursor` is a public seam (tests, a
        // future goto/mouse), so an out-of-range or mid-codepoint `pos` must not leave the cursor where
        // a later `col_of`/slice would panic. Vim never overshoots, so this is a no-op on valid input.
        let pos = snap(b, pos.min(b.len()));
        self.view.cursor = pos;
        self.view.curswant = self.wanted_col();
    }

    /// The wanted column ([`View::curswant`]) the caret sets where it stands now (#1090), pending: it is
    /// measured when first read ([`Want`], #1231).
    pub(super) fn wanted_col(&self) -> Want {
        let tab_end = match self.view.mode {
            Mode::Normal => self.view.caret == CaretGravity::OnChar,
            Mode::Visual { .. } => self.view.anchor.is_none_or(|a| self.view.cursor >= a),
            _ => false,
        };
        Want(WantKind::At {
            text: self.doc.text_arc(),
            pos: self.view.cursor,
            ts: self.view.indent.tabs(),
            tab_end,
            col: std::sync::OnceLock::new(),
        })
    }

    /// This View's caret gravity (D-050): `OnChar` (Vim/Neovim) or `BetweenChar` (Emacs profile).
    #[must_use]
    pub fn caret_gravity(&self) -> CaretGravity {
        self.view.caret
    }

    /// Select this View's caret gravity (D-050 / RFC-0015). The Emacs profile sets `BetweenChar` so its
    /// edits are not Vim-clamped; Vim/Neovim leave the `OnChar` default. Profile-init seam.
    pub fn set_caret_gravity(&mut self, gravity: CaretGravity) {
        self.view.caret = gravity;
    }

    /// The current mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.view.mode
    }

    /// Whether the buffer differs from what is on disk (delegates to the document's node-identity check).
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.doc.is_modified()
    }
}
