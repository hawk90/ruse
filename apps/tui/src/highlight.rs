//! Syntax highlighting via tree-sitter — a frontend concern (editor-core stays dep-free / IO-free). The
//! buffer is parsed read-only (a document SNAPSHOT, never the live buffer) into color spans that `render`
//! applies. Re-parsing is INCREMENTAL (F-015 #3): the previous parse tree is kept and, on each edit,
//! tree-sitter reuses the unchanged subtrees, so the per-keystroke cost is proportional to the edit rather
//! than a full reparse. That matters — a full reparse is the DOMINANT per-keystroke cost (measured
//! ~7.5 ms on a 1k-line file, ~3000× the buffer edit; see docs/operations/testing-and-benchmarks.md),
//! which is D-042's stated trigger for incremental parsing.

use crossterm::style::Color;
use ruse_core::Revision;
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, Tree};

/// A colored byte range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: Color,
}

/// A configured highlighter for one language: an owned parser plus the compiled highlights query.
pub struct Highlight {
    parser: Parser,
    query: Query,
}

impl Highlight {
    /// A Rust highlighter, or `None` if the grammar/query fails to load.
    pub fn rust() -> Option<Highlight> {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        let query = Query::new(&language, tree_sitter_rust::HIGHLIGHTS_QUERY).ok()?;
        Some(Highlight { parser, query })
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
    /// A cached Rust highlighter, or `None` if the grammar/query fails to load.
    pub fn rust() -> Option<CachedHighlight> {
        Some(CachedHighlight {
            hl: Highlight::rust()?,
            rev: None,
            key: None,
            tree: None,
            src: Vec::new(),
            spans: Vec::new(),
        })
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
            Some(tree) => self.hl.spans_from(tree, src, visible.clone()),
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

/// The tree-sitter `Point` (row, byte-column) of `byte` in `src`.
fn point_at(src: &[u8], byte: usize) -> Point {
    let byte = byte.min(src.len());
    let row = src[..byte].iter().filter(|&&c| c == b'\n').count();
    let line_start = src[..byte]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    Point {
        row,
        column: byte - line_start,
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
        let mut h = CachedHighlight::rust().expect("rust grammar loads");
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
        let mut h = CachedHighlight::rust().expect("rust grammar loads");
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
        let mut h = CachedHighlight::rust().expect("rust grammar loads");
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
        let mut inc = CachedHighlight::rust().expect("grammar");
        for (i, s) in states.iter().enumerate() {
            let inc_spans = inc.spans(Revision(i as u64), s, all(s)).to_vec();
            let mut fresh = CachedHighlight::rust().expect("grammar");
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
