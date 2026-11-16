//! `f` / `t` / `F` / `T` char-search targets.

use super::boundary::{at_bol, at_nul, char_len_at, Steps};

/// The bare-move landing for a char-search (`f`/`F`/`t`/`T`), or `None` if the `count`-th match does not
/// exist on the current line. `f`/`F` land on the char; `t`/`T` land one boundary short of it. Search is
/// confined to the current line (Vim never crosses a newline for these). A `;`/`,` repeat (`repeat`) of a
/// `t`/`T` with count 1 does not match the char right next to the cursor, which is where the last `t`
/// stopped short of (Vim `searchc` without `cpo-;`).
pub(super) fn find_char_target(
    b: &[u8],
    cursor: usize,
    ch: char,
    forward: bool,
    till: bool,
    repeat: bool,
    count: u32,
) -> Option<usize> {
    let n = count.max(1) as usize;
    // A target matches the FIRST char of a cluster only: `f` to a combining mark, an emoji modifier or the
    // second flag letter finds nothing inside a cluster, as nvim v0.12.6 (`f\u{301}` on `xáb` fails; #909).
    // The cells are walked from the cursor as the motions step them, so a byte that is not UTF-8 is a cell
    // of its own that matches no char, and the rest of the line is still searched (#1232).
    let steps = Steps::new(b);
    let first_char = |p: usize| {
        char_len_at(b, p)
            .and_then(|w| std::str::from_utf8(&b[p..p + w]).ok())
            .and_then(|s| s.chars().next())
    };
    // `t`/`T` stop one CHARACTER short of the match: a base with its marks, a flag, a ZWJ sequence (#909).
    let skip = (repeat && till && n == 1).then(|| {
        if forward {
            steps.next(cursor)
        } else {
            steps.prev(cursor)
        }
    });
    let is_hit = |p: usize| first_char(p) == Some(ch) && Some(p) != skip;
    let mut left = n;
    let mut p = cursor.min(b.len());
    if forward {
        // Cell starts strictly after the cursor, in order, up to the line's end; take the n-th match.
        while !at_nul(b, p) {
            p = steps.next(p);
            if !at_nul(b, p) && is_hit(p) {
                left -= 1;
                if left == 0 {
                    return Some(if till { steps.prev(p) } else { p });
                }
            }
        }
        None
    } else {
        // Cell starts strictly before the cursor, back to the line's start; take the n-th match.
        while !at_bol(b, p) {
            p = steps.prev(p);
            if is_hit(p) {
                left -= 1;
                if left == 0 {
                    return Some(if till { steps.next(p) } else { p });
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod find_char_tests {
    //! #1232: `f`/`t`/`F`/`T` walk the cells from the cursor instead of segmenting the whole line. On
    //! valid UTF-8 they must answer as the line-segmenting version they replaced, copied here verbatim.
    use super::*;
    use crate::motion::{line_end, line_start, next_grapheme, prev_grapheme};
    use proptest::prelude::*;
    use unicode_segmentation::UnicodeSegmentation;

    /// The pre-#1232 `find_char_target`, verbatim.
    fn old_find_char_target(
        b: &[u8],
        cursor: usize,
        ch: char,
        forward: bool,
        till: bool,
        repeat: bool,
        count: u32,
    ) -> Option<usize> {
        let n = count.max(1) as usize;
        let ls = line_start(b, cursor);
        let le = line_end(b, cursor); // newline / end position for this line
        let line = std::str::from_utf8(b.get(ls..le)?).ok()?;
        // A target matches the FIRST char of a cluster only: `f` to a combining mark, an emoji modifier or the
        // second flag letter finds nothing inside a cluster, as nvim v0.12.6 (`f\u{301}` on `xáb` fails; #909).
        let starts = line
            .grapheme_indices(true)
            .filter_map(|(i, g)| g.chars().next().map(|c| (ls + i, c)));
        // `t`/`T` stop one CHARACTER short of the match: a base with its marks, a flag, a ZWJ sequence (#909).
        let skip = (repeat && till && n == 1).then(|| {
            if forward {
                next_grapheme(b, cursor)
            } else {
                prev_grapheme(b, cursor)
            }
        });
        if forward {
            // Matches strictly after the cursor, in order; take the n-th.
            let hit = starts
                .filter(|&(pos, c)| pos > cursor && c == ch && Some(pos) != skip)
                .nth(n - 1)?
                .0;
            Some(if till { prev_grapheme(b, hit) } else { hit })
        } else {
            // Matches strictly before the cursor; take the n-th counting backward.
            let hit = starts
                .filter(|&(pos, c)| pos < cursor && c == ch && Some(pos) != skip)
                .rev()
                .nth(n - 1)?
                .0;
            Some(if till { next_grapheme(b, hit) } else { hit })
        }
    }

    proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// Every char search, from every char boundary, with any count, equals the old one on valid text.
        #[test]
        fn find_char_equals_the_line_segmenting_search(
            text in "[ab \n\u{301}가👍🏽🇰🇷x]{0,20}",
            count in 1u32..4,
        ) {
            let b = text.as_bytes();
            for cur in (0..=b.len()).filter(|&p| text.is_char_boundary(p)) {
                for ch in ['a', 'b', ' ', '가', 'x', '\u{301}', '👍', '🇰'] {
                    for (forward, till, repeat) in [
                        (true, false, false),
                        (true, true, false),
                        (true, true, true),
                        (false, false, false),
                        (false, true, false),
                        (false, true, true),
                    ] {
                        prop_assert_eq!(
                            find_char_target(b, cur, ch, forward, till, repeat, count),
                            old_find_char_target(b, cur, ch, forward, till, repeat, count),
                            "{:?} at {} ch={:?} fwd={} till={} rep={} n={}",
                            text, cur, ch, forward, till, repeat, count
                        );
                    }
                }
            }
        }
    }
}
