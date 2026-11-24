//! The `it` / `at` tag text object.

/// The leading tag NAME of a tag's interior bytes (the run after `<` or `</`): ASCII letters/digits and
/// `-`/`_`/`:`, stopping at whitespace, `/`, or end. `<div class=x>` → `div`; `</div>` interior → `div`.
fn tag_name(interior: &[u8]) -> &[u8] {
    let end = interior
        .iter()
        .position(|&c| !(c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':')))
        .unwrap_or(interior.len());
    &interior[..end]
}

/// The HTML/XML tag block enclosing `cur` (Vim `it`/`at`) — a nesting-aware byte scan (no syntax tree
/// needed, like bracket matching). `around` = `at` (include the `<tag>`/`</tag>` delimiters), else `it`
/// (the content between them). `count` picks how many nesting levels to climb from the innermost enclosing
/// pair: `1` (bare `it`/`at`) is the innermost, `2` (`2it`/`2at`) its enclosing tag, and so on (Vim's
/// count-expands-outward semantics). Returns `(cur, cur)` (a no-op) when the cursor is not inside any tag
/// pair, or when `count` exceeds the enclosing nesting depth (`3dit` on a 2-deep block is a no-op in Vim).
/// The returned offsets sit at ASCII `<`/`>` boundaries, so they are always char-boundary safe.
pub(super) fn tag_span(b: &[u8], cur: usize, around: bool, count: u32) -> (usize, usize) {
    let n = b.len();
    // Open tags awaiting their close: (name, open_start, open_end-exclusive).
    let mut stack: Vec<(&[u8], usize, usize)> = Vec::new();
    // Every completed pair CONTAINING `cur`, as (open_start, open_end, close_start, close_end). Collected
    // as their closes are found; later sorted innermost-first so `count` can index outward.
    let mut enclosing: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i + 1;
        while j < n && b[j] != b'>' {
            j += 1;
        }
        if j >= n {
            break; // unterminated `<…`
        }
        let end = j + 1; // one past the `>`
        let interior = &b[start + 1..j];
        if interior.first() == Some(&b'/') {
            // Close tag `</name>`: pop to the matching open, forming a pair. Names match without regard
            // to case: nvim's `current_tagblock` searches with `\c`, so `<A>x</a>` is a block.
            let name = tag_name(&interior[1..]);
            while let Some((oname, os, oe)) = stack.pop() {
                if oname.eq_ignore_ascii_case(name) {
                    if os <= cur && cur < end {
                        enclosing.push((os, oe, start, end));
                    }
                    break;
                }
                // Mismatched close (malformed markup): keep popping to recover.
            }
        } else if interior.last() != Some(&b'/') {
            // Open tag `<name …>` (a `<name/>` self-close has no content and is skipped).
            let name = tag_name(interior);
            if !name.is_empty() {
                stack.push((name, start, end));
            }
        }
        i = end;
    }
    // Innermost first (smallest span). `count == 1` picks it; each extra count climbs one level out.
    enclosing.sort_by_key(|&(os, _, _, ce)| ce - os);
    match enclosing.get((count.max(1) - 1) as usize) {
        Some(&(os, _, _, ce)) if around => (os, ce),
        Some(&(_, oe, cs, _)) => (oe, cs),
        None => (cur, cur),
    }
}

/// Whether `c` is a Vim keyword char under the default `'iskeyword'` (`@,48-57,_,192-255`).
fn is_keyword(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 192
}

/// Whether the backward start pattern of nvim's `current_tagblock`,
/// `<[^ \t>/!]\+\%(\_s\_[^>]\{-}[^/]>\|$\|\_s\=>\)`, matches at `i`: an open tag that is not
/// self-closing, or an open tag left unterminated at the end of its line.
fn open_tag_at(b: &[u8], i: usize) -> bool {
    let name_len = b[i + 1..]
        .iter()
        .position(|&c| matches!(c, b' ' | b'\t' | b'>' | b'/' | b'!' | b'\n'))
        .unwrap_or(b.len() - i - 1);
    if name_len == 0 {
        return false;
    }
    let j = i + 1 + name_len;
    match b.get(j) {
        None | Some(b'\n' | b'>') => true, // `$` or `>`
        Some(b' ' | b'\t') => after_space_closes(b, j),
        _ => false, // `/` or `!` after the name
    }
}

/// The tail `\_s\=>` or `\_s\_[^>]\{-}[^/]>` of a tag pattern, from the whitespace at `ws`: the tag
/// ends at its first `>`, which closes it unless a `/` sits right before it (`<a x/>`).
fn after_space_closes(b: &[u8], ws: usize) -> bool {
    if b.get(ws + 1) == Some(&b'>') {
        return true;
    }
    match b[ws + 1..].iter().position(|&c| c == b'>') {
        Some(k) => k > 0 && b[ws + k] != b'/',
        None => false,
    }
}

