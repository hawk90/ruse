//! Insert-mode completion candidate lists: `i_CTRL-N` keywords and `i_CTRL-X_CTRL-L` lines.

use unicode_segmentation::UnicodeSegmentation;

use super::boundary::Steps;
use super::word::cluster_class;

/// Every keyword in `b` as `(start, text)`, in positional order: runs of grapheme clusters of one Vim
/// class of 2 or more (nvim `find_word_end`); a byte that is not UTF-8 is a cluster of its own with the
/// class of its Latin-1 char (#1243).
fn keyword_runs(b: &[u8]) -> Vec<(usize, &[u8])> {
    let mut runs: Vec<(usize, &[u8])> = Vec::new();
    let mut open: Option<(usize, u32)> = None;
    let mut push = |i: usize, k: u32, open: &mut Option<(usize, u32)>| {
        if let Some((s, c)) = *open {
            if c == k {
                return;
            }
            runs.push((s, &b[s..i]));
            *open = None;
        }
        if k >= crate::charclass::WORD {
            *open = Some((i, k));
        }
    };
    if let Ok(text) = std::str::from_utf8(b) {
        for (i, g) in text.grapheme_indices(true) {
            push(i, keyword_class(g), &mut open);
        }
    } else {
        let steps = Steps::new(b);
        let mut i = 0;
        while i < b.len() {
            push(i, cluster_class(b, i), &mut open);
            i = steps.next(i);
        }
    }
    if let Some((s, _)) = open {
        runs.push((s, &b[s..]));
    }
    runs
}

/// The Vim class of a grapheme cluster (its first char's), 0 for a line break.
fn keyword_class(g: &str) -> u32 {
    match g.chars().next() {
        None | Some('\n') => 0,
        Some(c) => crate::charclass::vim_class(c),
    }
}

/// The current-buffer keyword-completion candidates for `i_CTRL-N` / `i_CTRL-P` (F-003), in Vim's scan
/// order. `base` is [`keyword_before_cursor`](super::keyword_before_cursor)'s text; the result is every keyword in the buffer that
/// STARTS WITH `base`, ordered forward from the caret to end-of-buffer then wrapping from the buffer
/// start back to the caret, with the word under construction (the run at the caret) excluded and later
/// DUPLICATES removed (first occurrence in scan order wins). Matching is case-sensitive (nvim default
/// `noignorecase`; the 'ignorecase'/'infercase' interaction is out of scope for this slice). C-P walks
/// this same list backward — the caller cycles the index, so only one order is built. Verified against
/// nvim v0.12.4.
pub(crate) fn keyword_completions(b: &[u8], cur: usize, base: &[u8]) -> Vec<String> {
    // The run under construction begins here; it is never offered as its own candidate. Only meaningful
    // when `base` is non-empty (an empty base sits after a non-word char, so no run starts at the caret).
    let base_start = cur.saturating_sub(base.len());
    let runs = keyword_runs(b);
    // Forward-from-caret first (runs starting at/after the caret), then wrap from the buffer start.
    let ordered = runs
        .iter()
        .filter(|(s, _)| *s >= cur)
        .chain(runs.iter().filter(|(s, _)| *s < cur));
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for (s, t) in ordered {
        if !base.is_empty() && *s == base_start {
            continue; // the word being typed — never its own candidate
        }
        if !t.starts_with(base) {
            continue;
        }
        // The candidates travel to the input engine as text: a byte that is not UTF-8 is spelled as its
        // escape char ([`crate::pattern::spell`]), which `CompleteWord` turns back into the byte (#1252).
        if seen.insert(t) {
            out.push(crate::pattern::spell(t).into_owned());
        }
    }
    out
}

/// The end of the leading indent (spaces/tabs) on the byte range `[start, end)` — the first offset whose
/// byte is neither a space nor a tab, clamped to `end` when the whole range is blank. Used by whole-line
/// completion, which IGNORES leading indent on both the typed prefix and every candidate line.
fn indent_end(b: &[u8], start: usize, end: usize) -> usize {
    let mut i = start;
    while i < end && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

/// Insert-mode WHOLE-LINE completion source for `i_CTRL-X CTRL-L` (F-003, current-buffer only), in Vim's
/// scan order. Returns `(base, candidates)`: `base` is the caret line's content AFTER its leading indent,
/// up to the caret (the text being completed); each candidate is another buffer line's content after its
/// OWN leading indent — indent is IGNORED for both the match and the inserted text (`:help i_CTRL-X_CTRL-L`,
/// "Indent is ignored") — that STARTS WITH `base`. The order mirrors Vim's backward search: the line just
/// ABOVE the caret, upward to the top, then WRAPPING from the bottom line upward back to the line just
/// below the caret; the caret's own line is never a candidate, and later DUPLICATES (by post-indent
/// content, so lines differing only in indent collapse) are removed — first in scan order wins. Matching is
/// case-sensitive (nvim default `noignorecase`; the 'ignorecase'/'infercase' interaction is out of scope
/// for this slice). C-P walks the same list backward — the caller cycles the index, so only one order is
/// built. Verified against nvim v0.12.4.
pub(crate) fn line_completions(b: &[u8], cur: usize) -> (String, Vec<String>) {
    // `base`: the caret line's post-indent text up to the caret (empty when the caret sits within indent).
    let ls = crate::pos::raw::line_start(b, cur);
    let bi = indent_end(b, ls, cur);
    // Text travels spelled ([`crate::pattern::spell`]): a byte that is not UTF-8 is its escape char, which
    // `CompleteWord` turns back into the byte (#1252). Matching is on the bytes.
    let base_bytes = &b[bi..cur];
    let base = crate::pattern::spell(base_bytes).into_owned();
    // Line geometry: `n` rows total, the caret on row `cr`. Scan backward from `cr` with wraparound.
    // The slot after a final newline is a row only while the caret is on it (a line being typed); else it
    // is no line and offers no empty candidate (#677).
    let cr = crate::pos::raw::line_of(b, cur);
    let last = crate::pos::raw::line_of(b, b.len());
    let n = if b.last() == Some(&b'\n') && cr < last {
        last
    } else {
        last + 1
    };
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 1..n {
        let row = (cr + n - i) % n; // cr-1, cr-2, …, 0, n-1, …, cr+1 — every row but the caret's
        let rs = crate::pos::raw::nth_line_start(b, row);
        let re = crate::pos::raw::line_end(b, rs);
        let ri = indent_end(b, rs, re);
        let content = &b[ri..re];
        if !content.starts_with(base_bytes) {
            continue;
        }
        let text = crate::pattern::spell(content).into_owned();
        if seen.insert(text.clone()) {
            out.push(text);
        }
    }
    (base, out)
}
