//! The column rule round-trips (P1/P2). Split from `render.rs` (#1386).

use super::*;
use proptest::prelude::*;

proptest! {
    #![proptest_config(crate::test_cases(256))]

    /// P1 (round-trip) + P2 (monotonic): the shared column rule [`line_display_col`] and its inverse
    /// [`line_byte_at_col`] agree, and the column never decreases in buffer order — over arbitrary
    /// lines mixing ASCII, tabs, wide CJK (가), and a combining mark (´).
    #[test]
    fn layout_col_roundtrips_and_is_monotonic(line in "[a-z\t \u{ac00}\u{0301}]{0,40}") {
        use unicode_segmentation::UnicodeSegmentation;
        let boundaries: Vec<usize> = line
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(line.len()))
            .collect();
        let mut prev = 0u16;
        for &b in &boundaries {
            let c = line_display_col(&line, b, TAB_WIDTH);
            prop_assert!(c >= prev, "P2: column decreased at byte {}", b); // monotonic
            prev = c;
            let back = line_byte_at_col(&line, c, TAB_WIDTH);
            // P1: col -> byte -> col lands on the same column (robust to zero-width clusters).
            prop_assert_eq!(line_display_col(&line, back, TAB_WIDTH), c, "P1: round-trip diverged at byte {}", b);
        }
    }
}
