//! Syntax highlighting via tree-sitter — a frontend concern (editor-core stays dep-free / IO-free). The
//! buffer is parsed read-only into highlight spans that `render` colors. v0 re-parses the whole buffer per
//! frame; incremental parsing on a rope is a later optimization.

use crossterm::style::Color;
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
}
