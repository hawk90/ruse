//! Syntax highlighting via tree-sitter — a frontend concern (editor-core stays dep-free / IO-free). The
//! buffer is parsed read-only into highlight spans that `render` colors. v0 re-parses the whole buffer per
//! frame; incremental parsing on a rope is a later optimization.

use crossterm::style::Color;
use ruse_core::Revision;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// The capture names we ask tree-sitter for, in a fixed order (the index maps to a color).
const NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.method",
    "function.macro",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// A colored byte range.
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: Color,
}

/// A configured highlighter for one language.
pub struct Highlight {
    hl: Highlighter,
    config: HighlightConfiguration,
}

impl Highlight {
    /// A Rust highlighter, or `None` if the grammar/query fails to load.
    pub fn rust() -> Option<Highlight> {
        let mut config = HighlightConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            "",
            "",
        )
        .ok()?;
        config.configure(NAMES);
        Some(Highlight {
            hl: Highlighter::new(),
            config,
        })
    }

    /// Highlight spans for `src` (empty on any parse error — highlighting is best-effort).
    pub fn spans(&mut self, src: &[u8]) -> Vec<Span> {
        let mut spans = Vec::new();
        let Ok(events) = self.hl.highlight(&self.config, src, None, |_| None) else {
            return spans;
        };
        let mut stack: Vec<usize> = Vec::new();
        for ev in events.flatten() {
            match ev {
                HighlightEvent::HighlightStart(h) => stack.push(h.0),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    if let Some(&idx) = stack.last() {
                        spans.push(Span {
                            start,
                            end,
                            color: color_for(NAMES[idx]),
                        });
                    }
                }
            }
        }
        spans
    }
}

/// A [`Highlight`] that recomputes spans only when the document revision changes (D-042 win A).
///
/// Re-parsing dominates the per-keystroke cost (~400–3200× the buffer copy at daily-driver sizes;
/// see `docs/operations/testing-and-benchmarks.md`), yet most frames — cursor motion, mode changes,
/// scrolling — leave the buffer untouched. [`Revision`] is a sound cache key: every buffer mutation
/// (apply/undo/redo) strictly advances it, and nothing else changes the bytes (INV-TXN).
pub struct CachedHighlight {
    hl: Highlight,
    rev: Option<Revision>,
    spans: Vec<Span>,
}

impl CachedHighlight {
    /// A cached Rust highlighter, or `None` if the grammar/query fails to load.
    pub fn rust() -> Option<CachedHighlight> {
        Some(CachedHighlight {
            hl: Highlight::rust()?,
            rev: None,
            spans: Vec::new(),
        })
    }

    /// Spans for the document at `rev`. Recomputes only when `rev` differs from the cached revision;
    /// otherwise returns the previously computed spans untouched.
    pub fn spans(&mut self, rev: Revision, src: &[u8]) -> &[Span] {
        if self.rev != Some(rev) {
            self.spans = self.hl.spans(src);
            self.rev = Some(rev);
        }
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

    #[test]
    fn highlights_rust() {
        let mut h = Highlight::rust().expect("rust grammar loads");
        let spans = h.spans(b"fn main() { let x = 1; }");
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

        let n0 = h.spans(r0, b"fn main() {}").len();
        assert!(n0 > 0, "first call at r0 parses");

        // Same revision but *different* bytes: a cache hit must return the stale spans (proves no reparse).
        let stale = h.spans(r0, b"this is not rust at all !!!").len();
        assert_eq!(
            stale, n0,
            "same revision reuses the cached spans, ignoring new bytes"
        );

        // A new revision forces a recompute against the bytes actually supplied (empty → no spans).
        let n1 = h.spans(r1, b"").len();
        assert_eq!(
            n1, 0,
            "a changed revision reparses; empty source yields no spans"
        );
    }
}
