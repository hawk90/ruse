//! File extension to tree-sitter grammar, highlights query and injections query.

/// The grammar + highlights query for a file extension, or `None` for an unsupported type. Adding a
/// language is one arm here plus its `tree-sitter-<lang>` dep — the rest of the pipeline is generic.
pub(super) fn grammar_for(ext: &str) -> Option<(tree_sitter::Language, &'static str)> {
    // Grammar crates disagree on the query const's name — some export HIGHLIGHTS_QUERY, some the
    // singular HIGHLIGHT_QUERY — so each arm names its own.
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
        "sh" | "bash" => (
            tree_sitter_bash::LANGUAGE.into(),
            tree_sitter_bash::HIGHLIGHT_QUERY,
        ),
        "c" | "h" => (
            tree_sitter_c::LANGUAGE.into(),
            tree_sitter_c::HIGHLIGHT_QUERY,
        ),
        "go" => (
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::HIGHLIGHTS_QUERY,
        ),
        "js" | "mjs" | "cjs" | "jsx" => (
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        ),
        "css" => (
            tree_sitter_css::LANGUAGE.into(),
            tree_sitter_css::HIGHLIGHTS_QUERY,
        ),
        // Markdown (F-031): the BLOCK grammar only. Its decorations (conceal + heading faces) are produced
        // by a tree walk ([`markdown_decorations`]), not this query, so the query is empty — the parser is
        // what we need here. The inline grammar (emphasis/links) is a following slice.
        "md" | "markdown" => (tree_sitter_md::LANGUAGE.into(), ""),
        // Org (F-031 slice 4) has NO tree-sitter grammar here (tree-sitter-org pins an incompatible
        // tree-sitter 0.20). This filler language/parser is never used — the org path is a hand-rolled
        // line scanner (`crate::org`) that `recompute` calls before any parse.
        "org" | "orgmode" => (tree_sitter_md::LANGUAGE.into(), ""),
        _ => return None,
    })
}

/// The tree-sitter injections query for `ext`, or `None` if the grammar ships none. Rust injects Rust
/// into macro `token_tree` bodies; JavaScript injects into template literals / embedded regions. Both
/// re-highlight content the outer grammar leaves opaque — see [`super::Highlight::injected_spans`].
pub(super) fn injections_for(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some(tree_sitter_rust::INJECTIONS_QUERY),
        "js" | "mjs" | "cjs" | "jsx" => Some(tree_sitter_javascript::INJECTIONS_QUERY),
        _ => None,
    }
}

/// Map an `injection.language` name (as set by an `#set!` directive in an injections query) to the file
/// extension that [`grammar_for`] dispatches on. `None` for a language we don't bundle — the injected
/// region is then left with only its outer-grammar highlighting.
pub(super) fn ext_for_injection_language(name: &str) -> Option<&'static str> {
    Some(match name {
        "rust" => "rs",
        "python" => "py",
        "json" => "json",
        "bash" => "sh",
        "c" => "c",
        "go" => "go",
        "javascript" => "js",
        "css" => "css",
        _ => return None,
    })
}
