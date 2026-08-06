//! Typed positions and the document revision stamp.
//!
//! Enforces **INV-POS-TYPED**: byte / char / grapheme / screen-cell coordinates are distinct types
//! and are never interchanged as bare `usize`. The **canonical** unit is the byte (RFC-0008); the
//! other units are *resolutions* of a byte position via the coordinate layer. This slice implements
//! the byte axis end-to-end and byte↔char conversion; grapheme/cell resolution (wcwidth-style) is the
//! render coordinate-layer follow-up (render-and-frontends.md) — the types exist now so they can never
//! be confused, per F-002 ("distinct types, not interchanged").

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

/// A position measured in grapheme clusters (user-perceived characters). Distinct type; full Unicode
/// segmentation is the coordinate-layer follow-up — present now so grapheme counts are never confused
/// with bytes or chars (INV-POS-TYPED, F-002).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct GraphemePos(pub usize);

/// A terminal screen column (display cell). Distinct type; wcwidth-style width resolution belongs to
/// the render coordinate layer (render-and-frontends.md). Present so cells are never treated as bytes.
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
