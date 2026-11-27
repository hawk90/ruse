//! Text that is not UTF-8 (#1243).
//!
//! The engine matches `&str`. A buffer byte that belongs to no valid UTF-8 char is one char to nvim's regex
//! (`.` takes it, `[^a]` takes it), with the class of the Latin-1 char of its value for the class atoms and
//! `\<`/`\>` (`\k` takes 0xFF, not 0x80), and it equals no typed char (`/ÿ` does not find it). The engine
//! sees such a byte as an ESCAPE char: byte `b` (0x80-0xFF) is U+10FF00 + `b`, a char at the end of plane
//! 16 that real text does not use. [`Hay`] spells a buffer that way and maps offsets back; a pattern built
//! from buffer text (`*`, `gd`) spells its bytes the same way ([`spell`]), and [`unspell`] turns such text
//! back into bytes. A real U+10FF80-U+10FFFF char in the text is taken for the byte it spells.

use super::Match;
use crate::pos::BytePos;

/// The first escape char's code point minus 0x80: byte `b` is spelled U+10FF00 + `b`.
pub(super) const ESCAPE_BASE: u32 = 0x10_FF00;

/// The escape char that spells the byte `b` (0x80-0xFF) that is not UTF-8.
pub(crate) fn escape_char(b: u8) -> char {
    char::from_u32(ESCAPE_BASE + u32::from(b)).expect("U+10FF80-U+10FFFF are chars")
}

/// The byte an escape char spells, or `None` for any other char.
#[must_use]
pub(crate) fn escaped_byte(c: char) -> Option<u8> {
    u32::from(c)
        .checked_sub(ESCAPE_BASE)
        .and_then(|n| u8::try_from(n).ok())
        .filter(|&b| b >= 0x80)
}

/// `b` as text the engine matches: each byte that belongs to no valid UTF-8 char is its escape char.
/// Borrowed when `b` is UTF-8.
#[must_use]
pub fn spell(b: &[u8]) -> std::borrow::Cow<'_, str> {
    if let Ok(s) = std::str::from_utf8(b) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(b.len() + 8);
    for chunk in b.utf8_chunks() {
        out.push_str(chunk.valid());
        out.extend(chunk.invalid().iter().map(|&x| escape_char(x)));
    }
    std::borrow::Cow::Owned(out)
}

