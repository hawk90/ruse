//! The buffer-line fuzzy picker overlay (F-013 NAT-3): list the focused buffer's lines, filter by a
//! typed query, and on Enter jump the focused cursor to the selected line.

use crossterm::event::KeyCode;

use ruse_core::Workspace;

/// A buffer-line fuzzy picker — the first picker "special view" overlay (view-window-workspace.md §7
/// VW-OVERLAY / F-013 NAT-3). It lists the focused buffer's lines, filters them by a typed query, and on
/// Enter jumps the focused cursor to the selected line. Structurally it is the same modal overlay as the
/// command [`Palette`] — a query, a filtered match list, a selection, and a transient keymap that owns the
/// keystream while open — but over a different item source (lines) and a different action (jump, not a
/// [`Command`]). The eventual home is one generic picker over VW-OVERLAY's `OverlayStack`; folding the
/// command palette into it is a follow-up. Buffer/file pickers wait on the multi-buffer arena.
pub(crate) struct LinePicker {
    /// The typed filter.
    pub(crate) query: String,
    /// Every line as `(display/search text, byte offset of the line start)`, in file order.
    lines: Vec<(String, usize)>,
    /// Indices into `lines` that match the current query (a subset).
    matches: Vec<usize>,
    /// Selected row into `matches`.
    selected: usize,
}

impl LinePicker {
    /// Open over `bytes`: one entry per line (a trailing final newline does not yield an empty last
    /// entry — there is no line to jump to there), each carrying the byte offset of its start.
    pub(crate) fn open(bytes: &[u8]) -> LinePicker {
        let mut lines = Vec::new();
        let mut start = 0usize;
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                lines.push((
                    String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                    start,
                ));
                start = i + 1;
            }
        }
        if start < bytes.len() {
            lines.push((String::from_utf8_lossy(&bytes[start..]).into_owned(), start));
        }
        let mut p = LinePicker {
            query: String::new(),
            lines,
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Recompute `matches` from the query (case-insensitive substring on the line text — the same match
    /// rule as the command palette; a fuzzy subsequence score is a later refinement).
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, (text, _))| q.is_empty() || text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// The byte offset of the selected line's start, or `None` when nothing matches.
    fn selected_offset(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|&i| self.lines[i].1)
    }

    /// The overlay's match rows (`"<1-based file line>: <text>"`, selected flag), reusing the same
    /// above-the-status-line paint path as the command palette.
    pub(crate) fn rows(&self) -> Vec<(String, bool)> {
        self.matches
            .iter()
            .enumerate()
            .map(|(row, &i)| {
                (
                    format!("{:>5}: {}", i + 1, self.lines[i].0),
                    row == self.selected,
                )
            })
            .collect()
    }
}

/// Handle one key of the line picker (F-013 NAT-3). Enter jumps the focused cursor to the selected line
/// (the frontend's per-frame viewport pass then scrolls it into view); Esc closes; typing filters.
pub(crate) fn line_picker_key(
    picker: &mut Option<LinePicker>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
) {
    let Some(p) = picker.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => *picker = None,
        KeyCode::Enter => {
            let offset = p.selected_offset();
            *picker = None;
            if let Some(offset) = offset {
                ws.place_focused_cursor(offset);
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
mod line_picker_tests {
    use super::*;

    /// F-013 NAT-3: the line picker lists every line with its byte offset; a trailing newline yields no
    /// empty final entry; the query narrows by substring and the selection resolves to the line's offset.
    #[test]
    fn line_picker_filters_and_resolves_offsets() {
        let src = b"alpha\nbeta line\ngamma\nbeta again\n";
        let p = LinePicker::open(src);
        assert_eq!(p.lines.len(), 4, "four lines, no empty trailing entry");
        assert_eq!(p.lines[0], ("alpha".to_string(), 0));
        assert_eq!(p.lines[2].1, 16, "gamma starts after the first two lines");

        let mut q = LinePicker::open(src);
        for c in "beta".chars() {
            q.query.push(c);
            q.refilter();
        }
        assert_eq!(q.matches.len(), 2, "two lines contain `beta`");
        // Selection defaults to the first match — the line starting at offset 6 (`beta line`).
        assert_eq!(q.selected_offset(), Some(6));
    }

    /// F-013 NAT-3: an empty query lists all lines; selection moves clamp; a no-match query resolves to
    /// None (nothing to jump to).
    #[test]
    fn line_picker_empty_and_nomatch() {
        let src = b"one\ntwo\nthree";
        let p = LinePicker::open(src);
        assert_eq!(p.matches.len(), 3, "empty query keeps every line");
        assert_eq!(p.selected_offset(), Some(0));

        let mut none = LinePicker::open(src);
        for c in "zzz".chars() {
            none.query.push(c);
            none.refilter();
        }
        assert!(none.matches.is_empty());
        assert_eq!(none.selected_offset(), None, "no match resolves to no jump");
    }
}
