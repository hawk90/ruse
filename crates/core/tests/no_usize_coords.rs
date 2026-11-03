//! `no-usize-coords` (RFC-0022 rule 5, D-063): the evidence `spec/POLICY.yaml` ENG-POS-001 names for
//! INV-POS-TYPED. It scans the source of `crates/core/src` and fails on a `pub fn` parameter, a `pub fn`
//! return, or a `pub` field whose NAME says position but whose type is a bare `usize`:
//!
//! - a parameter or field named one of [`POSITION_NAMES`];
//! - a return of a function whose name ends in one of them (`line_start`, `word_end`, `cursor`).
//!
//! "Bare `usize`" covers `usize`, `Option<usize>`, a `(usize, usize)` pair, `Range<usize>` and
//! `Vec<usize>`. A byte position crossing the boundary is a `BytePos`; line and column indices are named
//! for their unit instead (`line`, `col`, `start_line`), so they do not trip it. `pub(crate)` items and
//! test modules are inside the boundary and are not scanned.
//!
//! [`ALLOWED`] holds the explained exceptions. It can only shrink: an entry that no longer matches a
//! finding fails the test, so a fixed item must also leave the list.

use std::path::Path;

/// Names that say "this is a position" (RFC-0022 rule 5).
const POSITION_NAMES: &[&str] = &[
    "at", "pos", "offset", "cursor", "start", "end", "from", "to", "anchor",
];

/// Explained exceptions, as the finding's key (`<file>: <item>`). Empty: every finding was converted.
const ALLOWED: &[(&str, &str)] = &[];

/// One `pub` item that passes a position as a bare `usize`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    file: String,
    line: usize,
    item: String,
}

impl Finding {
    fn key(&self) -> String {
        format!("{}: {}", self.file, self.item)
    }
}

