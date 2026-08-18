//! Syntax highlighting via tree-sitter — a frontend concern (editor-core stays dep-free / IO-free). The
//! buffer is parsed read-only (a document SNAPSHOT, never the live buffer) into color spans that `render`
//! applies. Re-parsing is INCREMENTAL (F-015 #3): the previous parse tree is kept and, on each edit,
//! tree-sitter reuses the unchanged subtrees, so the per-keystroke cost is proportional to the edit rather
//! than a full reparse. That matters — a full reparse is the DOMINANT per-keystroke cost (measured
//! ~7.5 ms on a 1k-line file, ~3000× the buffer edit; see docs/operations/testing-and-benchmarks.md),
//! which is D-042's stated trigger for incremental parsing.

use crossterm::style::Color;
use ruse_core::{Regex, RegexOptions, Revision};
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, Tree};

/// A colored byte range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: Color,
}

/// A configured highlighter for one language: an owned parser plus the compiled highlights query, and
/// the compiled injections query when the grammar ships one (`None` otherwise).
pub struct Highlight {
    parser: Parser,
    query: Query,
    injections: Option<Query>,
}

/// The grammar + highlights query for a file extension, or `None` for an unsupported type. Adding a
/// language is one arm here plus its `tree-sitter-<lang>` dep — the rest of the pipeline is generic.
fn grammar_for(ext: &str) -> Option<(tree_sitter::Language, &'static str)> {
    Some(match ext {
        "rs" => (
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        ),
        "json" => (
            tree_sitter_json::LANGUAGE.into(),
            tree_sitter_json::HIGHLIGHTS_QUERY,
        ),
        "py" => (
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
        ),
        _ => return None,
    })
}

/// The tree-sitter injections query for `ext`, or `None` if the grammar ships none. Only Rust bundles
/// one today (it re-highlights macro `token_tree` bodies as Rust — see [`Highlight::injected_spans`]).
fn injections_for(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some(tree_sitter_rust::INJECTIONS_QUERY),
        _ => None,
    }
}

/// Map an `injection.language` name (as set by an `#set!` directive in an injections query) to the file
/// extension that [`grammar_for`] dispatches on. `None` for a language we don't bundle — the injected
/// region is then left with only its outer-grammar highlighting.
fn ext_for_injection_language(name: &str) -> Option<&'static str> {
    Some(match name {
        "rust" => "rs",
        "python" => "py",
        "json" => "json",
        _ => return None,
    })
}

impl Highlight {
    /// A highlighter for the file extension `ext`, or `None` if the type is unsupported or its
    /// grammar/query fails to load.
    pub fn for_ext(ext: &str) -> Option<Highlight> {
        let (language, query_src) = grammar_for(ext)?;
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        let query = Query::new(&language, query_src).ok()?;
        // An injections query is optional: a grammar without one (or with a query that fails to compile)
        // simply never injects — the outer highlighting still applies.
        let injections = injections_for(ext).and_then(|src| Query::new(&language, src).ok());
        Some(Highlight {
            parser,
            query,
            injections,
        })
    }

    /// Walk the highlights query over `tree`, but only over the byte range `visible`, and collect a
    /// colour span per capture. Restricting the query to the viewport is the real per-keystroke win: the
    /// incremental PARSE is cheap, but walking the whole document's captures is O(buffer) — so the query
    /// is bounded to what `render` will actually paint (`QueryCursor::set_byte_range`), making it
    /// O(viewport). Longest spans come FIRST so that, under the render's per-byte last-wins flatten, a
    /// shorter (more specific) capture overrides the broader one it sits inside.
    fn spans_from(&self, tree: &Tree, src: &[u8], visible: std::ops::Range<usize>) -> Vec<Span> {
        let names = self.query.capture_names();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(visible);
        let mut caps = cursor.captures(&self.query, tree.root_node(), src);
        let mut spans = Vec::new();
        while let Some((m, idx)) = caps.next() {
            let cap = m.captures[*idx];
            let name = names[cap.index as usize];
            spans.push(Span {
                start: cap.node.start_byte(),
                end: cap.node.end_byte(),
                color: color_for(name),
            });
        }
        spans.sort_by_key(|s| std::cmp::Reverse(s.end - s.start));
        spans
    }

