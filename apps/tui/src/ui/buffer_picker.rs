//! The buffer picker overlay (F-013 NAT-3): list the open buffers, filter by a typed name query, and on
//! Enter switch the focused window to the selected buffer. A [`Picker`]`<DocumentId>` whose payload is the
//! buffer id; the accept action (`ws.focus_buffer`) lives in `app::session`. Unblocked by the
//! multi-buffer arena (F-007).

use ruse_core::{DocumentId, Workspace};

use crate::ui::picker::{PickItem, Picker};

/// Open a buffer picker over the workspace's buffer list (`:ls` source), preselecting the ALTERNATE
/// buffer so `C-b` then Enter toggles to the last buffer (Vim `:b#`). Each row searches by name and shows
/// `%current`/`#alternate`/`+modified` flags; the payload is the buffer's [`DocumentId`].
pub(crate) fn open(ws: &Workspace) -> Picker<DocumentId> {
    let items = ws
        .buffers()
        .into_iter()
        .map(|b| {
            let cur = if b.current { "%" } else { "" };
            let alt = if b.alt { "#" } else { "" };
            let modified = if b.modified { "+" } else { "" };
            PickItem {
                display: format!("{}{cur}{alt} {}{modified}", b.id.0, b.name),
                search: b.name,
                payload: b.id,
            }
        })
        .collect();
    let mut picker = Picker::new(items);
    // Preselect the alternate buffer. `buffers()` order matches the picker's item order, so the alternate
    // is identified by re-reading the flag — but the payload is the id, so match on the workspace's alt.
    if let Some(alt) = ws.alternate() {
        picker.select_first(|&id| id == alt);
    }
    picker
}

#[cfg(test)]
mod buffer_picker_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        let p = open(&ws);
        assert_eq!(p.rows().len(), 3, "all three buffers listed");
        // Alternate is buffer 1 (alpha.rs) — preselected, so Enter would switch straight to it.
        assert_eq!(p.selected(), Some(&DocumentId(1)));

        let mut q = open(&ws);
        for c in "gamma".chars() {
            q.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(q.rows().len(), 1, "name filter narrows to gamma.txt");
        assert_eq!(q.selected(), Some(&DocumentId(3)));
    }

    /// F-013 NAT-3: a no-match query resolves to no switch.
    #[test]
    fn buffer_picker_nomatch_resolves_to_none() {
        let ws = ws3();
        let mut none = open(&ws);
        for c in "zzz".chars() {
            none.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(none.rows().is_empty());
        assert_eq!(none.selected(), None);
    }
}
