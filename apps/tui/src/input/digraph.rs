//! Insert-mode digraph table for `i_CTRL-K {c1}{c2}` (`:help digraphs`).
//!
//! In Insert mode `CTRL-K` followed by two characters inserts the corresponding digraph glyph — e.g.
//! `CTRL-K a :` → `ä`, `CTRL-K e '` → `é`, `CTRL-K C o` → `©`. This is a CURATED SUBSET of Vim's
//! ~1400-entry table (the common accented Latin letters and a handful of symbols), NOT the full set,
//! and there is no `:digraphs` listing command or user-defined `:digraph` support yet — both are
//! deliberate follow-ups (see the change's non-goals).
//!
//! Every `(c1, c2, glyph)` triple below was mechanically extracted from `:digraphs` on Vim 9.1 and is
//! byte-for-byte faithful to it — the codes are NOT invented. Where Vim accepts several codes for one
//! glyph, this table ships the intuitive punctuation form: `:` diaeresis, `'` acute, `` ` `` grave,
//! `^` circumflex, `~` tilde (all are genuine Vim aliases, verified against the live table).
//!
//! Divergence note: Vim's degree sign is `DG` (uppercase); the lowercase `dg` is NOT a Vim digraph, so
//! this table ships `DG` to stay faithful. Lookup is case-SENSITIVE, exactly like Vim.

/// The curated `(c1, c2, glyph)` table. A linear scan over ~80 entries is trivially fast and keeps this a
/// plain `const` slice (no allocation, no lazy map). Grouped by diacritic/category for readability.
const TABLE: &[(char, char, char)] = &[
    // Diaeresis (`:`)
    ('a', ':', '\u{00E4}'), // ä
    ('A', ':', '\u{00C4}'), // Ä
    ('e', ':', '\u{00EB}'), // ë
    ('E', ':', '\u{00CB}'), // Ë
    ('i', ':', '\u{00EF}'), // ï
    ('I', ':', '\u{00CF}'), // Ï
    ('o', ':', '\u{00F6}'), // ö
    ('O', ':', '\u{00D6}'), // Ö
    ('u', ':', '\u{00FC}'), // ü
    ('U', ':', '\u{00DC}'), // Ü
    ('y', ':', '\u{00FF}'), // ÿ
    // Acute (`'`)
    ('a', '\'', '\u{00E1}'), // á
    ('A', '\'', '\u{00C1}'), // Á
    ('e', '\'', '\u{00E9}'), // é
    ('E', '\'', '\u{00C9}'), // É
    ('i', '\'', '\u{00ED}'), // í
    ('I', '\'', '\u{00CD}'), // Í
    ('o', '\'', '\u{00F3}'), // ó
    ('O', '\'', '\u{00D3}'), // Ó
    ('u', '\'', '\u{00FA}'), // ú
    ('U', '\'', '\u{00DA}'), // Ú
    ('y', '\'', '\u{00FD}'), // ý
    // Grave (`` ` ``)
    ('a', '`', '\u{00E0}'), // à
    ('A', '`', '\u{00C0}'), // À
    ('e', '`', '\u{00E8}'), // è
    ('E', '`', '\u{00C8}'), // È
    ('i', '`', '\u{00EC}'), // ì
    ('I', '`', '\u{00CC}'), // Ì
    ('o', '`', '\u{00F2}'), // ò
    ('O', '`', '\u{00D2}'), // Ò
    ('u', '`', '\u{00F9}'), // ù
    ('U', '`', '\u{00D9}'), // Ù
    // Circumflex (`^`)
    ('a', '^', '\u{00E2}'), // â
    ('A', '^', '\u{00C2}'), // Â
    ('e', '^', '\u{00EA}'), // ê
    ('E', '^', '\u{00CA}'), // Ê
    ('i', '^', '\u{00EE}'), // î
    ('I', '^', '\u{00CE}'), // Î
    ('o', '^', '\u{00F4}'), // ô
    ('O', '^', '\u{00D4}'), // Ô
    ('u', '^', '\u{00FB}'), // û
    ('U', '^', '\u{00DB}'), // Û
    // Tilde (`~`)
    ('a', '~', '\u{00E3}'), // ã
    ('A', '~', '\u{00C3}'), // Ã
    ('n', '~', '\u{00F1}'), // ñ
    ('N', '~', '\u{00D1}'), // Ñ
    ('o', '~', '\u{00F5}'), // õ
    ('O', '~', '\u{00D5}'), // Õ
    // Letters / ligatures
    ('a', 'e', '\u{00E6}'), // æ
    ('A', 'E', '\u{00C6}'), // Æ
    ('o', 'e', '\u{0153}'), // œ
    ('O', 'E', '\u{0152}'), // Œ
    ('s', 's', '\u{00DF}'), // ß
    ('o', '/', '\u{00F8}'), // ø
    ('O', '/', '\u{00D8}'), // Ø
    // Currency / marks
    ('E', 'u', '\u{20AC}'), // €
    ('P', 'd', '\u{00A3}'), // £
    ('C', 't', '\u{00A2}'), // ¢
    ('S', 'E', '\u{00A7}'), // §
    ('C', 'o', '\u{00A9}'), // ©
    ('R', 'g', '\u{00AE}'), // ®
    ('M', 'y', '\u{00B5}'), // µ
    // Math / misc
    ('D', 'G', '\u{00B0}'), // ° (Vim uses uppercase `DG`)
    ('+', '-', '\u{00B1}'), // ±
    ('*', 'X', '\u{00D7}'), // ×
    ('1', '2', '\u{00BD}'), // ½
    ('1', '4', '\u{00BC}'), // ¼
    ('3', '4', '\u{00BE}'), // ¾
    ('.', 'M', '\u{00B7}'), // ·
    ('?', 'I', '\u{00BF}'), // ¿
    ('!', 'I', '\u{00A1}'), // ¡
    // Quotes / arrows / marks
    ('<', '<', '\u{00AB}'), // «
    ('>', '>', '\u{00BB}'), // »
    ('-', '>', '\u{2192}'), // →
    ('<', '-', '\u{2190}'), // ←
    ('-', '!', '\u{2191}'), // ↑
    ('-', 'v', '\u{2193}'), // ↓
    ('O', 'K', '\u{2713}'), // ✓
    ('X', 'X', '\u{2717}'), // ✗
];

