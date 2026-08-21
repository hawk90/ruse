//! Org-mode rich-render decorations (F-031 slice 4) — a hand-rolled LINE scanner, NOT tree-sitter:
//! `tree-sitter-org` pins tree-sitter 0.20, incompatible with the 0.26 every other grammar here uses.
//! Org's basic markup is regular enough for a small scanner, and it reuses the F-031 decoration channels
//! (conceal / face / virt_text) + `highlight::face_for` for faces consistent with Markdown. Closes F-031
//! acceptance #5 (Markdown AND Org render headings, emphasis, links, checkboxes). See docs/design/rich-rendering.md.

use std::ops::Range;

use crate::highlight::{face_for, Span, VirtLine};
use crate::screen::CellStyle;

/// The single-char inline emphasis markers and the face each selects (Org: `*bold*` `/italic/`
/// `~code~` `=verbatim=` `_underline_` `+strike+`).
const EMPHASIS: &[(u8, &str)] = &[
    (b'*', "markup.strong"),
    (b'/', "markup.emphasis"),
    (b'~', "markup.code"),
    (b'=', "markup.code"),
    (b'_', "markup.emphasis"),
    (b'+', "markup.strong"),
];

fn conceal(start: usize, end: usize) -> Span {
    Span {
        start,
        end,
        style: CellStyle::default(),
        conceal: true,
        virt: None,
    }
}
fn faced(start: usize, end: usize, name: &str) -> Span {
    Span {
        start,
        end,
        style: face_for(name),
        conceal: false,
        virt: None,
    }
}
fn bullet(start: usize, end: usize, glyph: &'static str) -> Span {
    Span {
        start,
        end,
        style: CellStyle::default(),
        conceal: true,
        virt: Some(glyph),
    }
}

/// Org decorations for the `visible` byte range of `src`. Line-oriented: headings (leading `*` run),
/// list bullets / checkboxes, inline links + emphasis, and inline IMAGES — a description-less
/// `[[file:x.png]]` link conceals its markup and reserves an `image_rows`-tall block below (mirrors
/// Markdown `![](…)`; the render loop draws real pixels or the text fallback via the returned `VirtLine`).
pub fn decorations(
    src: &[u8],
    visible: &Range<usize>,
    image_rows: u16,
) -> (Vec<Span>, Vec<VirtLine>) {
    let text = std::str::from_utf8(src).unwrap_or("");
    let mut spans = Vec::new();
    let mut virt = Vec::new();
    let mut base = 0usize;
    for (line_idx, line) in text.split_inclusive('\n').enumerate() {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if base < visible.end && base + line.len() > visible.start {
            scan_line(content, base, line_idx, image_rows, &mut spans, &mut virt);
        }
        base += line.len();
    }
    (spans, virt)
}

/// The image file `path` if `target` is an Org image link destination — a `file:`-prefixed or bare path
/// ending in a known image extension. `None` for a non-image link (a URL, a `.org`/`.txt` target, …).
fn image_path(target: &str) -> Option<String> {
    let p = target.strip_prefix("file:").unwrap_or(target);
    let ext = p.rsplit('.').next()?.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg"
    )
    .then(|| p.to_string())
}

