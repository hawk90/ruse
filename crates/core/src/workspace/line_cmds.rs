//! The Ex line-command facade: each method runs one `EditorState` line command on the focused
//! `(Document, View)` through the swap-trick.

use super::*;
use crate::editor::{GlobalCmd, LineAddr, SubFlags, SubOutcome, SubRange, Substitution};
use crate::pattern::RegexError;

impl Workspace {
    /// Run `:[range]s/pat/rep/flags` against the FOCUSED Window (the swap-trick, like [`Workspace::apply`]),
    /// applying every substitution as one undo group. Returns the count, or a [`RegexError`] (F-009 #2).
    pub fn substitute(
        &mut self,
        range: SubRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> Result<SubOutcome, RegexError> {
        self.with_focused(|st| st.substitute(range, pattern, replacement, flags))
    }

    /// Compute (do NOT apply) the substitutions `:s///c` would offer on the focused window — the
    /// interactive confirm loop presents these and applies the accepted subset with
    /// [`Workspace::apply_substitutions`] (F-009 #2, PR-c2).
    pub fn substitute_preview(
        &mut self,
        range: SubRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> Result<Vec<Substitution>, RegexError> {
        // `substitute_preview` reads only, but `EditorState` owns its parts and `Document` is not
        // `Clone`, so swap the focused (Document, View) out, compute, and swap them back unchanged
        // (like `apply`, but the document is never mutated). Runs once per `:s///c`, not per keystroke.
        self.with_focused(|st| st.substitute_preview(range, pattern, replacement, flags))
    }

    /// Count — WITHOUT editing — the matches `:[range]s/pat//n` (the `n` report-only flag) would act on
    /// the focused window (F-009 #2). Read-only (swap-trick like [`Workspace::substitute_preview`]): the
    /// buffer is unchanged and no undo entry is created. Returns the count or a [`RegexError`].
    pub fn substitute_count(
        &mut self,
        range: SubRange,
        pattern: &str,
        flags: SubFlags,
    ) -> Result<SubOutcome, RegexError> {
        self.with_focused(|st| st.substitute_count(range, pattern, flags))
    }

    /// Apply an accepted set of [`Substitution`]s to the focused window as one undo group (the tail of
    /// the `:s///c` confirm loop). Swap-trick like [`Workspace::apply`].
    pub fn apply_substitutions(&mut self, subs: &[Substitution]) -> SubOutcome {
        self.with_focused(|st| st.apply_substitutions(subs))
    }

    /// Run `:[range]g/pat/cmd` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]):
    /// two-pass mark-then-execute, one undo group. Returns the lines acted on (F-009 #4).
    pub fn global(
        &mut self,
        range: SubRange,
        pattern: &str,
        negate: bool,
        cmd: &GlobalCmd,
    ) -> Result<usize, RegexError> {
        self.with_focused(|st| st.global(range, pattern, negate, cmd))
    }

    /// PASS 1 of `:g` against the FOCUSED window: the 1-based line numbers the pattern marks (or, with
    /// `negate`, does NOT mark). The frontend's `:g/pat/normal` runner needs this mark set (computed against
    /// the untouched buffer) so it can replay the keys per marked line through the input engine — the core
    /// `d`/`s` payloads run inside [`Workspace::global`]. Returns 1-based numbers for the frontend's cursor
    /// placement helpers.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern is unrepresentable/malformed.
    pub fn global_marks(
        &mut self,
        range: SubRange,
        pattern: &str,
        negate: bool,
    ) -> Result<Vec<usize>, RegexError> {
        // The swap-trick (like [`Workspace::global`]): move the focused parts into an [`EditorState`] to
        // reuse its single marking rule, then swap them back untouched (marking is read-only).
        let result = self.with_focused(|st| st.global_marks(range, pattern, negate));
        Ok(result?.into_iter().map(|li| li + 1).collect())
    }

    /// Run `:[range]d` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]): delete the
    /// range's lines as one undo group. Returns the number of lines deleted.
    pub fn delete_lines(&mut self, range: SubRange) -> usize {
        self.delete_lines_into(range, None)
    }

    /// [`Workspace::delete_lines`] into register `reg` (`None` = the unnamed one), for `:d {x}` / `:y {x}`.
    pub fn delete_lines_into(&mut self, range: SubRange, reg: Option<char>) -> usize {
        self.with_focused(|st| st.delete_lines_into(range, reg))
    }

    /// Apply a disjoint `(start, end, text)` batch edit to the FOCUSED window (the swap-trick, like
    /// [`Workspace::apply`]) as one undo group, tagged with the caller-supplied `origin` — a
    /// provenance-agnostic primitive (the LSP frontend passes [`TransactionOrigin::Lsp`]). See
    /// [`EditorState::apply_edits`]: all or nothing, `false` when refused.
    pub fn apply_edits(
        &mut self,
        edits: &[(BytePos, BytePos, String)],
        origin: TransactionOrigin,
    ) -> bool {
        self.with_focused(|st| st.apply_edits(edits, origin))
    }

    /// Run `:[range]y` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]): yank the
    /// range's lines linewise into the unnamed register (and `"0`). Returns the number of lines yanked.
    pub fn yank_lines(&mut self, range: SubRange) -> usize {
        self.yank_lines_into(range, None)
    }

    /// [`Workspace::yank_lines`] into register `reg` (`None` = the unnamed one), for `:d {x}` / `:y {x}`.
    pub fn yank_lines_into(&mut self, range: SubRange, reg: Option<char>) -> usize {
        self.with_focused(|st| st.yank_lines_into(range, reg))
    }

    /// Run `:[range]j[oin][!]` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]): join
    /// the range's lines, reusing the same core join `J`/`gJ` emit. `no_space` is the `!` (raw `gJ`) form.
    /// Returns the number of lines joined.
    pub fn join_lines(&mut self, range: SubRange, no_space: bool) -> usize {
        self.with_focused(|st| st.join_lines(range, no_space))
    }

    /// Run `:[range]>` / `:[range]<` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]):
    /// shift the range's lines `levels` indent levels right (`left = false`) or left (`left = true`), reusing
    /// the core `>>`/`<<` shift. Returns the number of lines in the range.
    pub fn shift_lines(&mut self, range: SubRange, left: bool, levels: u32) -> usize {
        self.with_focused(|st| st.shift_lines(range, left, levels))
    }

    /// Run `:[range]m {addr}` against the FOCUSED window (swap-trick): move the range's lines to after the
    /// destination. Returns the lines moved, or `None` if the destination is inside the source.
    pub fn move_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        self.with_focused(|st| st.move_lines(range, dest))
    }

    /// Run `:[range]t {addr}` / `:copy` against the FOCUSED window (swap-trick): copy the range's lines to
    /// after the destination. Returns the lines copied.
    pub fn copy_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        self.with_focused(|st| st.copy_lines(range, dest))
    }

