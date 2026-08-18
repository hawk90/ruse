//! The buffer picker overlay (F-013 NAT-3): list the open buffers, filter by a typed name query, and on
//! Enter switch the focused window to the selected buffer. Unblocked by the multi-buffer arena (F-007).

use crossterm::event::KeyCode;

use ruse_core::{DocumentId, Workspace};

/// One buffer row: its id, display name, and status flags — the same source as `:ls`. Search matches on
/// the name only (not the id/flags), so typing filters by what the user reads.
struct Item {
    name: String,
    id: DocumentId,
    current: bool,
    alt: bool,
    modified: bool,
}

/// A buffer picker "special view" overlay (view-window-workspace.md §7 VW-OVERLAY / F-013 NAT-3) — the
/// same modal-overlay shape as the command [`Palette`] and the [`LinePicker`], over a different source
/// (the open buffers) and a different action (switch buffer, not a [`Command`] or a cursor jump). Opens
/// with the ALTERNATE buffer preselected, so `C-b` then Enter toggles to the last buffer (Vim `:b#`).
pub(crate) struct BufferPicker {
    /// The typed filter (matched against buffer names).
    pub(crate) query: String,
    /// Every listed buffer, in `:ls` order.
    items: Vec<Item>,
    /// Indices into `items` matching the current query.
    matches: Vec<usize>,
    /// Selected row into `matches`.
    selected: usize,
}

impl BufferPicker {
    /// Open over the workspace's buffer list, preselecting the alternate buffer (else the first).
    pub(crate) fn open(ws: &Workspace) -> BufferPicker {
        let items: Vec<Item> = ws
            .buffers()
            .into_iter()
            .map(|b| Item {
                name: b.name,
                id: b.id,
                current: b.current,
                alt: b.alt,
                modified: b.modified,
            })
            .collect();
        let mut p = BufferPicker {
            query: String::new(),
            items,
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        // Preselect the alternate buffer for a quick toggle (falls back to row 0 when there is none).
        if let Some(pos) = p.matches.iter().position(|&i| p.items[i].alt) {
            p.selected = pos;
        }
        p
    }

    /// Recompute `matches` from the query (case-insensitive substring on the buffer name — the same match
    /// rule as the other pickers).
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| q.is_empty() || it.name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// The `DocumentId` of the selected buffer, or `None` when nothing matches.
    fn selected_id(&self) -> Option<DocumentId> {
        self.matches.get(self.selected).map(|&i| self.items[i].id)
    }

    /// The overlay's match rows (`"<id><%/#> <name><+>"`, selected flag) — same paint slot as the other
    /// pickers. `%` = current, `#` = alternate, `+` = modified (mirrors `:ls`).
    pub(crate) fn rows(&self) -> Vec<(String, bool)> {
        self.matches
            .iter()
            .enumerate()
            .map(|(row, &i)| {
                let it = &self.items[i];
                let cur = if it.current { "%" } else { "" };
                let alt = if it.alt { "#" } else { "" };
                let modified = if it.modified { "+" } else { "" };
                (
                    format!("{}{cur}{alt} {}{modified}", it.id.0, it.name),
                    row == self.selected,
                )
            })
            .collect()
    }
}

/// Handle one key of the buffer picker (F-013 NAT-3). Enter switches the focused window to the selected
/// buffer (`ws.focus_buffer`); Esc closes; typing filters by name.
pub(crate) fn buffer_picker_key(
    picker: &mut Option<BufferPicker>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
) {
    let Some(p) = picker.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => *picker = None,
        KeyCode::Enter => {
            let id = p.selected_id();
            *picker = None;
            if let Some(id) = id {
                ws.focus_buffer(id);
            }
        }
        KeyCode::Up => p.selected = p.selected.saturating_sub(1),
        KeyCode::Down if p.selected + 1 < p.matches.len() => p.selected += 1,
        KeyCode::Backspace => {
            p.query.pop();
            p.refilter();
        }
        KeyCode::Char(c) => {
            p.query.push(c);
            p.refilter();
        }
        _ => {}
    }
}

#[cfg(test)]
mod buffer_picker_tests {
    use super::*;

    /// A workspace with three named buffers; buffer 2 is focused, so buffer 1 is the alternate.
    fn ws3() -> Workspace {
        let mut ws = Workspace::new(b"one".to_vec());
        ws.set_focused_buffer_name("alpha.rs");
        let b = ws.add_buffer(b"two".to_vec(), Some("beta.rs".into()));
        ws.add_buffer(b"three".to_vec(), Some("gamma.txt".into()));
        ws.focus_buffer(b); // focus beta → alpha becomes the alternate
        ws
    }

    /// F-013 NAT-3: the picker lists every buffer, preselects the alternate, and the name query narrows.
    #[test]
    fn buffer_picker_lists_and_preselects_alternate() {
        let ws = ws3();
        let p = BufferPicker::open(&ws);
        assert_eq!(p.matches.len(), 3, "all three buffers listed");
        // Alternate is buffer 1 (alpha.rs) — preselected, so Enter would switch straight to it.
        assert_eq!(p.selected_id(), Some(DocumentId(1)));

        let mut q = BufferPicker::open(&ws);
        for c in "gamma".chars() {
            q.query.push(c);
            q.refilter();
        }
        assert_eq!(q.matches.len(), 1, "name filter narrows to gamma.txt");
        assert_eq!(q.selected_id(), Some(DocumentId(3)));
    }

    /// F-013 NAT-3: a no-match query resolves to no switch; Enter on it is a no-op.
    #[test]
    fn buffer_picker_nomatch_resolves_to_none() {
        let ws = ws3();
        let mut none = BufferPicker::open(&ws);
        for c in "zzz".chars() {
            none.query.push(c);
            none.refilter();
        }
        assert!(none.matches.is_empty());
        assert_eq!(none.selected_id(), None);
    }
}
