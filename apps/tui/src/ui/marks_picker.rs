//! The `:marks` viewer overlay (F-003 / F-026): list the set marks — named `a`-`z`, the `.` last-change
//! mark, and the `^` last-insert mark — each with its 1-based line and a short text preview. A
//! [`Picker`]`<usize>` whose payload is the mark's byte offset; on Enter the session jumps the cursor there
//! (like the diagnostics picker).

use crate::ui::picker::{PickItem, Picker};

/// The 1-based line number of byte offset `pos`, its 0-based byte column within that line (Vim's `:marks`
/// col base), and that line's text (trimmed for the preview). Shared with the `:jumps`/`:changes` position
/// viewers ([`crate::ui::pos_picker`]).
pub(crate) fn line_col_text(bytes: &[u8], pos: usize) -> (usize, usize, String) {
    let pos = pos.min(bytes.len());
    let line = bytes[..pos].iter().filter(|&&b| b == b'\n').count() + 1;
    let start = bytes[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let col = pos - start; // 0-based byte column, like Vim's `:marks`/`:jumps` col
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |i| start + i);
    let text = String::from_utf8_lossy(&bytes[start..end])
        .trim()
        .chars()
        .take(50)
        .collect();
    (line, col, text)
}

/// Open a marks viewer over a `(name, byte offset)` snapshot (see `Workspace::marks_snapshot`) against the
/// buffer `bytes`. Each row mirrors Vim's `:marks` columns — `{mark}  {line} {col}  {text}` — and Enter jumps
/// to the offset.
pub(crate) fn open(snapshot: Vec<(char, usize)>, bytes: &[u8]) -> Picker<usize> {
    let items = snapshot
        .into_iter()
        .map(|(name, pos)| {
            let (line, col, text) = line_col_text(bytes, pos);
            PickItem {
                display: format!("{name}  {line:>4} {col:>3}  {text}"),
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
    fn line_col_text_reports_line_col_and_trimmed_text() {
        let b = b"first\n  second line  \nthird";
        assert_eq!(line_col_text(b, 0), (1, 0, "first".to_string()));
        // A byte inside line 2 → line 2, 0-based byte col from the line start, trimmed text.
        assert_eq!(line_col_text(b, 8), (2, 2, "second line".to_string()));
        assert_eq!(line_col_text(b, b.len()), (3, 5, "third".to_string()));
    }

    #[test]
    fn open_lists_each_mark_with_line_col_and_offset_payload() {
        let b = b"aa\nbb\ncc";
        let p = open(vec![('a', 0), ('.', 4)], b);
        assert_eq!(p.rows().len(), 2);
        // Vim `:marks` column order: mark, line, col, text.
        assert_eq!(
            p.rows()[0].0,
            "a     1   0  aa",
            "mark a at line 1 col 0; got {:?}",
            p.rows()
        );
        assert_eq!(
            p.rows()[1].0,
            ".     2   1  bb",
            "last-change at line 2 col 1; got {:?}",
            p.rows()
        );
    }
}
