//! Window layout and focus: the flat MVP window list, its split direction, and the `C-w` moves
//! (focus next/prev, split, close, only).

use super::*;

impl Workspace {
    /// Number of Windows currently in the layout.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// The buffer each Window shows, in tile order (the focused one at [`Workspace::focus`]).
    #[must_use]
    pub fn window_buffers(&self) -> Vec<DocumentId> {
        self.windows
            .iter()
            .filter_map(|w| self.views[w.view.0].as_ref().map(View::doc))
            .collect()
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

    /// The View the focused Window shows. Unlike [`Workspace::focus`], it names the window itself: closing
    /// the focused first window leaves the index at 0 but focuses another View.
    #[must_use]
    pub fn focused_view(&self) -> ViewId {
        self.windows[self.focus].view
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

    /// Move focus to the next Window in tile order, wrapping (Vim `C-w w`). No-op with one Window.
    pub fn focus_next(&mut self) {
        if !self.windows.is_empty() {
            self.focus = (self.focus + 1) % self.windows.len();
        }
    }

    /// Move focus to the previous Window in tile order, wrapping (Vim `C-w W`). No-op with one Window.
    pub fn focus_prev(&mut self) {
        if !self.windows.is_empty() {
            self.focus = (self.focus + self.windows.len() - 1) % self.windows.len();
        }
    }

    /// Focus the `i`th Window in tile order (0-based), clamped to the last one (Vim `{count}C-w w`).
    pub fn focus_window(&mut self, i: usize) {
        if !self.windows.is_empty() {
            self.focus = i.min(self.windows.len() - 1);
        }
    }

    /// Split the focused Window into a second Window onto the SAME buffer, in direction `dir`, and
    /// focus the new pane (Vim opens and focuses the new split). The new View is a CLONE of the focused
    /// View — same buffer id, same cursor/scroll/config snapshot — so both panes start at the
    /// same place then diverge independently (F-007 acceptance #1). `dir` sets the whole flat layout's
    /// tiling (MVP: one direction, no tree). Returns the new [`ViewId`]. Registers are not cloned: the
    /// Workspace owns the one store both panes share (#944).
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

    /// Close the focused Window (Vim `C-w c`): drop the layout slot and retire its View. Its buffer stays
    /// loaded, hidden if no other window shows it (F-007 acceptance #3). Refuses to close the LAST Window (returns `false`); an
    /// app-level `:q` handles quitting. On success returns `true` and focus lands on the neighbour.
    pub fn close_focused(&mut self) -> bool {
        if self.windows.len() <= 1 {
            return false; // never leave zero windows — `:q` is the quit path
        }
        let win = self.windows.remove(self.focus);
        // Retire only the View. The Document stays loaded even when no window shows it any more: it becomes
        // hidden, still `:ls`-listed and reopenable with `:b`, as with `:only`. Freeing it would drop unsaved
        // edits (#736).
        self.views[win.view.0] = None;

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
}
