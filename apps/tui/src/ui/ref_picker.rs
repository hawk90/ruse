//! The LSP references picker overlay (F-014): list every reference the language server returned for the
//! symbol under the cursor, filter by a typed path/position query, and on Enter jump to the selected
//! location. A [`Picker`]`<(uri, line, character)>` whose payload is the raw LSP location; the accept
//! action (open-or-focus the file, then place the cursor) lives in `app::session`, reusing the goto path.

use std::path::Path;

use crate::ui::picker::{PickItem, Picker};

/// Open a references picker over the LSP `Location`s (`(uri, line, character)`, LSP 0-based). Each row
/// shows `relpath:line:col` (1-based, human-facing), relativized to `root` when possible; the payload is
/// the raw location so the caller can jump there. Preserves the server's order (usually by file/position).
pub(crate) fn open(locs: Vec<(String, u32, u32)>, root: &Path) -> Picker<(String, u32, u32)> {
    let items = locs
        .into_iter()
        .map(|(uri, line, col)| {
            let path = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
            let rel = Path::new(&path)
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.clone());
            let display = format!("{}:{}:{}", rel, line + 1, col + 1);
            PickItem {
                search: display.clone(),
                display,
                payload: (uri, line, col),
            }
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod ref_picker_tests {
    use super::*;

    #[test]
    fn rows_are_relative_one_based_positions() {
        let root = Path::new("/home/u/proj");
        let locs = vec![
            ("file:///home/u/proj/src/a.rs".to_string(), 0, 0),
            ("file:///home/u/proj/src/b.rs".to_string(), 41, 8),
        ];
        let p = open(locs, root);
        let rows: Vec<String> = p.rows().into_iter().map(|(r, _)| r).collect();
        assert_eq!(rows[0], "src/a.rs:1:1");
        assert_eq!(rows[1], "src/b.rs:42:9");
        // The payload keeps the raw LSP 0-based location for the jump.
        assert_eq!(
            p.selected(),
            Some(&("file:///home/u/proj/src/a.rs".to_string(), 0, 0))
        );
    }
}
