//! A position-list viewer overlay (F-003 / F-026) shared by `:jumps` and `:changes`: list byte offsets,
//! each shown as `{n}  L{line}  {preview}` (oldest → newest, `n` 1-based), and jump the cursor to the
//! selected one on Enter. A [`Picker`]`<usize>` whose payload is the byte offset (like the marks picker).

use crate::ui::marks_picker::line_and_text;
use crate::ui::picker::{PickItem, Picker};

/// Open a position viewer over `positions` (byte offsets, oldest → newest) against the buffer `bytes`.
pub(crate) fn open(positions: Vec<usize>, bytes: &[u8]) -> Picker<usize> {
    let items = positions
        .into_iter()
        .enumerate()
        .map(|(n, pos)| {
            let (line, text) = line_and_text(bytes, pos);
            PickItem {
                display: format!("{:>3}  L{line}  {text}", n + 1),
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
    fn open_numbers_positions_and_carries_offset_payload() {
        let b = b"aa\nbb\ncc";
        let p = open(vec![0, 6], b); // line 1 and line 3
        assert_eq!(p.rows().len(), 2);
        assert!(
            p.rows().iter().any(|(t, _)| t.contains("1  L1  aa")),
            "first entry numbered 1 on line 1; got {:?}",
            p.rows()
        );
        assert!(
            p.rows().iter().any(|(t, _)| t.contains("2  L3  cc")),
            "second entry on line 3",
        );
    }
}