    /// Base highlights plus any injected-language highlights, merged and ordered longest-first so the
    /// render's per-byte last-wins flatten lets the most specific (shortest) capture win — including a
    /// short injected span sitting inside the broad outer region it was injected from.
    fn spans_with_injections(
        &self,
        tree: &Tree,
        src: &[u8],
        visible: std::ops::Range<usize>,
    ) -> Vec<Span> {
        let mut spans = self.spans_from(tree, src, visible.clone());
        spans.extend(self.injected_spans(tree, src, visible));
        spans.sort_by_key(|s| std::cmp::Reverse(s.end - s.start));
        spans
    }

    /// Highlights contributed by language injections. For each `@injection.content` region the
    /// injections query matches within `visible`, parse that region with the injected language's own
    /// grammar and map its highlight spans back into the parent's byte coordinates. Rust's injections
    /// re-highlight macro `token_tree` bodies — which the outer grammar leaves as opaque tokens — as
    /// Rust, so identifiers/keywords inside `println!(...)`, `vec![...]` and `macro_rules!` bodies get
    /// colored.
    ///
    /// The injected region is parsed fresh each call (no incremental reuse): regions are small and the
    /// whole computation is viewport-bounded and cached per `(revision, viewport)` by [`CachedHighlight`],
    /// so the cost is paid only on an edit or a scroll. Injection is depth-1 — a macro nested inside an
    /// already-injected body is not recursively re-injected (the sub-parse uses the base highlights only).
    fn injected_spans(
        &self,
        tree: &Tree,
        src: &[u8],
        visible: std::ops::Range<usize>,
    ) -> Vec<Span> {
        let Some(inj) = &self.injections else {
            return Vec::new();
        };
        let content_idx = inj.capture_index_for_name("injection.content");
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(visible.clone());
        let mut matches = cursor.matches(inj, tree.root_node(), src);
        // Compile each injected language's grammar at most once per call — a viewport may hold many macro
        // invocations, and recompiling the highlights query per match would dominate the cost.
        let mut sub_cache: std::collections::HashMap<&'static str, Highlight> =
            std::collections::HashMap::new();
        let mut spans = Vec::new();
        while let Some(m) = matches.next() {
            // The injected language is carried by an `#set! injection.language "<name>"` on the pattern.
            let sub_ext = inj
                .property_settings(m.pattern_index)
                .iter()
                .find(|p| &*p.key == "injection.language")
                .and_then(|p| p.value.as_deref())
                .and_then(ext_for_injection_language);
            let Some(sub_ext) = sub_ext else { continue };
            let sub = match sub_cache.entry(sub_ext) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => match Highlight::for_ext(sub_ext) {
                    Some(h) => e.insert(h),
                    None => continue,
                },
            };
            for cap in m.captures {
                if Some(cap.index) != content_idx {
                    continue;
                }
                let region = cap.node.byte_range();
                let Some(slice) = src.get(region.clone()) else {
                    continue;
                };
                let Some(subtree) = sub.parser.parse(slice, None) else {
                    continue;
                };
                // The sub-parse's byte offsets are relative to `slice`; shift them into the parent's
                // coordinates and keep only what actually falls inside the viewport.
                for mut s in sub.spans_from(&subtree, slice, 0..slice.len()) {
                    s.start += region.start;
                    s.end += region.start;
                    if s.end > visible.start && s.start < visible.end {
                        spans.push(s);
                    }
                }
            }
        }
        spans
    }
}