/// The bytes `s` spells: each escape char back to its byte ([`spell`]'s inverse). Borrowed when `s` has
/// no escape char.
#[must_use]
pub fn unspell(s: &str) -> std::borrow::Cow<'_, [u8]> {
    if !s.chars().any(|c| escaped_byte(c).is_some()) {
        return std::borrow::Cow::Borrowed(s.as_bytes());
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match escaped_byte(c) {
            Some(b) => out.push(b),
            None => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// A buffer as the engine's haystack ([`spell`]), with the offsets of the bytes it escaped so a match
/// maps back to buffer offsets. A UTF-8 buffer is borrowed as it is, and its offsets are the buffer's.
#[derive(Debug)]
pub struct Hay<'a> {
    text: std::borrow::Cow<'a, str>,
    /// The buffer offsets of the escaped bytes, ascending.
    escaped: Vec<usize>,
}

/// How many bytes longer an escape char is than the byte it spells.
const ESCAPE_GROWTH: usize = 3;

impl<'a> Hay<'a> {
    /// The haystack for buffer `b`.
    #[must_use]
    pub fn new(b: &'a [u8]) -> Hay<'a> {
        let text = spell(b);
        let escaped = match text {
            std::borrow::Cow::Borrowed(_) => Vec::new(),
            std::borrow::Cow::Owned(_) => {
                let mut v = Vec::new();
                let mut at = 0;
                for chunk in b.utf8_chunks() {
                    at += chunk.valid().len();
                    v.extend(at..at + chunk.invalid().len());
                    at += chunk.invalid().len();
                }
                v
            }
        };
        Hay { text, escaped }
    }

    /// The text the engine matches.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The buffer offset of haystack offset `h` (a char boundary of [`Hay::as_str`]).
    #[must_use]
    pub fn to_buf(&self, h: usize) -> usize {
        // The escapes that end at or before `h`: the k-th escape starts at `escaped[k] + 3k` in the hay.
        let mut lo = 0;
        let mut hi = self.escaped.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.escaped[mid] + ESCAPE_GROWTH * mid < h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        h - ESCAPE_GROWTH * lo
    }

    /// The haystack offset of buffer offset `p` (a char boundary of the buffer).
    #[must_use]
    pub fn to_hay(&self, p: usize) -> usize {
        p + ESCAPE_GROWTH * self.escaped.partition_point(|&x| x < p)
    }

    /// A match as buffer offsets.
    #[must_use]
    pub fn to_buf_match(&self, m: Match) -> Match {
        Match {
            start: BytePos(self.to_buf(m.start.0)),
            end: BytePos(self.to_buf(m.end.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern::{Options, Regex};

    /// #1243: a buffer that is not UTF-8 is searched; a byte that belongs to no valid char is one char
    /// (`.` takes it, a cut sequence is one char per byte), equals no typed char, and has the class of its
    /// Latin-1 char for `\k` / `\w` and the `\<` / `\>` edges (nvim v0.12.6, the `inv_search_*` fixtures).
    #[test]
    fn a_byte_that_is_not_utf8_is_one_char_of_its_latin1_class() {
        let spans = |pat: &str, b: &[u8]| -> Vec<(usize, usize)> {
            let hay = Hay::new(b);
            let re = Regex::compile(pat, Options::default()).expect("compiles");
            re.search_all(hay.as_str())
                .into_iter()
                .map(|m| (hay.to_buf(m.start.0), hay.to_buf(m.end.0)))
                .collect()
        };
        assert_eq!(spans("a.b", b"x a\xffb a\xffb"), vec![(2, 5), (6, 9)]);
        assert_eq!(spans("ab", b"\xffab cd ab"), vec![(1, 3), (7, 9)]);
        assert_eq!(spans("x..y", b"zz x\xe9\xb0y"), vec![(3, 7)]);
        assert_eq!(spans("x.y", b"zz x\xe9\xb0y"), vec![]);
        assert_eq!(spans("\\k\\k", b"a\x80b ab"), vec![(4, 6)]);
        assert_eq!(spans("\\k\\k\\k", b"a\xffb"), vec![(0, 3)]);
        assert_eq!(spans("\\w\\w", b"a\xffb \xff"), vec![]);
        assert_eq!(spans("\\<cd\\>", b"ab\x80cd ab"), vec![(3, 5)]);
        assert_eq!(spans("\\<cd\\>", b"ab\xffcd cd"), vec![(6, 8)]);
        assert_eq!(spans("[^a]", b"aa\xffb"), vec![(2, 3), (3, 4)]);
        assert_eq!(spans("\u{ff}", b"a\xff \xc3\xbf"), vec![(3, 5)]);
        // #1269: `[:lower:]`/`[:upper:]` test the byte as its Latin-1 char (nvim v0.12.6 `utf_islower`):
        // µ ß à-ö ø-ÿ are lowercase, À-Ö Ø-Þ uppercase, × ÷ neither.
        let lower: Vec<u8> = (0x80..=0xffu8)
            .filter(|&b| !spans("[[:lower:]]", &[b]).is_empty())
            .collect();
        let want: Vec<u8> = [0xb5, 0xdf]
            .into_iter()
            .chain(0xe0..=0xf6)
            .chain(0xf8..=0xff)
            .collect();
        assert_eq!(lower, want);
        let upper: Vec<u8> = (0x80..=0xffu8)
            .filter(|&b| !spans("[[:upper:]]", &[b]).is_empty())
            .collect();
        let want: Vec<u8> = (0xc0..=0xd6).chain(0xd8..=0xde).collect();
        assert_eq!(upper, want);
        assert_eq!(spans("[^[:lower:]]", b"\xe9a\xc9"), vec![(2, 3)]);
        // A pattern spelled from buffer text finds that text (`*` on `foo<ff>bar`).
        let word = spell(b"foo\xffbar");
        assert_eq!(
            spans(&format!("\\<{word}\\>"), b"foo\xffbar x foo\xffbar foo"),
            vec![(0, 7), (10, 17)]
        );
    }

    /// #1243: [`spell`] and [`unspell`] are inverse, borrow UTF-8 text as it is, and [`Hay`] maps every
    /// buffer char boundary to the haystack and back.
    #[test]
    fn hay_offsets_round_trip() {
        let b: &[u8] = b"a\xff\xc3\xa9\xe9\xb0\n\x80z";
        let hay = Hay::new(b);
        assert_eq!(unspell(hay.as_str()).as_ref(), b);
        for p in [0, 1, 2, 4, 5, 6, 7, 8, 9] {
            let h = hay.to_hay(p);
            assert!(hay.as_str().is_char_boundary(h), "{p} -> {h}");
            assert_eq!(hay.to_buf(h), p, "{p} -> {h}");
        }
        assert!(matches!(spell(b"abc"), std::borrow::Cow::Borrowed("abc")));
        assert!(matches!(unspell("abc"), std::borrow::Cow::Borrowed(b"abc")));
        assert_eq!(escaped_byte(escape_char(0x80)), Some(0x80));
        assert_eq!(escaped_byte(escape_char(0xff)), Some(0xff));
        assert_eq!(escaped_byte('a'), None);
        assert_eq!(escaped_byte('\u{10FF7F}'), None);
    }

    proptest::proptest! {
        /// #1243: on any bytes, the haystack spells each byte that is not UTF-8 as one escape char, and
        /// every char boundary of the buffer maps there and back.
        #[test]
        fn hay_maps_any_bytes(b in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64)) {
            let hay = Hay::new(&b);
            let back = unspell(hay.as_str()).into_owned();
            proptest::prop_assert_eq!(back, b.clone());
            for p in 0..=b.len() {
                if crate::motion::is_boundary(&b, p) {
                    let h = hay.to_hay(p);
                    proptest::prop_assert!(hay.as_str().is_char_boundary(h));
                    proptest::prop_assert_eq!(hay.to_buf(h), p);
                }
            }
        }
    }
}
