//! The **Workspace** — the top-level container that owns many buffers ([`Document`]s) and the
//! [`View`]s / [`Window`]s onto them (F-007). It is the headless counterpart of the TUI's screen: the
//! frontend drives a `Workspace`, not a single [`EditorState`], so `:split`/`:vsplit` can show two
//! Windows onto ONE Document with independent cursors and scroll (F-007 acceptance #1), and closing a
//! Window retires its View while the shared Document survives as long as another View holds it (#3).
//!
//! **Ownership (arena + handles, not `Rc<RefCell>`).** The Workspace owns Documents and Views in
//! arenas keyed by [`DocumentId`] / [`ViewId`]; a [`View`] only NAMES its Document by id, never a
//! borrow, so "same buffer in N Views" is N Views naming one id — no interior mutability, no reference
//! cycles (INV-DOC-VIEW / INV-HANDLE). A [`Window`] is a distinct layout slot naming a View, so
//! Buffer ≠ View ≠ Window (#4). Retired slots become `None` holes; generational handles and slot
//! compaction are post-MVP (a stale id resolves to a `None` hole — an assert, not silent aliasing).
//!
//! **Editing (the swap-trick).** To run a command, the focused `(Document, View)` is swapped OUT of
//! its arena slots into an [`EditorState`], the UNCHANGED `plan`/`commit`/`apply_command` pipeline
//! runs, and the pair is swapped back. Nothing else can reference the Document during that window (one
//! command at a time, single-threaded; other Views hold only its id), so the move is sound and the
//! single-window path stays byte-identical to F-007 step (a).
//!
//! **Layout (flat, MVP).** The MVP layout is a flat list of Windows with one split direction
//! ([`SplitDir`]) and a focus index — enough for equal `:split`/`:vsplit` into 2+ panes, `C-w w`
//! (focus next), `C-w c` (close focused). The full recursive layout tree, tab pages, resize
//! constraints, and the rest of the `C-w` family are deferred (docs/design/view-window-workspace.md).

use crate::clipboard::{Clipboard, NoClipboard};
use crate::command::Command;
use crate::document::{Document, DocumentId};
use crate::editor::{
    apply_command, CaretGravity, EditorState, GlobalCmd, LineAddr, SubFlags, SubOutcome, SubRange,
    Substitution, View,
};
use crate::effect::Effect;
use crate::pattern::RegexError;
use crate::transaction::TransactionOrigin;

/// A handle to a [`View`] in the workspace arena (INV-HANDLE). Distinct from [`DocumentId`] (a buffer)
/// so the type system keeps Buffer ≠ View (F-007 acceptance #4). The inner index is the arena slot;
/// generational validation is post-MVP (a retired slot is a `None` hole).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ViewId(pub usize);

/// How the flat MVP layout tiles its Windows: `Horizontal` stacks them top-to-bottom (`:split`),
/// `Vertical` places them left-to-right (`:vsplit`). One direction for the whole flat list (no tree).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDir {
    /// `:split` — panes stacked top to bottom, each a full-width horizontal band.
    Horizontal,
    /// `:vsplit` — panes side by side, each a full-height vertical column.
    Vertical,
}

/// One layout slot: names the [`View`] it displays. Its geometry (the on-screen sub-rectangle) is not
/// stored — the frontend computes it from the window's index, the window count, and the [`SplitDir`]
/// (MVP is equal splits). A `Window` is a DISTINCT type from a `View` and a `Document`, so tabs are
/// window layouts, never buffers-as-tabs (F-007 acceptance #4).
#[derive(Clone, Copy, Debug)]
pub struct Window {
    view: ViewId,
}

/// A read-only view of one Window for rendering / viewport math: whether it is focused, plus borrows
/// of its [`View`] (cursor, mode, scroll, selection) and shared [`Document`] (text). The frontend
/// paints each `Pane` into its sub-rectangle of the F-006 cell grid.
pub struct Pane<'a> {
    /// Whether this pane holds the input focus (owns the terminal cursor, drives the status line).
    pub focused: bool,
    /// The pane's per-View state (cursor, mode, scroll top, selection) — view-local.
    pub view: &'a View,
    /// The buffer this pane shows — shared: two panes of one split borrow the same `Document`.
    pub doc: &'a Document,
}

/// One row of the buffer list (`:ls`) — a buffer's id, display name, and its status flags, for the
/// status line and the buffer picker. `[No Name]` stands in for an unnamed/scratch buffer.
#[derive(Clone, Debug)]
pub struct BufferInfo {
    /// The buffer's stable id.
    pub id: DocumentId,
    /// Display name (`[No Name]` when the buffer has none).
    pub name: String,
    /// Whether this buffer is the one the focused window currently shows (`%` in `:ls`).
    pub current: bool,
    /// Whether this is the alternate buffer (`#` in `:ls`, the `:b#` target).
    pub alt: bool,
    /// Whether the buffer has unsaved edits (`+` in `:ls`).
    pub modified: bool,
}

/// The container the frontend drives (F-007). See the module docs for ownership / editing / layout.
pub struct Workspace {
    /// Buffer arena; slot `i` holds `DocumentId(i as u64 + 1)`. `None` = a retired buffer.
    docs: Vec<Option<Document>>,
    /// View arena; slot `i` holds [`ViewId`]`(i)`. `None` = a retired view.
    views: Vec<Option<View>>,
    /// The flat MVP layout — the Windows in tile order.
    windows: Vec<Window>,
    /// How the flat window list tiles (last `:split`/`:vsplit` wins for the whole list, MVP).
    split: SplitDir,
    /// Index into [`Workspace::windows`] of the focused Window.
    focus: usize,
    /// Per-buffer display name, parallel to `docs` (`None` = an unnamed/scratch buffer, shown as
    /// `[No Name]`; `None` slot = a retired buffer). The name is a display label, not the on-disk path —
    /// the frontend owns the path/format (this crate is IO-free).
    names: Vec<Option<String>>,
    /// The buffer list in `:ls`/`:bnext` order (the Listed buffers, view-window-workspace.md §8.1). A
    /// buffer joins on creation and leaves when retired; the order is stable so `:bn`/`:bp` are predictable.
    buffer_order: Vec<DocumentId>,
    /// The alternate buffer (Vim `#`): the buffer focus last left, so `:b#` toggles back. `None` until
    /// the first switch.
    alt: Option<DocumentId>,
    /// The workspace's DEFAULT search-case (`'ignorecase'`/`'smartcase'`), applied to every View it creates
    /// so a shipped-profile default (which the app turns ON) reaches new buffers and splits, not just the
    /// first one. The engine/`EditorState::new` default stays vanilla-off (the differential parity oracle
    /// drives `EditorState` directly), so this Workspace-level seam is where ruse's better-than-Vim default
    /// lives without the oracle ever seeing it. `(false, false)` = Vim factory until the frontend sets it.
    default_ignore_case: bool,
    default_smart_case: bool,
    /// The injected OS-clipboard provider behind the `"+`/`"*` registers (`:help quoteplus`). Defaults to
    /// [`NoClipboard`] (a no-op) so core stays pure and CI — which has no clipboard — is deterministic; the
    /// frontend installs a real shell-out provider at startup via [`Workspace::set_clipboard`], and unit
    /// tests inject [`MemClipboard`](crate::clipboard::MemClipboard).
    clipboard: Box<dyn Clipboard>,
}

impl Workspace {
    /// A fresh workspace over `initial` bytes: one buffer, one View, one Window, focused. Reuses
    /// [`EditorState::new`]'s initialisation (mark-saved + a fresh View over `DocumentId(1)`) so the
    /// single-window buffer is identical to the pre-Workspace path.
    #[must_use]
    pub fn new(initial: impl Into<Vec<u8>>) -> Workspace {
        let (doc, view) = EditorState::new(initial).into_parts();
        let id = doc.id();
        Workspace {
            docs: vec![Some(doc)],
            views: vec![Some(view)],
            windows: vec![Window { view: ViewId(0) }],
            split: SplitDir::Horizontal,
            focus: 0,
            names: vec![None],
            buffer_order: vec![id],
            alt: None,
            default_ignore_case: false,
            default_smart_case: false,
            clipboard: Box::new(NoClipboard),
        }
    }

    /// Install the OS-clipboard provider backing `"+`/`"*` (`:help quoteplus`). The frontend calls this once
    /// at startup with a real shell-out provider; tests inject an in-memory double. Without it the workspace
    /// keeps the [`NoClipboard`] default, so `"+p` is a graceful no-op and `"+y` silently drops.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    /// Set the workspace default search-case and apply it to EVERY live view (existing and, via
    /// [`Self::apply_default_search_case`], any created afterward). The frontend calls this once at startup
    /// to install ruse's shipped default (ignorecase + smartcase ON) so it holds across `:e`/`:split`/reload
    /// — without touching `EditorState::new`, which the parity oracle drives at the Vim factory default.
    pub fn set_default_search_case(&mut self, ignore_case: bool, smart_case: bool) {
        self.default_ignore_case = ignore_case;
        self.default_smart_case = smart_case;
        for vid in 0..self.views.len() {
            if self.views[vid].is_some() {
                self.apply_default_search_case(ViewId(vid));
            }
        }
    }