/// A [`Highlight`] that reparses only when the document revision changes (D-042 win A), and does so
/// INCREMENTALLY: it keeps the previous parse tree and the bytes it was parsed from, computes the
/// `InputEdit` between old and new bytes, and lets tree-sitter reuse unchanged subtrees.
///
/// [`Revision`] is a sound cache key: every buffer mutation (apply/undo/redo) strictly advances it, and
/// nothing else changes the bytes (INV-TXN). So on an unchanged revision — cursor motion, mode changes,
/// scrolling — no work is done at all.
pub struct CachedHighlight {
    hl: Highlight,
    rev: Option<Revision>,
    /// The `(revision, visible-range)` the cached spans were computed for. Spans are recomputed when
    /// EITHER changes — a new edit (revision) or a scroll (visible range) — since the query is bounded
    /// to the viewport.
    key: Option<(Revision, std::ops::Range<usize>)>,
    tree: Option<Tree>,
    /// The bytes of the last parse, kept so the next edit's `InputEdit` can be derived by diffing.
    src: Vec<u8>,
    spans: Vec<Span>,
}

impl CachedHighlight {
    /// A cached highlighter for the file extension `ext`, or `None` if unsupported.
    pub fn for_ext(ext: &str) -> Option<CachedHighlight> {
        Some(CachedHighlight {
            hl: Highlight::for_ext(ext)?,
            rev: None,
            key: None,
            tree: None,
            src: Vec::new(),
            spans: Vec::new(),
        })
    }

    /// Drop the cached tree/spans but KEEP the compiled parser + query, so the next `spans` call does a
    /// full from-scratch parse. Used by the `highlight_parse` bench to measure a full reparse WITHOUT
    /// re-paying the one-time query compilation (which a fresh `rust()` would, inflating the number).
    /// Also the right primitive for a future buffer-switch; unused in the bin today.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.rev = None;
        self.key = None;
        self.tree = None;
        self.src.clear();
        self.spans.clear();
    }

    /// Spans for the `visible` byte range of the document at `rev`. Reuses the cache when neither the
    /// revision nor the viewport changed. The TREE is reparsed only on a revision change (incrementally,
    /// against the previous tree); a scroll re-runs only the viewport-bounded query.
    pub fn spans(&mut self, rev: Revision, src: &[u8], visible: std::ops::Range<usize>) -> &[Span] {
        if self.key.as_ref() == Some(&(rev, visible.clone())) {
            return &self.spans;
        }
        if self.rev != Some(rev) {
            // Edit the old tree to match the new bytes so the parse reuses unchanged subtrees. No old
            // tree (first parse) or no diff means a full parse.
            if let Some(tree) = self.tree.as_mut() {
                if let Some(edit) = input_edit(&self.src, src) {
                    tree.edit(&edit);
                }
            }
            self.tree = self.hl.parser.parse(src, self.tree.as_ref());
            self.src.clear();
            self.src.extend_from_slice(src);
            self.rev = Some(rev);
        }
        self.spans = match &self.tree {
            Some(tree) => self.hl.spans_with_injections(tree, src, visible.clone()),
            None => Vec::new(),
        };
        self.key = Some((rev, visible));
        &self.spans
    }
}

/// The `InputEdit` transforming `old` into `new`, found by trimming the common prefix and suffix so the
/// edit span is minimal. `None` when the bytes are identical (nothing to edit). Columns are byte offsets
/// within the line, which is what tree-sitter's UTF-8 `Point` expects.
fn input_edit(old: &[u8], new: &[u8]) -> Option<InputEdit> {
    if old == new {
        return None;
    }
    let max = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < max - prefix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    let start_byte = prefix;
    let old_end_byte = old.len() - suffix;
    let new_end_byte = new.len() - suffix;
    Some(InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: point_at(old, start_byte),
        old_end_position: point_at(old, old_end_byte),
        new_end_position: point_at(new, new_end_byte),
    })
}

