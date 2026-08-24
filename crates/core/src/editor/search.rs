//! The buffer search primitives (F-009): forward/backward Vim-regex match lookup with wrap-around. Split
//! out of `editor/mod.rs` as pure, self-contained functions — they take bytes + a pattern + options and
//! return a byte offset, touching no `View`/`EditorState`, so the search command handling in the engine
//! core calls them without any state coupling.

use crate::pattern;

/// The byte offset of the next Vim-regex match at/after `from`, wrapping to the document start (F-009).
/// Invalid UTF-8, an unrepresentable/malformed pattern, or no match yields `None` — the caller keeps the
/// cursor (Vim rings the bell). Replaces the v0 literal `search::find_next`.
pub(crate) fn search_fwd(
    b: &[u8],
    pattern: &str,
    from: usize,
    opts: pattern::Options,
) -> Option<usize> {
    let hay = std::str::from_utf8(b).ok()?;
    let mut from = from.min(hay.len());
    while from < hay.len() && !hay.is_char_boundary(from) {
        from += 1; // a regex search must start on a char boundary
    }
    let re = pattern::Regex::compile(pattern, opts).ok()?;
    re.find_at(hay, from)
        .or_else(|| re.find_at(hay, 0))
        .map(|m| m.start)
}

/// Every match of `pattern` in `b`, as `(start, end)` byte spans (end exclusive), in document order.
/// Zero-width matches are dropped — a text object over an empty span (`gn`) is meaningless and would make
/// selection/advance loop. Invalid UTF-8 or an unrepresentable/malformed pattern yields an empty list.
pub(crate) fn match_spans(b: &[u8], pattern: &str, opts: pattern::Options) -> Vec<(usize, usize)> {
    let Ok(hay) = std::str::from_utf8(b) else {
        return Vec::new();
    };
    let Ok(re) = pattern::Regex::compile(pattern, opts) else {
        return Vec::new();
    };
    re.find_all(hay)
        .into_iter()
        .filter(|m| m.end > m.start)
        .map(|m| (m.start, m.end))
        .collect()
}

/// The last landable cursor byte in `b` — the last char of the last line (Vim never rests the Normal
/// cursor past it, nor on a line-terminating `\n`). `0` for an empty buffer. The clamp target when a
/// rightward character offset (`/pat/e+N`) runs off the end of the buffer.
fn last_landable(b: &[u8]) -> usize {
    let mut p = b.len();
    while p > 0 {
        let pp = crate::motion::prev_boundary(b, p);
        if b[pp] != b'\n' {
            return pp;
        }
        p = pp;
    }
    0
}

/// Move `n` CHARACTERS from `pos` (right if `n > 0`, left if `n < 0`), crossing line boundaries the way
/// Vim's `e`/`s`/`b` search offsets do (`:help search-offset`): one step right off a line's last char
/// lands on the NEXT line's first column, and a line-terminating `\n` is never itself a landing spot.
/// Clamps to the buffer — running off the end stops on the last landable char, off the front stops at 0.
/// (Empty lines are a documented corner: a char offset stepping ACROSS a truly empty line skips over it.)
pub(crate) fn step_char(b: &[u8], pos: usize, n: i32) -> usize {
    let mut p = pos.min(b.len());
    if n >= 0 {
        for _ in 0..n {
            let mut np = crate::motion::next_boundary(b, p);
            while np < b.len() && b[np] == b'\n' {
                np = crate::motion::next_boundary(b, np);
            }
            if np >= b.len() {
                return last_landable(b);
            }
            p = np;
        }
    } else {
        for _ in 0..n.unsigned_abs() {
            if p == 0 {
                return 0;
            }
            let mut pp = crate::motion::prev_boundary(b, p);
            while pp > 0 && b[pp] == b'\n' {
                pp = crate::motion::prev_boundary(b, pp);
            }
            p = pp;
        }
    }
    p
}

/// The byte offset of the previous match starting strictly before `before`, wrapping to the last match.
pub(crate) fn search_bwd(
    b: &[u8],
    pattern: &str,
    before: usize,
    opts: pattern::Options,
) -> Option<usize> {
    let hay = std::str::from_utf8(b).ok()?;
    let re = pattern::Regex::compile(pattern, opts).ok()?;
    let all = re.find_all(hay);
    all.iter()
        .rev()
        .find(|m| m.start < before)
        .or_else(|| all.last())
        .map(|m| m.start)
}