/// Look up the digraph glyph for the two-character code `c1 c2`. Returns `None` for any pair not in the
/// curated table; the caller applies Vim's fallback (insert the second char literally). Case-sensitive.
pub(crate) fn digraph(c1: char, c2: char) -> Option<char> {
    TABLE
        .iter()
        .find(|(a, b, _)| *a == c1 && *b == c2)
        .map(|(_, _, glyph)| *glyph)
}

#[cfg(test)]
mod tests {
    use super::digraph;

    #[test]
    fn known_accented_vowels_resolve() {
        // The four diacritic families, spot-checked against Vim's `:digraphs` (case-sensitive).
        assert_eq!(digraph('a', ':'), Some('ä'));
        assert_eq!(digraph('o', ':'), Some('ö'));
        assert_eq!(digraph('u', ':'), Some('ü'));
        assert_eq!(digraph('e', '\''), Some('é'));
        assert_eq!(digraph('a', '`'), Some('à'));
        assert_eq!(digraph('o', '^'), Some('ô'));
        assert_eq!(digraph('n', '~'), Some('ñ'));
        // Uppercase forms are distinct entries.
        assert_eq!(digraph('A', ':'), Some('Ä'));
        assert_eq!(digraph('E', '\''), Some('É'));
    }

    #[test]
    fn known_symbols_resolve() {
        assert_eq!(digraph('C', 'o'), Some('©'));
        assert_eq!(digraph('R', 'g'), Some('®'));
        assert_eq!(digraph('D', 'G'), Some('°'));
        assert_eq!(digraph('+', '-'), Some('±'));
        assert_eq!(digraph('<', '<'), Some('«'));
        assert_eq!(digraph('>', '>'), Some('»'));
        assert_eq!(digraph('O', 'K'), Some('✓'));
        assert_eq!(digraph('X', 'X'), Some('✗'));
        assert_eq!(digraph('-', '>'), Some('→'));
        assert_eq!(digraph('E', 'u'), Some('€'));
        assert_eq!(digraph('s', 's'), Some('ß'));
    }

    #[test]
    fn unknown_pairs_return_none() {
        // Not in the curated table.
        assert_eq!(digraph('z', 'z'), None);
        assert_eq!(digraph('q', 'q'), None);
        // Case-sensitive: `dg` (lowercase) is NOT a Vim digraph; only `DG` is.
        assert_eq!(digraph('d', 'g'), None);
        // Order matters: `:a` is not the same as `a:`.
        assert_eq!(digraph(':', 'a'), None);
    }

    #[test]
    fn table_has_no_duplicate_codes() {
        // Guards against a copy-paste double-entry silently shadowing a code (the generator once emitted
        // `Pd` twice). Every `(c1, c2)` key must be unique.
        let mut seen = std::collections::HashSet::new();
        for (c1, c2, _) in super::TABLE {
            assert!(seen.insert((*c1, *c2)), "duplicate digraph code: {c1}{c2}");
        }
    }
}