/// `src` with every `#[cfg(test)] mod … { … }` block blanked out (its newlines kept, so line numbers
/// still match): test modules are not API, but an item after one still is.
fn non_test(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut from = 0;
    while let Some(i) = src[from..].find("#[cfg(test)]") {
        let at = from + i;
        let after = at + "#[cfg(test)]".len();
        let rest = &src[after..];
        let skipped = rest.len() - rest.trim_start().len();
        if !rest.trim_start().starts_with("mod ") {
            out.push_str(&src[from..after]);
            from = after;
            continue;
        }
        // The module's extent: to its `;` (an out-of-line `mod tests;`) or its matching `}`.
        let body = after + skipped;
        let mut end = src.len();
        let mut depth = 0;
        for (j, c) in src[body..].char_indices() {
            match c {
                ';' if depth == 0 => {
                    end = body + j + 1;
                    break;
                }
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = body + j + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push_str(&src[from..at]);
        out.extend(src[at..end].chars().filter(|&c| c == '\n'));
        from = end;
    }
    out.push_str(&src[from..]);
    out
}

fn is_bare_usize(ty: &str) -> bool {
    let t: String = ty.split_whitespace().collect();
    matches!(
        t.as_str(),
        "usize"
            | "Option<usize>"
            | "(usize,usize)"
            | "Option<(usize,usize)>"
            | "Range<usize>"
            | "std::ops::Range<usize>"
            | "Vec<usize>"
    )
}

fn says_position(name: &str) -> bool {
    POSITION_NAMES.contains(&name)
}

/// Split `s` at its top-level commas (not inside `<>`, `()` or `[]`).
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !s[start..].trim().is_empty() {
        out.push(&s[start..]);
    }
    out
}

/// Every finding in one file's source. `file` is the label findings carry.
fn scan(file: &str, src: &str) -> Vec<Finding> {
    let owned = non_test(src);
    let src = owned.as_str();
    let line_of = |at: usize| src[..at].matches('\n').count() + 1;
    let mut found = Vec::new();
    // `pub fn name<...>(args) -> ret` — `pub(crate) fn` does not match `pub fn`.
    let mut from = 0;
    while let Some(i) = src[from..].find("pub fn ") {
        let at = from + i;
        from = at + 1;
        let before = src[..at].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let rest = &src[at + "pub fn ".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let Some(open) = rest.find('(') else { continue };
        let mut depth = 0;
        let mut close = None;
        for (j, c) in rest[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { continue };
        for arg in split_top(&rest[open + 1..close]) {
            let Some((pat, ty)) = arg.split_once(':') else {
                continue; // `self`, `&self`, `&mut self`
            };
            let pat = pat.trim().trim_start_matches("mut ").trim();
            if says_position(pat) && is_bare_usize(ty) {
                found.push(Finding {
                    file: file.to_string(),
                    line: line_of(at),
                    item: format!("fn {name} param {pat}: {}", ty.trim()),
                });
            }
        }
        let tail = rest[close + 1..].trim_start();
        if let Some(ret) = tail.strip_prefix("->") {
            let end = ret.find(['{', ';']).unwrap_or(ret.len());
            let ret = ret[..end].split(" where ").next().unwrap_or("").trim();
            let last = name.rsplit('_').next().unwrap_or("");
            if says_position(last) && is_bare_usize(ret) {
                found.push(Finding {
                    file: file.to_string(),
                    line: line_of(at),
                    item: format!("fn {name} -> {ret}"),
                });
            }
        }
    }
    // `pub name: Type,` fields (one per line, as rustfmt writes them).
    let mut offset = 0;
    for line in src.split_inclusive('\n') {
        let t = line.trim();
        if let Some(field) = t.strip_prefix("pub ") {
            if let Some((name, ty)) = field.split_once(':') {
                let name = name.trim();
                let ty = ty.trim().trim_end_matches(',');
                if name.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && says_position(name)
                    && is_bare_usize(ty)
                {
                    found.push(Finding {
                        file: file.to_string(),
                        line: line_of(offset),
                        item: format!("field {name}: {ty}"),
                    });
                }
            }
        }
        offset += line.len();
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("crates/core/src is readable")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// The findings over the real `crates/core/src`, minus the files that are whole test modules.
fn scan_core() -> Vec<Finding> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut found = Vec::new();
    for p in files {
        if p.file_name().is_some_and(|n| n == "tests.rs") {
            continue; // `editor/tests.rs` is `#[cfg(test)] mod tests;`
        }
        let label = p
            .strip_prefix(&root)
            .expect("under src")
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(&p).expect("readable source");
        found.extend(scan(&label, &src));
    }
    found
}

/// Check `found` against `allowed`: every finding must be allowed, and every allowed entry must still
/// match a finding (the list only shrinks). Returns the problems, empty when clean.
fn check(found: &[Finding], allowed: &[(&str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();
    for f in found {
        if !allowed.iter().any(|(k, _)| *k == f.key()) {
            problems.push(format!(
                "{}:{}: `{}` passes a position as bare usize — make it BytePos (or another unit type), \
                 name it for its unit, or make it pub(crate) (RFC-0022)",
                f.file, f.line, f.item
            ));
        }
    }
    for (k, why) in allowed {
        assert!(
            !why.trim().is_empty(),
            "allow-list entry `{k}` has no reason"
        );
        if !found.iter().any(|f| f.key() == *k) {
            problems.push(format!(
                "stale allow-list entry `{k}`: nothing matches it any more — delete the line"
            ));
        }
    }
    problems
}

#[test]
fn no_pub_core_item_passes_a_position_as_bare_usize() {
    let problems = check(&scan_core(), ALLOWED);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_scan_catches_a_planted_violation() {
    let src = "\
pub struct S {
    pub start: usize,
    pub line: usize,
    pub end: BytePos,
}
pub fn place(cursor: usize, line: usize) {}
pub fn word_end(b: &[u8], at: BytePos) -> usize {}
pub fn line_count(b: &[u8]) -> usize {}
pub(crate) fn inner(at: usize) -> usize {}
pub fn span(&self, from: Option<usize>, to: (usize, usize)) -> Option<usize> {}
#[cfg(test)]
mod tests {
    pub fn helper(at: usize) {}
}
pub fn after_tests(offset: usize) {}
";
    let items: Vec<String> = scan("planted.rs", src)
        .into_iter()
        .map(|f| f.item)
        .collect();
    assert_eq!(
        items,
        [
            "fn place param cursor: usize",
            "fn word_end -> usize",
            "fn span param from: Option<usize>",
            "fn span param to: (usize, usize)",
            "fn after_tests param offset: usize",
            "field start: usize",
        ]
    );
}

#[test]
fn an_allow_list_entry_that_matches_nothing_fails() {
    let found = scan("a.rs", "pub fn f(at: usize) {}\n");
    assert!(check(&found, &[("a.rs: fn f param at: usize", "reason")]).is_empty());
    assert_eq!(check(&found, &[]).len(), 1, "an unlisted finding fails");
    let stale = check(&[], &[("a.rs: fn f param at: usize", "reason")]);
    assert_eq!(stale.len(), 1);
    assert!(stale[0].starts_with("stale allow-list entry"), "{stale:?}");
}
