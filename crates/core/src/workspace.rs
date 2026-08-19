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

use crate::command::Command;
use crate::document::{Document, DocumentId};
use crate::editor::{
    apply_command, CaretGravity, EditorState, GlobalCmd, LineAddr, SubFlags, SubOutcome, SubRange,
    Substitution, View,
};
use crate::effect::Effect;
use crate::pattern::RegexError;

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
        }
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
        let effects = apply_command(&mut st, cmd);
        let (doc, view) = st.into_parts();

        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        effects
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
    pub fn sort_lines(
        &mut self,
        range: SubRange,
        reverse: bool,
        numeric: bool,
        unique: bool,
    ) -> usize {
        let vid = self.windows[self.focus].view;
        let view = self.views[vid.0].take().expect("focused view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("focused doc live");

        let mut st = EditorState::from_parts(doc, view);
        let n = st.sort_lines(range, reverse, numeric, unique);
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
        });
        assert_eq!(w.focused().doc.bytes(), b"one\none\ntwo\ntwo\nthree\n");

        // An unterminated last line still yanks a clean linewise block.
        let mut w = Workspace::new(b"a\nb".to_vec());
        assert_eq!(w.yank_lines(SubRange::Lines(2, 2)), 1);
        w.apply(&Command::Paste {
            after: true,
            count: 1,
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

    /// A pathological `{count}p` (a digit-spam count that saturates to `u32::MAX`) must not request a
    /// multi-gigabyte allocation: the paste is clamped to a bounded number of bytes and completes.
    #[test]
    fn huge_paste_count_is_bounded() {
        let mut w = Workspace::new(b"ab\n".to_vec());
        w.yank_lines(SubRange::CurrentLine); // register := "ab\n" (linewise)
        w.apply(&Command::Paste {
            after: true,
            count: u32::MAX,
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
        let mut w = Workspace::new(b"banana\napple\ncherry\n".to_vec());
        w.sort_lines(SubRange::WholeFile, false, false, false);
        assert_eq!(w.focused().doc.bytes(), b"apple\nbanana\ncherry\n");

        // numeric: 10 sorts after 2, not lexicographically before it.
        let mut w = Workspace::new(b"10\n2\n1\n".to_vec());
        w.sort_lines(SubRange::WholeFile, false, true, false);
        assert_eq!(w.focused().doc.bytes(), b"1\n2\n10\n");

        // reverse (descending).
        let mut w = Workspace::new(b"a\nb\nc\n".to_vec());
        w.sort_lines(SubRange::WholeFile, true, false, false);
        assert_eq!(w.focused().doc.bytes(), b"c\nb\na\n");

        // unique drops duplicate lines after sorting.
        let mut w = Workspace::new(b"b\na\nb\na\n".to_vec());
        assert_eq!(
            w.sort_lines(SubRange::WholeFile, false, false, true),
            2,
            "2 dupes removed"
        );
        assert_eq!(w.focused().doc.bytes(), b"a\nb\n");

        // A range limits the sort to those lines; the rest stays put.
        let mut w = Workspace::new(b"z\n3\n1\n2\n".to_vec());
        w.sort_lines(SubRange::Lines(2, 4), false, true, false);
        assert_eq!(w.focused().doc.bytes(), b"z\n1\n2\n3\n");
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
