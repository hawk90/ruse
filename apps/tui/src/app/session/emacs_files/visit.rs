//! Visiting a file (#619, #1503): `find-file` under `find-file-wildcards`, `directory-abbrev-alist`,
//! `find-file-visit-truename` and `large-file-warning-threshold`, the large-file question, the visit
//! with its changed-on-disk question, and `find-sibling-file`.

use crossterm::event::KeyEvent;

use super::memory::{human_size, out_of_memory_warning};
use super::revert::emacs_match;
use super::{plain, EmacsPrompt, Then};
use crate::app::session::Session;
use crate::persist;

impl Session {
    /// `find-file` of `name`: wildcards visit every match (`find-file-wildcards`), `directory-abbrev-alist`
    /// and `find-file-visit-truename` shape the name, and past `large-file-warning-threshold` ask first
    /// (`abort-if-file-too-large`).
    pub(super) fn find_file(&mut self, name: &str) {
        let name = crate::persist::backup::expand(name.trim());
        if name.is_empty() {
            return;
        }
        let vars = self.emacs_vars();
        if vars.find_file_wildcards && !std::path::Path::new(&name).exists() {
            let matches = expand_wildcards(&name);
            if !matches.is_empty() {
                // Emacs visits them all and shows the first.
                for m in matches.iter().rev() {
                    self.find_file_one(m);
                }
                return;
            }
        }
        self.find_file_one(&name);
    }

    fn find_file_one(&mut self, name: &str) {
        let vars = self.emacs_vars();
        let mut name = name.to_string();
        for (from, to) in &vars.directory_abbrev_alist {
            if let Some(end) = emacs_match_end_at_start(from, &name) {
                name = format!("{to}{}", &name[end..]);
            }
        }
        if vars.find_file_visit_truename {
            if let Ok(t) = std::fs::canonicalize(&name) {
                name = t.display().to_string();
            }
        }
        let path = std::path::PathBuf::from(&name);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        let vars = self.emacs_vars();
        let visited = crate::app::dispatch::buffer_for_path(&self.files, &path).is_some();
        if !visited
            && vars
                .large_file_warning_threshold
                .is_some_and(|limit| size > limit)
        {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let question = format!(
                "File {base} is large ({}), really open? ([Y]es, [N]o, [L]iterally, [?]): ",
                human_size(size as f64)
            );
            self.ask(EmacsPrompt::LargeFile { name, question });
            return;
        }
        self.visit(&name, false);
    }

    /// Visit `name` (`literally`: `find-file-literally`, read with `'binary'`): switch to its buffer, asking
    /// or telling about a file changed on disk, and warn past `out-of-memory-warning-percentage`.
    fn visit(&mut self, name: &str, literally: bool) {
        let path = std::path::PathBuf::from(name);
        let existing = crate::app::dispatch::buffer_for_path(&self.files, &path);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        let new = std::fs::symlink_metadata(&path).is_err();
        // `find-file-existing-other-name`: a buffer visiting the same file under another name is used, and
        // Emacs says so unless `find-file-suppress-same-file-warnings`.
        let same_file = existing.and_then(|id| {
            let other = self.files.get(&id)?.path.clone();
            let a = std::path::absolute(&path).ok()?;
            let b = std::path::absolute(&other).ok()?;
            (a != b).then(|| (a.display().to_string(), b.display().to_string()))
        });
        {
            let Session {
                ws,
                files,
                highlighters,
                status,
                ..
            } = self;
            let opened = crate::app::dispatch::open_file_with(name, ws, files, highlighters, |f| {
                if literally {
                    crate::app::file_cmds::with_ff(
                        f,
                        crate::input::FileOpts {
                            ff: None,
                            bin: Some(true),
                        },
                    )
                } else {
                    f
                }
            });
            status.give(opened.status);
            if opened.id.is_none() {
                return;
            }
        }
        let Some(id) = existing else {
            self.status.clear();
            if new {
                self.status.echo("(New file)");
            }
            if let Some(warning) = out_of_memory_warning(size, &self.emacs_vars()) {
                self.status.warn(warning);
            }
            return;
        };
        self.status.clear();
        if let Some((a, b)) = same_file {
            if !self.emacs_vars().find_file_suppress_same_file_warnings {
                self.status.echo(format!("{a} and {b} are the same file"));
            }
        }
        let Some(bf) = self.files.get(&id) else {
            return;
        };
        if persist::disk::check(&bf.path) != persist::disk::State::Changed {
            return;
        }
        let vars = self.emacs_vars();
        let file = std::path::absolute(&bf.path).unwrap_or_else(|_| bf.path.clone());
        let base = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let modified = self.ws.document(id).is_some_and(|d| d.is_modified());
        let shown = file.display().to_string();
        if !modified
            && vars
                .revert_without_query
                .iter()
                .any(|re| emacs_match(re, &shown))
        {
            self.revert_now(id);
            self.status.echo(format!("Reverting file {shown}...done"));
        } else if !vars.query_about_changed_file {
            self.status.echo(format!(
                "File {base} changed on disk.  C-x x g to load new contents{}",
                if modified {
                    " and discard your edits"
                } else {
                    ""
                }
            ));
        } else {
            let question = if modified {
                format!("File {base} changed on disk.  Discard your edits? ")
            } else {
                format!("File {base} changed on disk.  Reread from disk? ")
            };
            self.ask_yes(question, false, Then::RevertBuffer(id));
        }
    }

