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
//! single-window path stays byte-identical to F-007 step (a). The register store is editor-global
//! (D-057 slice 2, #944): the Workspace owns it and lends it into the swapped-out View for that window
//! ([`Workspace::lend`] / [`Workspace::reclaim`]), so a View at rest holds an empty store.
//!
//! **Layout (flat, MVP).** The MVP layout is a flat list of Windows with one split direction
//! ([`SplitDir`]) and a focus index — enough for equal `:split`/`:vsplit` into 2+ panes, `C-w w`
//! (focus next), `C-w c` (close focused). The full recursive layout tree, tab pages, resize
//! constraints, and the rest of the `C-w` family are deferred (docs/design/view-window-workspace.md).
//!
//! **Modules.** This file keeps the arenas and the swap-trick (`lend` → edit → `reclaim`, `with_view`,
//! `with_focused`, `apply`). Each child adds one `impl Workspace` block: `registers` (registers and the
//! OS clipboard), `windows` (layout and focus), `view_state` (per-view cursor, scroll and mark state),
//! `line_cmds` (the Ex line commands), `buffers` (the buffer list), `queries` (read-only text queries at
//! the cursor) and `options` (option levels).

use crate::anchor::{LineGravity, Tracked};
use crate::clipboard::{Clipboard, NoClipboard};
use crate::command::Command;
use crate::document::{Document, DocumentId};
use crate::editor::{apply_command, EditorState, View, ViewOptions};
use crate::effect::Effect;
use crate::pos::BytePos;
use crate::register::RegisterStore;
use crate::transaction::TransactionOrigin;
use crate::undo::UndoLimits;

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
    /// The GLOBAL option values (Vim's global copy of each option), given to every View the workspace
    /// creates so a setting reaches new buffers, not just the first one (#754): the shipped search-case
    /// default (which the app turns ON; the engine/`EditorState::new` default stays vanilla-off for the
    /// differential parity oracle), the profile's caret gravity, the indent config, and every `:set`.
    defaults: ViewOptions,
    /// The global copy of the window-local display options (#615, #616): what a View the workspace creates
    /// for a buffer starts with, unless it replaces the focused window's View (then it keeps that window's
    /// values, as an nvim window keeps its options across `:edit`).
    window_defaults: crate::WindowOptions,
    /// The injected OS-clipboard provider behind the `"+`/`"*` registers (`:help quoteplus`). Defaults to
    /// [`NoClipboard`] (a no-op) so core stays pure and CI — which has no clipboard — is deterministic; the
    /// frontend installs a real shell-out provider at startup via [`Workspace::set_clipboard`], and unit
    /// tests inject [`MemClipboard`](crate::clipboard::MemClipboard).
    clipboard: Box<dyn Clipboard>,
    /// A command applied since the last [`Workspace::take_beep`] failed ([`Effect::Beep`], #1085).
    beeped: bool,
    /// An operator applied since the last [`Workspace::take_refused`] was refused by its register
    /// ([`Effect::RegisterRefused`], #1184).
    refused: bool,
    /// An Ex edit applied since the last [`Workspace::take_not_modifiable`] was refused by
    /// `'nomodifiable'` (#619).
    not_modifiable: bool,
    /// The `"+`/`"*` value the last clipboard pull wrote, when the pull changed the slot, until the frontend
    /// takes it for the trace ([`Workspace::take_clipboard_read`], RFC-0020 R6).
    clipboard_read: Option<crate::register::Register>,
    /// The editor-global register store (Vim registers are shared by every buffer and window; D-057
    /// slice 2, #944). Lent into the focused View for one command by [`Workspace::lend`].
    registers: RegisterStore,
    /// The positions the views at rest pinned in the lent document ([`Workspace::lend`]), read back and
    /// removed by [`Workspace::reclaim`]. Empty between the two.
    pins: Vec<(ViewId, std::ops::Range<usize>)>,
    /// Who causes the edits commands make (INV-ORIGIN): [`TransactionOrigin::UserInput`] unless the
    /// frontend says otherwise ([`Workspace::set_origin`], #880).
    origin: TransactionOrigin,
    /// The undo bound every buffer keeps (`'undolevels'` and the byte budget, RFC-0026 §1), given to each
    /// Document the workspace creates.
    undo_limits: UndoLimits,
}