/// The tree-sitter `Point` (row, byte-column) of `byte` in `src`, over the shared `ruse_core::pos`
/// line math (tree-sitter columns are byte offsets within the line).
fn point_at(src: &[u8], byte: usize) -> Point {
    Point {
        row: ruse_core::pos::line_of(src, byte),
        column: byte.min(src.len()) - ruse_core::pos::line_start(src, byte),
    }
}

/// Cached hlsearch / incsearch match spans for the focused pane (F-009 #1). Same shape as
/// [`CachedHighlight`]: keyed on `(revision, viewport, pattern)`, so an unchanged frame — cursor motion,
/// a mode switch — does no work, and a scroll or a new revision recomputes only the VISIBLE byte range
/// rather than re-running a full-buffer regex every frame. The Vim regex is compiled only when the
/// pattern changes (incsearch re-types the pattern each keystroke; hlsearch holds it fixed).
#[derive(Default)]
pub struct CachedSearch {
    key: Option<(Revision, std::ops::Range<usize>, String)>,
    compiled: Option<(String, Regex)>,
    spans: Vec<(usize, usize)>,
}

impl CachedSearch {
    /// Match spans of `pattern` within the `visible` byte range of `bytes` at `rev`. Empty on an
    /// unmatchable pattern (highlighting is best-effort). Spans are in absolute buffer byte offsets.
    pub fn spans(
        &mut self,
        rev: Revision,
        bytes: &[u8],
        visible: std::ops::Range<usize>,
        pattern: &str,
    ) -> &[(usize, usize)] {
        if self
            .key
            .as_ref()
            .is_some_and(|(r, v, p)| *r == rev && *v == visible && p == pattern)
        {
            return &self.spans;
        }
        // Recompile only when the pattern itself changed (a scroll or an edit keeps the same regex).
        if self.compiled.as_ref().is_none_or(|(p, _)| p != pattern) {
            self.compiled = Regex::compile(pattern, RegexOptions::default())
                .ok()
                .map(|re| (pattern.to_string(), re));
        }
        self.spans.clear();
        let vis = visible.start.min(bytes.len())..visible.end.min(bytes.len());
        if let Some((_, re)) = &self.compiled {
            // The viewport range is line-aligned (see `nth_line_start`), so a match on any visible line
            // is fully inside it — searching the slice and offsetting is complete for what render paints.
            if let Ok(hay) = std::str::from_utf8(&bytes[vis.clone()]) {
                for m in re.find_all(hay) {
                    self.spans.push((vis.start + m.start, vis.start + m.end));
                }
            }
        }
        self.key = Some((rev, visible, pattern.to_string()));
        &self.spans
    }
}

