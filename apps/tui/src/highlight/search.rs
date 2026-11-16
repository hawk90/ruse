//! Search-match spans cached per buffer, revision, viewport and pattern.

use ruse_core::{DocumentId, Regex, RegexOptions, Revision};

/// Cached hlsearch / incsearch match spans for the focused pane (F-009 #1). Same shape as
/// [`CachedHighlight`](super::CachedHighlight): keyed on `(revision, viewport, pattern)`, so an
/// unchanged frame — cursor motion, a mode switch — does no work, and a scroll or a new revision
/// recomputes only the VISIBLE byte range rather than re-running a full-buffer regex every frame. The
/// Vim regex is compiled only when the pattern changes (incsearch re-types the pattern each keystroke;
/// hlsearch holds it fixed). The key includes the buffer: revisions count per document, so two buffers
/// share revision numbers (#687).
#[derive(Default)]
pub struct CachedSearch {
    key: Option<(DocumentId, Revision, std::ops::Range<usize>, String)>,
    compiled: Option<(String, Regex)>,
    spans: Vec<(usize, usize)>,
}

impl CachedSearch {
    /// [`CachedSearch::spans_in`] for a single-buffer caller (benches, tests).
    pub fn spans(
        &mut self,
        rev: Revision,
        bytes: &[u8],
        visible: std::ops::Range<usize>,
        pattern: &str,
    ) -> &[(usize, usize)] {
        self.spans_in(DocumentId(0), rev, bytes, visible, pattern)
    }

    /// Match spans of `pattern` within the `visible` byte range of buffer `doc`'s `bytes` at `rev`. Empty
    /// on an unmatchable pattern (highlighting is best-effort). Spans are in absolute buffer byte offsets.
    pub fn spans_in(
        &mut self,
        doc: DocumentId,
        rev: Revision,
        bytes: &[u8],
        visible: std::ops::Range<usize>,
        pattern: &str,
    ) -> &[(usize, usize)] {
        if self
            .key
            .as_ref()
            .is_some_and(|(d, r, v, p)| *d == doc && *r == rev && *v == visible && p == pattern)
        {
            return &self.spans;
        }
        // Recompile only when the pattern itself changed (a scroll or an edit keeps the same regex).
        if self.compiled.as_ref().is_none_or(|(p, _)| p != pattern) {
            self.compiled = Regex::compile(pattern, RegexOptions::default())
                .ok()
                .map(|re| (pattern.to_string(), re));
        }
        self.spans.clear();
        let vis = visible.start.min(bytes.len())..visible.end.min(bytes.len());
        if let Some((_, re)) = &self.compiled {
            // The viewport range is line-aligned (see `nth_line_start`), so a match on any visible line
            // is fully inside it — searching the slice and offsetting is complete for what render paints.
            // A byte that is not UTF-8 is one char to the pattern (#1243).
            let hay = ruse_core::pattern::Hay::new(&bytes[vis.clone()]);
            for m in re.find_all(hay.as_str()) {
                let m = hay.to_buf_match(m);
                self.spans
                    .push((vis.start + m.start.0, vis.start + m.end.0));
            }
        }
        self.key = Some((doc, rev, visible, pattern.to_string()));
        &self.spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_search_matches_and_bounds_to_viewport() {
        let mut s = CachedSearch::default();
        let bytes = b"a b a";
        // Full range: every match (Vim-magic regex).
        assert_eq!(
            s.spans(Revision(0), bytes, 0..bytes.len(), "a"),
            &[(0, 1), (4, 5)]
        );
        // Magic quantifier, not literal.
        assert_eq!(
            s.spans(Revision(1), b"aa b aaa", 0..8, "a\\+"),
            &[(0, 2), (5, 8)]
        );
        // An unrepresentable/invalid pattern highlights nothing (never errors).
        assert!(s.spans(Revision(2), b"abc", 0..3, "\\1").is_empty());
        // Viewport bound: the first line's match is excluded when only the second line is visible.
        let two = b"a\na\n";
        assert_eq!(s.spans(Revision(3), two, 2..4, "a"), &[(2, 3)]);
    }

    /// Regression (issue #514): an empty-matching search pattern used to spin `find_all` forever inside
    /// hlsearch highlighting, hanging the editor on `/a*`, `/$`, `/.*`, `/x*`, or an empty `/`. This drives
    /// the real `CachedSearch::spans` path — it would NOT return if the bug regressed. Spans are the
    /// nvim v0.12.4-faithful zero-width behavior (no trailing empty match at end-of-line for an unanchored
    /// pattern; a lone anchored `$` match at EOL is kept).
    #[test]
    fn cached_search_empty_matching_pattern_terminates() {
        let mut s = CachedSearch::default();
        // `a*` on "aax": only the "aa" run (no empty span after it, none at EOL).
        assert_eq!(s.spans(Revision(0), b"aax", 0..3, "a*"), &[(0, 2)]);
        // `x*` on "abc": a zero-width match before each char, but NOT at end-of-line.
        assert_eq!(
            s.spans(Revision(1), b"abc", 0..3, "x*"),
            &[(0, 0), (1, 1), (2, 2)]
        );
        // `$` on "abc": the lone anchored empty match at EOL is kept.
        assert_eq!(s.spans(Revision(2), b"abc", 0..3, "$"), &[(3, 3)]);
        // Empty pattern / empty buffer: the sole zero-width match is kept.
        assert_eq!(s.spans(Revision(3), b"", 0..0, "x*"), &[(0, 0)]);
    }

    #[test]
    fn cached_search_reuses_on_unchanged_key() {
        // Same (revision, viewport, pattern) but different bytes → a cache hit returns the stale spans.
        let mut s = CachedSearch::default();
        let n = s.spans(Revision(0), b"a a", 0..3, "a").len();
        let stale = s.spans(Revision(0), b"zzzzz", 0..3, "a").len();
        assert_eq!(stale, n, "an unchanged key does not re-search");
    }
}
