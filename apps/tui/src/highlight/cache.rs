//! The incremental reparse cache: a [`Highlight`] kept across revisions, with the edit derived by diffing.

use ruse_core::Revision;
use tree_sitter::{InputEdit, Parser, Point, Tree};

use super::markdown::markdown_decorations;
use super::{Highlight, Span, VirtLine, IMAGE_PLACEHOLDER_ROWS};

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

    /// The cached tree lined up with `src`: when the buffer moved on since the last parse (an edit earlier in
    /// the same frame), a copy edited by the difference, so its nodes sit at `src`'s offsets (the text that
    /// changed is not reparsed). For the Python indenter's string and comment lookups (#1205).
    pub fn tree_matching(&self, src: &[u8]) -> Option<Tree> {
        let mut tree = self.tree.clone()?;
        if self.src != src {
            if let Some(edit) = input_edit(&self.src, src) {
                tree.edit(&edit);
            }
        }
        Some(tree)
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
            let had_tree = self.tree.is_some() || self.rev.is_none();
            self.tree = self.hl.parser.parse(src, self.tree.as_ref());
            // No tree means no highlighting until a parse succeeds again: logged when it starts.
            if self.tree.is_none() && had_tree {
                crate::log::degrade("syntax", "parse_failed", "");
            }
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
        row: ruse_core::pos::line_of(src, ruse_core::BytePos(byte)),
        column: byte.min(src.len()) - ruse_core::pos::line_start(src, ruse_core::BytePos(byte)).0,
    }
}