#[allow(clippy::too_many_arguments)] // a line scan legitimately needs the full decoration context
fn scan_line(
    content: &str,
    base: usize,
    line_idx: usize,
    image_rows: u16,
    out: &mut Vec<Span>,
    out_virt: &mut Vec<VirtLine>,
) {
    let b = content.as_bytes();
    // Heading: a leading run of `*` then a space — conceal `«stars» ` and face the title.
    let stars = b.iter().take_while(|&&c| c == b'*').count();
    if stars > 0 && b.get(stars) == Some(&b' ') {
        out.push(conceal(base, base + stars + 1));
        out.push(faced(
            base + stars + 1,
            base + content.len(),
            "markup.heading",
        ));
        return;
    }
    let indent = b.iter().take_while(|&&c| c == b' ').count();
    let rest = &b[indent..];
    // Table row: optional indent then `|`. A SEPARATOR row (only `|`/`-`/`+`/space, e.g. `|---+---|`) is
    // dimmed whole; a DATA row dims just its `|` delimiters, leaving cell text (and its inline markup)
    // normal. Header-vs-body styling needs cross-line lookahead and is deferred.
    if rest.first() == Some(&b'|') {
        if rest.iter().all(|&c| matches!(c, b'|' | b'-' | b'+' | b' ')) {
            out.push(faced(base + indent, base + content.len(), "comment"));
        } else {
            for (i, &c) in b.iter().enumerate() {
                if c == b'|' {
                    out.push(faced(base + i, base + i + 1, "comment"));
                }
            }
        }
        scan_inline(content, indent, base, line_idx, image_rows, out, out_virt);
        return;
    }
    // List item: optional indent, then `-`/`+`, then a space. A `[ ]`/`[X]` after it is a checkbox.
    if matches!(rest.first(), Some(b'-') | Some(b'+')) && rest.get(1) == Some(&b' ') {
        let after = &b[indent + 2..];
        let (glyph, prefix_end): (&'static str, usize) =
            if after.len() >= 3 && after[0] == b'[' && after[2] == b']' {
                let g = match after[1] {
                    b'X' | b'x' => "\u{2611} ", // ☑
                    b' ' => "\u{2610} ",        // ☐
                    _ => "\u{2022} ",           // • (a non-checkbox bracket)
                };
                let mut pe = indent + 2 + 3;
                if b.get(pe) == Some(&b' ') {
                    pe += 1;
                }
                (g, pe)
            } else {
                ("\u{2022} ", indent + 2)
            };
        out.push(bullet(base + indent, base + prefix_end, glyph));
        scan_inline(
            content, prefix_end, base, line_idx, image_rows, out, out_virt,
        );
        return;
    }
    scan_inline(content, 0, base, line_idx, image_rows, out, out_virt);
}

#[allow(clippy::too_many_arguments)] // the inline scan carries the same decoration context as scan_line
fn scan_inline(
    content: &str,
    from: usize,
    base: usize,
    line_idx: usize,
    image_rows: u16,
    out: &mut Vec<Span>,
    out_virt: &mut Vec<VirtLine>,
) {
    let b = content.as_bytes();
    let mut i = from;
    while i < b.len() {
        // Org link: `[[target]]` or `[[target][label]]` — show only the label (or target), hide the rest.
        if b[i] == b'[' && b.get(i + 1) == Some(&b'[') {
            if let Some(close) = content[i + 2..].find("]]").map(|p| i + 2 + p) {
                let inner = &content[i + 2..close];
                if let Some(sep) = inner.find("][") {
                    let label_start = i + 2 + sep + 2;
                    out.push(conceal(base + i, base + label_start));
                    out.push(faced(base + label_start, base + close, "markup.link"));
                } else if let Some(path) = image_path(inner) {
                    // A description-less image link: conceal the whole `[[…]]` on the source line and
                    // reserve an image block below it (F-031). Real pixels / text fallback come from the
                    // render loop reading `path` — the same VirtLine channel Markdown `![](…)` uses.
                    out.push(conceal(base + i, base + close + 2));
                    let label = path.rsplit('/').next().unwrap_or(&path).to_string();
                    out_virt.push(VirtLine {
                        after_line: line_idx,
                        height: image_rows,
                        label,
                        path: Some(path),
                    });
                    i = close + 2;
                    continue;
                } else {
                    out.push(conceal(base + i, base + i + 2));
                    out.push(faced(base + i + 2, base + close, "markup.link"));
                }
                out.push(conceal(base + close, base + close + 2));
                i = close + 2;
                continue;
            }
        }
        // Inline emphasis: a marker pair with Org word-boundary context.
        if let Some((face, close)) = try_emphasis(b, i) {
            out.push(conceal(base + i, base + i + 1));
            out.push(faced(base + i + 1, base + close, face));
            out.push(conceal(base + close, base + close + 1));
            i = close + 1;
            continue;
        }
        i += 1;
    }
}

/// A byte that may precede an OPENING emphasis marker (start-of-line or an opener/space).
fn pre_ok(c: Option<u8>) -> bool {
    matches!(
        c,
        None | Some(b' ') | Some(b'\t') | Some(b'(') | Some(b'{') | Some(b'\'') | Some(b'"')
    )
}
/// A byte that may follow a CLOSING emphasis marker (end-of-line or a closer/space/punctuation).
fn post_ok(c: Option<u8>) -> bool {
    matches!(
        c,
        None | Some(b' ')
            | Some(b'\t')
            | Some(b')')
            | Some(b'}')
            | Some(b'.')
            | Some(b',')
            | Some(b';')
            | Some(b':')
            | Some(b'!')
            | Some(b'?')
            | Some(b'\'')
            | Some(b'"')
    )
}

/// If a valid emphasis span opens at `b[i]`, return its face and the byte index of its CLOSING marker.
/// Org's rule (approximated): the opener is at a word boundary and not followed by space; the closer is
/// the next same marker that is not preceded by space and is at a word boundary; the content is non-empty.
fn try_emphasis(b: &[u8], i: usize) -> Option<(&'static str, usize)> {
    let m = b[i];
    let face = EMPHASIS.iter().find(|(c, _)| *c == m)?.1;
    let pre = if i == 0 { None } else { Some(b[i - 1]) };
    if !pre_ok(pre) {
        return None;
    }
    match b.get(i + 1) {
        Some(&c) if c != b' ' && c != b'\t' && c != m => {}
        _ => return None,
    }
    let mut j = i + 2;
    while j < b.len() {
        if b[j] == m && b[j - 1] != b' ' && b[j - 1] != b'\t' && post_ok(b.get(j + 1).copied()) {
            return Some((face, j));
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(src: &str) -> Vec<Span> {
        decorations(src.as_bytes(), &(0..src.len()), 12).0
    }
    fn virt(src: &str) -> Vec<VirtLine> {
        decorations(src.as_bytes(), &(0..src.len()), 12).1
    }

    #[test]
    fn heading_conceals_stars_and_faces_title() {
        let s = spans("** Sub heading\n");
        assert!(
            s.iter().any(|x| x.start == 0 && x.end == 3 && x.conceal),
            "the `** ` prefix is concealed; got {s:?}",
        );
        assert!(
            s.iter().any(|x| x.start == 3 && !x.conceal && x.style.bold),
            "title is faced bold; got {s:?}",
        );
    }

    #[test]
    fn emphasis_conceals_markers_and_faces_content() {
        let s = spans("a *bold* and /it/ and ~code~ here\n");
        // *bold*: markers at 2 and 7 concealed, "bold" (3..7) strong.
        assert!(s.iter().any(|x| x.start == 2 && x.end == 3 && x.conceal));
        assert!(s
            .iter()
            .any(|x| x.start == 3 && x.end == 7 && x.style.bold && !x.conceal));
        assert!(s.iter().any(|x| x.start == 7 && x.end == 8 && x.conceal));
        // /it/ italic and ~code~ code present.
        assert!(s.iter().any(|x| !x.conceal && x.style.italic));
        assert!(s
            .iter()
            .any(|x| !x.conceal && x.style.fg == crossterm::style::Color::Green));
    }

    #[test]
    fn emphasis_ignores_bare_operators() {
        // `2 * 3 / 4` has spaces around the markers -> no emphasis, no conceal.
        let s = spans("2 * 3 / 4\n");
        assert!(
            !s.iter().any(|x| x.conceal),
            "bare operators are not emphasis; got {s:?}"
        );
    }

    #[test]
    fn link_shows_label_and_hides_target() {
        let s = spans("see [[https://x.io][the site]] now\n");
        // "the site" is faced link; the `[[…][` prefix and `]]` are concealed.
        assert!(
            s.iter().any(|x| !x.conceal && x.style.underline),
            "label is a faced link; got {s:?}",
        );
        assert!(
            s.iter().filter(|x| x.conceal).count() >= 2,
            "target + brackets concealed"
        );
    }

    #[test]
    fn image_link_reserves_a_block_and_conceals_markup() {
        // A description-less image link on line 1 → concealed markup + one VirtLine after line 1.
        let src = "before\n[[file:/tmp/pic.png]]\nafter\n";
        let v = virt(src);
        assert_eq!(v.len(), 1, "one image block; got {v:?}");
        assert_eq!(v[0].after_line, 1, "block sits after the link's line");
        assert_eq!(v[0].height, 12, "reserves the graphics-height rows");
        assert_eq!(
            v[0].path.as_deref(),
            Some("/tmp/pic.png"),
            "file: prefix stripped"
        );
        assert_eq!(v[0].label, "pic.png", "label is the basename");
        // The whole [[…]] is concealed on the source line.
        let base = "before\n".len();
        let s = spans(src);
        assert!(
            s.iter().any(|x| x.conceal && x.start == base),
            "link markup concealed; got {s:?}",
        );
    }

    #[test]
    fn described_link_and_non_image_target_are_not_images() {
        // A link WITH a description is a normal link even if it points at an image.
        assert!(
            virt("[[file:/tmp/pic.png][a pic]]\n").is_empty(),
            "described link is not inlined"
        );
        // A non-image target reserves no block.
        assert!(
            virt("[[https://example.com]]\n").is_empty(),
            "URL is not an image"
        );
        assert!(
            virt("[[file:notes.org]]\n").is_empty(),
            ".org is not an image"
        );
    }

    #[test]
    fn table_dims_separators_and_pipe_delimiters() {
        use crossterm::style::Color;
        let src = "| a | b |\n|---+---|\n| 1 | 2 |\n";
        let s = spans(src);
        // The separator row (line 2, bytes 10..19) is dimmed whole.
        let sep_start = "| a | b |\n".len();
        assert!(
            s.iter()
                .any(|x| x.start == sep_start && !x.conceal && x.style.fg == Color::DarkGrey),
            "separator row dimmed whole; got {s:?}",
        );
        // The header row's leading `|` (byte 0) is dimmed, its cell text is not.
        assert!(
            s.iter()
                .any(|x| x.start == 0 && x.end == 1 && x.style.fg == Color::DarkGrey),
            "data-row `|` delimiter dimmed; got {s:?}",
        );
    }

    #[test]
    fn list_bullet_and_checkboxes() {
        let s = spans("- one\n- [ ] todo\n- [X] done\n");
        assert!(
            s.iter().any(|x| x.conceal && x.virt == Some("\u{2022} ")),
            "bullet"
        );
        assert!(
            s.iter().any(|x| x.conceal && x.virt == Some("\u{2610} ")),
            "unchecked box"
        );
        assert!(
            s.iter().any(|x| x.conceal && x.virt == Some("\u{2611} ")),
            "checked box"
        );
    }
}
