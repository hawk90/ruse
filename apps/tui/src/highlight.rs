//! Syntax highlighting via tree-sitter — a frontend concern (editor-core stays dep-free / IO-free). The
//! buffer is parsed read-only (a document SNAPSHOT, never the live buffer) into color spans that `render`
//! applies. Re-parsing is INCREMENTAL (F-015 #3): the previous parse tree is kept and, on each edit,
//! tree-sitter reuses the unchanged subtrees, so the per-keystroke cost is proportional to the edit rather
//! than a full reparse. That matters — a full reparse is the DOMINANT per-keystroke cost (measured
//! ~7.5 ms on a 1k-line file, ~3000× the buffer edit; see docs/operations/testing-and-benchmarks.md),
//! which is D-042's stated trigger for incremental parsing.

mod cache;
mod faces;
mod grammars;
mod markdown;
mod search;

pub use cache::CachedHighlight;
pub(crate) use faces::face_for;
use grammars::{ext_for_injection_language, grammar_for, injections_for};
pub use search::CachedSearch;

use crate::screen::CellStyle;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

/// A decoration over `[start, end)`: a syntax FACE (fg colour + text attributes) and, for F-031 rich
/// rendering, an optional CONCEAL flag (the range is hidden from layout — 0 cells — unless its line is
/// revealed under the caret). Slice 0 widened this from a bare colour to a [`CellStyle`]; slice 1 adds
/// `conceal` (see docs/design/rich-rendering.md; the decoration model's `face | conceal` facets).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub style: CellStyle,
    /// Hide this range from the display (F-031 conceal). Always `false` for code-syntax spans; set by the
    /// Markdown/Org providers on markup that should not show (heading `#` prefixes, emphasis markers).
    pub conceal: bool,
    /// Virtual text to paint IN PLACE of this (concealed) range — F-031 slice 2 virt_text. `Some("• ")`
    /// turns a hidden list marker into a bullet, `Some("☐ ")`/`Some("☑ ")` a task marker into a checkbox.
    /// Shown only where the range is concealed (i.e. off the caret's revealed line); `None` for a plain
    /// hide or a face-only span. Kept `&'static` — the glyphs are fixed, so `Span` stays `Copy`.
    pub virt: Option<&'static str>,
}

/// A block of virtual display ROWS inserted after buffer line `after_line` (F-031 slice 3a). Its `height`
/// rows are not backed by any buffer bytes; `label` is the text to show — an image's alt text, for the
/// low-capability PLACEHOLDER rung. A graphics-capable terminal replaces the placeholder with real pixels
/// in slice 3b (INV-CAP-DEGRADE). This is the first `virt_lines` consumer and what forces the display-row
/// coordinate model (`cursor_cell` / `paint_pane` must count these rows).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VirtLine {
    pub after_line: usize,
    pub height: u16,
    pub label: String,
    /// The image's source path (an `![](path)` destination) — the render loop reads it to draw real pixels
    /// on a graphics-capable terminal (F-031 slice 3b-2b). `None` for a non-image block.
    pub path: Option<String>,
}

/// The height (display rows) of an image placeholder block — a small fixed box for slice 3a.
const IMAGE_PLACEHOLDER_ROWS: u16 = 2;

/// A configured highlighter for one language: an owned parser plus the compiled highlights query, and
/// the compiled injections query when the grammar ships one (`None` otherwise).
pub struct Highlight {
    parser: Parser,
    query: Query,
    injections: Option<Query>,
}

impl Highlight {
    /// A highlighter for the file extension `ext`, or `None` if the type is unsupported or its
    /// grammar/query fails to load.
    pub fn for_ext(ext: &str) -> Option<Highlight> {
        let (language, query_src) = grammar_for(ext)?;
        let mut parser = Parser::new();
        // A grammar or query that ships with ruse and does not load drops the file to no highlighting
        // (stability §7); say so in the log (#966).
        if parser.set_language(&language).is_err() {
            crate::log::degrade("syntax", "grammar_load_failed", ext);
            return None;
        }
        let Ok(query) = Query::new(&language, query_src) else {
            crate::log::degrade("syntax", "query_load_failed", ext);
            return None;
        };
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
            let cap = m.captures()[*idx];
            let name = names[cap.index as usize];
            spans.push(Span {
                start: cap.node.start_byte(),
                end: cap.node.end_byte(),
                style: face_for(name),
                conceal: false,
                virt: None,
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
            for cap in m.captures() {
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

#[cfg(test)]
mod markdown_tests;
#[cfg(test)]
mod tests;
