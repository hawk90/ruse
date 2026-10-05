//! The `:digraphs` / `:dig` listing overlay: dump the curated insert-mode digraph table (the one
//! `i_CTRL-K {c1}{c2}` uses) so the available codes are discoverable. A [`Picker`]`<char>` whose payload
//! is the glyph; it is VIEW-ONLY (Vim's `:digraphs` does not act on a selection), so the session just
//! closes it on Enter — mirroring the `:registers` viewer.
//!
//! Each row mirrors Vim's `:digraphs` columns roughly: the two-char code, the glyph, and its decimal
//! codepoint value (e.g. `a:  ä  228`), searchable by code + glyph.

use crate::input::digraph;
use crate::ui::picker::{PickItem, Picker};

/// Render one digraph entry as a `:digraphs` row: `{c1}{c2}  {glyph}  {decimal}` (decimal codepoint value,
/// like Vim's third column). Kept separate for a direct unit test on the formatting.
fn row(c1: char, c2: char, glyph: char) -> String {
    format!("{c1}{c2}  {glyph}  {}", glyph as u32)
}

/// Open the digraph listing over the curated table (declaration order). View-only.
pub(crate) fn open() -> Picker<char> {
    let items = digraph::entries()
        .iter()
        .map(|&(c1, c2, glyph)| PickItem {
            display: row(c1, c2, glyph),
            // Searchable by the two-char code and the glyph itself.
            search: format!("{c1}{c2} {glyph}"),
            payload: glyph,
        })
        .collect();
    Picker::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_shows_code_glyph_and_decimal_value() {
        // `ä` is U+00E4 = 228 decimal (Vim's `:digraphs` third column).
        assert_eq!(row('a', ':', 'ä'), "a:  ä  228");
        assert_eq!(row('-', '>', '→'), "->  →  8594");
    }

    #[test]
    fn open_lists_every_curated_entry_with_known_rows() {
        let p = open();
        assert_eq!(
            p.rows().len(),
            digraph::entries().len(),
            "one row per entry"
        );
        let rows = p.rows();
        let texts: Vec<&str> = rows.iter().map(|(s, _)| s.as_str()).collect();
        // Spot-check known curated entries appear in the rendered listing.
        assert!(texts.contains(&"a:  ä  228"), "diaeresis a; got {texts:?}");
        assert!(texts.contains(&"Co  ©  169"), "copyright; got {texts:?}");
        assert!(texts.contains(&"Eu  €  8364"), "euro; got {texts:?}");
    }
}
