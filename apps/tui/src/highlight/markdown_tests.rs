use crossterm::style::Color;
use ruse_core::Revision;

use super::tests::all;
use super::*;

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
