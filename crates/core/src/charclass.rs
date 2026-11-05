//! Vim's character classes (`mb_get_class`), the rule behind `\<` / `\>` and the keyword `*` / `#` /
//! `gd` pick up (#745).
//!
//! A class is 0 (blank), 1 (punctuation), 2 (word), 3 (emoji) or a per-script value (0x3040
//! Hiragana, 0x30A0 Katakana, 0x4E00 CJK, 0xAC00 Hangul, 0x2070 superscripts, ...). A word starts where
//! the class changes to one of 2 or more, so "API를" is two words for Vim, not one as for Unicode
//! `\b`. Below U+0100 the class follows 'iskeyword'; ruse has no 'iskeyword' option and uses Vim's
//! default "@,48-57,_,192-255", which makes Latin-1 `×` and `÷` word characters. The table is captured
//! from the pinned Neovim by `tools/parity/char_classes.py` into `char_class_table.rs`.

use crate::char_class_table::NVIM_CLASS_RUNS;

/// The blank class: NUL, space, tab, U+00A0, the wide spaces. A line break counts as blank on the
/// side after a line (Vim sees the line's NUL there).
pub(crate) const BLANK: u32 = 0;
/// The punctuation class.
pub(crate) const PUNCT: u32 = 1;
/// The word class: every character not in the table.
pub(crate) const WORD: u32 = 2;

