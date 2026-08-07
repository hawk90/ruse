//! Literal (substring) search over the document — the v0 search primitive. Vim-dialect regex is C-REGEX,
//! a later and bigger piece; the `SearchNext`/`SearchPrev` commands already have the right shape to carry a
//! richer pattern when it lands. Matches over valid UTF-8 land on char boundaries (a UTF-8 needle only
//! matches char-aligned).

fn matches_at(hay: &[u8], needle: &[u8], i: usize) -> bool {
    i + needle.len() <= hay.len() && &hay[i..i + needle.len()] == needle
}

/// The byte offset of the next occurrence of `needle` at a position `>= from`, wrapping to the start of the
/// document. `None` if `needle` is empty or does not occur.
#[must_use]
pub fn find_next(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let start = from.min(hay.len());
    (start..=last)
        .find(|&i| matches_at(hay, needle, i))
        .or_else(|| (0..=last).find(|&i| matches_at(hay, needle, i)))
}

/// The byte offset of the previous occurrence starting strictly before `before`, wrapping to the end.
#[must_use]
pub fn find_prev(hay: &[u8], needle: &[u8], before: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let b = before.min(hay.len());
    (0..b)
        .rev()
        .find(|&i| matches_at(hay, needle, i))
        .or_else(|| (0..=last).rev().find(|&i| matches_at(hay, needle, i)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_and_wrap() {
        let h = b"foo bar foo baz";
        assert_eq!(find_next(h, b"foo", 1), Some(8)); // next foo after pos 1
        assert_eq!(find_next(h, b"foo", 9), Some(0)); // wraps to the first
        assert_eq!(find_next(h, b"zzz", 0), None);
        assert_eq!(find_next(h, b"", 0), None);
    }

    #[test]
    fn prev_and_wrap() {
        let h = b"foo bar foo baz";
        assert_eq!(find_prev(h, b"foo", 8), Some(0)); // last foo starting before pos 8
        assert_eq!(find_prev(h, b"foo", 0), Some(8)); // nothing before 0 → wraps to the last
    }
}
