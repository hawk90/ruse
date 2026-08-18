//! The file picker overlay (F-013 NAT-3): a fuzzy finder over the files under the working directory. On
//! Enter it opens the selected path into a new buffer (the interactive form of `:e`). A [`Picker`]`<PathBuf>`
//! whose payload is the path to open; the accept action lives in `app::session`.

use std::path::{Path, PathBuf};

use crate::ui::picker::{PickItem, Picker};

/// The most files the walk will collect, and how deep it descends — bounds so a huge tree can't hang the
/// overlay open. A fuller build would honour `.gitignore` and stream results.
const MAX_FILES: usize = 5000;
const MAX_DEPTH: usize = 12;

/// Open a file picker over the current working directory.
pub(crate) fn open() -> Picker<PathBuf> {
    open_in(Path::new("."))
}

/// Open a file picker over `root` (a bounded recursive walk skipping hidden entries and heavy build dirs).
pub(crate) fn open_in(root: &Path) -> Picker<PathBuf> {
    let mut files = Vec::new();
    collect(root, &mut files, 0);
    files.sort();
    let items = files
        .into_iter()
        .map(|p| {
            let display = p.strip_prefix("./").unwrap_or(&p).display().to_string();
            PickItem {
                search: display.clone(),
                display,
                payload: p,
            }
        })
        .collect();
    Picker::new(items)
}

/// Recursively collect file paths under `dir`, skipping dotfiles/dot-dirs (incl. `.git`) and the usual
/// heavy build directories, bounded by [`MAX_FILES`] / [`MAX_DEPTH`].
fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if out.len() >= MAX_FILES || depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            break;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || matches!(name.as_ref(), "target" | "node_modules") {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect(&path, out, depth + 1),
            Ok(ft) if ft.is_file() => out.push(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod file_picker_tests {
    use super::*;

    /// F-013 NAT-3: the picker walks the tree and lists real files; the name query narrows; hidden and
    /// build dirs are skipped. Runs against this crate's own `src/` (cwd is the package root under cargo).
    #[test]
    fn file_picker_walks_and_filters() {
        let p = open_in(Path::new("src"));
        assert!(!p.rows().is_empty(), "src/ has files to list");
        assert!(
            p.rows().iter().any(|(d, _)| d.contains("picker.rs")),
            "the picker source is listed: {:?}",
            p.rows()
        );

        let mut q = open_in(Path::new("src"));
        for c in "picker.rs".chars() {
            q.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert!(!q.rows().is_empty(), "query narrows to picker sources");
        assert!(
            q.rows().iter().all(|(d, _)| d.contains("picker")),
            "every remaining row matches the query"
        );
    }
}