    /// The large-file question: open, open literally, or abort.
    pub(super) fn answer_large_file(&mut self, name: String, question: String, key: KeyEvent) {
        if plain(key, 'y') || plain(key, 'Y') {
            self.status.clear();
            self.visit(&name, false);
        } else if plain(key, 'l') || plain(key, 'L') {
            self.status.clear();
            self.visit(&name, true);
        } else if plain(key, 'n') || plain(key, 'N') {
            self.status.error("Aborted");
        } else {
            self.ask(EmacsPrompt::LargeFile { name, question });
        }
    }

    /// `find-sibling-file`: the first existing file a `find-sibling-rules` rule names from the visited one.
    pub(super) fn find_sibling_file(&mut self) {
        let id = self.ws.focused_buffer();
        let Some(path) = self.files.get(&id).map(|bf| bf.path.clone()) else {
            self.status.error("Not visiting a file");
            return;
        };
        let rules = self.emacs_vars().find_sibling_rules;
        if rules.is_empty() {
            self.status
                .error("The `find-sibling-rules' variable has not been configured");
            return;
        }
        let file = std::path::absolute(&path)
            .unwrap_or(path)
            .display()
            .to_string();
        let found = sibling_files(&file, &rules);
        match found.first() {
            Some(f) => {
                let f = f.clone();
                self.find_file(&f);
            }
            None => self.status.error("Couldn't find any sibling files"),
        }
    }
}

/// `file-expand-wildcards` for `find-file-wildcards`: the files matching a name whose last component has
/// `*`, `?` or `[...]`, sorted; empty for a name without wildcards or with no match.
pub(super) fn expand_wildcards(name: &str) -> Vec<String> {
    let path = std::path::Path::new(name);
    let Some(base) = path.file_name().map(|b| b.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    if !base.contains(['*', '?', '[']) {
        return Vec::new();
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            // A wildcard does not match a leading dot, as the shell's.
            let hidden = n.starts_with('.') && !base.starts_with('.');
            (!hidden && crate::persist::backup::glob_match(base.as_bytes(), n.as_bytes()))
                .then(|| dir.join(n).display().to_string())
        })
        .collect();
    out.sort();
    out
}

/// `find-sibling-file-search`: for each rule whose MATCH matches `file`, its EXPANSION with `\N` and `\&`
/// replaced by the match's groups, read relative to the directory before the match; the existing ones,
/// in rule order, without `file` itself. An EXPANSION is a file name here, not a regexp.
fn sibling_files(file: &str, rules: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (pattern, expansion) in rules {
        let Ok(re) =
            ruse_core::pattern::Regex::compile(pattern, ruse_core::pattern::Options::default())
        else {
            continue;
        };
        let Some(groups) = re.groups(file) else {
            continue;
        };
        let group = |i: usize| {
            groups
                .get(i)
                .copied()
                .flatten()
                .map_or("", |(a, b)| &file[a..b])
        };
        let mut name = String::new();
        let mut chars = expansion.chars().peekable();
        while let Some(c) = chars.next() {
            match (c, chars.peek()) {
                ('\\', Some('&')) => {
                    chars.next();
                    name.push_str(group(0));
                }
                ('\\', Some(d)) if d.is_ascii_digit() => {
                    let i = d.to_digit(10).unwrap_or(0) as usize;
                    chars.next();
                    name.push_str(group(i));
                }
                _ => name.push(c),
            }
        }
        let start = groups[0].map_or(0, |(a, _)| a);
        let candidate = std::path::Path::new(&file[..start]).join(&name);
        let shown = candidate.display().to_string();
        if shown != file && candidate.is_file() && !out.contains(&shown) {
            out.push(shown);
        }
    }
    out
}

/// Where a `directory-abbrev-alist` FROM matches at the start of `name`, its end.
fn emacs_match_end_at_start(re: &str, name: &str) -> Option<usize> {
    let re = ruse_core::pattern::Regex::compile(re, ruse_core::pattern::Options::default()).ok()?;
    let m = re.find_at(name, ruse_core::BytePos(0))?;
    (m.start.0 == 0).then_some(m.end.0)
}
