//! Typed positions and the document revision stamp.
//!
//! Enforces **INV-POS-TYPED**: byte / char / grapheme / screen-cell coordinates are distinct types
//! and are never interchanged as bare `usize`. The **canonical** unit is the byte (RFC-0008); the
//! other units are *resolutions* of a byte position via the coordinate layer. All four axes resolve
//! here: byte↔char, byte↔**grapheme** (UAX#29 clusters, `unicode-segmentation`), and byte→**cell**
//! (wcwidth via `unicode-width`, tab-aware). The four are distinct types and never interchanged as bare
//! `usize`, per F-002 ("distinct types, not interchanged").

/// A strictly-monotonic per-Document version stamp (**INV-TXN**, RFC-0007 §2).
///
/// Opaque and comparable; it is **not** a wall clock and **not** a tree position. *Every* apply —
/// including an undo's inverse — increases it (persistence-and-recovery §1), so "is the buffer
/// modified?" is answered by undo-node identity, never by comparing revision magnitudes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Revision(pub u64);

impl Revision {
    /// The revision of a freshly-created, unedited document.
    pub const ZERO: Revision = Revision(0);

    /// The successor revision. Applying a transaction moves `r -> r.next()`.
    #[must_use]
    pub fn next(self) -> Revision {
        Revision(self.0 + 1)
    }
}

/// A gap position measured in **bytes** — the canonical unit (RFC-0008; anchor-store "Offset").
///
/// Half-open over an `N`-byte document: `0` is the gap before the first byte, `N` the gap after the
/// last. A position sits *between* two bytes, which is what lets an anchor cling to one side (bias).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct BytePos(pub usize);

/// A position measured in Unicode scalar values (`char`s). Distinct from [`BytePos`] (INV-POS-TYPED).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct CharPos(pub usize);

/// A position measured in grapheme clusters (user-perceived characters), resolved via UAX#29
/// ([`byte_to_grapheme`]). Distinct from bytes/chars (INV-POS-TYPED, F-002): `👨‍👩‍👧` is many bytes and
/// several `char`s but one grapheme.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct GraphemePos(pub usize);

/// A terminal screen column (display cell), resolved wcwidth-style via [`byte_to_cell`]. Distinct
/// again: a wide CJK glyph is one grapheme but two cells; a combining mark is zero.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct CellCol(pub usize);

/// Convert a byte position to a char position over UTF-8 `text` — counts `char`s in `text[..b]`.
///
/// `b` must be a char boundary and within `text` (a caller-side INV-POS-TYPED discipline); this
/// asserts both in debug builds rather than returning a silently wrong count.
#[must_use]
pub fn byte_to_char(text: &str, b: BytePos) -> CharPos {
    debug_assert!(
        b.0 <= text.len() && text.is_char_boundary(b.0),
        "byte pos not on a char boundary"
    );
    CharPos(text[..b.0].chars().count())
}

/// Convert a char position to a byte position over UTF-8 `text`. Clamps to `text.len()` if `c` is past
/// the end (an empty tail), matching how an end-of-document coordinate resolves.
#[must_use]
pub fn char_to_byte(text: &str, c: CharPos) -> BytePos {
    match text.char_indices().nth(c.0) {
        Some((byte, _)) => BytePos(byte),
        None => BytePos(text.len()),
    }
}

/// Resolve a byte position to a **grapheme-cluster** position — the count of UAX#29 extended grapheme
/// clusters in `text[..b]` (F-002). This is what "column 3" means to a user: three user-perceived
/// characters, however many bytes or `char`s each took. `b` must be on a char boundary.
#[must_use]
pub fn byte_to_grapheme(text: &str, b: BytePos) -> GraphemePos {
    use unicode_segmentation::UnicodeSegmentation;
    debug_assert!(
        b.0 <= text.len() && text.is_char_boundary(b.0),
        "byte pos not on a char boundary"
    );
    GraphemePos(text[..b.0].graphemes(true).count())
}

/// Resolve a grapheme-cluster position back to a byte position; clamps to `text.len()` past the end.
#[must_use]
pub fn grapheme_to_byte(text: &str, g: GraphemePos) -> BytePos {
    use unicode_segmentation::UnicodeSegmentation;
    match text.grapheme_indices(true).nth(g.0) {
        Some((byte, _)) => BytePos(byte),
        None => BytePos(text.len()),
    }
}

/// The **display cell column** just before byte position `b` (F-002): the summed terminal width of
/// `text[..b]`, where a wide/CJK glyph is 2 cells, a zero-width combining mark 0, and a tab advances
/// to the next `tab_width` stop. This is the coordinate the renderer places the cursor at — distinct
/// from the grapheme count (`宽` is one grapheme but two cells).
#[must_use]
pub fn byte_to_cell(text: &str, b: BytePos, tab_width: usize) -> CellCol {
    use unicode_width::UnicodeWidthChar;
    debug_assert!(
        b.0 <= text.len() && text.is_char_boundary(b.0),
        "byte pos not on a char boundary"
    );
    let tab = tab_width.max(1);
    let mut col = 0usize;
    for ch in text[..b.0].chars() {
        col += match ch {
            '\t' => tab - (col % tab),
            c => c.width().unwrap_or(0),
        };
    }
    CellCol(col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_count_treats_a_zwj_emoji_as_one() {
        // A family emoji is 4 scalar values joined by ZWJ — many chars, ONE user-perceived character.
        let fam = "👨\u{200D}👩\u{200D}👧";
        assert!(fam.chars().count() > 1);
        assert_eq!(byte_to_grapheme(fam, BytePos(fam.len())), GraphemePos(1));
        // And a base letter + combining acute is one grapheme too.
        let combining = "e\u{0301}"; // é as e + U+0301
        assert_eq!(
            byte_to_grapheme(combining, BytePos(combining.len())),
            GraphemePos(1)
        );
    }

    #[test]
    fn grapheme_and_byte_round_trip_across_clusters() {
        let s = "a👨\u{200D}👩b"; // 'a', a ZWJ pair, 'b' = 3 graphemes
        assert_eq!(byte_to_grapheme(s, BytePos(s.len())), GraphemePos(3));
        // The 2nd grapheme (index 1) starts right after 'a'.
        assert_eq!(grapheme_to_byte(s, GraphemePos(1)), BytePos(1));
        // Round-trip: grapheme 2 -> byte -> grapheme.
        let b = grapheme_to_byte(s, GraphemePos(2));
        assert_eq!(byte_to_grapheme(s, b), GraphemePos(2));
    }

    #[test]
    fn cell_width_is_wide_for_cjk_zero_for_combining_and_expands_tabs() {
        // Wide CJK glyph = 2 cells (one grapheme).
        assert_eq!(byte_to_cell("宽", BytePos("宽".len()), 4), CellCol(2));
        // Base + combining mark = 1 cell (the mark is zero-width).
        let combining = "e\u{0301}";
        assert_eq!(
            byte_to_cell(combining, BytePos(combining.len()), 4),
            CellCol(1)
        );
        // Tab advances to the next stop: after "ab" (col 2) a tab jumps to col 4 (width 2).
        assert_eq!(byte_to_cell("ab\t", BytePos(3), 4), CellCol(4));
        // A tab at col 0 fills a whole stop.
        assert_eq!(byte_to_cell("\t", BytePos(1), 4), CellCol(4));
    }

    #[test]
    fn ascii_coords_are_unchanged() {
        let s = "hello";
        assert_eq!(byte_to_grapheme(s, BytePos(3)), GraphemePos(3));
        assert_eq!(byte_to_cell(s, BytePos(3), 4), CellCol(3));
    }
}