mod buffers;
mod line_cmds;
mod options;
mod queries;
mod registers;
mod view_state;
mod windows;

pub use options::OptionLevel;
use registers::pull_clipboard;

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
            defaults: ViewOptions::DEFAULT,
            window_defaults: crate::WindowOptions::default(),
            clipboard: Box::new(NoClipboard),
            beeped: false,
            not_modifiable: false,
            refused: false,
            clipboard_read: None,
            registers: RegisterStore::new(),
            pins: Vec::new(),
            origin: TransactionOrigin::UserInput,
            undo_limits: UndoLimits::default(),
        }
    }

    /// The swap-trick's first half: an [`EditorState`] over `doc`/`view` holding the workspace register
    /// store. Every `lend` is paired with a [`Workspace::reclaim`], or the store is lost.
    fn lend(&mut self, mut doc: Document, view: View) -> EditorState {
        // Every OTHER view of this document (the lent one is out of the arena) pins its marks and its
        // cursor, so an edit made through the lent view moves them too (#686), as nvim's `mark_adjust`
        // moves other windows' cursors and jumplists.
        for (i, v) in self.views.iter().enumerate() {
            let Some(v) = v.as_ref().filter(|v| v.doc() == doc.id()) else {
                continue;
            };
            let mut t = v.tracked();
            t.push(Tracked::line(Some(v.cursor()), LineGravity::Cursor));
            let at = doc.pin_tracked(&t);
            self.pins.push((ViewId(i), at..at + t.len()));
        }
        let mut st = EditorState::from_parts(doc, view);
        st.swap_registers(&mut self.registers);
        st.set_origin(self.origin);
        st
    }

    /// Set who causes the edits the commands applied from now on make (INV-ORIGIN, #880): the frontend
    /// sets [`TransactionOrigin::Macro`] while it replays a register's keys (`@{reg}`, `@:`, `Q`) and puts
    /// [`TransactionOrigin::UserInput`] back after. [`Workspace::apply_edits`] takes its origin directly.
    pub fn set_origin(&mut self, origin: TransactionOrigin) {
        self.origin = origin;
    }

    /// Bound the undo history of every buffer, and of each one made later (RFC-0026 §1). The states past
    /// it are dropped at a buffer's next change.
    pub fn set_undo_limits(&mut self, limits: UndoLimits) {
        self.undo_limits = limits;
        for d in self.docs.iter_mut().flatten() {
            d.set_undo_limits(limits);
        }
    }

    /// The undo bound the buffers keep ([`Workspace::set_undo_limits`]).
    #[must_use]
    pub fn undo_limits(&self) -> UndoLimits {
        self.undo_limits
    }

    /// The origin the commands applied now are recorded under ([`Workspace::set_origin`]).
    #[must_use]
    pub fn origin(&self) -> TransactionOrigin {
        self.origin
    }

    /// The swap-trick's second half: take the register store back from `st` and split it into parts.
    fn reclaim(&mut self, mut st: EditorState) -> (Document, View) {
        st.swap_registers(&mut self.registers);
        let (mut doc, view) = st.into_parts();
        let pinned = doc.take_pinned();
        for (vid, range) in std::mem::take(&mut self.pins) {
            let Some(v) = self.views[vid.0].as_mut() else {
                continue;
            };
            let (cursor, marks) = pinned[range]
                .split_last()
                .expect("the cursor is pinned last");
            v.untrack(marks);
            let cursor = cursor.pos.map_or(0, |p| p.0).min(doc.len());
            v.set_cursor(BytePos(crate::motion::snap(doc.bytes(), cursor)));
        }
        (doc, view)
    }

    /// The swap-trick in one place (#769): take view `vid` and its document out of their arena slots, lend
    /// them to `f` as an [`EditorState`] (with the clipboard provider, for the `"+`/`"*` sync), then reclaim
    /// them and put them back. The only caller of [`Workspace::lend`] / [`Workspace::reclaim`], so no path
    /// can forget the register store or the other views' pinned marks.
    fn with_view<R>(
        &mut self,
        vid: ViewId,
        f: impl FnOnce(&mut EditorState, &mut dyn Clipboard) -> R,
    ) -> R {
        let view = self.views[vid.0].take().expect("view live");
        let slot = Self::doc_slot(view.doc());
        let doc = self.docs[slot].take().expect("doc live");
        let mut st = self.lend(doc, view);
        let out = f(&mut st, self.clipboard.as_mut());
        // PUSH the `"+`/`"*` mirror to the OS clipboard only when this call WROTE it (a `"+y`/`"+d`, or
        // `:y +` on any lend path). A paste reads the register and never writes it, so `"+p` leaves the OS
        // clipboard (and any rich data another app put there) alone (#755). `from_utf8_lossy` keeps a
        // non-UTF-8 buffer from ever panicking the write.
        if st.take_clipboard_written() {
            let bytes = st.registers().clipboard().text().to_vec();
            self.clipboard.set(&String::from_utf8_lossy(&bytes));
        }
        let (doc, view) = self.reclaim(st);
        self.docs[slot] = Some(doc);
        self.views[vid.0] = Some(view);
        out
    }

    /// [`Workspace::with_view`] on the FOCUSED window.
    fn with_focused<R>(&mut self, f: impl FnOnce(&mut EditorState) -> R) -> R {
        self.with_view(self.windows[self.focus].view, |st, _| f(st))
    }

    /// Whether a command failed (Vim beeped) since the last call, clearing the flag. The frontend flushes the
    /// keys that were not typed (a macro's rest) when it is set, as Vim's `beep_flush` does (#1085).
    pub fn take_beep(&mut self) -> bool {
        std::mem::take(&mut self.beeped)
    }

    /// Whether an Ex edit was refused by `'nomodifiable'` since the last call (`E21`), clearing the flag.
    pub fn take_not_modifiable(&mut self) -> bool {
        std::mem::take(&mut self.not_modifiable)
    }

    /// Whether an operator was refused by its register ([`Effect::RegisterRefused`]) since the last call,
    /// clearing the flag: the failure behind a beep that nvim saves for `.` (#1184).
    pub fn take_refused(&mut self) -> bool {
        std::mem::take(&mut self.refused)
    }

    /// Record a failure the frontend found itself (an unknown key, Vim's `nv_error`), so it flushes the
    /// untyped keys as for a failed command (#1111).
    pub fn beep(&mut self) {
        self.beeped = true;
    }

    /// Append a fresh View onto `doc` carrying the global option values ([`Workspace::defaults`]), and
    /// return its id. Every View the workspace creates (other than a split's clone) comes from here.
    fn push_view(&mut self, doc: DocumentId) -> ViewId {
        let mut view = View::fresh(doc);
        view.set_options(self.defaults.clone());
        view.set_window_options(self.window_defaults.clone());
        self.views.push(Some(view));
        ViewId(self.views.len() - 1)
    }

    /// The arena slot index for a `DocumentId` (ids are 1-based, assigned monotonically, never reused).
    fn doc_slot(id: DocumentId) -> usize {
        id.0 as usize - 1
    }

    /// The View window `i` shows, mutably (`None` for a retired slot). Panics on an `i` past the
    /// window list, as indexing `windows` does.
    fn window_view_mut(&mut self, i: usize) -> Option<&mut View> {
        let vid = self.windows[i].view;
        self.views[vid.0].as_mut()
    }

    /// Run a command against the FOCUSED Window (the swap-trick — see the module docs). Swaps the
    /// focused `(Document, View)` out of their arena slots into an [`EditorState`], runs the unchanged
    /// `apply_command` pipeline, swaps them back, and returns the [`Effect`]s for the frontend.
    pub fn apply(&mut self, cmd: &Command) -> Vec<Effect> {
        let vid = self.windows[self.focus].view;
        let effects = self.with_view(vid, |st, clipboard| {
            // `"+`/`"*` (`:help quoteplus`): this command touches the system clipboard when the one-shot
            // pending register is `+`/`*` (a `"+y`/`"+p`/`"+d`), or it is an `i_CTRL-R` insert directly from
            // `+`/`*`. The OS clipboard is an impure side effect, so it is synced HERE (the orchestration
            // boundary), never in the pure planner: PULL the external clipboard into the mirror slot before
            // the command so a paste reflects what another app copied. The PUSH of a yank/delete happens in
            // `with_view`, only when the command wrote the mirror.
            let touches_clipboard =
                crate::register::RegisterStore::is_clipboard(st.pending_register())
                    || matches!(
                        cmd,
                        Command::InsertRegister('+') | Command::InsertRegister('*')
                    );
            let pulled = if touches_clipboard {
                pull_clipboard(st, clipboard)
            } else {
                None
            };
            let effects = apply_command(st, cmd);
            (effects, pulled)
        });
        let (effects, pulled) = effects;
        self.note_clipboard_read(pulled);
        self.beeped |= effects.contains(&Effect::Beep);
        self.refused |= effects.contains(&Effect::RegisterRefused);
        effects
    }

    /// The invariant walker over the whole editor state (#906): every buffer and every view by
    /// [`Document::assert_invariants`] and [`View::assert_invariants`], plus the workspace's own rules —
    /// buffer slot `i` holds `DocumentId(i + 1)` (so no id is used twice), every window shows a live view
    /// of a live buffer and no two windows share a view, focus names a window, the buffer list holds each
    /// live buffer once, the alternate buffer is live, and no positions stay pinned between commands.
    /// The flat MVP layout has no tree, so there is no cycle to look for. Panics on the first broken
    /// rule; property and fuzz tests call it after every step.
    pub fn assert_invariants(&self) {
        assert_eq!(
            self.names.len(),
            self.docs.len(),
            "a buffer slot lost its name"
        );
        for (i, d) in self.docs.iter().enumerate() {
            if let Some(d) = d {
                assert_eq!(
                    d.id(),
                    DocumentId(i as u64 + 1),
                    "buffer slot {i} holds another id"
                );
                d.assert_invariants();
            }
        }
        for (i, v) in self.views.iter().enumerate() {
            if let Some(v) = v {
                let d = self
                    .document(v.doc())
                    .unwrap_or_else(|| panic!("view {i} shows retired buffer {:?}", v.doc()));
                v.assert_invariants(d.bytes());
            }
        }
        assert!(!self.windows.is_empty(), "no window");
        assert!(self.focus < self.windows.len(), "focus past the windows");
        let mut shown = std::collections::HashSet::new();
        for w in &self.windows {
            assert!(
                self.views.get(w.view.0).is_some_and(Option::is_some),
                "a window shows retired view {:?}",
                w.view
            );
            assert!(shown.insert(w.view), "two windows share view {:?}", w.view);
        }
        let mut listed = std::collections::HashSet::new();
        for id in &self.buffer_order {
            assert!(
                self.doc_is_live(*id),
                "the buffer list holds retired {id:?}"
            );
            assert!(listed.insert(*id), "{id:?} is listed twice");
        }
        assert_eq!(
            listed.len(),
            self.docs.iter().flatten().count(),
            "a live buffer is not listed"
        );
        if let Some(a) = self.alt {
            assert!(self.doc_is_live(a), "the alternate buffer {a:?} is retired");
        }
        assert!(
            self.pins.is_empty(),
            "positions left pinned after a command"
        );
        for d in self.docs.iter().flatten() {
            assert!(
                d.pinned_is_empty(),
                "{:?} still carries pinned positions",
                d.id()
            );
        }
    }
}

#[cfg(test)]
mod tests;
