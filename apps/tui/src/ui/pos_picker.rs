//! A position-list viewer overlay (F-003 / F-026) shared by `:jumps` and `:changes`: list byte offsets,
//! each shown with Vim's `line col text` columns (`{n}  {line} {col}  {preview}`, oldest → newest, `n`
//! 1-based), and jump the cursor to the selected one on Enter. A [`Picker`]`<usize>` whose payload is the
//! byte offset (like the marks picker). Vim's `>` current-position marker has no picker equivalent — the
//! interactive selection highlight is its analogue.

use crate::ui::marks_picker::line_col_text;
use crate::ui::picker::{PickItem, Picker};

/// Open a position viewer over `positions` (byte offsets, oldest → newest) against the buffer `bytes`.
pub(crate) fn open(positions: Vec<usize>, bytes: &[u8]) -> Picker<usize> {
    let items = positions
        .into_iter()
        .enumerate()
        .map(|(n, pos)| {
            let (line, col, text) = line_col_text(bytes, pos);
            PickItem {
                display: format!("{:>3}  {line:>4} {col:>3}  {text}", n + 1),
                search: text,
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
    fn open_numbers_positions_with_line_col_and_offset_payload() {
        let b = b"aa\nbb\ncc";
        let p = open(vec![0, 7], b); // line 1 col 0, and line 3 col 1
        assert_eq!(p.rows().len(), 2);
        // `{n:>3}  {line:>4} {col:>3}  {text}` — Vim's jump/change `line col text` columns.
        assert_eq!(
            p.rows()[0].0,
            "  1     1   0  aa",
            "first entry numbered 1 at line 1 col 0; got {:?}",
            p.rows()
        );
        assert_eq!(
            p.rows()[1].0,
            "  2     3   1  cc",
            "second entry at line 3 col 1; got {:?}",
            p.rows()
        );
    }
}