/// Whether the end pattern `</[^>]*>` matches at `i` (on one line).
fn close_tag_at(b: &[u8], i: usize) -> bool {
    b.get(i + 1) == Some(&b'/')
        && b[i + 2..]
            .iter()
            .find(|&&c| c == b'>' || c == b'\n')
            .is_some_and(|&c| c == b'>')
}

/// `do_searchpair(start, "", "</[^>]*>", BACKWARD)` from `pos`: the nearest open tag before `pos` not
/// closed before it. Like nvim, the nesting is counted without regard to tag names.
fn search_open_back(b: &[u8], pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..pos.min(b.len())).rev() {
        if b[i] != b'<' {
            continue;
        }
        if close_tag_at(b, i) {
            depth += 1;
        } else if open_tag_at(b, i) {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

/// `do_searchpair(spat, "", epat, FORWARD)` past `from` for the open tag NAME: `spat` is
/// `<name\>\%(\_s\_[^>]\{-}\_[^/]>\|\_s\?>\)\c` and `epat` is `</name>\c`. Returns the `<` of the
/// matching `</name>`.
fn search_close_fwd(b: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let ci = |at: usize| {
        b.get(at..at + name.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
    };
    let mut depth = 0usize;
    for i in from + 1..b.len() {
        if b[i] != b'<' {
            continue;
        }
        if b.get(i + 1) == Some(&b'/') {
            if ci(i + 2) && b.get(i + 2 + name.len()) == Some(&b'>') {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
        } else if ci(i + 1) && name.last().is_some_and(|&c| is_keyword(c)) {
            let j = i + 1 + name.len();
            let opens = match b.get(j) {
                Some(b'>') => true,
                Some(b' ' | b'\t' | b'\n') => after_space_closes(b, j),
                _ => false,
            };
            if opens {
                depth += 1;
            }
        }
    }
    None
}

/// nvim's `in_html_tag`: whether `cur` sits inside a start tag (`end_tag == false`, one not closed by
/// `/>`) or an end tag (`end_tag == true`). Returns the tag's `<`. The `<` is looked for on the
/// cursor's line, stopping at a `>` before the cursor.
fn in_html_tag(b: &[u8], cur: usize, end_tag: bool) -> Option<usize> {
    let line = b[..cur]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |p| p + 1);
    let mut p = cur;
    while p > line {
        if b.get(p) == Some(&b'<') {
            break;
        }
        p -= 1;
        if b[p] == b'>' {
            break;
        }
    }
    if b.get(p) != Some(&b'<') {
        return None;
    }
    if end_tag {
        return (b.get(p + 1) == Some(&b'/')).then_some(p);
    }
    if b.get(p + 1) == Some(&b'/') {
        return None;
    }
    let gt = p + 1 + b[p + 1..].iter().position(|&c| c == b'>')?;
    (b[gt - 1] != b'/').then_some(p)
}

/// Vim `it`/`at` in Visual mode — a port of nvim's `current_tagblock` (textobject.c) with the Visual
/// area `anchor`..=`cursor` (inclusive, `'selection'` inclusive). Returns the new `(anchor, cursor)`,
/// or `None` when no block encloses the area (the selection is left as it was).
///
/// The rules that differ from the operator form:
/// - On a one-char area the search starts at the cursor (moved to the `>` of a start tag it is in, or
///   before the `<` of an end tag); on a wider one it starts at the area's low end.
/// - A block whose end tag starts before the area's high end does not enclose it: the search climbs
///   one level out from that block (`count = 1; goto again`).
/// - `it` whose inner text is exactly the current area (`vit` on `<a>x</a>`, or `vitit`) takes the
///   whole block instead (`do_include = true`), so repeating `it`/`at` widens level by level.
/// - An empty inner (`<a></a>`) selects the char at its start.
pub(crate) fn tag_visual(
    b: &[u8],
    anchor: usize,
    cursor: usize,
    around: bool,
    count: u32,
) -> Option<(usize, usize)> {
    let count_arg = count.max(1);
    let cursor = cursor.min(b.len());
    let anchor = anchor.min(b.len());
    // `old_start`/`old_end`: the area before the object (inclusive). `pos`: where the search starts.
    let (mut pos, old_start, mut old_end);
    if anchor == cursor {
        old_start = cursor;
        old_end = cursor;
        pos = cursor;
        // "ignore indent": step off leading blanks, stopping at the line's end.
        let line = b[..pos]
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |p| p + 1);
        while pos < b.len()
            && matches!(b[pos], b' ' | b'\t')
            && b[line..pos].iter().all(|&c| matches!(c, b' ' | b'\t'))
        {
            pos += 1;
        }
        if in_html_tag(b, pos, false).is_some() {
            // On a start tag: move to its '>'.
            pos += b[pos..].iter().position(|&c| c == b'>').unwrap_or(0);
        } else if let Some(lt) = in_html_tag(b, pos, true) {
            // On an end tag: move to just before it.
            pos = super::prev_grapheme(b, lt);
            old_end = pos;
        }
    } else if anchor < cursor {
        old_start = anchor;
        old_end = cursor;
        pos = anchor;
    } else {
        old_start = cursor;
        old_end = anchor;
        pos = cursor;
    }
    let mut count = count_arg;
    let mut include = around;
    loop {
        // "again": search backwards `count` times for an unclosed "<aaa>".
        let mut open = pos;
        for _ in 0..count {
            open = search_open_back(b, open)?;
        }
        let name_len = b[open + 1..]
            .iter()
            .position(|&c| matches!(c, b'>' | b' ' | b'\t' | b'\n'))
            .unwrap_or(b.len() - open - 1);
        if name_len == 0 {
            return None;
        }
        let name = &b[open + 1..open + 1 + name_len];
        let close = match search_close_fwd(b, open + 1, name) {
            Some(c) if c >= old_end => c,
            // No matching end, or it ends before the area: try the next enclosing start tag.
            _ => {
                count = 1;
                pos = open;
                continue;
            }
        };
        let (start, end) = if include {
            let gt = close + b[close..].iter().position(|&c| c == b'>').unwrap_or(0);
            (open, gt)
        } else {
            // Exclude the '<' of the end tag and the start tag through its '>'.
            let end = super::prev_grapheme(b, close);
            let gt = open + b[open..].iter().position(|&c| c == b'>').unwrap_or(0);
            let start = gt + 1;
            // "If we are in Visual mode and now have the same text as before set do_include and
            // try again."
            if start == old_start && end == old_end {
                include = true;
                pos = old_start;
                count = count_arg;
                continue;
            }
            (start, end)
        };
        // No text between the tags: select the char under the cursor (the start).
        return Some(if end < start {
            (start, start)
        } else {
            (start, end)
        });
    }
}

#[cfg(test)]
mod tag_object_tests {
    //! Vim `it`/`at` — HTML/XML tag text objects (nesting-aware byte scan).
    use crate::motion::{char_span, Motion};

    fn tag(around: bool) -> Motion {
        Motion::Tag { around }
    }

    #[test]
    fn inner_and_around_tag() {
        let b = b"<div>hello</div>"; // content "hello" at [5,10); whole at [0,16)
        assert_eq!(char_span(b, 6, tag(false), 1), (5, 10), "it = content");
        assert_eq!(char_span(b, 6, tag(true), 1), (0, 16), "at = incl. tags");
        // Cursor ON a tag still resolves the block.
        assert_eq!(char_span(b, 1, tag(false), 1), (5, 10));
    }

    #[test]
    fn nesting_picks_the_innermost() {
        let b = b"<a><b>x</b></a>";
        assert_eq!(
            char_span(b, 6, tag(false), 1),
            (6, 7),
            "it = innermost content 'x'"
        );
        assert_eq!(
            char_span(b, 6, tag(true), 1),
            (3, 11),
            "at = innermost '<b>x</b>'"
        );
    }

    #[test]
    fn no_enclosing_tag_is_a_noop() {
        assert_eq!(char_span(b"plain text", 3, tag(false), 1), (3, 3));
        // A self-closing tag has no content to enclose the cursor.
        assert_eq!(char_span(b"<br/>x", 5, tag(true), 1), (5, 5));
    }

    /// nvim v0.12.6 (#760): open and close names match without regard to case.
    #[test]
    fn names_match_without_regard_to_case() {
        let b = b"<A>x</a>";
        assert_eq!(
            char_span(b, 3, tag(false), 1),
            (3, 4),
            "dit on <A>x</a> deletes x"
        );
        assert_eq!(
            char_span(b, 0, tag(false), 1),
            (3, 4),
            "from the open tag too"
        );
        assert_eq!(char_span(b"y<Div>x</DIV>z", 6, tag(true), 1), (1, 13));
        let nested = b"<a><B>x</b></A>"; // <a>[0,15), <B>[3,11)
        assert_eq!(
            char_span(nested, 6, tag(true), 1),
            (3, 11),
            "dat = <B>x</b>"
        );
        assert_eq!(
            char_span(nested, 6, tag(false), 2),
            (3, 11),
            "2dit = <a>'s inner"
        );
        // `<a><A>x</a></A>`: `</a>` closes the innermost open `<A>`, so `dit` deletes only `x`.
        assert_eq!(char_span(b"<a><A>x</a></A>", 6, tag(false), 1), (6, 7));
    }

    #[test]
    fn count_climbs_outward_one_level_per_count() {
        let b = b"<a><b>x</b></a>"; // <a>[0,15), <b>[3,11), content 'x' at [6,7)
        assert_eq!(char_span(b, 6, tag(false), 1), (6, 7), "1it = innermost");
        assert_eq!(char_span(b, 6, tag(false), 2), (3, 11), "2it = <a>'s inner");
        assert_eq!(char_span(b, 6, tag(true), 2), (0, 15), "2at = whole <a>");
        // Count beyond the nesting depth is a no-op (Vim).
        assert_eq!(char_span(b, 6, tag(false), 3), (6, 6));
    }
}

#[cfg(test)]
mod tag_visual_tests {
    //! nvim v0.12.6 `current_tagblock` in Visual mode (#1561): `(anchor, cursor)` in, `(anchor, cursor)` out.
    use super::tag_visual;

    #[test]
    fn one_char_area_that_is_the_inner_takes_the_block() {
        assert_eq!(tag_visual(b"<a>x</a>", 3, 3, false, 1), Some((0, 7)));
        assert_eq!(tag_visual(b"<a>xy</a>", 3, 3, false, 1), Some((3, 4)));
        assert_eq!(tag_visual(b"<a>xy</a>", 3, 4, false, 1), Some((0, 8)));
    }

    #[test]
    fn repeats_climb_one_level() {
        let b = b"<a><b>x</b></a>"; // <a>[0,15), <b>[3,11)
        assert_eq!(
            tag_visual(b, 6, 6, false, 1),
            Some((3, 10)),
            "vit = <b>x</b>"
        );
        assert_eq!(
            tag_visual(b, 3, 10, false, 1),
            Some((0, 14)),
            "vitit = whole"
        );
        assert_eq!(tag_visual(b, 6, 6, true, 1), Some((3, 10)), "vat");
        assert_eq!(tag_visual(b, 3, 10, true, 1), Some((0, 14)), "vatat");
        assert_eq!(tag_visual(b, 0, 14, true, 1), None, "past the top fails");
        assert_eq!(
            tag_visual(b, 6, 6, false, 3),
            None,
            "count past the depth fails"
        );
        let w = b"<a><b>xy</b></a>";
        assert_eq!(
            tag_visual(w, 6, 7, false, 1),
            Some((3, 11)),
            "area = inner of <b>"
        );
        assert_eq!(
            tag_visual(w, 7, 6, false, 1),
            Some((3, 11)),
            "o-swapped area"
        );
        assert_eq!(tag_visual(w, 6, 6, false, 2), Some((3, 11)), "v2it");
    }

    #[test]
    fn an_area_past_the_block_end_climbs_out() {
        let b = b"<a><b>xy</b> z</a>"; // z at 13
        assert_eq!(tag_visual(b, 6, 13, false, 1), Some((3, 13)));
        assert_eq!(tag_visual(b, 13, 6, false, 1), Some((3, 13)));
        assert_eq!(tag_visual(b"<a>x</a>", 3, 5, false, 1), None);
    }

    #[test]
    fn cursor_on_a_tag_and_empty_blocks() {
        let b = b"<a>x</a>";
        assert_eq!(tag_visual(b, 0, 0, false, 1), Some((3, 3)), "on <");
        assert_eq!(tag_visual(b, 2, 2, false, 1), Some((3, 3)), "on >");
        assert_eq!(tag_visual(b, 5, 5, false, 1), Some((3, 3)), "on /");
        let e = b"<a></a>";
        assert_eq!(
            tag_visual(e, 0, 0, false, 1),
            Some((3, 3)),
            "empty: the < of </a>"
        );
        assert_eq!(
            tag_visual(e, 3, 3, false, 1),
            Some((0, 6)),
            "on </: the block"
        );
        assert_eq!(
            tag_visual(b"  <a>x</a>", 0, 0, false, 1),
            Some((5, 5)),
            "indent skipped"
        );
        assert_eq!(tag_visual(b"plain", 1, 1, false, 1), None);
    }

    #[test]
    fn multiline_inner_spans_the_line_breaks() {
        let b = b"<div>\nhello\n</div>"; // '\n' at 5 and 11
        assert_eq!(tag_visual(b, 6, 6, false, 1), Some((5, 11)));
        assert_eq!(tag_visual(b, 5, 11, false, 1), Some((0, 17)));
    }

    #[test]
    fn names_and_markup_variants() {
        assert_eq!(tag_visual(b"<A>x</a>", 3, 3, false, 1), Some((0, 7)));
        // An unclosed outer start tag is not a block, so the climb fails.
        assert_eq!(tag_visual(b"<p><a>xy</a>", 6, 7, false, 1), Some((3, 11)));
        assert_eq!(tag_visual(b"<p><a>xy</a>", 3, 11, false, 1), None);
        // A self-closing tag inside is not a block.
        assert_eq!(tag_visual(b"<a>x<br/>y</a>", 9, 9, true, 1), Some((0, 13)));
    }
}
