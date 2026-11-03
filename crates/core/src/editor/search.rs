//! The buffer search primitives (F-009): forward/backward Vim-regex match lookup with wrap-around. Split
//! out of `editor/mod.rs` as pure, self-contained functions — they take bytes + a pattern + options and
//! return a byte offset, touching no `View`/`EditorState`, so the search command handling in the engine
//! core calls them without any state coupling.

use crate::pattern;

/// The byte offset of the first search match whose landing is at/after `from`, wrapping to the first
/// match of the document (F-009). Matches are the ones a `/` search sees ([`search_spans`]), compared by
/// [`landing_key`], so `n` agrees with `/`: a `\zs` match is found by its reported start, and an empty
/// match (`^`, `$`, `\<`) counts. Invalid UTF-8, an unrepresentable/malformed pattern, or no match yields
/// `None` — the caller keeps the cursor (Vim rings the bell).
pub(crate) fn search_fwd(
    b: &[u8],
    pattern: &str,
    from: usize,
    opts: pattern::Options,
) -> Option<usize> {
    let spans = search_spans(b, pattern, opts);
    spans
        .iter()
        .map(|&(s, _)| s)
        .find(|&s| landing_key(b, s) >= from)
        .or_else(|| spans.first().map(|&(s, _)| s))
}

/// Take `count` (at least one) steps of `step` from `start`, or `None` when a step finds nothing. A search
/// step is a pure function of the position and every landing is one of the buffer's finitely many matches,
/// so the walk becomes periodic once a landing repeats: the remaining steps are taken modulo that period. A
/// hostile count (`99999999/foo`, `999999999n`) then costs at most one step per distinct landing plus one
/// period, not `count` steps (#682).
pub(crate) fn walk_count(
    start: usize,
    count: u32,
    mut step: impl FnMut(usize) -> Option<usize>,
) -> Option<usize> {
    let count = count.max(1);
    let mut seen: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
    let mut pos = start;
    let mut i = 0;
    while i < count {
        if let Some(&j) = seen.get(&pos) {
            for _ in 0..(count - i) % (i - j) {
                pos = step(pos)?;
            }
            return Some(pos);
        }
        seen.insert(pos, i);
        pos = step(pos)?;
        i += 1;
    }
    Some(pos)
}

/// Every match a `/` or `?` search can land on, as `(start, end)` byte spans (end exclusive), in
/// document order: [`pattern::Regex::search_all`], empty matches included. Invalid UTF-8 or an
/// unrepresentable/malformed pattern yields an empty list.
pub(crate) fn search_spans(b: &[u8], pattern: &str, opts: pattern::Options) -> Vec<(usize, usize)> {
    let Ok(hay) = std::str::from_utf8(b) else {
        return Vec::new();
    };
    let Ok(re) = pattern::Regex::compile(pattern, opts) else {
        return Vec::new();
    };
    re.search_all(hay)
        .into_iter()
        .map(|m| (m.start, m.end))
        .collect()
}

/// [`search_spans`] without the empty matches: the matches `gn` can select. A text object over an
/// empty span is meaningless and would make selection/advance loop (nvim: `/\s` then `gnd` on "a\nb"
/// selects nothing).
pub(crate) fn match_spans(b: &[u8], pattern: &str, opts: pattern::Options) -> Vec<(usize, usize)> {
    let mut spans = search_spans(b, pattern, opts);
    spans.retain(|&(s, e)| e > s);
    spans
}

/// Where a search compares a match landing at `pos` against the cursor: one character back when `pos`
/// is the end of a non-empty line (a `$` match), since the Normal cursor is put there afterwards. This is
/// nvim's `searchit` rule that keeps `/$` and `n` from sticking at the end of a line; the landing itself
/// stays at `pos`.
pub(crate) fn landing_key(b: &[u8], pos: usize) -> usize {
    let at_eol = pos >= b.len() || b[pos] == b'\n';
    if at_eol && pos > 0 && pos <= b.len() && b[pos - 1] != b'\n' {
        crate::motion::prev_grapheme(b, pos)
    } else {
        pos
    }
}

/// The last landable cursor byte in `b` — the last char of the last line (Vim never rests the Normal
/// cursor past it, nor on a line-terminating `\n`). `0` for an empty buffer. The clamp target when a
/// rightward character offset (`/pat/e+N`) runs off the end of the buffer.
fn last_landable(b: &[u8]) -> usize {
    let mut p = b.len();
    while p > 0 {
        let pp = crate::motion::prev_grapheme(b, p);
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
            let mut np = crate::motion::next_grapheme(b, p);
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
            let mut pp = crate::motion::prev_grapheme(b, p);
            while pp > 0 && b[pp] == b'\n' {
                pp = crate::motion::prev_grapheme(b, pp);
            }
            p = pp;
        }
    }
    p
}

/// The byte offset of the previous search match landing strictly before `before`, wrapping to the last
/// match (see [`search_fwd`] for which matches count).
pub(crate) fn search_bwd(
    b: &[u8],
    pattern: &str,
    before: usize,
    opts: pattern::Options,
) -> Option<usize> {
    let spans = search_spans(b, pattern, opts);
    spans
        .iter()
        .rev()
        .map(|&(s, _)| s)
        .find(|&s| landing_key(b, s) < before)
        .or_else(|| spans.last().map(|&(s, _)| s))
}

#[cfg(test)]
mod walk_tests {
    use super::walk_count;

    /// The naive walk `walk_count` replaces: `count` steps, one by one.
    fn naive(start: usize, count: u32, step: impl Fn(usize) -> Option<usize>) -> Option<usize> {
        let mut pos = start;
        for _ in 0..count.max(1) {
            pos = step(pos)?;
        }
        Some(pos)
    }

    #[test]
    fn walk_count_agrees_with_the_naive_walk() {
        // Wrapping walks over a few landing sets, including one that enters its cycle late (a tail).
        let sets: [&[usize]; 4] = [&[3], &[1, 4, 9], &[0, 2, 5, 7, 11], &[2, 2, 6]];
        for set in sets {
            let fwd = |p: usize| Some(set.iter().copied().find(|&s| s > p).unwrap_or(set[0]));
            let tail = |p: usize| {
                Some(if p < 100 {
                    p + 30
                } else {
                    set[0] + 100 + (p + 1) % 3
                })
            };
            for start in 0..12 {
                for count in 0..40 {
                    assert_eq!(walk_count(start, count, fwd), naive(start, count, fwd));
                    assert_eq!(walk_count(start, count, tail), naive(start, count, tail));
                }
            }
        }
        assert_eq!(
            walk_count(0, u32::MAX, |p| Some((p + 1) % 7)),
            Some(u32::MAX as usize % 7)
        );
        assert_eq!(walk_count(0, 5, |_| None), None);
    }
}
