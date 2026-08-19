//! The file picker overlay (F-013 NAT-3): a fuzzy finder over the files under the working directory. On
//! Enter it opens the selected path into a new buffer (the interactive form of `:e`). A [`Picker`]`<PathBuf>`
//! whose payload is the path to open; the accept action lives in `app::session`.

use std::path::{Path, PathBuf};

use crate::ui::picker::{PickItem, Picker};

/// The most files the walk will collect, and how deep it descends — bounds so a huge tree can't hang the
/// overlay open. A fuller build would stream results.
const MAX_FILES: usize = 5000;
const MAX_DEPTH: usize = 12;

/// Open a file picker over the current working directory.
pub(crate) fn open() -> Picker<PathBuf> {
    open_in(Path::new("."))
}

/// Open a file picker over `root` (a bounded recursive walk skipping hidden entries, heavy build dirs, and
/// entries matched by the root `.gitignore`).
pub(crate) fn open_in(root: &Path) -> Picker<PathBuf> {
    let mut files = Vec::new();
    let ignore = Gitignore::load(root);
    collect(root, &mut files, 0, &ignore);
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

/// Recursively collect file paths under `dir`, skipping dotfiles/dot-dirs (incl. `.git`), the usual heavy
/// build directories, and entries matched by the root `.gitignore`, bounded by [`MAX_FILES`]/[`MAX_DEPTH`].
fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize, ignore: &Gitignore) {
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
            Ok(ft) if ft.is_dir() && !ignore.is_ignored(&name, true) => {
                collect(&path, out, depth + 1, ignore)
            }
            Ok(ft) if ft.is_file() && !ignore.is_ignored(&name, false) => out.push(path),
            _ => {}
        }
    }
}

/// A minimal `.gitignore` matcher: the root file's patterns applied by base name. It covers the common
/// forms — bare names (`diagnostic-bundle`), directory patterns (`target/`, `/dist/`), and single globs
/// (`*.log`, `.env.*`) — with leading `/` and `**/` stripped. It deliberately does NOT implement nested
/// `.gitignore` files, `!` negation, or path-anchored globs; those are rare in this tree and the walk
/// already skips dotfiles and `target`/`node_modules`. Absent or unreadable file ⇒ matches nothing.
struct Gitignore {
    /// `(pattern, dir_only)` — `dir_only` set when the pattern ended in `/`.
    rules: Vec<(String, bool)>,
}

impl Gitignore {
    fn load(root: &Path) -> Gitignore {
        let text = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
        let rules = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
            .map(|l| {
                let dir_only = l.ends_with('/');
                let pat = l.trim_end_matches('/').trim_start_matches("**/");
                let pat = pat.strip_prefix('/').unwrap_or(pat);
                (pat.to_string(), dir_only)
            })
            .filter(|(p, _)| !p.is_empty() && !p.contains('/')) // name-level patterns only
            .collect();
        Gitignore { rules }
    }

    fn is_ignored(&self, name: &str, is_dir: bool) -> bool {
        self.rules.iter().any(|(pat, dir_only)| {
            if *dir_only && !is_dir {
                return false;
            }
            glob_name(pat, name)
        })
    }
}

/// Match one name-level `.gitignore` pattern against a base `name`: exact when there is no `*`, else a
/// single leading (`*.ext` ⇒ suffix) or trailing (`name.*` ⇒ prefix) wildcard. Other `*` placements are
/// treated conservatively as non-matching.
fn glob_name(pat: &str, name: &str) -> bool {
    match pat.find('*') {
        None => pat == name,
        Some(0) => name.ends_with(&pat[1..]),
        Some(i) if i == pat.len() - 1 => name.starts_with(&pat[..i]),
        Some(_) => false,
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

    #[test]
    fn glob_name_matches_gitignore_forms() {
        assert!(glob_name("target", "target")); // bare name
        assert!(!glob_name("target", "targets"));
        assert!(glob_name("*.log", "ruse.log")); // suffix glob
        assert!(!glob_name("*.log", "log.txt"));
        assert!(glob_name(".env.*", ".env.local")); // prefix glob
        assert!(
            !glob_name(".env.*", ".environment"),
            "prefix is only up to the dot"
        );
        assert!(!glob_name("a*b", "aXXb")); // interior glob treated as non-match
    }

    #[test]
    fn gitignore_skips_matched_dirs_and_files() {
        let ig = Gitignore {
            rules: vec![
                ("dist".into(), true),   // dist/ — directory only
                ("*.log".into(), false), // any *.log
                (".env".into(), false),  // bare name
            ],
        };
        assert!(ig.is_ignored("dist", true), "dist/ dir is ignored");
        assert!(
            !ig.is_ignored("dist", false),
            "a file named dist is NOT (dir-only rule)"
        );
        assert!(ig.is_ignored("build.log", false), "*.log file is ignored");
        assert!(ig.is_ignored(".env", false));
        assert!(!ig.is_ignored("main.rs", false), "source file is kept");
    }

    #[test]
    fn gitignore_load_parses_common_patterns() {
        // Hermetic: write a .gitignore into a temp dir and load it. Path-anchored (`/target/`) and `**/`
        // patterns reduce to the base name; comments, blanks, `!` negations, and nested-path rules drop.
        let dir = std::env::temp_dir().join(format!("ruse_gi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join(".gitignore"),
            "# comment\n/target/\n**/*.rs.bk\n*.pyc\ndiagnostic-bundle/\n!keep.me\nsub/dir\n",
        )
        .expect("write .gitignore");

        let ig = Gitignore::load(&dir);
        assert!(
            ig.is_ignored("target", true),
            "/target/ → target dir ignored"
        );
        assert!(
            ig.is_ignored("mod.rs.bk", false),
            "**/*.rs.bk suffix ignored"
        );
        assert!(ig.is_ignored("x.pyc", false), "*.pyc ignored");
        assert!(ig.is_ignored("diagnostic-bundle", true));
        assert!(
            !ig.is_ignored("keep.me", false),
            "! negation lines are dropped, not applied"
        );
        assert!(
            !ig.is_ignored("dir", true),
            "nested-path `sub/dir` is dropped (name-level only)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
