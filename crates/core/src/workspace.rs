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
use crate::editor::{apply_command, EditorState, View};
use crate::effect::Effect;

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
}

impl Workspace {
    /// A fresh workspace over `initial` bytes: one buffer, one View, one Window, focused. Reuses
    /// [`EditorState::new`]'s initialisation (mark-saved + a fresh View over `DocumentId(1)`) so the
    /// single-window buffer is identical to the pre-Workspace path.
    #[must_use]
    pub fn new(initial: impl Into<Vec<u8>>) -> Workspace {
        let (doc, view) = EditorState::new(initial).into_parts();
        Workspace {
            docs: vec![Some(doc)],
            views: vec![Some(view)],
            windows: vec![Window { view: ViewId(0) }],
            split: SplitDir::Horizontal,
            focus: 0,
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
        }

        if self.focus >= self.windows.len() {
            self.focus = self.windows.len() - 1; // closed the last-in-order pane
        }
        true
    }

    /// Whether a `DocumentId` still has a live buffer slot (for tests / retirement assertions).
    #[must_use]
    pub fn doc_is_live(&self, id: DocumentId) -> bool {
        self.docs
            .get(Self::doc_slot(id))
            .is_some_and(Option::is_some)
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
}