    /// Apply the stored default search-case to one view (the [`EditorState::from_parts`] round-trip the other
    /// per-view mutations use). Called for every newly created view so the default propagates to new buffers.
    fn apply_default_search_case(&mut self, vid: ViewId) {
        let view = self.views[vid.0].take().expect("view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("doc live");
        let mut st = EditorState::from_parts(doc, view);
        st.set_search_case(self.default_ignore_case, self.default_smart_case);
        let (doc, view) = st.into_parts();
        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
    }

    /// The arena slot index for a `DocumentId` (ids are 1-based, assigned monotonically, never reused).
    fn doc_slot(id: DocumentId) -> usize {
        id.0 as usize - 1
    }

    /// Number of Windows currently in the layout.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// The current split direction of the flat layout.
    #[must_use]
    pub fn split_dir(&self) -> SplitDir {
        self.split
    }

    /// The focused Window's index (into the tile order).
    #[must_use]
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Borrow the `i`th Window as a [`Pane`] for rendering / viewport math. Panics on a bad index or a
    /// retired slot (an INV-HANDLE assert: the layout never names a dead View/Document).
    #[must_use]
    pub fn pane(&self, i: usize) -> Pane<'_> {
        let win = self.windows[i];
        let view = self.views[win.view.0]
            .as_ref()
            .expect("window names a live view");
        let doc = self.docs[Self::doc_slot(view.doc())]
            .as_ref()
            .expect("view names a live document");
        Pane {
            focused: i == self.focus,
            view,
            doc,
        }
    }

