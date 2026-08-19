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
