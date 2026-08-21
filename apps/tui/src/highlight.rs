//! Syntax highlighting via tree-sitter — a frontend concern (editor-core stays dep-free / IO-free). The
//! buffer is parsed read-only (a document SNAPSHOT, never the live buffer) into color spans that `render`
//! applies. Re-parsing is INCREMENTAL (F-015 #3): the previous parse tree is kept and, on each edit,
//! tree-sitter reuses the unchanged subtrees, so the per-keystroke cost is proportional to the edit rather
//! than a full reparse. That matters — a full reparse is the DOMINANT per-keystroke cost (measured
//! ~7.5 ms on a 1k-line file, ~3000× the buffer edit; see docs/operations/testing-and-benchmarks.md),
//! which is D-042's stated trigger for incremental parsing.

use crossterm::style::Color;
use ruse_core::{Regex, RegexOptions, Revision};

use crate::screen::CellStyle;
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, Tree};

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

/// The grammar + highlights query for a file extension, or `None` for an unsupported type. Adding a
/// language is one arm here plus its `tree-sitter-<lang>` dep — the rest of the pipeline is generic.
fn grammar_for(ext: &str) -> Option<(tree_sitter::Language, &'static str)> {
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
/// re-highlight content the outer grammar leaves opaque — see [`Highlight::injected_spans`].
fn injections_for(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some(tree_sitter_rust::INJECTIONS_QUERY),
        "js" | "mjs" | "cjs" | "jsx" => Some(tree_sitter_javascript::INJECTIONS_QUERY),
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
        "bash" => "sh",
        "c" => "c",
        "go" => "go",
        "javascript" => "js",
        "css" => "css",
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
    /// Markdown uses a tree WALK ([`markdown_decorations`]) instead of the generic highlights query — the
    /// parser is still `hl.parser` (the block grammar), but span production branches on this (F-031).
    markdown: bool,
    /// Org uses a hand-rolled LINE scanner ([`crate::org`]) and never touches the tree-sitter tree (F-031
    /// slice 4). Mutually exclusive with `markdown`.
    org: bool,
    /// The tree-sitter-md INLINE grammar parser (F-031 slice 1b). The block grammar leaves heading /
    /// paragraph text as opaque `inline` nodes; this re-parses each such node's bytes to recover
    /// emphasis / strong / code-span / link structure. `Some` only when `markdown`.
    inline: Option<Parser>,
    /// The virtual-line blocks (image placeholders) for the current frame, refreshed by [`Self::recompute`]
    /// alongside `spans`. Empty for non-Markdown highlighters (F-031 slice 3a).
    virt_lines: Vec<VirtLine>,
    /// The display height (rows) an image block reserves — tall on a graphics-capable terminal (room for
    /// real pixels), short otherwise (just the alt placeholder). Set by [`Self::set_image_rows`] from the
    /// pinned graphics capability (F-031 slice 3b-2b); defaults to the placeholder height.
    image_rows: u16,
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
        let markdown = matches!(ext, "md" | "markdown");
        // Build the inline parser once, alongside the block parser, when this is Markdown. If the inline
        // grammar fails to load we simply emit no inline decorations (block-level conceal still works).
        let inline = if markdown {
            let mut p = Parser::new();
            p.set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
                .ok()
                .map(|()| p)
        } else {
            None
        };
        Some(CachedHighlight {
            hl: Highlight::for_ext(ext)?,
            markdown,
            org: matches!(ext, "org" | "orgmode"),
            inline,
            virt_lines: Vec::new(),
            image_rows: IMAGE_PLACEHOLDER_ROWS,
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
    /// The cached parse tree (current as of the last [`Self::spans`] call — refreshed every render), for
    /// the tree-aware indent engine. `None` before the first parse.
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    pub fn spans(&mut self, rev: Revision, src: &[u8], visible: std::ops::Range<usize>) -> &[Span] {
        self.recompute(rev, src, visible);
        &self.spans
    }

    /// Set the reserved display height for image blocks (F-031 slice 3b-2b). A graphics-capable terminal
    /// wants room for real pixels; a plain one only needs the short alt placeholder. Invalidates the cache
    /// so the next `spans` call re-emits blocks at the new height.
    pub fn set_image_rows(&mut self, rows: u16) {
        if rows != self.image_rows {
            self.image_rows = rows.max(1);
            self.key = None;
        }
    }

    /// Spans PLUS the virtual-line blocks (F-031 slice 3a: Markdown image placeholders) for the same
    /// frame — both borrow `self` immutably, so the caller holds them together across one `render`.
    pub fn spans_and_virt(
        &mut self,
        rev: Revision,
        src: &[u8],
        visible: std::ops::Range<usize>,
    ) -> (&[Span], &[VirtLine]) {
        self.recompute(rev, src, visible);
        (&self.spans, &self.virt_lines)
    }

    /// Reparse (incrementally) if needed and refresh the cached spans + virt_lines for `(rev, visible)`.
    fn recompute(&mut self, rev: Revision, src: &[u8], visible: std::ops::Range<usize>) {
        if self.key.as_ref() == Some(&(rev, visible.clone())) {
            return;
        }
        // Org is a pure line scanner — no tree-sitter parse (F-031 slice 4). Short-circuit before the tree.
        if self.org {
            let (spans, virt) = crate::org::decorations(src, &visible, self.image_rows);
            self.spans = spans;
            self.virt_lines = virt;
            self.key = Some((rev, visible));
            return;
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
        let (spans, virt) = match &self.tree {
            Some(tree) if self.markdown => {
                let mut virt = Vec::new();
                let spans = markdown_decorations(
                    tree,
                    src,
                    self.inline.as_mut(),
                    &visible,
                    &mut virt,
                    self.image_rows,
                );
                (spans, virt)
            }
            Some(tree) => (
                self.hl.spans_with_injections(tree, src, visible.clone()),
                Vec::new(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        self.spans = spans;
        self.virt_lines = virt;
        self.key = Some((rev, visible));
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

/// The FACE for a tree-sitter capture: its foreground colour (as before) plus text attributes. Slice 0
/// of F-031 adds the attribute channel — comments render italic and keywords bold, the way most editors
/// style them — proving the decoration model carries more than colour. Colours are unchanged from the
/// previous `color_for`, so non-attributed captures paint byte-identically (the P6 identity guard).
pub(crate) fn face_for(name: &str) -> CellStyle {
    // F-031 markup faces (Markdown/Org): a heading is a bold title; slice 1b adds the inline faces the
    // INLINE grammar drives — emphasis (italic), strong (bold), code span (a distinct fg), and links (a
    // blue underline). These are exact-match names (the `.`-head fallback below would flatten them to a
    // bare "markup" head), so they are handled before the split — mirroring `markup.heading`.
    match name {
        "markup.heading" => {
            return CellStyle {
                fg: Color::Yellow,
                bold: true,
                ..CellStyle::default()
            };
        }
        "markup.strong" => {
            return CellStyle {
                bold: true,
                ..CellStyle::default()
            };
        }
        "markup.emphasis" => {
            return CellStyle {
                italic: true,
                ..CellStyle::default()
            };
        }
        "markup.code" => {
            return CellStyle {
                fg: Color::Green,
                ..CellStyle::default()
            };
        }
        "markup.link" => {
            return CellStyle {
                fg: Color::Blue,
                underline: true,
                ..CellStyle::default()
            };
        }
        "markup.quote" => {
            return CellStyle {
                fg: Color::DarkGrey,
                italic: true,
                ..CellStyle::default()
            };
        }
        _ => {}
    }
    let head = name.split('.').next().unwrap_or(name);
    let fg = match head {
        "keyword" => Color::Magenta,
        "string" => Color::Green,
        "comment" => Color::DarkGrey,
        "type" => Color::Yellow,
        "function" | "constructor" => Color::Blue,
        "constant" | "number" => Color::Cyan,
        "attribute" | "label" => Color::DarkYellow,
        _ => Color::Reset,
    };
    CellStyle {
        fg,
        bold: head == "keyword",
        italic: head == "comment",
        ..CellStyle::default()
    }
}

/// Markdown rich-render decorations (F-031). The BLOCK grammar (slice 1) handles ATX headings: CONCEAL
/// the `# ` prefix (`[heading.start, content.start)`) and FACE the heading content as a bold title. The
/// INLINE grammar (slice 1b) handles the paragraph text the block grammar leaves as opaque `inline`
/// nodes: each such node's bytes are re-parsed ([`inline_decorations`]) to conceal emphasis / strong /
/// code-span markers and link punctuation while facing their content. Only decorations intersecting
/// `visible` are emitted. Produced by a tree walk rather than a highlights query because conceal is not a
/// colour. `inline` is the INLINE parser (see [`CachedHighlight::inline`]); `None` disables inline faces.
fn markdown_decorations(
    tree: &Tree,
    src: &[u8],
    mut inline: Option<&mut Parser>,
    visible: &std::ops::Range<usize>,
    out_virt: &mut Vec<VirtLine>,
    image_rows: u16,
) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "atx_heading" {
            // The heading content is the `inline` child; the prefix before it (`# `) is what we conceal.
            // Heading content keeps its bold-title face and is NOT re-parsed for inline markup (slice 1b
            // scopes inline faces to paragraph text; headings stay as they were).
            let mut cursor = node.walk();
            let content = node
                .children(&mut cursor)
                .find(|c| matches!(c.kind(), "inline" | "heading_content"));
            if let Some(inline) = content {
                let (hstart, cstart, cend) =
                    (node.start_byte(), inline.start_byte(), inline.end_byte());
                if cstart < visible.end && cend > visible.start {
                    if cstart > hstart {
                        spans.push(Span {
                            start: hstart,
                            end: cstart,
                            style: CellStyle::default(),
                            conceal: true,
                            virt: None,
                        });
                    }
                    spans.push(Span {
                        start: cstart,
                        end: cend,
                        style: face_for("markup.heading"),
                        conceal: false,
                        virt: None,
                    });
                }
            }
            continue; // headings have no nested blocks we care about
        }
        // Paragraph / list-item body text is an opaque `inline` node to the block grammar: re-parse it
        // with the INLINE grammar to recover emphasis/strong/code/link. (Heading `inline` children never
        // reach here — the heading arm `continue`s above.)
        if node.kind() == "inline" {
            if let Some(parser) = inline.as_deref_mut() {
                inline_decorations(node, src, parser, visible, &mut spans, out_virt, image_rows);
            }
            continue;
        }
        // A list item: conceal the `- ` / `- [ ] ` prefix and show a bullet / checkbox glyph in its place
        // (virt_text). UNORDERED markers only (`-`/`*`/`+`); ordered markers (`1.` / `1)`) keep their
        // number. Does NOT `continue` — we fall through to descend so the item's inline text is re-parsed.
        if node.kind() == "list_item" {
            let mut cursor = node.walk();
            let marker = node.children(&mut cursor).find(|c| {
                matches!(
                    c.kind(),
                    "list_marker_minus" | "list_marker_star" | "list_marker_plus"
                )
            });
            if let Some(marker) = marker {
                // The glyph: a checkbox if the item is a task, else a bullet.
                let glyph: &'static str = match first_descendant(
                    node,
                    &["task_list_marker_checked", "task_list_marker_unchecked"],
                ) {
                    Some(t) if t.kind() == "task_list_marker_checked" => "☑ ",
                    Some(_) => "☐ ",
                    None => "• ",
                };
                // Conceal `[marker.start, content.start)` — the marker plus any task box and trailing space
                // — replacing it with the glyph. Content start is the item's first inline text.
                let mstart = marker.start_byte();
                let cstart = first_descendant(node, &["inline"])
                    .map(|n| n.start_byte())
                    .unwrap_or(marker.end_byte());
                if cstart > mstart && mstart < visible.end && cstart > visible.start {
                    spans.push(Span {
                        start: mstart,
                        end: cstart,
                        style: CellStyle::default(),
                        conceal: true,
                        virt: Some(glyph),
                    });
                }
            }
        }
        // A Markdown table (tree-sitter-md pipe_table): the HEADER row is faced bold and the DELIMITER row
        // (`|---|---|`) dimmed. Neither `continue`s — the walk descends so cell inline markup still parses;
        // the shorter inline spans sort after these longer ones and win. Data-row `|` dimming is a follow-up.
        if node.kind() == "pipe_table_header" {
            let (s, e) = (node.start_byte(), node.end_byte());
            if s < visible.end && e > visible.start {
                spans.push(Span {
                    start: s,
                    end: e,
                    style: face_for("markup.strong"),
                    conceal: false,
                    virt: None,
                });
            }
        }
        if node.kind() == "pipe_table_delimiter_row" {
            let (s, e) = (node.start_byte(), node.end_byte());
            if s < visible.end && e > visible.start {
                spans.push(Span {
                    start: s,
                    end: e,
                    style: face_for("comment"),
                    conceal: false,
                    virt: None,
                });
            }
        }
        // Dim the `|` delimiters in header + data rows (matching the Org table slice). These length-1 spans
        // sort after the longer header-bold span and win, so the pipes read as separators, cells stay normal.
        if matches!(node.kind(), "pipe_table_header" | "pipe_table_row") {
            let (s, e) = (node.start_byte(), node.end_byte().min(src.len()));
            for (off, &byte) in src[s..e].iter().enumerate() {
                let i = s + off;
                if byte == b'|' && i >= visible.start && i < visible.end {
                    spans.push(Span {
                        start: i,
                        end: i + 1,
                        style: face_for("comment"),
                        conceal: false,
                        virt: None,
                    });
                }
            }
        }
        // A fenced code block's info string (the ```lang tag): face the language label dim so it reads as
        // a distinct tag rather than plain text. The body is left to the injection query (known languages
        // are highlighted; unknown ones stay plain). Faces the whole info_string span; falls through.
        if node.kind() == "info_string" {
            let (ls, le) = (node.start_byte(), node.end_byte());
            if ls < le && ls < visible.end && le > visible.start {
                spans.push(Span {
                    start: ls,
                    end: le,
                    style: face_for("label"),
                    conceal: false,
                    virt: None,
                });
            }
        }
        // A block quote: face the whole `> …` block dim italic (markup.quote). Does NOT `continue` — we
        // fall through to descend so the quote's inline text is re-parsed; those shorter emphasis/strong/
        // code spans sort AFTER this longer one and win last, so inner markup stays visible over the quote.
        if node.kind() == "block_quote" {
            let (qs, qe) = (node.start_byte(), node.end_byte());
            if qs < visible.end && qe > visible.start {
                spans.push(Span {
                    start: qs,
                    end: qe,
                    style: face_for("markup.quote"),
                    conceal: false,
                    virt: None,
                });
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    spans.sort_by_key(|s| std::cmp::Reverse(s.end - s.start));
    spans
}

/// The first descendant of `node` (pre-order, document order) whose kind is in `kinds`, or `None`.
fn first_descendant<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return Some(child);
        }
        if let Some(found) = first_descendant(child, kinds) {
            return Some(found);
        }
    }
    None
}

/// Re-parse one block-level `inline` node's bytes with the INLINE grammar and emit F-031 decorations for
/// the inline constructs, shifting offsets from the sub-parse's local coordinates into buffer coordinates
/// (the `injected_spans` offset-shift pattern). Concealed: the emphasis/strong/code delimiters and the
/// link brackets + destination; faced: the content each wraps. Only spans intersecting `visible` are kept.
fn inline_decorations(
    inline_node: tree_sitter::Node,
    src: &[u8],
    parser: &mut Parser,
    visible: &std::ops::Range<usize>,
    out: &mut Vec<Span>,
    out_virt: &mut Vec<VirtLine>,
    image_rows: u16,
) {
    let region = inline_node.byte_range();
    let Some(slice) = src.get(region.clone()) else {
        return;
    };
    let Some(subtree) = parser.parse(slice, None) else {
        return;
    };
    let base = region.start;
    // Push a decoration in BUFFER coordinates (local `+ base`) only if it intersects the viewport.
    let mut emit = |start: usize, end: usize, style: CellStyle, conceal: bool| {
        if end <= start {
            return;
        }
        let (s, e) = (base + start, base + end);
        if e > visible.start && s < visible.end {
            out.push(Span {
                start: s,
                end: e,
                style,
                conceal,
                virt: None,
            });
        }
    };
    let mut stack = vec![subtree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // `*x*` / `_x_`, `**x**` / `__x__`, `` `x` ``: the marker run at each end is concealed and the
            // content between them is faced. tree-sitter-md emits ONE `emphasis_delimiter` node per marker
            // char, so `**` is two adjacent delimiter nodes — collapse the leading and trailing contiguous
            // runs to find the content span (the delimiters always sit flush against the node's edges).
            kind @ ("emphasis" | "strong_emphasis" | "code_span") => {
                let delim_kind = if kind == "code_span" {
                    "code_span_delimiter"
                } else {
                    "emphasis_delimiter"
                };
                let mut cur = node.walk();
                let delims: Vec<_> = node
                    .children(&mut cur)
                    .filter(|c| c.kind() == delim_kind)
                    .collect();
                if !delims.is_empty() {
                    let face = match kind {
                        "emphasis" => face_for("markup.emphasis"),
                        "strong_emphasis" => face_for("markup.strong"),
                        _ => face_for("markup.code"),
                    };
                    // Leading run: delimiters flush against the node start.
                    let mut open_end = node.start_byte();
                    let mut i = 0;
                    while i < delims.len() && delims[i].start_byte() == open_end {
                        open_end = delims[i].end_byte();
                        i += 1;
                    }
                    // Trailing run: delimiters flush against the node end (stop before the leading run).
                    let mut close_start = node.end_byte();
                    let mut j = delims.len();
                    while j > i && delims[j - 1].end_byte() == close_start {
                        close_start = delims[j - 1].start_byte();
                        j -= 1;
                    }
                    emit(node.start_byte(), open_end, CellStyle::default(), true);
                    emit(open_end, close_start, face, false);
                    emit(close_start, node.end_byte(), CellStyle::default(), true);
                }
            }
            // `[label](url)`: show only `label` (faced as a link); conceal the `[`, and the `](url)` tail
            // (bracket + destination + any title + closing paren) after it.
            "inline_link" => {
                let label = {
                    let mut cur = node.walk();
                    let found = node
                        .children(&mut cur)
                        .find(|c| c.kind() == "link_text")
                        .map(|t| (t.start_byte(), t.end_byte()));
                    found
                };
                if let Some((ts, te)) = label {
                    emit(node.start_byte(), ts, CellStyle::default(), true);
                    emit(ts, te, face_for("markup.link"), false);
                    emit(te, node.end_byte(), CellStyle::default(), true);
                }
            }
            // `![alt](url)`: hide the raw markup on the source line and reserve a placeholder BLOCK below
            // it (F-031 slice 3a). Real pixels replace the block on a graphics-capable terminal (3b).
            "image" => {
                let after_line = ruse_core::pos::line_of(src, base + node.start_byte());
                // The alt text (`image_description`) labels the placeholder; the destination
                // (`link_destination`) is the file the render loop reads for real pixels.
                let child_range = |kind: &str| -> Option<(usize, usize)> {
                    let mut cur = node.walk();
                    let found = node.children(&mut cur).find(|c| c.kind() == kind);
                    found.map(|c| (c.start_byte(), c.end_byte()))
                };
                let text = |range: Option<(usize, usize)>| {
                    range
                        .and_then(|(s, e)| slice.get(s..e))
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .map(str::to_string)
                };
                let alt =
                    text(child_range("image_description")).unwrap_or_else(|| "image".to_string());
                let path = text(child_range("link_destination"));
                emit(
                    node.start_byte(),
                    node.end_byte(),
                    CellStyle::default(),
                    true,
                );
                out_virt.push(VirtLine {
                    after_line,
                    height: image_rows,
                    label: alt,
                    path,
                });
                continue; // don't descend into the image's alt/url children
            }
            _ => {}
        }
        // Descend so nested constructs (emphasis inside strong, a code span or emphasis inside a link
        // label) are decorated too; shorter inner spans sort after the outer ones and win last-wins.
        let mut cur = node.walk();
        stack.extend(node.children(&mut cur));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A normalized, order-independent key for comparing two span sets.
    fn key(spans: &[Span]) -> Vec<(usize, usize, Color)> {
        let mut v: Vec<_> = spans.iter().map(|s| (s.start, s.end, s.style.fg)).collect();
        v.sort();
        v
    }

    /// The whole-document range, for tests that want unrestricted spans.
    fn all(src: &[u8]) -> std::ops::Range<usize> {
        0..src.len()
    }

    #[test]
    fn markdown_conceals_heading_prefix_and_faces_content() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"# Title\n\nbody\n";
        let spans = h.spans(Revision(0), src, all(src));
        // The `# ` prefix (marker + space, bytes 0..2) is concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 0 && s.end == 2 && s.conceal),
            "the `# ` heading prefix is concealed; got {spans:?}",
        );
        // The heading content "Title" (bytes 2..7) is a bold title face, not concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 2 && !s.conceal && s.style.bold),
            "heading content is faced bold; got {spans:?}",
        );
        // Body text carries no decoration (nothing concealed off the heading).
        assert!(
            !spans.iter().any(|s| s.conceal && s.start >= 9),
            "no conceal leaks into the body; got {spans:?}",
        );
    }

    #[test]
    fn markdown_conceals_strong_markers_and_faces_content_bold() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"a **bold** z\n";
        let spans = h.spans(Revision(0), src, all(src));
        // The two `**` runs (bytes 2..4 and 8..10) are concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 2 && s.end == 4 && s.conceal),
            "opening `**` concealed; got {spans:?}",
        );
        assert!(
            spans
                .iter()
                .any(|s| s.start == 8 && s.end == 10 && s.conceal),
            "closing `**` concealed; got {spans:?}",
        );
        // `bold` (bytes 4..8) is faced bold and NOT concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 4 && s.end == 8 && s.style.bold && !s.conceal),
            "`bold` content faced bold, visible; got {spans:?}",
        );
    }

    #[test]
    fn markdown_conceals_code_backticks_and_faces_span() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"a `code` z\n";
        let spans = h.spans(Revision(0), src, all(src));
        // Backticks at bytes 2 and 7 are single-char delimiters, concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 2 && s.end == 3 && s.conceal),
            "opening backtick concealed; got {spans:?}",
        );
        assert!(
            spans
                .iter()
                .any(|s| s.start == 7 && s.end == 8 && s.conceal),
            "closing backtick concealed; got {spans:?}",
        );
        // `code` (bytes 3..7) is faced (distinct green) and not concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 3 && s.end == 7 && s.style.fg == Color::Green && !s.conceal),
            "`code` content faced, visible; got {spans:?}",
        );
    }

    #[test]
    fn markdown_conceals_emphasis_markers_and_faces_italic() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"a *x* z\n";
        let spans = h.spans(Revision(0), src, all(src));
        // Single `*` markers at bytes 2 and 4 are concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 2 && s.end == 3 && s.conceal),
            "opening `*` concealed; got {spans:?}",
        );
        assert!(
            spans
                .iter()
                .any(|s| s.start == 4 && s.end == 5 && s.conceal),
            "closing `*` concealed; got {spans:?}",
        );
        // `x` (byte 3..4) is faced italic and visible.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 3 && s.end == 4 && s.style.italic && !s.conceal),
            "`x` faced italic, visible; got {spans:?}",
        );
    }

    #[test]
    fn markdown_shows_link_label_and_conceals_url() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        // `[lab](http://x)` — label at bytes 1..4, the `[` at 0, and `](http://x)` at 4..15.
        let src = b"[lab](http://x)\n";
        let spans = h.spans(Revision(0), src, all(src));
        // The label is faced as a link (blue underline) and visible.
        assert!(
            spans.iter().any(|s| s.start == 1
                && s.end == 4
                && s.style.fg == Color::Blue
                && s.style.underline
                && !s.conceal),
            "link label faced + visible; got {spans:?}",
        );
        // The opening `[` (0..1) is concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 0 && s.end == 1 && s.conceal),
            "opening `[` concealed; got {spans:?}",
        );
        // The `](url)` tail (4..15) is concealed, so the URL never shows.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 4 && s.end == 15 && s.conceal),
            "the `](url)` tail concealed; got {spans:?}",
        );
    }

    #[test]
    fn markdown_heading_content_is_not_inline_reparsed() {
        // Heading behavior is unchanged by slice 1b: `# *hi*` still conceals only the `# ` prefix and
        // faces the whole content as a heading — the `*` markers are NOT concealed as emphasis.
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"# *hi*\n";
        let spans = h.spans(Revision(0), src, all(src));
        // `# ` prefix concealed.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 0 && s.end == 2 && s.conceal),
            "`# ` prefix concealed; got {spans:?}",
        );
        // No emphasis-marker conceal inside the heading content (the `*` at byte 2 stays visible).
        assert!(
            !spans
                .iter()
                .any(|s| s.start == 2 && s.end == 3 && s.conceal),
            "heading content is not inline-reparsed; got {spans:?}",
        );
    }

    #[test]
    fn markdown_image_makes_a_virtline_placeholder() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"text\n\n![a cat](cat.png)\n\nmore\n";
        let (spans, virt) = h.spans_and_virt(Revision(0), src, all(src));
        assert_eq!(
            virt.len(),
            1,
            "one image -> one placeholder block; got {virt:?}"
        );
        assert_eq!(virt[0].label, "a cat", "the alt text is the block label");
        assert_eq!(
            virt[0].after_line, 2,
            "block sits on the image's buffer line (0-based)"
        );
        assert!(virt[0].height >= 1);
        assert!(
            spans.iter().any(|s| s.conceal),
            "the raw image markup is concealed; got {spans:?}",
        );
    }

    #[test]
    fn markdown_list_markers_become_bullets_and_checkboxes() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"- a\n\n- [ ] b\n\n- [x] c\n";
        let spans = h.spans(Revision(0), src, all(src));
        assert!(
            spans
                .iter()
                .any(|s| s.conceal && s.virt == Some("\u{2022} ")),
            "unordered `- ` -> bullet virt; got {spans:?}",
        );
        assert!(
            spans
                .iter()
                .any(|s| s.conceal && s.virt == Some("\u{2610} ")),
            "task `[ ]` -> unchecked box virt; got {spans:?}",
        );
        assert!(
            spans
                .iter()
                .any(|s| s.conceal && s.virt == Some("\u{2611} ")),
            "task `[x]` -> checked box virt; got {spans:?}",
        );
    }

    #[test]
    fn markdown_table_header_bold_and_delimiter_dim() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"| a | b |\n|---|---|\n| 1 | 2 |\n";
        let spans = h.spans(Revision(0), src, all(src));
        // The header row (bytes 0..9) carries a bold face.
        assert!(
            spans
                .iter()
                .any(|s| s.start == 0 && !s.conceal && s.style.bold),
            "header row is bold; got {spans:?}",
        );
        // The delimiter row (starts at byte 10) is dimmed (DarkGrey).
        let delim = "| a | b |\n".len();
        assert!(
            spans
                .iter()
                .any(|s| s.start == delim && !s.conceal && s.style.fg == Color::DarkGrey),
            "delimiter row is dimmed; got {spans:?}",
        );
        // The header row's leading `|` (byte 0) is dimmed as a length-1 span (wins over the bold).
        assert!(
            spans
                .iter()
                .any(|s| s.start == 0 && s.end == 1 && s.style.fg == Color::DarkGrey),
            "header pipe delimiter is dimmed; got {spans:?}",
        );
    }

    #[test]
    fn markdown_fenced_code_language_label_is_faced() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"```rust\nfn main() {}\n```\n";
        let spans = h.spans(Revision(0), src, all(src));
        // "rust" (bytes 3..7) is faced as a label (DarkYellow), not concealed.
        let label_fg = face_for("label").fg;
        assert!(
            spans
                .iter()
                .any(|s| s.start == 3 && s.end == 7 && !s.conceal && s.style.fg == label_fg),
            "the ```rust language label is faced; got {spans:?}",
        );
    }

    #[test]
    fn markdown_block_quote_is_faced_dim_and_inner_markup_wins() {
        let mut h = CachedHighlight::for_ext("md").expect("markdown grammar loads");
        let src = b"> quoted **bold** text\n";
        let spans = h.spans(Revision(0), src, all(src));
        // The block quote carries a dim (DarkGrey) italic face over its whole span.
        assert!(
            spans
                .iter()
                .any(|s| !s.conceal && s.style.fg == Color::DarkGrey && s.style.italic),
            "block quote is faced dim italic; got {spans:?}",
        );
        // The inner `**bold**` still emits a bold face (a shorter span that wins over the quote base).
        assert!(
            spans.iter().any(|s| !s.conceal && s.style.bold),
            "inner strong markup survives inside the quote; got {spans:?}",
        );
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
                .any(|s| s.start == 0 && s.style.fg == Color::Magenta && s.style.bold),
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
                .any(|s| s.start == 10 && s.style.fg == Color::Magenta),
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
    fn each_supported_language_loads_and_highlights() {
        // Every dispatched extension must build a highlighter and produce at least one span for a small
        // real snippet — this pins that the per-crate query-const name (HIGHLIGHT_QUERY vs
        // HIGHLIGHTS_QUERY) and the grammar are wired correctly. Alias extensions map to the same grammar.
        let cases: &[(&str, &[u8])] = &[
            ("rs", b"fn main() { let x = 1; }"),
            ("json", br#"{"k": 1}"#),
            ("py", b"def f():\n    return 1\n"),
            ("sh", b"echo \"$HOME\"\n"),
            ("bash", b"for i in 1 2; do echo $i; done\n"),
            ("c", b"int main(void) { return 0; }"),
            ("h", b"#define N 1\n"),
            ("go", b"package main\nfunc main() {}\n"),
            ("js", b"const x = () => 42;"),
            ("jsx", b"const el = <div/>;"),
            ("css", b"a { color: red; }"),
        ];
        for (ext, src) in cases {
            let mut h =
                CachedHighlight::for_ext(ext).unwrap_or_else(|| panic!("{ext} grammar loads"));
            assert!(
                !h.spans(Revision(0), src, 0..src.len()).is_empty(),
                "{ext} produces highlight spans",
            );
        }
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
                .map(|s| s.style.fg)
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