    /// The focused Window as a [`Pane`] — the buffer/view the status line and cursor track.
    #[must_use]
    pub fn focused(&self) -> Pane<'_> {
        self.pane(self.focus)
    }

    /// Place the FOCUSED view's cursor at byte offset `pos` (the frontend uses this to scroll a
    /// `:s///c` match into view during the confirm loop). Clamped to the buffer.
    pub fn place_focused_cursor(&mut self, pos: usize) {
        let vid = self.windows[self.focus].view;
        let did = self.views[vid.0].as_ref().expect("focused view live").doc();
        let len = self.docs[Self::doc_slot(did)]
            .as_ref()
            .expect("focused doc live")
            .bytes()
            .len();
        if let Some(v) = self.views[vid.0].as_mut() {
            v.set_cursor(pos.min(len));
        }
    }

    /// Select the caret gravity for every live View (D-050 / RFC-0015): the frontend calls this once at
    /// startup with `BetweenChar` when the Emacs profile is active, so its edits rest on Emacs point rather
    /// than being Vim-clamped. Splits clone the focused View, so a later `:split` inherits the gravity.
    pub fn set_caret_gravity(&mut self, gravity: CaretGravity) {
        for v in self.views.iter_mut().flatten() {
            v.set_caret_gravity(gravity);
        }
    }

    /// Set the `i`th Window's scroll position (the frontend viewport pass, after computing it from the
    /// pane's cursor and its sub-rectangle height).
    pub fn set_top(&mut self, i: usize, top: usize) {
        let vid = self.windows[i].view;
        if let Some(v) = self.views[vid.0].as_mut() {
            v.set_top(top);
        }
    }

    /// The focused buffer, mutably — for a durable save to `mark_saved()` after writing to disk.
    pub fn focused_doc_mut(&mut self) -> &mut Document {
        let vid = self.windows[self.focus].view;
        let did = self.views[vid.0].as_ref().expect("focused view live").doc();
        self.docs[Self::doc_slot(did)]
            .as_mut()
            .expect("focused doc live")
    }

    /// Run a command against the FOCUSED Window (the swap-trick — see the module docs). Swaps the
    /// focused `(Document, View)` out of their arena slots into an [`EditorState`], runs the unchanged
    /// `apply_command` pipeline, swaps them back, and returns the [`Effect`]s for the frontend.
    pub fn apply(&mut self, cmd: &Command) -> Vec<Effect> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let did = view.doc();
        let slot = Self::doc_slot(did);
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);

        // `"+`/`"*` (`:help quoteplus`): this command touches the system clipboard when the one-shot pending
        // register is `+`/`*` (a `"+y`/`"+p`/`"+d`), or it is an `i_CTRL-R` insert directly from `+`/`*`. The
        // OS clipboard is an impure side effect, so it is synced HERE (the orchestration boundary), never in
        // the pure planner: PULL the external clipboard into the mirror slot before the command so a paste
        // reflects what another app copied, then PUSH the mirror slot out after so a yank/delete propagates.
        let touches_clipboard = crate::register::RegisterStore::is_clipboard(st.pending_register())
            || matches!(
                cmd,
                Command::InsertRegister('+') | Command::InsertRegister('*')
            );
        if touches_clipboard {
            if let Some(text) = self.clipboard.get() {
                st.sync_clipboard_in(text.into_bytes());
            }
        }

        let effects = apply_command(&mut st, cmd);

        if touches_clipboard {
            // The mirror slot now holds the yanked/deleted bytes (a paste leaves it unchanged, so the push is
            // idempotent). `String::from_utf8_lossy` keeps a non-UTF-8 buffer from ever panicking the write.
            let bytes = st.registers().clipboard().text().to_vec();
            self.clipboard.set(&String::from_utf8_lossy(&bytes));
        }

        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        effects
    }

    /// Store raw macro bytes into a named register of the FOCUSED view (D-055; the swap-trick). Shares the
    /// a-z slots with yank/paste, so a recorded macro pastes as text and yanked text runs as a macro.
    pub fn set_register_raw(&mut self, name: Option<char>, bytes: Vec<u8>) {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        st.set_register_raw(name, bytes);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
    }

    /// The raw bytes of a named register of the FOCUSED view (D-055 macro replay; the swap-trick). Empty
    /// when the register is unset.
    pub fn register_bytes(&mut self, name: Option<char>) -> Vec<u8> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let st = EditorState::from_parts(doc, view);
        let bytes = st.registers().get(name).text().to_vec();
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        bytes
    }

    /// A snapshot of the FOCUSED view's NON-EMPTY registers as `(name, bytes)` — the unnamed slot (`"`), the
    /// yank register (`0`), and the named `a`-`z` — for `:registers` (the swap-trick). Order: `"`, `0`, a..z.
    pub fn register_snapshot(&mut self) -> Vec<(char, Vec<u8>)> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let st = EditorState::from_parts(doc, view);
        let regs = st.registers();
        let mut out = Vec::new();
        // Vim `:reg` order: unnamed, the yank register "0, the numbered delete-ring "1–"9, the small-delete
        // register "-, then the named slots "a–"z. Empty slots are omitted.
        for (name, r) in std::iter::once(('"', regs.get(None)))
            .chain(std::iter::once(('0', regs.yank0())))
            .chain(('1'..='9').map(|c| (c, regs.get(Some(c)))))
            .chain(std::iter::once(('-', regs.get(Some('-')))))
            .chain(('a'..='z').map(|c| (c, regs.get(Some(c)))))
        {
            if !r.is_empty() {
                out.push((name, r.text().to_vec()));
            }
        }
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        out
    }

    /// The FOCUSED view's set marks (`a`-`z`, `.`, `^`) as `(name, byte offset)` for `:marks` (swap-trick).
    pub fn marks_snapshot(&mut self) -> Vec<(char, usize)> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let st = EditorState::from_parts(doc, view);
        let marks = st.marks_snapshot();
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        marks
    }

    /// The FOCUSED view's jumplist positions for `:jumps` (swap-trick).
    pub fn jumps_snapshot(&mut self) -> Vec<usize> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");
        let st = EditorState::from_parts(doc, view);
        let out = st.jumps_snapshot();
        let (doc, view) = st.into_parts();
        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        out
    }

    /// The FOCUSED view's change-list positions for `:changes` (swap-trick).
    pub fn changes_snapshot(&mut self) -> Vec<usize> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");
        let st = EditorState::from_parts(doc, view);
        let out = st.changes_snapshot();
        let (doc, view) = st.into_parts();
        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        out
    }

    /// Run `:[range]s/pat/rep/flags` against the FOCUSED Window (the swap-trick, like [`Workspace::apply`]),
    /// applying every substitution as one undo group. Returns the count, or a [`RegexError`] (F-009 #2).
    pub fn substitute(
        &mut self,
        range: SubRange,
        pattern: &str,
        replacement: &str,
        flags: SubFlags,
    ) -> Result<SubOutcome, RegexError> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let did = view.doc();
        let slot = Self::doc_slot(did);
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let result = st.substitute(range, pattern, replacement, flags);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        result
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
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let st = EditorState::from_parts(doc, view);
        let out = st.substitute_preview(range, pattern, replacement, flags);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        out
    }

    /// Apply an accepted set of [`Substitution`]s to the focused window as one undo group (the tail of
    /// the `:s///c` confirm loop). Swap-trick like [`Workspace::apply`].
    pub fn apply_substitutions(&mut self, subs: &[Substitution]) -> SubOutcome {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let out = st.apply_substitutions(subs);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        out
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
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let result = st.global(range, pattern, negate, cmd);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        result
    }

    /// PASS 1 of `:g` against the FOCUSED window: the 1-based line numbers the pattern marks (or, with
    /// `negate`, does NOT mark). The frontend's `:g/pat/normal` runner needs this mark set (computed against
    /// the untouched buffer) so it can replay the keys per marked line through the input engine — the core
    /// `d`/`s` payloads run inside [`Workspace::global`]. Returns 1-based numbers for the frontend's cursor
    /// placement helpers.
    ///
    /// # Errors
    /// [`RegexError`] if the `:g` pattern is unrepresentable/malformed, or the buffer is not valid UTF-8.
    pub fn global_marks(
        &mut self,
        range: SubRange,
        pattern: &str,
        negate: bool,
    ) -> Result<Vec<usize>, RegexError> {
        // The swap-trick (like [`Workspace::global`]): move the focused parts into an [`EditorState`] to
        // reuse its single marking rule, then swap them back untouched (marking is read-only).
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let st = EditorState::from_parts(doc, view);
        let result = st.global_marks(range, pattern, negate);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        Ok(result?.into_iter().map(|li| li + 1).collect())
    }

    /// Run `:[range]d` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]): delete the
    /// range's lines as one undo group. Returns the number of lines deleted.
    pub fn delete_lines(&mut self, range: SubRange) -> usize {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let n = st.delete_lines(range);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        n
    }

    /// Apply a disjoint `(start, end, text)` batch edit to the FOCUSED window (the swap-trick, like
    /// [`Workspace::apply`]) as one undo group, tagged with the caller-supplied `origin` — a
    /// provenance-agnostic primitive (the LSP frontend passes [`TransactionOrigin::Lsp`]). See
    /// [`EditorState::apply_edits`].
    pub fn apply_edits(&mut self, edits: &[(usize, usize, String)], origin: TransactionOrigin) {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        st.apply_edits(edits, origin);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
    }

    /// Run `:[range]y` against the FOCUSED window (the swap-trick, like [`Workspace::apply`]): yank the
    /// range's lines linewise into the unnamed register (and `"0`). Returns the number of lines yanked.
    pub fn yank_lines(&mut self, range: SubRange) -> usize {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let n = st.yank_lines(range);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        n
    }

    /// Run `:[range]m {addr}` against the FOCUSED window (swap-trick): move the range's lines to after the
    /// destination. Returns the lines moved, or `None` if the destination is inside the source.
    pub fn move_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let n = st.move_lines(range, dest);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        n
    }

    /// Run `:[range]t {addr}` / `:copy` against the FOCUSED window (swap-trick): copy the range's lines to
    /// after the destination. Returns the lines copied.
    pub fn copy_lines(&mut self, range: SubRange, dest: LineAddr) -> Option<usize> {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let n = st.copy_lines(range, dest);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        n
    }

    /// Run `:[range]sort` against the FOCUSED window (swap-trick): sort the range's lines as one undo
    /// group. Returns the number of lines removed by the `unique` flag.
    pub fn sort_lines(&mut self, range: SubRange, opts: &crate::SortOptions) -> usize {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let n = st.sort_lines(range, opts);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        n
    }

    /// Move focus to the next Window in tile order, wrapping (Vim `C-w w`). No-op with one Window.
    pub fn focus_next(&mut self) {
        if !self.windows.is_empty() {
            self.focus = (self.focus + 1) % self.windows.len();
        }
    }

    /// Split the focused Window into a second Window onto the SAME buffer, in direction `dir`, and
    /// focus the new pane (Vim opens and focuses the new split). The new View is a CLONE of the focused
    /// View — same buffer id, same cursor/scroll/registers/config snapshot — so both panes start at the
    /// same place then diverge independently (F-007 acceptance #1). `dir` sets the whole flat layout's
    /// tiling (MVP: one direction, no tree). Returns the new [`ViewId`].
    ///
    /// Registers/config are cloned here rather than shared live — a documented post-MVP Vim-parity
    /// refinement (global registers across Views), tracked with folds/session in the F-007 backlog.
    pub fn split(&mut self, dir: SplitDir) -> ViewId {
        self.split = dir;
        let src = self.windows[self.focus].view;
        let clone = self.views[src.0]
            .as_ref()
            .expect("focused view live")
            .clone();
        let new_vid = ViewId(self.views.len());
        self.views.push(Some(clone));
        // Insert the new Window right after the focused one and focus it.
        let at = self.focus + 1;
        self.windows.insert(at, Window { view: new_vid });
        self.focus = at;
        new_vid
    }

    /// Close the focused Window (Vim `C-w c`): drop the layout slot, retire its View, and — if no other
    /// View still names its buffer — retire the shared Document too (F-007 acceptance #3: the buffer
    /// survives while another View holds it). Refuses to close the LAST Window (returns `false`); an
    /// app-level `:q` handles quitting. On success returns `true` and focus lands on the neighbour.
    pub fn close_focused(&mut self) -> bool {
        if self.windows.len() <= 1 {
            return false; // never leave zero windows — `:q` is the quit path
        }
        let win = self.windows.remove(self.focus);
        let did = self.views[win.view.0]
            .as_ref()
            .expect("closing view live")
            .doc();
        self.views[win.view.0] = None; // retire the View

        // Retire the Document iff no surviving Window's View still names it (refcount-by-scan).
        let still_held = self.windows.iter().any(|w| {
            self.views[w.view.0]
                .as_ref()
                .is_some_and(|v| v.doc() == did)
        });
        if !still_held {
            self.docs[Self::doc_slot(did)] = None;
            self.names[Self::doc_slot(did)] = None;
            self.buffer_order.retain(|&id| id != did);
            if self.alt == Some(did) {
                self.alt = None;
            }
        }

        if self.focus >= self.windows.len() {
            self.focus = self.windows.len() - 1; // closed the last-in-order pane
        }
        true
    }

    /// Close every window except the focused one (`:only`). Retires the other windows' Views but — unlike
    /// [`Workspace::close_focused`] — keeps ALL Documents/buffers loaded: `:only` is about windows, not
    /// buffers, so the others become hidden (still `:ls`-listed, reopenable via `:b`). Returns the number
    /// of windows closed (0 when already sole).
    pub fn only(&mut self) -> usize {
        if self.windows.len() <= 1 {
            return 0;
        }
        let kept = self.windows[self.focus];
        let others: Vec<ViewId> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.focus)
            .map(|(_, w)| w.view)
            .collect();
        let closed = others.len();
        for v in others {
            self.views[v.0] = None; // retire the pane's View; its Document stays loaded
        }
        self.windows = vec![kept];
        self.focus = 0;
        closed
    }

    /// Whether a `DocumentId` still has a live buffer slot (for tests / retirement assertions).
    #[must_use]
    pub fn doc_is_live(&self, id: DocumentId) -> bool {
        self.docs
            .get(Self::doc_slot(id))
            .is_some_and(Option::is_some)
    }

    /// The `DocumentId` of the buffer the FOCUSED window currently shows.
    #[must_use]
    pub fn focused_buffer(&self) -> DocumentId {
        let vid = self.windows[self.focus].view;
        self.views[vid.0].as_ref().expect("focused view live").doc()
    }

    /// The display name of buffer `id` (`None` for an unnamed/scratch buffer, or a retired slot).
    #[must_use]
    pub fn buffer_name(&self, id: DocumentId) -> Option<&str> {
        self.names
            .get(Self::doc_slot(id))
            .and_then(|n| n.as_deref())
    }

    /// The alternate buffer (`#`), if one is set and still live.
    #[must_use]
    pub fn alternate(&self) -> Option<DocumentId> {
        self.alt.filter(|&id| self.doc_is_live(id))
    }

    /// Name (or rename) the FOCUSED buffer — the frontend calls this to label the initial buffer with its
    /// file path, and to name a buffer when it gains a file.
    pub fn set_focused_buffer_name(&mut self, name: impl Into<String>) {
        let slot = Self::doc_slot(self.focused_buffer());
        self.names[slot] = Some(name.into());
    }

    /// Replace the focused buffer's contents with `bytes` (Vim `:e!` reload from disk): a fresh, saved
    /// Document under the SAME id, so unsaved changes and undo history are discarded. The focused cursor
    /// resets to the top. Splits of this buffer see the new content (shared Document).
    pub fn reload_focused(&mut self, bytes: impl Into<Vec<u8>>) {
        let id = self.focused_buffer();
        let slot = Self::doc_slot(id);
        let mut doc = Document::new(id, bytes);
        doc.mark_saved();
        self.docs[slot] = Some(doc);
        self.place_focused_cursor(0);
    }

    /// Add a new buffer over `bytes` with optional display `name`, plus a fresh View onto it (not shown
    /// in any window yet). Appends to the buffer list and returns the new buffer's id. `focus_buffer`
    /// brings it into the focused window.
    pub fn add_buffer(&mut self, bytes: impl Into<Vec<u8>>, name: Option<String>) -> DocumentId {
        let id = DocumentId(self.docs.len() as u64 + 1);
        let mut doc = Document::new(id, bytes);
        doc.mark_saved();
        self.docs.push(Some(doc));
        self.names.push(name);
        self.views.push(Some(View::fresh(id)));
        self.apply_default_search_case(ViewId(self.views.len() - 1));
        self.buffer_order.push(id);
        id
    }

    /// A [`ViewId`] naming buffer `id` that is safe to install in the focused window: a live view for the
    /// buffer that no OTHER window shows (a resident/hidden view, whose cursor is preserved), or a fresh
    /// one if every existing view for the buffer is already on screen (so two windows never share a View).
    fn view_for_buffer(&mut self, id: DocumentId) -> ViewId {
        let shown_elsewhere = |ws: &Self, vid: ViewId| {
            ws.windows
                .iter()
                .enumerate()
                .any(|(i, w)| i != ws.focus && w.view == vid)
        };
        let reusable = self.views.iter().enumerate().find_map(|(i, v)| {
            let vid = ViewId(i);
            match v {
                Some(view) if view.doc() == id && !shown_elsewhere(self, vid) => Some(vid),
                _ => None,
            }
        });
        reusable.unwrap_or_else(|| {
            let vid = ViewId(self.views.len());
            self.views.push(Some(View::fresh(id)));
            self.apply_default_search_case(vid);
            vid
        })
    }

    /// Show buffer `id` in the FOCUSED window (Vim `:buffer`). Records the buffer being left as the
    /// alternate (`#`). No-op (returns `true`) if it is already focused; `false` if `id` is not a live
    /// buffer. The previously shown view stays in the arena, so returning to a buffer restores its cursor.
    pub fn focus_buffer(&mut self, id: DocumentId) -> bool {
        if !self.doc_is_live(id) {
            return false;
        }
        let current = self.focused_buffer();
        if current == id {
            return true;
        }
        self.alt = Some(current);
        let vid = self.view_for_buffer(id);
        self.windows[self.focus].view = vid;
        true
    }

    /// The buffer list in `:ls` order, each with its display name and status flags (current/alt/modified).
    #[must_use]
    pub fn buffers(&self) -> Vec<BufferInfo> {
        let current = self.focused_buffer();
        let alt = self.alternate();
        self.buffer_order
            .iter()
            .filter(|&&id| self.doc_is_live(id))
            .map(|&id| BufferInfo {
                id,
                name: self
                    .buffer_name(id)
                    .map_or_else(|| "[No Name]".to_string(), str::to_string),
                current: id == current,
                alt: Some(id) == alt,
                modified: self.docs[Self::doc_slot(id)]
                    .as_ref()
                    .is_some_and(Document::is_modified),
            })
            .collect()
    }

    /// Switch the focused window to the next (`:bnext`) or previous (`:bprevious`) live buffer in `:ls`
    /// order, wrapping. No-op with a single buffer. `forward` selects the direction.
    pub fn cycle_buffer(&mut self, forward: bool) {
        let live: Vec<DocumentId> = self
            .buffer_order
            .iter()
            .copied()
            .filter(|&id| self.doc_is_live(id))
            .collect();
        if live.len() <= 1 {
            return;
        }
        let current = self.focused_buffer();
        let Some(pos) = live.iter().position(|&id| id == current) else {
            return;
        };
        let n = live.len();
        let next = if forward {
            (pos + 1) % n
        } else {
            (pos + n - 1) % n
        };
        self.focus_buffer(live[next]);
    }

    /// Delete buffer `id` from the buffer list (Vim `:bd`): retire its Document + Views and repoint every
    /// window showing it to a replacement buffer (the alternate if live, else the next buffer in `:ls`
    /// order, else a fresh scratch when it was the last buffer). Returns `false` if `id` is not live. The
    /// caller enforces the unsaved-changes guard (E89) — this is the unconditional removal.
    pub fn remove_buffer(&mut self, id: DocumentId) -> bool {
        if !self.doc_is_live(id) {
            return false;
        }
        // Pick the replacement: the alternate (`#`) if usable, else the next live buffer after `id` in
        // `:ls` order (wrapping), else a brand-new scratch (deleting the last buffer never leaves zero).
        let replacement = self
            .alt
            .filter(|&a| a != id && self.doc_is_live(a))
            .or_else(|| {
                let p = self.buffer_order.iter().position(|&b| b == id)?;
                self.buffer_order
                    .iter()
                    .cycle()
                    .skip(p + 1)
                    .take(self.buffer_order.len())
                    .copied()
                    .find(|&b| b != id && self.doc_is_live(b))
            })
            .unwrap_or_else(|| self.add_buffer(Vec::new(), None));

        // Repoint every window showing `id` to a fresh View of the replacement.
        for i in 0..self.windows.len() {
            let vid = self.windows[i].view;
            if self.views[vid.0].as_ref().map(View::doc) == Some(id) {
                let nv = ViewId(self.views.len());
                self.views.push(Some(View::fresh(replacement)));
                self.apply_default_search_case(nv);
                self.windows[i].view = nv;
            }
        }
        // Retire every View that named `id`, then the buffer itself.
        for v in self.views.iter_mut() {
            if v.as_ref().map(View::doc) == Some(id) {
                *v = None;
            }
        }
        let slot = Self::doc_slot(id);
        self.docs[slot] = None;
        self.names[slot] = None;
        self.buffer_order.retain(|&b| b != id);
        if self.alt == Some(id) {
            self.alt = None;
        }
        true
    }

    /// Set the focused View's indent config (`>>`/`<<`) — the seam a config loader/test uses; mirrors
    /// [`EditorState::set_indent`]. No new schema key (editor.tab_width / editor.indent_style).
    pub fn set_indent(&mut self, tab_width: usize, indent_style: crate::editor::IndentStyle) {
        // Round-trip through EditorState so the single set_indent implementation stays authoritative.
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let did = view.doc();
        let slot = Self::doc_slot(did);
        let doc = self.docs[slot].take().expect("focused doc live");
        let mut st = EditorState::from_parts(doc, view);
        st.set_indent(tab_width, indent_style);
        let (doc, view) = st.into_parts();
        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
    }

    /// The 0-based inclusive line range a `=`/`Reindent` motion covers on the focused buffer (its op-span's
    /// first/last line), or `None` if the span is empty. The frontend uses this to compute tree-aware indent
    /// levels for the same lines the core's bracket-depth `=` would reindent.
    #[must_use]
    pub fn reindent_range(&self, motion: crate::Motion, count: u32) -> Option<(usize, usize)> {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let (s, e, _) = crate::editor::op_span(b, pane.view.cursor(), motion, count);
        if s >= e {
            return None;
        }
        Some((crate::pos::line_of(b, s), crate::pos::line_of(b, e - 1)))
    }

    /// The keyword under (or forward-on-line from) the focused cursor — the pattern source for Vim `*`/`#`.
    /// `None` when the current line has no keyword at/after the cursor.
    #[must_use]
    pub fn word_under_cursor(&self) -> Option<String> {
        let pane = self.focused();
        let b = pane.doc.bytes();
        let (s, e) = crate::motion::word_under_cursor(b, pane.view.cursor())?;
        std::str::from_utf8(&b[s..e]).ok().map(str::to_string)
    }

    /// Set one `:set` option on the focused view (swap-trick, like [`Workspace::set_indent`]).
    pub fn set_option(&mut self, opt: crate::editor::EditorOption) {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");
        let mut st = EditorState::from_parts(doc, view);
        st.set_option(opt);
        let (doc, view) = st.into_parts();
        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::motion::Motion;

    fn ws() -> Workspace {
        Workspace::new(b"hello\nworld\nfoo\nbar\n".to_vec())
    }

    use crate::clipboard::MemClipboard;
    use crate::motion::Motion as Mo;

    /// `"+yiw` yanks the word into the SYSTEM clipboard (via the injected provider), and mirrors unnamed.
    #[test]
    fn clipboard_yank_word_reaches_the_provider() {
        let clip = MemClipboard::new();
        let mut w = Workspace::new(b"hello\nworld\n".to_vec());
        w.set_clipboard(Box::new(clip.clone()));
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Yank(1, Mo::InnerWord));
        assert_eq!(
            clip.contents().as_deref(),
            Some("hello"),
            "\"+yiw pushes the word to the OS clipboard"
        );
        assert_eq!(
            w.register_bytes(None),
            b"hello",
            "a clipboard yank still mirrors unnamed (Vim)"
        );
    }

    /// A plain (unregistered) yank must NEVER touch the clipboard provider.
    #[test]
    fn plain_yank_leaves_clipboard_untouched() {
        let clip = MemClipboard::new();
        let mut w = Workspace::new(b"hello\nworld\n".to_vec());
        w.set_clipboard(Box::new(clip.clone()));
        w.apply(&Command::Yank(1, Mo::InnerWord));
        assert!(
            clip.contents().is_none(),
            "an unnamed yank never writes the OS clipboard"
        );
    }

    /// `"+p` pastes FROM whatever an external app put on the clipboard (charwise, inline).
    #[test]
    fn clipboard_paste_reads_external_contents() {
        let clip = MemClipboard::new();
        clip.preload("PASTED");
        let mut w = Workspace::new(b"xy\n".to_vec());
        w.set_clipboard(Box::new(clip.clone()));
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Paste {
            after: true,
            count: 1,
            move_after: false,
        });
        assert_eq!(
            w.focused().doc.bytes(),
            b"xPASTEDy\n",
            "\"+p inserts the external clipboard text after the cursor"
        );
    }

    /// Linewise geometry survives the clipboard round-trip: `"+yy` then `"+p` opens a whole NEW line, even
    /// though the OS clipboard only carries bytes (the mirror slot keeps its RegKind when bytes are unchanged).
    #[test]
    fn clipboard_linewise_round_trip_preserves_geometry() {
        let clip = MemClipboard::new();
        let mut w = Workspace::new(b"alpha\nbeta\n".to_vec());
        w.set_clipboard(Box::new(clip.clone()));
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Yank(1, Mo::Line)); // "+yy
        assert_eq!(clip.contents().as_deref(), Some("alpha\n"));
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Paste {
            after: true,
            count: 1,
            move_after: false,
        }); // "+p
        assert_eq!(
            w.focused().doc.bytes(),
            b"alpha\nalpha\nbeta\n",
            "a linewise clipboard register pastes as a new line, not inline"
        );
    }

    /// `"+dd` deletes into the clipboard and removes the line from the buffer.
    #[test]
    fn clipboard_delete_line_cuts_to_the_provider() {
        let clip = MemClipboard::new();
        let mut w = Workspace::new(b"alpha\nbeta\n".to_vec());
        w.set_clipboard(Box::new(clip.clone()));
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Delete(1, Mo::Line)); // "+dd
        assert_eq!(clip.contents().as_deref(), Some("alpha\n"));
        assert_eq!(w.focused().doc.bytes(), b"beta\n", "the line is cut out");
    }

    /// Graceful degradation: with the default [`NoClipboard`], `"+p` pastes nothing and never panics.
    #[test]
    fn clipboard_absent_is_a_graceful_noop() {
        let mut w = Workspace::new(b"hi\n".to_vec());
        w.apply(&Command::SetRegister(Some('+')));
        w.apply(&Command::Paste {
            after: true,
            count: 1,
            move_after: false,
        });
        assert_eq!(
            w.focused().doc.bytes(),
            b"hi\n",
            "no clipboard tool -> \"+p is inert, not a crash"
        );
    }

    /// F-007 #1: splitting shows two Views of ONE Document with INDEPENDENT cursors and scroll.
    #[test]
    fn split_gives_two_views_one_buffer_independent_cursors() {
        let mut w = ws();
        assert_eq!(w.window_count(), 1);
        let a_doc = w.focused().view.doc();

        w.split(SplitDir::Horizontal);
        assert_eq!(w.window_count(), 2, "split adds a window");

        // Both panes name the SAME buffer …
        let did0 = w.pane(0).view.doc();
        let did1 = w.pane(1).view.doc();
        assert_eq!(did0, a_doc);
        assert_eq!(did1, a_doc, "both views share one document");
        // … but the two Views are DISTINCT handles.
        assert_ne!(
            w.windows[0].view, w.windows[1].view,
            "distinct view handles"
        );

        // Move the cursor in the focused (new) pane; the other pane's cursor is untouched.
        w.focus = 0;
        let before = w.pane(1).view.cursor();
        w.apply(&Command::Move(1, Motion::Down));
        assert_ne!(w.pane(0).view.cursor(), before, "focused cursor moved");
        assert_eq!(
            w.pane(1).view.cursor(),
            before,
            "the other view's cursor is independent"
        );

        // Independent scroll: setting one pane's top does not move the other's.
        w.set_top(0, 2);
        assert_eq!(w.pane(0).view.top(), 2);
        assert_eq!(
            w.pane(1).view.top(),
            0,
            "the other view scrolls independently"
        );
    }

    /// `:only` keeps the focused window and closes the rest, but leaves every buffer loaded (windows,
    /// not buffers) — the others become hidden and stay `:ls`-listed / reopenable via `:b`.
    #[test]
    fn only_keeps_focused_window_and_leaves_buffers_loaded() {
        let mut w = ws();
        let b = w.add_buffer(b"second\n".to_vec(), Some("second".to_string()));
        w.split(SplitDir::Horizontal); // 2 windows on the original buffer
        w.focus_buffer(b); // focused window now shows the second buffer
        w.split(SplitDir::Vertical); // 3 windows
        assert_eq!(w.window_count(), 3);
        let focused_doc = w.focused().view.doc();

        let closed = w.only();
        assert_eq!(closed, 2, "the two non-focused windows are closed");
        assert_eq!(w.window_count(), 1, "only the focused window remains");
        assert_eq!(
            w.focused().view.doc(),
            focused_doc,
            "focused buffer unchanged"
        );
        assert!(
            w.doc_is_live(b),
            "the second buffer is still loaded (hidden)"
        );
        assert_eq!(w.buffers().len(), 2, "buffer list intact after :only");

        assert_eq!(w.only(), 0, ":only on a sole window is a no-op");
    }

    /// `:[range]d` deletes whole lines as one undo group; the no-trailing-newline last line and the
    /// whole-buffer case leave no dangling blank line.
    #[test]
    fn delete_lines_over_a_range() {
        let mut w = Workspace::new(b"one\ntwo\nthree\nfour\n".to_vec());
        assert_eq!(
            w.delete_lines(SubRange::Lines(2, 3)),
            2,
            ":2,3d deletes 2 lines"
        );
        assert_eq!(w.focused().doc.bytes(), b"one\nfour\n");
        // One undo restores all of it (single group).
        w.apply(&Command::Undo);
        assert_eq!(w.focused().doc.bytes(), b"one\ntwo\nthree\nfour\n");

        // No range → the cursor's line (like `dd`).
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        assert_eq!(w.delete_lines(SubRange::CurrentLine), 1);
        assert_eq!(w.focused().doc.bytes(), b"b\nc\n");

        // Deleting through an UNTERMINATED last line leaves no dangling blank line.
        let mut w = Workspace::new(b"a\nb".to_vec());
        assert_eq!(w.delete_lines(SubRange::Lines(2, 2)), 1);
        assert_eq!(w.focused().doc.bytes(), b"a");

        // Whole buffer.
        let mut w = Workspace::new(b"x\ny\n".to_vec());
        w.delete_lines(SubRange::WholeFile);
        assert_eq!(w.focused().doc.bytes(), b"");
    }

    /// `:[range]y` yanks whole lines LINEWISE (non-destructive), so a following `p` opens them below.
    #[test]
    fn yank_lines_is_linewise_and_non_destructive() {
        let mut w = Workspace::new(b"one\ntwo\nthree\n".to_vec());
        assert_eq!(
            w.yank_lines(SubRange::Lines(1, 2)),
            2,
            ":1,2y yanks two lines"
        );
        // The buffer is untouched by a yank.
        assert_eq!(w.focused().doc.bytes(), b"one\ntwo\nthree\n");
        // `p` (paste after) opens the yanked block below the cursor line (linewise).
        w.apply(&Command::Paste {
            after: true,
            count: 1,
            move_after: false,
        });
        assert_eq!(w.focused().doc.bytes(), b"one\none\ntwo\ntwo\nthree\n");

        // An unterminated last line still yanks a clean linewise block.
        let mut w = Workspace::new(b"a\nb".to_vec());
        assert_eq!(w.yank_lines(SubRange::Lines(2, 2)), 1);
        w.apply(&Command::Paste {
            after: true,
            count: 1,
            move_after: false,
        });
        assert_eq!(w.focused().doc.bytes(), b"a\nb\nb");
    }

    /// `:[range]m {addr}` moves whole lines to after the destination as one undo group; a destination
    /// inside the source is declined (E134).
    #[test]
    fn move_lines_relocates_the_span() {
        let mut w = Workspace::new(b"one\ntwo\nthree\nfour\n".to_vec());
        assert_eq!(
            w.move_lines(SubRange::Lines(1, 2), LineAddr::Line(4)),
            Some(2)
        );
        assert_eq!(w.focused().doc.bytes(), b"three\nfour\none\ntwo\n");
        w.apply(&Command::Undo);
        assert_eq!(
            w.focused().doc.bytes(),
            b"one\ntwo\nthree\nfour\n",
            "one undo group"
        );

        // `:3,4m0` → to the top.
        let mut w = Workspace::new(b"one\ntwo\nthree\nfour\n".to_vec());
        assert_eq!(
            w.move_lines(SubRange::Lines(3, 4), LineAddr::Line(0)),
            Some(2)
        );
        assert_eq!(w.focused().doc.bytes(), b"three\nfour\none\ntwo\n");

        // `:m$` on the cursor's line (line 1) → to the end.
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        assert_eq!(w.move_lines(SubRange::CurrentLine, LineAddr::Last), Some(1));
        assert_eq!(w.focused().doc.bytes(), b"b\nc\na\n");

        // Destination inside the source → declined (Vim E134).
        let mut w = Workspace::new(b"a\nb\nc\nd\n".to_vec());
        assert_eq!(w.move_lines(SubRange::Lines(2, 3), LineAddr::Line(2)), None);
        assert_eq!(
            w.focused().doc.bytes(),
            b"a\nb\nc\nd\n",
            "declined move leaves the buffer"
        );
    }

    /// `:[range]t {addr}` copies whole lines to after the destination, leaving the source in place.
    #[test]
    fn copy_lines_duplicates_the_span() {
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        assert_eq!(
            w.copy_lines(SubRange::Lines(1, 1), LineAddr::Line(2)),
            Some(1)
        );
        assert_eq!(w.focused().doc.bytes(), b"a\nb\na\nc\n");

        // `:1,2t0` copies the block to the top.
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        assert_eq!(
            w.copy_lines(SubRange::Lines(1, 2), LineAddr::Line(0)),
            Some(2)
        );
        assert_eq!(w.focused().doc.bytes(), b"a\nb\na\nb\nc\n");
    }

    /// `SetIndents` (the tree-aware `=` seam) sets each line's leading whitespace to `level × shiftwidth`,
    /// leaves blank lines empty, and is one undo group.
    #[test]
    fn set_indents_applies_explicit_levels() {
        let mut w = Workspace::new("a\nb\nc\n".as_bytes().to_vec());
        w.apply(&Command::SetIndents {
            first_line: 0,
            last_line: 2,
            levels: vec![0, 1, 2],
        });
        assert_eq!(w.focused().doc.bytes(), "a\n    b\n        c\n".as_bytes());
        w.apply(&Command::Undo);
        assert_eq!(
            w.focused().doc.bytes(),
            "a\nb\nc\n".as_bytes(),
            "one undo group"
        );

        // A blank line stays empty regardless of its level.
        let mut w = Workspace::new("{\n\nx\n".as_bytes().to_vec());
        w.apply(&Command::SetIndents {
            first_line: 0,
            last_line: 2,
            levels: vec![0, 5, 1],
        });
        assert_eq!(w.focused().doc.bytes(), "{\n\n    x\n".as_bytes());
    }

    /// `=` reindents lines to their bracket depth (net unclosed `([{` × shiftwidth; closer-first lines
    /// dedent; blank lines stay empty), as one undo group.
    #[test]
    fn reindent_by_bracket_depth() {
        let src = "fn main() {\nlet x = foo(\n1,\n2,\n);\n}\n";
        let mut w = Workspace::new(src.as_bytes().to_vec());
        // Default config: shiftwidth 4, spaces. `=G` reindents the whole buffer.
        w.apply(&Command::Reindent {
            count: 1,
            motion: Motion::LastLine,
        });
        let expected = "fn main() {\n    let x = foo(\n        1,\n        2,\n    );\n}\n";
        assert_eq!(w.focused().doc.bytes(), expected.as_bytes());

        // One undo restores the original in a single step.
        w.apply(&Command::Undo);
        assert_eq!(w.focused().doc.bytes(), src.as_bytes());

        // Shiftwidth drives the unit; a blank line stays empty.
        let mut w = Workspace::new("{\nx\n\n}\n".as_bytes().to_vec());
        w.set_option(crate::editor::EditorOption::ShiftWidth(2));
        w.apply(&Command::Reindent {
            count: 1,
            motion: Motion::LastLine,
        });
        assert_eq!(w.focused().doc.bytes(), "{\n  x\n\n}\n".as_bytes());
    }

    /// `>`/`<` {motion} shift the motion's LINES one indent level (linewise), regardless of the motion's
    /// own wise-ness.
    #[test]
    fn shift_motion_indents_the_lines() {
        // `>j` from line 0 indents lines 0 and 1 by one level (default 4 spaces); line 2 is untouched.
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        w.apply(&Command::ShiftMotion {
            left: false,
            count: 1,
            motion: Motion::Down,
        });
        assert_eq!(w.focused().doc.bytes(), b"    a\n    b\nc\n");

        // `<j` removes one level from those two lines.
        w.apply(&Command::ShiftMotion {
            left: true,
            count: 1,
            motion: Motion::Down,
        });
        assert_eq!(w.focused().doc.bytes(), b"a\nb\nc\n");
    }

    /// `gu`/`gU`/`g~` {motion} recase the operator span (lower / upper / toggle) as one edit, leaving
    /// the cursor at the span start.
    #[test]
    fn case_motion_recases_the_span() {
        use crate::command::WordCase;
        // gUw → uppercase the word span "hello " (the space is unaffected).
        let mut w = Workspace::new(b"hello world\n".to_vec());
        w.apply(&Command::CaseMotion {
            count: 1,
            motion: Motion::WordFwd,
            case: WordCase::Upcase,
        });
        assert_eq!(w.focused().doc.bytes(), b"HELLO world\n");

        // g~$ → toggle to end of line.
        let mut w = Workspace::new(b"Hello\n".to_vec());
        w.apply(&Command::CaseMotion {
            count: 1,
            motion: Motion::LineEnd,
            case: WordCase::Toggle,
        });
        assert_eq!(w.focused().doc.bytes(), b"hELLO\n");

        // guu (linewise) → lowercase the whole line.
        let mut w = Workspace::new(b"MixedCase Line\n".to_vec());
        w.apply(&Command::CaseMotion {
            count: 1,
            motion: Motion::Line,
            case: WordCase::Downcase,
        });
        assert_eq!(w.focused().doc.bytes(), b"mixedcase line\n");
    }

    /// `*`/`#` word extraction: the keyword under the cursor, or the next one forward on the line; and a
    /// whole-word `\<…\>` search (as `*` builds) skips substring hits.
    #[test]
    fn word_under_cursor_and_whole_word_search() {
        // On a keyword → that word.
        let w = Workspace::new(b"foo bar\n".to_vec());
        assert_eq!(w.word_under_cursor().as_deref(), Some("foo"));

        // On whitespace → the next keyword forward on the line.
        let mut w = Workspace::new(b"foo bar\n".to_vec());
        w.place_focused_cursor(3); // the space
        assert_eq!(w.word_under_cursor().as_deref(), Some("bar"));

        // No keyword on the line → None.
        let w = Workspace::new(b"!!! ???\n".to_vec());
        assert_eq!(w.word_under_cursor(), None);
        assert_eq!(Workspace::new(b"".to_vec()).word_under_cursor(), None);

        // The whole-word pattern `*` builds skips the substring hit ("foobar") and lands on the next
        // standalone "foo".
        let mut w = Workspace::new(b"foo foobar foo\n".to_vec());
        w.apply(&Command::SearchNext("\\<foo\\>".to_string()));
        assert_eq!(
            w.focused().view.cursor(),
            11,
            "whole-word search skips foobar"
        );
    }

    /// `gd`/`gD` (go-to-declaration, TEXT heuristic): the frontend reads the keyword under the cursor and
    /// rewrites to `GotoFirstMatch("\<word\>")`, which lands on the FIRST whole-word match from the TOP of
    /// the file (matching nvim v0.12.4 for both `gd` and `gD`, verified against nvim). It is a JUMP, so the
    /// leaving position is recorded and `CTRL-O` returns. No match keeps the cursor.
    #[test]
    fn goto_declaration_first_match_from_top_and_jumplist() {
        // Buffer: `int foo;` then two uses of `foo`. Cursor on the LAST `foo` (line 3, "int baz = foo;").
        let src = b"int foo;\nvoid bar() { foo = 1; }\nint baz = foo;\n".to_vec();
        let decl = src
            .windows(3)
            .position(|w| w == b"foo")
            .expect("foo present"); // byte offset of the first `foo` (in `int foo;`)
        let last_foo = src
            .windows(3)
            .enumerate()
            .rfind(|(_, w)| *w == b"foo")
            .expect("foo present")
            .0;

        let mut w = Workspace::new(src.clone());
        w.place_focused_cursor(last_foo);
        // Mirror the frontend rewrite (session.rs): keyword under cursor → whole-word first-match jump.
        let word = w.word_under_cursor().expect("on a keyword");
        assert_eq!(word, "foo");
        w.apply(&Command::GotoFirstMatch(format!("\\<{word}\\>")));
        assert_eq!(
            w.focused().view.cursor(),
            decl,
            "gd/gD lands on the first whole-word match from the top of the file"
        );
        // It was a JUMP: the leaving position (last `foo`) is on the jumplist and `CTRL-O` returns there.
        assert!(
            w.jumps_snapshot().contains(&last_foo),
            "the jump records the position it left"
        );
        w.apply(&Command::GotoOlderJump); // CTRL-O
        assert_eq!(
            w.focused().view.cursor(),
            last_foo,
            "CTRL-O returns to where gd/gD jumped from"
        );

        // No match → the cursor stays put (Vim rings the bell).
        let mut w = Workspace::new(src);
        w.place_focused_cursor(last_foo);
        w.apply(&Command::GotoFirstMatch("\\<nope\\>".to_string()));
        assert_eq!(
            w.focused().view.cursor(),
            last_foo,
            "no match leaves the cursor put"
        );

        // The whole-word pattern skips substrings: `foobar` is not a match for `\<foo\>`.
        let mut w = Workspace::new(b"foobar\nfoo\n".to_vec());
        w.place_focused_cursor(7); // on the standalone `foo` (line 2)
        w.apply(&Command::GotoFirstMatch("\\<foo\\>".to_string()));
        assert_eq!(
            w.focused().view.cursor(),
            7,
            "the first WHOLE-word match is the standalone foo, not the foobar substring"
        );
    }

    /// A pathological `{count}p` (a digit-spam count that saturates to `u32::MAX`) must not request a
    /// multi-gigabyte allocation: the paste is clamped to a bounded number of bytes and completes.
    #[test]
    fn huge_paste_count_is_bounded() {
        let mut w = Workspace::new(b"ab\n".to_vec());
        w.yank_lines(SubRange::CurrentLine); // register := "ab\n" (linewise)
        w.apply(&Command::Paste {
            after: true,
            count: u32::MAX,
            move_after: false,
        });
        // Bounded to ~64 MiB, NOT 3 × 4.29e9 ≈ 12 GiB.
        assert!(
            w.focused().doc.bytes().len() <= (1 << 26) + 16,
            "paste bytes are clamped, not unbounded"
        );
    }

    /// `:e!` (reload) replaces the focused buffer with fresh bytes and marks it saved, discarding the
    /// unsaved edit.
    #[test]
    fn reload_focused_replaces_and_marks_saved() {
        let mut w = Workspace::new(b"old\n".to_vec());
        w.apply(&Command::EnterInsert);
        w.apply(&Command::InsertChar('x'));
        assert!(w.focused().doc.is_modified(), "the edit dirtied the buffer");
        w.reload_focused(b"fresh\ndisk\n".to_vec());
        assert_eq!(w.focused().doc.bytes(), b"fresh\ndisk\n");
        assert!(
            !w.focused().doc.is_modified(),
            "the reloaded buffer is clean"
        );
    }

    /// `:set ignorecase` flips the focused view's search case, observable through `:s` matching.
    #[test]
    fn set_option_ignorecase_changes_matching() {
        use crate::editor::EditorOption;
        let mut w = Workspace::new(b"Foo\n".to_vec());
        // Default is case-sensitive: `:s/foo/X/` matches nothing.
        let out = w
            .substitute(SubRange::CurrentLine, "foo", "X", SubFlags::default())
            .expect("valid regex");
        assert_eq!(out.replacements, 0);
        // `:set ignorecase` → now it matches.
        w.set_option(EditorOption::IgnoreCase(true));
        let out = w
            .substitute(SubRange::CurrentLine, "foo", "X", SubFlags::default())
            .expect("valid regex");
        assert_eq!(out.replacements, 1);
        assert_eq!(w.focused().doc.bytes(), b"X\n");
    }

    /// `set_default_search_case` applies to the initial buffer AND propagates to buffers created afterward,
    /// so the shipped ignorecase+smartcase default holds across `:e` (not just the first buffer).
    #[test]
    fn default_search_case_propagates_to_new_buffers() {
        let mut w = Workspace::new(b"Foo\n".to_vec());
        w.set_default_search_case(true, true); // ruse's shipped default
                                               // The initial buffer is now case-insensitive.
        let out = w
            .substitute(SubRange::CurrentLine, "foo", "X", SubFlags::default())
            .expect("valid regex");
        assert_eq!(out.replacements, 1, "initial buffer honors the default");
        // A buffer opened AFTER the default was set also gets it.
        let b2 = w.add_buffer(b"Bar\n".to_vec(), Some("b2".to_string()));
        w.focus_buffer(b2);
        let out = w
            .substitute(SubRange::CurrentLine, "bar", "Y", SubFlags::default())
            .expect("valid regex");
        assert_eq!(out.replacements, 1, "new buffer inherits the default");
        // smartcase: an uppercase char in the pattern forces case-sensitivity even with ignorecase on.
        let b3 = w.add_buffer(b"baz Baz\n".to_vec(), Some("b3".to_string()));
        w.focus_buffer(b3);
        let out = w
            .substitute(SubRange::CurrentLine, "Baz", "Z", SubFlags::default())
            .expect("valid regex");
        assert_eq!(
            out.replacements, 1,
            "smartcase: 'Baz' matches only the capitalized one"
        );
        assert_eq!(w.focused().doc.bytes(), b"baz Z\n");
    }

    /// `:bd` retires a buffer and repoints the window to the alternate; deleting the last buffer opens a
    /// fresh scratch (never zero buffers); a non-live id is a no-op.
    #[test]
    fn remove_buffer_repoints_and_retires() {
        let mut w = Workspace::new(b"first\n".to_vec()); // buffer 1
        let b2 = w.add_buffer(b"second\n".to_vec(), Some("second".to_string()));
        w.focus_buffer(b2); // focused = b2, alternate = buffer 1
        assert_eq!(w.focused_buffer(), b2);

        assert!(w.remove_buffer(b2), "delete the focused buffer");
        assert!(!w.doc_is_live(b2), "b2 is retired");
        assert_eq!(w.focused_buffer().0, 1, "window shows the alternate");
        assert_eq!(w.buffers().len(), 1);

        // Deleting the last buffer opens a fresh scratch — never zero buffers.
        let last = w.focused_buffer();
        assert!(w.remove_buffer(last));
        assert_eq!(w.buffers().len(), 1, "a scratch replaces the last buffer");
        assert_ne!(w.focused_buffer(), last, "focused a new buffer");

        // A non-live id is a no-op.
        assert!(!w.remove_buffer(last));
    }

    /// `:[range]sort` sorts lines lexicographically or numerically, with `!` reverse and `u` unique, as
    /// one undo group; a range limits it to those lines.
    #[test]
    fn sort_lines_variants() {
        use crate::SortOptions;
        let opts = |reverse, numeric, unique| SortOptions {
            reverse,
            numeric,
            unique,
            ..SortOptions::default()
        };
        let mut w = Workspace::new(b"banana\napple\ncherry\n".to_vec());
        w.sort_lines(SubRange::WholeFile, &opts(false, false, false));
        assert_eq!(w.focused().doc.bytes(), b"apple\nbanana\ncherry\n");

        // numeric: 10 sorts after 2, not lexicographically before it.
        let mut w = Workspace::new(b"10\n2\n1\n".to_vec());
        w.sort_lines(SubRange::WholeFile, &opts(false, true, false));
        assert_eq!(w.focused().doc.bytes(), b"1\n2\n10\n");

        // reverse (descending).
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        w.sort_lines(SubRange::WholeFile, &opts(true, false, false));
        assert_eq!(w.focused().doc.bytes(), b"c\nb\na\n");

        // unique drops duplicate lines after sorting.
        let mut w = Workspace::new(b"b\na\nb\na\n".to_vec());
        assert_eq!(
            w.sort_lines(SubRange::WholeFile, &opts(false, false, true)),
            2,
            "2 dupes removed"
        );
        assert_eq!(w.focused().doc.bytes(), b"a\nb\n");

        // A range limits the sort to those lines; the rest stays put.
        let mut w = Workspace::new(b"z\n3\n1\n2\n".to_vec());
        w.sort_lines(SubRange::Lines(2, 4), &opts(false, true, false));
        assert_eq!(w.focused().doc.bytes(), b"z\n1\n2\n3\n");
    }

    #[test]
    fn sort_lines_ignore_case_and_pattern() {
        use crate::SortOptions;
        // `i` — case-insensitive: "Banana" sorts with "apple"/"cherry", not before all lowercase.
        let mut w = Workspace::new(b"cherry\nBanana\napple\n".to_vec());
        w.sort_lines(
            SubRange::WholeFile,
            &SortOptions {
                ignore_case: true,
                ..SortOptions::default()
            },
        );
        assert_eq!(w.focused().doc.bytes(), b"apple\nBanana\ncherry\n");

        // `/pattern/` — sort on the text AFTER the match (here, after the leading `id=` tag).
        let mut w = Workspace::new(b"id=charlie\nid=alice\nid=bob\n".to_vec());
        w.sort_lines(
            SubRange::WholeFile,
            &SortOptions {
                pattern: Some("id=".into()),
                ..SortOptions::default()
            },
        );
        assert_eq!(w.focused().doc.bytes(), b"id=alice\nid=bob\nid=charlie\n");

        // `/pattern/` + `r` — sort on the MATCHED text itself (the number), numerically.
        let mut w = Workspace::new(b"a99\nb100\nc9\n".to_vec());
        w.sort_lines(
            SubRange::WholeFile,
            &SortOptions {
                pattern: Some("\\d\\+".into()),
                use_match: true,
                numeric: true,
                ..SortOptions::default()
            },
        );
        assert_eq!(w.focused().doc.bytes(), b"c9\na99\nb100\n");
    }

    /// D-055: raw macro bytes round-trip through a named register — `set_register_raw` then `register_bytes`
    /// return the same key stream, and it shares the a-z slots with yank/paste (unset reads empty).
    #[test]
    fn macro_register_bytes_round_trip() {
        let mut w = Workspace::new(b"hello\n".to_vec());
        assert_eq!(w.register_bytes(Some('a')), b"", "unset register is empty");
        w.set_register_raw(Some('a'), b"iZ\x1b".to_vec()); // a macro: insert Z, escape
        assert_eq!(w.register_bytes(Some('a')), b"iZ\x1b");
        // A different register is independent.
        assert_eq!(w.register_bytes(Some('b')), b"");
    }

    /// F-009: a whole-file substitute (what `g&` repeats) applies across every line, honouring the flags.
    #[test]
    fn substitute_whole_file_is_what_g_ampersand_repeats() {
        let mut w = Workspace::new(b"x x\nx x\n".to_vec());
        // `g&` reuses the last flags — here global (all matches on every line).
        let out = w
            .substitute(
                SubRange::WholeFile,
                "x",
                "y",
                SubFlags {
                    global: true,
                    ignore_case: None,
                },
            )
            .unwrap();
        assert_eq!(w.focused().doc.bytes(), b"y y\ny y\n");
        assert_eq!(out.replacements, 4);
    }

    /// F-009: a current-line substitute (what `&` repeats) acts only on the cursor's line, first match by
    /// default — running it again after moving the cursor repeats on the new line.
    #[test]
    fn substitute_current_line_repeats_on_the_cursors_line() {
        let mut w = Workspace::new(b"x x\nx x\n".to_vec());
        w.substitute(SubRange::CurrentLine, "x", "y", SubFlags::default())
            .unwrap();
        assert_eq!(w.focused().doc.bytes(), b"y x\nx x\n", "first x on line 1");
        w.place_focused_cursor(4); // start of line 2
        w.substitute(SubRange::CurrentLine, "x", "y", SubFlags::default())
            .unwrap();
        assert_eq!(w.focused().doc.bytes(), b"y x\ny x\n", "repeat on line 2");
    }

    /// F-003: `marks_snapshot` lists the set named marks then `.` (last change) and `^` (last insert).
    #[test]
    fn marks_snapshot_lists_named_then_dot_and_caret() {
        let mut w = Workspace::new(b"abc\ndef\n".to_vec());
        assert!(w.marks_snapshot().is_empty(), "nothing set yet");
        w.place_focused_cursor(5);
        w.apply(&Command::SetNamedMark('a'));
        w.apply(&Command::DeleteUnder(1)); // sets the `.` last-change mark
        let snap = w.marks_snapshot();
        assert!(
            snap.iter().any(|&(c, _)| c == 'a'),
            "named mark a present; got {snap:?}"
        );
        assert!(
            snap.iter().any(|&(c, _)| c == '.'),
            "last-change `.` present; got {snap:?}"
        );
    }

    /// F-029: `register_snapshot` lists only the NON-EMPTY registers, in `"`, `0`, a..z order.
    #[test]
    fn register_snapshot_lists_non_empty_in_order() {
        let mut w = Workspace::new(b"hello\n".to_vec());
        assert!(w.register_snapshot().is_empty(), "nothing set yet");
        w.set_register_raw(Some('b'), b"dd".to_vec());
        w.set_register_raw(Some('a'), b"iZ\x1b".to_vec());
        let snap = w.register_snapshot();
        assert_eq!(
            snap,
            vec![('a', b"iZ\x1b".to_vec()), ('b', b"dd".to_vec())],
            "a before b; empty slots omitted",
        );
    }

    /// F-007 #2: view-local state (cursor/selection/mode) lives in the View; an edit through one View
    /// is visible in the OTHER View (shared buffer), while that other View keeps its own cursor.
    #[test]
    fn edit_is_shared_but_view_state_is_local() {
        let mut w = ws();
        w.split(SplitDir::Vertical);
        w.focus = 0;

        // Enter insert at the start and type into pane 0.
        w.apply(&Command::EnterInsert);
        w.apply(&Command::InsertChar('Z'));
        assert_eq!(w.pane(0).view.mode(), crate::editor::Mode::Insert);

        // Pane 1 sees the edited bytes (shared Document) …
        assert!(
            w.pane(1).doc.bytes().starts_with(b"Zhello"),
            "the edit is visible through the other view"
        );
        // … yet pane 1 is still in its own Normal mode (mode is view-local, not a buffer fact).
        assert_eq!(w.pane(1).view.mode(), crate::editor::Mode::Normal);
    }

    /// F-007 #3: closing one Window keeps the shared buffer while another View holds it.
    #[test]
    fn closing_a_window_keeps_the_shared_buffer() {
        let mut w = ws();
        let did = w.focused().view.doc();
        w.split(SplitDir::Horizontal); // 2 windows, 1 shared doc
        assert!(w.close_focused(), "closes the focused window");
        assert_eq!(w.window_count(), 1);
        assert!(
            w.doc_is_live(did),
            "the buffer survives — the other view still holds it"
        );
        // The surviving view can still edit the buffer.
        w.apply(&Command::EnterInsert);
        w.apply(&Command::InsertChar('X'));
        assert!(w.focused().doc.bytes().starts_with(b"Xhello"));
    }

    /// F-007 #3 (retirement): closing the LAST View that names a buffer retires the buffer. Here both
    /// windows share one doc, so it is retired only after the second close — which is refused (last
    /// window), proving the buffer is never orphaned while a Window holds it.
    #[test]
    fn last_window_close_is_refused() {
        let mut w = ws();
        w.split(SplitDir::Horizontal);
        assert!(w.close_focused());
        assert!(
            !w.close_focused(),
            "the last window cannot be closed via C-w c"
        );
        assert_eq!(w.window_count(), 1);
    }

    /// F-007 #4: Buffer, View, and Window are distinct — two Windows, two Views, one Document.
    #[test]
    fn buffer_view_window_are_distinct() {
        let mut w = ws();
        w.split(SplitDir::Horizontal);
        assert_eq!(w.window_count(), 2, "two windows");
        let v0 = w.windows[0].view;
        let v1 = w.windows[1].view;
        assert_ne!(v0, v1, "two distinct views");
        assert_eq!(
            w.pane(0).view.doc(),
            w.pane(1).view.doc(),
            "one shared document"
        );
    }

    /// `C-w w` cycles focus and wraps.
    #[test]
    fn focus_next_wraps() {
        let mut w = ws();
        w.split(SplitDir::Horizontal); // focus now on the new pane (index 1)
        assert_eq!(w.focus(), 1);
        w.focus_next();
        assert_eq!(w.focus(), 0, "wraps to the first window");
        w.focus_next();
        assert_eq!(w.focus(), 1);
    }

    /// F-007 multi-buffer: a fresh workspace lists exactly its one buffer; `add_buffer` appends without
    /// changing focus, and `focus_buffer` switches the focused window while preserving each buffer's cursor.
    #[test]
    fn add_and_switch_buffers_preserves_per_buffer_cursor() {
        let mut w = ws();
        w.set_focused_buffer_name("a.txt");
        let a = w.focused_buffer();
        assert_eq!(w.buffers().len(), 1, "one buffer to start");

        let b = w.add_buffer(b"second buffer\ntext\n".to_vec(), Some("b.txt".into()));
        assert_eq!(w.buffers().len(), 2, "add_buffer appends to the list");
        assert_eq!(w.focused_buffer(), a, "add_buffer does not change focus");

        // Move the cursor in buffer A, then switch to B — B starts at its own cursor (0).
        w.apply(&Command::Move(1, Motion::Down));
        let a_cursor = w.focused().view.cursor();
        assert!(a_cursor > 0);
        assert!(w.focus_buffer(b), "switch to a live buffer succeeds");
        assert_eq!(w.focused_buffer(), b);
        assert_eq!(w.focused().view.cursor(), 0, "B has its own cursor");

        // Back to A — its cursor is preserved (the hidden view survived).
        assert!(w.focus_buffer(a));
        assert_eq!(
            w.focused().view.cursor(),
            a_cursor,
            "A's cursor is restored on return"
        );
    }

    /// F-007 multi-buffer: `:ls` flags (current/alt/modified), the `#` alternate, and `:bn`/`:bp` cycling.
    #[test]
    fn buffer_list_flags_alternate_and_cycling() {
        let mut w = ws();
        w.set_focused_buffer_name("a.txt");
        let a = w.focused_buffer();
        let b = w.add_buffer(b"bbb".to_vec(), Some("b.txt".into()));
        let c = w.add_buffer(b"ccc".to_vec(), Some("c.txt".into()));

        w.focus_buffer(b); // leaving A → A is the alternate
        assert_eq!(w.alternate(), Some(a));
        let info = w.buffers();
        assert!(info.iter().find(|i| i.id == b).unwrap().current);
        assert!(info.iter().find(|i| i.id == a).unwrap().alt);

        // Cycle forward from B: order is [a, b, c] → next after b is c; wraps c → a.
        w.cycle_buffer(true);
        assert_eq!(w.focused_buffer(), c);
        w.cycle_buffer(true);
        assert_eq!(w.focused_buffer(), a, "cycling wraps");
        // Backward from A wraps to c.
        w.cycle_buffer(false);
        assert_eq!(w.focused_buffer(), c);

        // A modified buffer shows the flag; switching to a dead id fails.
        w.focus_buffer(a);
        w.apply(&Command::EnterInsert);
        w.apply(&Command::InsertChar('X'));
        assert!(w.buffers().iter().find(|i| i.id == a).unwrap().modified);
        assert!(
            !w.focus_buffer(DocumentId(999)),
            "unknown buffer id is a no-op"
        );
    }
}