    /// Run `:[line]put [reg]` against the FOCUSED window (swap-trick): put the register's text LINEWISE as
    /// new whole line(s) after the addressed line. Returns the number of lines inserted. `:put +`/`:put *`
    /// read the system clipboard, so — like [`Workspace::apply`] — pull the external clipboard into the
    /// mirror slot first (the orchestration boundary; the pure core never does IO) so the put reflects what
    /// another app copied. A put only READS a register, so nothing is pushed back out.
    pub fn put_lines(&mut self, addr: LineAddr, reg: Option<char>) -> usize {
        let vid = self.windows[self.focus].view;
        let (n, pulled) = self.with_view(vid, |st, clipboard| {
            let pulled = if crate::register::RegisterStore::is_clipboard(reg) {
                pull_clipboard(st, clipboard)
            } else {
                None
            };
            (st.put_lines(addr, reg), pulled)
        });
        self.note_clipboard_read(pulled);
        n
    }

    /// Run `:[addr]r {file}` / `:[addr]r !{cmd}` against the FOCUSED window (swap-trick): insert `text` (a
    /// file's contents or a command's stdout, read by the frontend) as new line(s) below the addressed line.
    /// `cursor_on_last` picks the cursor's landing line (last inserted for a command read, first otherwise —
    /// nvim's quirk). Returns the number of lines inserted.
    pub fn read_lines(&mut self, addr: LineAddr, text: &[u8], cursor_on_last: bool) -> usize {
        self.with_focused(|st| st.read_lines(addr, text, cursor_on_last))
    }

