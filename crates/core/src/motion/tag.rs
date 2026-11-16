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
            // Close tag `</name>`: pop to the matching open, forming a pair.
            let name = tag_name(&interior[1..]);
            while let Some((oname, os, oe)) = stack.pop() {
                if oname == name {
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