fn color_for(name: &str) -> Color {
    match name.split('.').next().unwrap_or(name) {
        "keyword" => Color::Magenta,
        "string" => Color::Green,
        "comment" => Color::DarkGrey,
        "type" => Color::Yellow,
        "function" | "constructor" => Color::Blue,
        "constant" | "number" => Color::Cyan,
        "attribute" | "label" => Color::DarkYellow,
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A normalized, order-independent key for comparing two span sets.
    fn key(spans: &[Span]) -> Vec<(usize, usize, Color)> {
        let mut v: Vec<_> = spans.iter().map(|s| (s.start, s.end, s.color)).collect();
        v.sort();
        v
    }

    /// The whole-document range, for tests that want unrestricted spans.
    fn all(src: &[u8]) -> std::ops::Range<usize> {
        0..src.len()
    }

    #[test]
    fn highlights_rust() {
        let mut h = CachedHighlight::for_ext("rs").expect("rust grammar loads");
        let src = b"fn main() { let x = 1; }";
        let spans = h.spans(Revision(0), src, all(src));
        assert!(!spans.is_empty(), "produces highlight spans for real Rust");
        // `fn` (bytes 0..2) is a keyword → Magenta.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 0 && s.color == Color::Magenta),
            "the `fn` keyword is colored",
        );
    }

    #[test]
    fn cache_recomputes_only_on_revision_change() {
        let mut h = CachedHighlight::for_ext("rs").expect("rust grammar loads");
        let r0 = Revision(0);
        let r1 = Revision(1);

        let a = b"fn main() {}";
        let n0 = h.spans(r0, a, all(a)).len();
        assert!(n0 > 0, "first call at r0 parses");

        // Same revision AND viewport but *different* bytes: a cache hit returns the stale spans.
        let n0b = b"this is not rust at all !!!";
        let stale = h.spans(r0, n0b, all(a)).len();
        assert_eq!(
            stale, n0,
            "same key reuses the cached spans, ignoring new bytes"
        );

        // A new revision forces a recompute against the bytes actually supplied (empty → no spans).
        let n1 = h.spans(r1, b"", 0..0).len();
        assert_eq!(
            n1, 0,
            "a changed revision reparses; empty source yields no spans"
        );
    }

    #[test]
    fn query_is_bounded_to_the_visible_range() {
        // The viewport win: a capture entirely OUTSIDE the visible range is not returned. Two functions;
        // ask only for the second line's bytes → only its `fn` keyword shows.
        let mut h = CachedHighlight::for_ext("rs").expect("rust grammar loads");
        let src = b"fn a() {}\nfn b() {}\n";
        let line2 = 10..src.len(); // from the start of `fn b`
        let spans = h.spans(Revision(0), src, line2);
        assert!(
            spans.iter().all(|s| s.end > 10),
            "no span from the hidden first line: {spans:?}"
        );
        assert!(
            spans
                .iter()
                .any(|s| s.start == 10 && s.color == Color::Magenta),
            "the visible line's `fn` keyword is colored"
        );
    }

    #[test]
    fn highlights_other_languages_by_extension() {
        // JSON and Python load + produce spans (multi-language, F-015). An unknown extension is None.
        let mut j = CachedHighlight::for_ext("json").expect("json grammar loads");
        assert!(
            !j.spans(Revision(0), br#"{"k": 1}"#, 0..8).is_empty(),
            "json produces highlight spans"
        );
        let src = b"def f():\n    return 1\n";
        let mut p = CachedHighlight::for_ext("py").expect("python grammar loads");
        assert!(
            !p.spans(Revision(0), src, all(src)).is_empty(),
            "python produces highlight spans"
        );
        assert!(
            CachedHighlight::for_ext("xyz").is_none(),
            "an unsupported extension has no highlighter"
        );
    }

    #[test]
    fn injections_highlight_rust_macro_bodies() {
        // A macro body is a flat `token_tree` to the outer grammar: individual keyword TOKENS still
        // highlight, but STRUCTURE does not — a call `work()` is just an identifier token, never a
        // `call_expression`, so the outer query cannot color `work` as a function. The rust->rust
        // injection re-parses the body as Rust, recovering that structure. `work` at the same offset is
        // Reset without injection and @function (Blue) with it.
        let mut h = Highlight::for_ext("rs").expect("rust grammar");
        let src = b"macro_rules! m { () => { work() }; }";
        let tree = h.parser.parse(src, None).expect("parse");
        let at = src
            .windows(4)
            .position(|w| w == b"work")
            .expect("source has `work`");
        let fn_color_at = |spans: &[Span]| {
            spans
                .iter()
                .find(|s| s.start == at && s.end == at + 4)
                .map(|s| s.color)
        };
        let base = h.spans_from(&tree, src, 0..src.len());
        let full = h.spans_with_injections(&tree, src, 0..src.len());
        assert_ne!(
            fn_color_at(&base),
            Some(Color::Blue),
            "outer grammar sees the call target as a flat token, not a function: {base:?}"
        );
        assert_eq!(
            fn_color_at(&full),
            Some(Color::Blue),
            "injection re-parses the macro body, coloring the call target as a function: {full:?}"
        );
    }

    #[test]
    fn injected_spans_are_bounded_to_the_viewport() {
        // Injected highlights obey the same viewport bound as base highlights: a macro body entirely
        // above the visible range contributes nothing.
        let mut h = Highlight::for_ext("rs").expect("rust grammar");
        let src = b"macro_rules! m { () => { let x = 1; }; }\nfn tail() {}\n";
        let tree = h.parser.parse(src, None).expect("parse");
        let tail = src
            .windows(2)
            .position(|w| w == b"fn")
            .expect("has a tail fn");
        let full = h.spans_with_injections(&tree, src, tail..src.len());
        assert!(
            full.iter().all(|s| s.end > tail),
            "no injected span leaks from the hidden macro line: {full:?}"
        );
    }

    #[test]
    fn non_injecting_grammars_are_unaffected() {
        // JSON ships no injections query, so its spans are exactly the base highlights (no panic, no
        // change). This pins that the injection path is a no-op when `injections` is None.
        let mut j = CachedHighlight::for_ext("json").expect("json grammar loads");
        assert!(
            !j.spans(Revision(0), br#"{"k": [1, 2, 3]}"#, 0..16)
                .is_empty(),
            "json still highlights with the injection path in place"
        );
    }

    #[test]
    fn cached_search_matches_and_bounds_to_viewport() {
        let mut s = CachedSearch::default();
        let bytes = b"a b a";
        // Full range: every match (Vim-magic regex).
        assert_eq!(
            s.spans(Revision(0), bytes, 0..bytes.len(), "a"),
            &[(0, 1), (4, 5)]
        );
        // Magic quantifier, not literal.
        assert_eq!(
            s.spans(Revision(1), b"aa b aaa", 0..8, "a\\+"),
            &[(0, 2), (5, 8)]
        );
        // An unrepresentable/invalid pattern highlights nothing (never errors).
        assert!(s.spans(Revision(2), b"abc", 0..3, "\\1").is_empty());
        // Viewport bound: the first line's match is excluded when only the second line is visible.
        let two = b"a\na\n";
        assert_eq!(s.spans(Revision(3), two, 2..4, "a"), &[(2, 3)]);
    }

    #[test]
    fn cached_search_reuses_on_unchanged_key() {
        // Same (revision, viewport, pattern) but different bytes → a cache hit returns the stale spans.
        let mut s = CachedSearch::default();
        let n = s.spans(Revision(0), b"a a", 0..3, "a").len();
        let stale = s.spans(Revision(0), b"zzzzz", 0..3, "a").len();
        assert_eq!(stale, n, "an unchanged key does not re-search");
    }

    #[test]
    fn incremental_matches_full_parse_across_edits() {
        // The correctness contract for incremental parsing: reusing the previous tree must yield the
        // SAME spans as parsing each state from scratch, across a realistic edit sequence (append,
        // insert-in-middle, delete, replace). tree-sitter guarantees an incremental parse equals a
        // fresh one; this pins that our InputEdit diff feeds it correctly.
        let states: &[&[u8]] = &[
            b"fn main() {}",
            b"fn main() { let x = 1; }",
            b"fn main() {\n    let x = 1;\n}",
            b"fn main() {\n    let x = 42;\n}",
            b"fn main() {\n    let x = 42;\n    // done\n}",
            b"fn add(a: u32) -> u32 {\n    a + 1\n}",
            b"",
            b"struct S { field: String }",
        ];
        let mut inc = CachedHighlight::for_ext("rs").expect("grammar");
        for (i, s) in states.iter().enumerate() {
            let inc_spans = inc.spans(Revision(i as u64), s, all(s)).to_vec();
            let mut fresh = CachedHighlight::for_ext("rs").expect("grammar");
            let fresh_spans = fresh.spans(Revision(0), s, all(s)).to_vec();
            assert_eq!(
                key(&inc_spans),
                key(&fresh_spans),
                "incremental spans diverged from a full parse at state {i}: {:?}",
                std::str::from_utf8(s)
            );
        }
    }
}
