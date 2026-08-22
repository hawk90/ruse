//! The `:marks` viewer overlay (F-003 / F-026): list the set marks — named `a`-`z`, the `.` last-change
//! mark, and the `^` last-insert mark — each with its 1-based line and a short text preview. A
//! [`Picker`]`<usize>` whose payload is the mark's byte offset; on Enter the session jumps the cursor there
//! (like the diagnostics picker).

use crate::ui::picker::{PickItem, Picker};

/// The 1-based line number of byte offset `pos` and that line's text (trimmed for the preview). Shared with
/// the `:jumps`/`:changes` position viewers ([`crate::ui::pos_picker`]).
pub(crate) fn line_and_text(bytes: &[u8], pos: usize) -> (usize, String) {
    let pos = pos.min(bytes.len());
    let line = bytes[..pos].iter().filter(|&&b| b == b'\n').count() + 1;
    let start = bytes[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |i| start + i);
    let text = String::from_utf8_lossy(&bytes[start..end])
        .trim()
        .chars()
        .take(50)
        .collect();
    (line, text)
}

/// Open a marks viewer over a `(name, byte offset)` snapshot (see `Workspace::marks_snapshot`) against the
/// buffer `bytes` (for the line + preview). Each row is `mark  L{line}  {text}`; Enter jumps to the offset.
pub(crate) fn open(snapshot: Vec<(char, usize)>, bytes: &[u8]) -> Picker<usize> {
    let items = snapshot
        .into_iter()
        .map(|(name, pos)| {
            let (line, text) = line_and_text(bytes, pos);
            PickItem {
                display: format!("{name}  L{line}  {text}"),
                search: format!("{name} {text}"),
                payload: pos,
            }
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_text_reports_1_based_line_and_trimmed_text() {
        let b = b"first\n  second line  \nthird";
        assert_eq!(line_and_text(b, 0), (1, "first".to_string()));
        // A byte inside line 2 → line 2, trimmed text.
        assert_eq!(line_and_text(b, 8), (2, "second line".to_string()));
        assert_eq!(line_and_text(b, b.len()), (3, "third".to_string()));
    }

    #[test]
    fn open_lists_each_mark_with_line_and_offset_payload() {
        let b = b"aa\nbb\ncc";
        let p = open(vec![('a', 0), ('.', 3)], b);
        assert_eq!(p.rows().len(), 2);
        assert!(
            p.rows().iter().any(|(t, _)| t.contains("a  L1  aa")),
            "mark a on line 1; got {:?}",
            p.rows()
        );
        assert!(
            p.rows().iter().any(|(t, _)| t.contains(".  L2  bb")),
            "last-change on line 2",
        );
    }
}