/// Vim's class of `c` (`mb_get_class` with the default 'iskeyword').
#[must_use]
pub(crate) fn vim_class(c: char) -> u32 {
    let n = u32::from(c);
    if n < 0x80 {
        // ASCII fast path; `ascii_fast_path_follows_the_table` keeps it equal to the table.
        return match c {
            ' ' | '\t' | '\0' => BLANK,
            '0'..='9' | 'A'..='Z' | 'a'..='z' | '_' => WORD,
            _ => PUNCT,
        };
    }
    match NVIM_CLASS_RUNS.binary_search_by(|&(first, last, _)| {
        if last < n {
            std::cmp::Ordering::Less
        } else if first > n {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => NVIM_CLASS_RUNS[i].2,
        Err(_) => WORD,
    }
}

/// The class of the character at byte `pos` of `hay` as a line sees it: a line break or the end of
/// the text is the line's NUL, so blank.
fn class_at(hay: &str, pos: usize) -> u32 {
    match hay[pos..].chars().next() {
        None | Some('\n') => BLANK,
        Some(c) => hay_class(c),
    }
}

/// The class of the character before byte `pos`, `None` at the start of a line (Vim's
/// `reg_prev_class` returns -1 there).
fn class_before(hay: &str, pos: usize) -> Option<u32> {
    match hay[..pos].chars().next_back() {
        None | Some('\n') => None,
        Some(c) => Some(hay_class(c)),
    }
}

/// Vim's class of a char of a regex haystack: an escape char ([`crate::pattern::Hay`]) spells a byte that
/// is not UTF-8, whose class is the Latin-1 char's of its value (nvim's `utf_ptr2char` gives the byte).
fn hay_class(c: char) -> u32 {
    vim_class(crate::pattern::escaped_byte(c).map_or(c, char::from))
}

/// Vim's class of the cell at byte `p` of `b`: its char's, or for a byte that belongs to no valid UTF-8
/// char, the Latin-1 char's of its value, as nvim's `utf_ptr2char` reads it (#1243): 0xA0 is blank, 0xB5
/// and 0xC0-0xFF are keyword chars, the other bytes from 0x80 punctuation. `None` past the end.
pub(crate) fn cell_class(b: &[u8], p: usize) -> Option<u32> {
    cell_char(b, p).map(vim_class)
}

/// The last two chars of `s` as [`cell_char`] reads them, the last first (nvim's `endcurr1`/`endcurr2`).
pub(crate) fn last_two_chars(s: &[u8]) -> (Option<char>, Option<char>) {
    let last = (!s.is_empty()).then(|| crate::motion::prev_boundary(s, s.len()));
    let before = last
        .filter(|&p| p > 0)
        .map(|p| crate::motion::prev_boundary(s, p));
    (
        last.and_then(|p| cell_char(s, p)),
        before.and_then(|p| cell_char(s, p)),
    )
}

/// The char of the cell at byte `p` of `b` as Vim's classes read it: the UTF-8 char there, or the Latin-1
/// char of a byte that belongs to no valid char. `None` past the end. `p` is a char boundary.
pub(crate) fn cell_char(b: &[u8], p: usize) -> Option<char> {
    let lead = *b.get(p)?;
    if lead < 0x80 {
        return Some(char::from(lead));
    }
    Some(
        crate::motion::char_len_at(b, p)
            .and_then(|w| std::str::from_utf8(&b[p..p + w]).ok())
            .and_then(|s| s.chars().next())
            .unwrap_or(char::from(lead)),
    )
}

/// Vim's `\<` at byte `pos` (`nfa_regmatch` NFA_BOW): the character there is a word character and
/// the one before it, if any on the line, has another class.
#[must_use]
pub(crate) fn at_word_start(hay: &str, pos: usize) -> bool {
    let this = class_at(hay, pos);
    this >= WORD && class_before(hay, pos) != Some(this)
}

/// Vim's `\>` at byte `pos` (NFA_EOW): the character before it is a word character and the one at
/// `pos` (a line end counts as blank) has another class.
#[must_use]
pub(crate) fn at_word_end(hay: &str, pos: usize) -> bool {
    match class_before(hay, pos) {
        Some(prev) if prev >= WORD => class_at(hay, pos) != prev,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_class(n: u32) -> u32 {
        NVIM_CLASS_RUNS
            .iter()
            .find(|&&(a, b, _)| a <= n && n <= b)
            .map_or(WORD, |&(_, _, k)| k)
    }

    #[test]
    fn ascii_fast_path_follows_the_table() {
        for n in 0..0x80u32 {
            let c = char::from_u32(n).expect("ASCII");
            assert_eq!(vim_class(c), table_class(n), "U+{n:04X}");
        }
    }

    #[test]
    fn runs_are_sorted_and_disjoint() {
        for w in NVIM_CLASS_RUNS.windows(2) {
            assert!(w[0].0 <= w[0].1 && w[0].1 < w[1].0, "{:X?}", w);
        }
    }

    #[test]
    fn table_comes_from_the_pinned_nvim() {
        let pins = include_str!("../../../spec/parity/upstreams.yaml");
        let nvim = crate::char_class_table::NVIM_VERSION.trim_start_matches("NVIM ");
        assert!(pins.contains(&format!("version_label: {nvim}")), "{nvim}");
    }

    #[test]
    fn scripts_have_their_own_classes() {
        assert_eq!(vim_class('a'), WORD);
        assert_eq!(vim_class('×'), WORD); // Latin-1 192-255 is in the default 'iskeyword'
        assert_eq!(vim_class('÷'), WORD);
        assert_eq!(vim_class('µ'), WORD);
        assert_eq!(vim_class('§'), PUNCT);
        assert_eq!(vim_class('\u{A0}'), BLANK);
        assert_eq!(vim_class('한'), 0xAC00);
        assert_eq!(vim_class('漢'), 0x4E00);
        assert_eq!(vim_class('か'), 0x3040);
        assert_eq!(vim_class('カ'), 0x30A0);
        assert_eq!(vim_class('、'), PUNCT);
        assert_eq!(vim_class('\u{3000}'), BLANK);
        assert_eq!(vim_class('😀'), 3);
    }

    #[test]
    fn word_edges_follow_the_class_change() {
        let s = "x API를 API";
        assert!(at_word_start(s, 2));
        assert!(at_word_end(s, 5)); // "API" | "를"
        assert!(at_word_start(s, 5));
        assert!(!at_word_start(s, 3));
        assert!(at_word_end(s, s.len()));
        assert!(!at_word_end(s, 0));
        assert!(!at_word_start(s, 1)); // a blank is not a word start
        assert!(at_word_start("abc한글", 3));
        assert!(!at_word_start("a×b", 3));
        assert!(at_word_start("a\nb", 2)); // a line start has no previous character
        assert!(at_word_end("a\nb", 1));
    }
}
