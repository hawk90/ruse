//! The Markdown tree walk: heading, inline, list, table and quote faces, conceal, and image blocks.

use tree_sitter::{Parser, Tree};

use super::{face_for, Span, VirtLine};
use crate::screen::CellStyle;

/// Markdown rich-render decorations (F-031). The BLOCK grammar (slice 1) handles ATX headings: CONCEAL
/// the `# ` prefix (`[heading.start, content.start)`) and FACE the heading content as a bold title. The
/// INLINE grammar (slice 1b) handles the paragraph text the block grammar leaves as opaque `inline`
/// nodes: each such node's bytes are re-parsed ([`inline_decorations`]) to conceal emphasis / strong /
/// code-span markers and link punctuation while facing their content. Only decorations intersecting
/// `visible` are emitted. Produced by a tree walk rather than a highlights query because conceal is not a
/// colour. `inline` is the INLINE parser (see [`super::CachedHighlight::inline`]); `None` disables
/// inline faces.
pub(super) fn markdown_decorations(
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
            face_node(&mut spans, node, visible, "markup.strong");
        }
        if node.kind() == "pipe_table_delimiter_row" {
            face_node(&mut spans, node, visible, "comment");
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
            face_node(&mut spans, node, visible, "markup.quote");
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    spans.sort_by_key(|s| std::cmp::Reverse(s.end - s.start));
    spans
}

/// Face the whole of `node` with the `face` capture's style, when it intersects `visible`.
fn face_node(
    spans: &mut Vec<Span>,
    node: tree_sitter::Node,
    visible: &std::ops::Range<usize>,
    face: &str,
) {
    let (s, e) = (node.start_byte(), node.end_byte());
    if s < visible.end && e > visible.start {
        spans.push(Span {
            start: s,
            end: e,
            style: face_for(face),
            conceal: false,
            virt: None,
        });
    }
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
                let after_line =
                    ruse_core::pos::line_of(src, ruse_core::BytePos(base + node.start_byte()));
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
