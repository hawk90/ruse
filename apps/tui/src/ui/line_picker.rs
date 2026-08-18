//! The buffer-line fuzzy picker overlay (F-013 NAT-3): list the focused buffer's lines, filter by a
//! typed query, and on Enter jump the focused cursor to the selected line. A [`Picker`]`<usize>` whose
//! payload is the line's start byte offset; the accept action (jump the cursor) lives in `app::session`.

use crate::ui::picker::{PickItem, Picker};

/// Open a line picker over `bytes`: one item per line (a trailing final newline yields no empty last
/// entry — there is no line to jump to there), each searchable/displayed by its text and carrying the
/// byte offset of its start as the payload.
pub(crate) fn open(bytes: &[u8]) -> Picker<usize> {
    // (line-start byte offset, line text), one per line; a trailing final newline adds no empty entry.
    let mut spans: Vec<(usize, String)> = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            spans.push((
                start,
                String::from_utf8_lossy(&bytes[start..i]).into_owned(),
            ));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        spans.push((start, String::from_utf8_lossy(&bytes[start..]).into_owned()));
    }
    let items = spans
        .into_iter()
        .enumerate()
        .map(|(n, (offset, text))| PickItem {
            display: format!("{:>5}: {text}", n + 1),
            search: text,
            payload: offset,
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod line_picker_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn typed(p: &mut Picker<usize>, s: &str) {
        for c in s.chars() {
            p.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// F-013 NAT-3: the line picker lists every line with its byte offset; a trailing newline yields no
    /// empty final entry; the query narrows by substring and the selection resolves to the line's offset.
    #[test]
    fn line_picker_filters_and_resolves_offsets() {
        let src = b"alpha\nbeta line\ngamma\nbeta again\n";
        let p = open(src);
        assert_eq!(p.rows().len(), 4, "four lines, no empty trailing entry");
        assert_eq!(p.selected(), Some(&0), "first line starts at offset 0");

        let mut q = open(src);
        typed(&mut q, "beta");
        assert_eq!(q.rows().len(), 2, "two lines contain `beta`");
        // Selection defaults to the first match — the line starting at offset 6 (`beta line`).
        assert_eq!(q.selected(), Some(&6));
    }

    /// F-013 NAT-3: an empty query lists all lines; a no-match query resolves to None (nothing to jump to).
    #[test]
    fn line_picker_empty_and_nomatch() {
        let src = b"one\ntwo\nthree";
        let p = open(src);
        assert_eq!(p.rows().len(), 3, "empty query keeps every line");
        assert_eq!(p.selected(), Some(&0));

        let mut none = open(src);
        typed(&mut none, "zzz");
        assert!(none.rows().is_empty());
        assert_eq!(none.selected(), None, "no match resolves to no jump");
    }
}
