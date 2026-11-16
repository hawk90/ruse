//! The buffer list: add, focus, cycle, remove, name and reload buffers, and look them up by id.

use super::*;

impl Workspace {
    /// The focused buffer, mutably — for a durable save to `mark_saved()` after writing to disk.
    pub fn focused_doc_mut(&mut self) -> &mut Document {
        let did = self.focused_buffer();
        self.docs[Self::doc_slot(did)]
            .as_mut()
            .expect("focused doc live")
    }

    /// Make `id` the alternate buffer (`None` clears it), as `:keepalt` restores it (#609).
    pub fn set_alternate(&mut self, id: Option<DocumentId>) {
        self.alt = id;
    }

    /// Name (or rename) buffer `id`; a retired id is ignored (`:cd` renames every buffer, #610).
    pub fn set_buffer_name(&mut self, id: DocumentId, name: impl Into<String>) {
        if let Some(slot) = self.names.get_mut(Self::doc_slot(id)) {
            *slot = Some(name.into());
        }
    }

    /// Remove the FOCUSED buffer's name (`:0file`, #610).
    pub fn clear_focused_buffer_name(&mut self) {
        let slot = Self::doc_slot(self.focused_buffer());
        self.names[slot] = None;
    }

    /// Whether a `DocumentId` still has a live buffer slot (for tests / retirement assertions).
    #[must_use]
    pub fn doc_is_live(&self, id: DocumentId) -> bool {
        self.document(id).is_some() // id 0 is no buffer (#870), never a slot underflow
    }

    /// The document of buffer `id`, or `None` for a retired slot. Read-only, so a frontend can observe an
    /// unfocused buffer (e.g. to journal its unsaved bytes) without focusing it.
    #[must_use]
    pub fn document(&self, id: DocumentId) -> Option<&Document> {
        let slot = (id.0 as usize).checked_sub(1)?;
        self.docs.get(slot).and_then(Option::as_ref)
    }

    /// The buffer `id`, mutably, or `None` if it is not live (an undo block or anchor owner that outlived
    /// a buffer switch).
    pub fn document_mut(&mut self, id: DocumentId) -> Option<&mut Document> {
        let slot = (id.0 as usize).checked_sub(1)?;
        self.docs.get_mut(slot).and_then(Option::as_mut)
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
        let last = self.docs[slot]
            .as_ref()
            .map_or(crate::pos::Revision::ZERO, Document::revision);
        let modifiable = self.docs[slot].as_ref().is_none_or(Document::is_modifiable);
        let mut doc = Document::replacing(id, bytes, last);
        doc.mark_saved();
        doc.set_modifiable(modifiable);
        // Every other view of the buffer keeps its place, clamped into the new text (#686, #687).
        for v in self.views.iter_mut().flatten().filter(|v| v.doc() == id) {
            let c = crate::motion::snap(doc.bytes(), v.cursor().0);
            v.set_cursor(BytePos(c));
            v.snap_offsets(doc.bytes());
        }
        self.docs[slot] = Some(doc);
        self.place_focused_cursor(BytePos(0));
    }

    /// Re-read buffer `id` from its file's new `bytes` as ONE undoable change (nvim's `'autoread'` reload,
    /// which `u` takes back while the file is under `'undoreload'`), then mark it saved. Every view of it
    /// keeps its place, clamped into the new text. `'nomodifiable'` does not stop a reload. Returns whether
    /// the text changed.
    pub fn reload_undoable(&mut self, id: DocumentId, bytes: Vec<u8>) -> bool {
        let Some(doc) = self.document(id) else {
            return false;
        };
        if doc.bytes() == bytes.as_slice() {
            if let Some(d) = self.document_mut(id) {
                d.mark_saved();
            }
            return false;
        }
        let ok = if id == self.focused_buffer() {
            self.with_focused(|st| st.with_tracked(|d, t| d.reload_tracked(bytes, t)))
        } else {
            match self.document_mut(id) {
                Some(d) => d.reload_tracked(bytes, &mut []),
                None => false,
            }
        };
        let slot = Self::doc_slot(id);
        if let Some(doc) = self.docs[slot].as_ref() {
            for v in self.views.iter_mut().flatten().filter(|v| v.doc() == id) {
                let c = crate::motion::snap(doc.bytes(), v.cursor().0);
                v.set_cursor(BytePos(c));
                v.snap_offsets(doc.bytes());
            }
        }
        ok
    }

    /// Add a new buffer over `bytes` with optional display `name`, plus a fresh View onto it (not shown
    /// in any window yet). Appends to the buffer list and returns the new buffer's id. `focus_buffer`
    /// brings it into the focused window.
    pub fn add_buffer(&mut self, bytes: impl Into<Vec<u8>>, name: Option<String>) -> DocumentId {
        let id = DocumentId(self.docs.len() as u64 + 1);
        // Stability §1: a duplicate DocumentId is an invariant violation, not an error.
        debug_assert!(
            self.docs.iter().flatten().all(|d| d.id() != id),
            "duplicate {id:?}"
        );
        let mut doc = Document::new(id, bytes);
        doc.mark_saved();
        self.docs.push(Some(doc));
        self.names.push(name);
        self.push_view(id);
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
            // A buffer new to this window keeps the window's display options (nvim's window-local
            // options stay with the window across `:edit` / `:buffer`).
            let opts = self.focused().view.window_options().clone();
            let vid = self.push_view(id);
            if let Some(v) = self.views[vid.0].as_mut() {
                v.set_window_options(opts);
            }
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
                let nv = self.push_view(replacement);
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
}
