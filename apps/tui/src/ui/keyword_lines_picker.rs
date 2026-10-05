//! The `[I` / `]I` listing overlay: dump the buffer lines that contain the keyword under the cursor
//! (`:help [I`). A [`Picker`]`<usize>` whose payload is the match's 1-based line number; it is VIEW-ONLY
//! (Vim's `[I`/`]I` only display — they do not jump to a selection), so the session just closes it on
//! Enter, mirroring the `:registers` / `:digraphs` viewers.
//!
//! Each row mirrors nvim v0.12.4's `[I` columns: `{idx:>3}: {lnum:>4} {text}` — the 1-based position in
//! the LIST, a colon, the 1-based line number, then the line text verbatim (indentation preserved). The
//! per-buffer header line nvim prints above the list (the file path) is shown on the status line instead
//! (a documented divergence: the overlay is a flat searchable list, not a titled dump).

use crate::ui::picker::{PickItem, Picker};

/// Render one `[I` row: `{idx:>3}: {lnum:>4} {text}` (verified byte-for-byte against nvim v0.12.4). `idx`
/// is the 1-based position in the displayed list; `lnum` is the buffer line number; `text` is verbatim.
fn row(idx: usize, lnum: usize, text: &str) -> String {
    format!("{idx:>3}: {lnum:>4} {text}")
}

/// Open the listing over `(line_number, line_text)` matches (already resolved + ordered by the frontend
/// via `ruse_core::keyword_list`). View-only; payload is the line number. An empty `matches` yields an
/// empty picker (the caller avoids opening one in that case).
pub(crate) fn open(matches: &[(usize, String)]) -> Picker<usize> {
    let items = matches
        .iter()
        .enumerate()
        .map(|(i, (lnum, text))| PickItem {
            display: row(i + 1, *lnum, text),
            // Searchable by line number and text.
            search: format!("{lnum} {text}"),
            payload: *lnum,
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_matches_nvim_column_layout() {
        // nvim `[I` prints `  1:    1 alpha foo beta` — idx right-aligned width 3, `: `, lnum width 4, ` `.
        assert_eq!(row(1, 1, "alpha foo beta"), "  1:    1 alpha foo beta");
        // Indentation of the matched line is preserved verbatim.
        assert_eq!(
            row(2, 2, "    indented foo here"),
            "  2:    2     indented foo here"
        );
        assert_eq!(row(4, 5, "theta foo iota"), "  4:    5 theta foo iota");
    }

    #[test]
    fn open_lists_each_match_with_line_payload() {
        let p = open(&[(1, "foo a1".into()), (3, "foo c3".into())]);
        assert_eq!(p.rows().len(), 2);
        assert_eq!(p.rows()[0].0, "  1:    1 foo a1");
        assert_eq!(p.rows()[1].0, "  2:    3 foo c3");
        assert_eq!(p.selected().copied(), Some(1)); // payload = first match's line number
    }
}