    /// The text of the FOCUSED window's lines in `range` (each with a trailing `\n`), as the frontend feeds a
    /// `:{range}!{cmd}` filter's stdin, as raw bytes. `None` for an empty buffer. Read-only in effect, but
    /// takes `&mut self` to reuse the swap-trick that reconstitutes the [`EditorState`] (which owns the
    /// resolver).
    pub fn range_text(&mut self, range: SubRange) -> Option<Vec<u8>> {
        self.with_focused(|st| st.range_text(range))
    }

    /// Run `:{range}!{cmd}` against the FOCUSED window (swap-trick): replace the range's lines with `text`
    /// (the filter command's stdout, captured by the frontend). Returns the number of input lines filtered.
    pub fn filter_lines(&mut self, range: SubRange, text: &[u8]) -> usize {
        self.with_focused(|st| st.filter_lines(range, text))
    }

    /// Run `:[range]retab[!] [N]` against the FOCUSED window (swap-trick; see [`EditorState::retab`]).
    /// Returns whether any line changed.
    pub fn retab(
        &mut self,
        range: SubRange,
        bang: bool,
        new_tabstop: Option<crate::cells::VarStops>,
    ) -> bool {
        self.with_focused(|st| st.retab(range, bang, new_tabstop))
    }

    /// Run `:[range]sort` against the FOCUSED window (swap-trick): sort the range's lines as one undo
    /// group. Returns the number of lines removed by the `unique` flag.
    ///
    /// # Errors
    /// [`RegexError`] if the sort pattern does not compile; the buffer is left as it was.
    pub fn sort_lines(
        &mut self,
        range: SubRange,
        opts: &crate::SortOptions,
    ) -> Result<usize, RegexError> {
        self.with_focused(|st| st.sort_lines(range, opts))
    }

    /// Run one resolved Ex edit ([`crate::ExCmd`], RFC-0020) against the FOCUSED window: the swap-trick, the
    /// `"+`/`"*` pull a `:put +` needs (as [`Workspace::put_lines`]), and a `Set` that also writes the
    /// global value (as [`Workspace::set_option`]). The frontend records the command and runs it here; a
    /// trace replays it through `apply_command`, which calls the same [`EditorState::apply_ex`].
    pub fn apply_ex(&mut self, ex: &crate::ExCmd) -> crate::ExOutcome {
        match ex {
            crate::ExCmd::Set(opt) => {
                self.set_option(opt.clone());
                return crate::ExOutcome::Done;
            }
            // `:keepjumps` reaches every view, as [`Workspace::set_keep_jumps`] does.
            crate::ExCmd::KeepJumps(on) => {
                self.set_keep_jumps(*on);
                return crate::ExOutcome::Done;
            }
            _ => {}
        }
        let vid = self.windows[self.focus].view;
        let (out, pulled) = self.with_view(vid, |st, clipboard| {
            let pulled = match ex {
                crate::ExCmd::Put { reg, .. }
                    if crate::register::RegisterStore::is_clipboard(*reg) =>
                {
                    pull_clipboard(st, clipboard)
                }
                _ => None,
            };
            (st.apply_ex(ex), pulled)
        });
        self.note_clipboard_read(pulled);
        self.not_modifiable |= out == crate::ExOutcome::NotModifiable;
        out
    }

    /// The 1-based inclusive lines `range` names in the FOCUSED window, resolved exactly as the Ex line
    /// operations resolve it (no range = the cursor's line; both ends clamped into the buffer), so an
    /// [`crate::ExCmd`] built from them runs on the same lines.
    #[must_use]
    pub fn resolve_range(&self, range: SubRange) -> (usize, usize) {
        let p = self.focused();
        let cursor_line = p.doc.line_of(p.view.cursor());
        let (first, last) =
            crate::editor::resolve_lines(range, crate::editor::line_count(p.doc), cursor_line);
        (first + 1, last + 1)
    }

    /// The insert index `addr` names in the FOCUSED window ("after line N", `0..=lines`), as `:m`/`:t`/
    /// `:put`/`:r` resolve it, or `None` for a relative address off the buffer (the command then does
    /// nothing).
    #[must_use]
    pub fn resolve_addr(&self, addr: LineAddr) -> Option<usize> {
        let p = self.focused();
        // The emptied buffer has its one line (Vim's `ML_EMPTY`: `line('$')` is 1), so `:put` goes after it
        // and `:0put` before it (RFC-0024 §5).
        let nlines = crate::editor::line_count(p.doc).max(1);
        addr.insert_index(p.doc.line_of(p.view.cursor()), nlines)
    }
}
