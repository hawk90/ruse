//! Keyword-line lookup — the pure engine behind Vim's `[i` / `]i` / `[I` / `]I` (`:help [i`): show the
//! line(s) in the CURRENT buffer that contain the keyword under the cursor. These are DISPLAY commands
//! (no buffer mutation); the frontend resolves the keyword (like `*`/`gd`) and renders the echo / list.
//!
//! Scope is the current buffer only. Vim's `#include`-following forms (`:checkpath`, searching included
//! files) are OUT (they need a cross-file include graph). ruse has no `iskeyword` option, so a "whole
//! word" here means the keyword bounded by non-Word bytes, where Word = alnum + `_` + non-ASCII — the
//! same class `*`/`#`/`gd`/word-motions use (a documented divergence from Vim's `iskeyword`-driven match).
//!
//! Semantics VERIFIED against nvim v0.12.4 (headless):
//!   - `[i` / `N[i` — the N-th (default 1) whole-word match scanning from the TOP of the file. If that
//!     match lands on the CURSOR's line → `E387: Match is on current line`; if there are fewer than N
//!     matches → `E389: Couldn't find pattern`.
//!   - `]i` / `N]i` — the N-th match strictly BELOW the cursor line. If there is no N-th such match:
//!     `E387` when the keyword also appears on another line, else `E389` (the keyword is unique to the
//!     current line — which always contains it, since it is under the cursor).
//!   - `[I` — list ALL matching lines of the file (always non-empty; the cursor's line matches).
//!   - `]I` — list the matching lines strictly below the cursor; if there are none, list the current
//!     line (nvim's fallback when the cursor sits on the last / only match).

use crate::pos::{line_end, nth_line_start};

/// A Word-class byte (alnum + `_` + non-ASCII), mirroring `motion::class`'s `Class::Word`. ruse has no
/// `iskeyword`; keyword boundaries follow this class everywhere (`*`/`#`/`gd`), so `[i`/`]i` do too.
fn is_keyword_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80
}

/// Whether `line` contains `kw` as a whole word — an occurrence bounded on BOTH sides by a non-Word byte
/// (or the line edge). `kw` is the raw keyword bytes; it is itself a Word run (resolved from the buffer),
/// so this is a byte substring search with keyword-boundary guards.
fn line_has_keyword(line: &[u8], kw: &[u8]) -> bool {
    if kw.is_empty() || kw.len() > line.len() {
        return false;
    }
    let mut i = 0;
    while i + kw.len() <= line.len() {
        if &line[i..i + kw.len()] == kw {
            let before_ok = i == 0 || !is_keyword_byte(line[i - 1]);
            let after = i + kw.len();
            let after_ok = after == line.len() || !is_keyword_byte(line[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// The 1-based line numbers (file order) whose line contains `keyword` as a whole word. Empty `keyword`
/// yields an empty list. A trailing `\n` does not create a spurious final match (the empty tail can't
/// match a non-empty keyword).
pub fn keyword_line_numbers(bytes: &[u8], keyword: &str) -> Vec<usize> {
    let kw = keyword.as_bytes();
    if kw.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|&b| b == b'\n')
        .enumerate()
        .filter(|(_, line)| line_has_keyword(line, kw))
        .map(|(idx, _)| idx + 1)
        .collect()
}

/// The verbatim text of the 1-based `line` (no trailing `\n`), lossily decoded. Used to build the echoed
/// line for `[i`/`]i` and the list rows for `[I`/`]I` (Vim echoes the matching line's text as-is,
/// including its indentation).
pub fn line_text(bytes: &[u8], line: usize) -> String {
    if line == 0 {
        return String::new();
    }
    let start = nth_line_start(bytes, line - 1);
    let end = line_end(bytes, start);
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

/// The outcome of a `[i` / `]i` single-line lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordEcho {
    /// Show this 1-based line (echo its text).
    Line(usize),
    /// `E387: Match is on current line`.
    CurrentLine,
    /// `E389: Couldn't find pattern`.
    NotFound,
}

/// Resolve `[i`/`]i` (single-line echo). `matches` are the file-order 1-based match lines (from
/// [`keyword_line_numbers`]), `cursor_line` is 1-based, `above` selects `[i` (from the top) vs `]i`
/// (below the cursor), and `count` is the 1-based match selector (clamped to ≥ 1 by the caller).
pub fn keyword_echo(
    matches: &[usize],
    cursor_line: usize,
    above: bool,
    count: usize,
) -> KeywordEcho {
    let n = count.max(1);
    if above {
        // `[i` — the N-th match scanning from the top of the file.
        match matches.get(n - 1) {
            Some(&line) if line == cursor_line => KeywordEcho::CurrentLine,
            Some(&line) => KeywordEcho::Line(line),
            None => KeywordEcho::NotFound,
        }
    } else {
        // `]i` — the N-th match strictly below the cursor line.
        let below: Vec<usize> = matches
            .iter()
            .copied()
            .filter(|&l| l > cursor_line)
            .collect();
        match below.get(n - 1) {
            Some(&line) => KeywordEcho::Line(line),
            // No N-th match below: E387 if the keyword also appears elsewhere (another line), else E389
            // (the current line — always a match, since the keyword is under the cursor — is the only one).
            None if matches.len() > 1 => KeywordEcho::CurrentLine,
            None => KeywordEcho::NotFound,
        }
    }
}

/// Resolve `[I`/`]I` (list-all). Returns the 1-based line numbers to display, in file order. `[I`
/// (`above`) lists every match; `]I` lists the matches strictly below the cursor, or — when there are
/// none — the current line (nvim's fallback when the cursor is on the last / only match). Never empty
/// when the cursor sits on a keyword (its line always matches).
pub fn keyword_list(matches: &[usize], cursor_line: usize, above: bool) -> Vec<usize> {
    if above {
        return matches.to_vec();
    }
    let below: Vec<usize> = matches
        .iter()
        .copied()
        .filter(|&l| l > cursor_line)
        .collect();
    if below.is_empty() {
        // Fall back to the current line's match(es) (the cursor line always contains the keyword).
        matches
            .iter()
            .copied()
            .filter(|&l| l == cursor_line)
            .collect()
    } else {
        below
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUF: &[u8] = b"alpha foo beta\n    indented foo here\nfoo epsilon\nzeta eta\ntheta foo iota\nfoobar notmatch\n";

    #[test]
    fn whole_word_match_excludes_substring_occurrences() {
        // `foo` matches lines 1,2,3,5 but NOT line 6 (`foobar` — a longer Word run), matching nvim.
        assert_eq!(keyword_line_numbers(BUF, "foo"), vec![1, 2, 3, 5]);
    }

    #[test]
    fn no_match_and_empty_keyword_yield_empty() {
        assert_eq!(keyword_line_numbers(BUF, "absent"), Vec::<usize>::new());
        assert_eq!(keyword_line_numbers(BUF, ""), Vec::<usize>::new());
        assert_eq!(keyword_line_numbers(b"", "foo"), Vec::<usize>::new());
    }

    #[test]
    fn keyword_at_line_edges_matches() {
        // Keyword at the very start and end of a line (no neighbour byte on one side).
        assert_eq!(
            keyword_line_numbers(b"foo\nx foo\nfoo", "foo"),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn underscore_and_unicode_are_keyword_chars() {
        // `foo_bar` is one Word run: a search for `foo` must NOT match it.
        assert_eq!(keyword_line_numbers(b"foo_bar\nfoo\n", "foo"), vec![2]);
        // Non-ASCII (Hangul) is a Word byte: `가` inside `가나` is not a whole-word `가`.
        assert_eq!(keyword_line_numbers("가나\n가\n".as_bytes(), "가"), vec![2]);
    }

    #[test]
    fn line_text_returns_verbatim_line_including_indent() {
        assert_eq!(line_text(BUF, 1), "alpha foo beta");
        assert_eq!(line_text(BUF, 2), "    indented foo here"); // indentation preserved
        assert_eq!(line_text(BUF, 5), "theta foo iota");
    }

    #[test]
    fn echo_above_picks_nth_from_top_and_flags_current_line() {
        let m = keyword_line_numbers(BUF, "foo"); // [1,2,3,5]
                                                  // `[i` on line 3 → first match from top = line 1.
        assert_eq!(keyword_echo(&m, 3, true, 1), KeywordEcho::Line(1));
        // `2[i` on line 3 → second match = line 2.
        assert_eq!(keyword_echo(&m, 3, true, 2), KeywordEcho::Line(2));
        // `3[i` on line 3 → third match = line 3 = current line → E387.
        assert_eq!(keyword_echo(&m, 3, true, 3), KeywordEcho::CurrentLine);
        // `4[i` on line 3 → fourth match = line 5.
        assert_eq!(keyword_echo(&m, 3, true, 4), KeywordEcho::Line(5));
        // `[i` on line 1 (current line is the first match) → E387.
        assert_eq!(keyword_echo(&m, 1, true, 1), KeywordEcho::CurrentLine);
        // `9[i` → out of range → E389.
        assert_eq!(keyword_echo(&m, 3, true, 9), KeywordEcho::NotFound);
    }

    #[test]
    fn echo_below_picks_nth_strictly_below_with_e387_e389_split() {
        let m = keyword_line_numbers(BUF, "foo"); // [1,2,3,5]
                                                  // `]i` on line 1 → first match below = line 2.
        assert_eq!(keyword_echo(&m, 1, false, 1), KeywordEcho::Line(2));
        // `2]i` on line 1 → second below = line 3.
        assert_eq!(keyword_echo(&m, 1, false, 2), KeywordEcho::Line(3));
        // `]i` on line 3 → first below = line 5.
        assert_eq!(keyword_echo(&m, 3, false, 1), KeywordEcho::Line(5));
        // `]i` on line 5 (last match): none below, keyword appears elsewhere → E387.
        assert_eq!(keyword_echo(&m, 5, false, 1), KeywordEcho::CurrentLine);
        // Keyword unique to the current line: `]i` → E389 (not E387).
        let uniq = keyword_line_numbers(b"nothing here\nsecond\n", "nothing"); // [1]
        assert_eq!(keyword_echo(&uniq, 1, false, 1), KeywordEcho::NotFound);
    }

    #[test]
    fn list_above_lists_whole_file() {
        let m = keyword_line_numbers(BUF, "foo");
        assert_eq!(keyword_list(&m, 3, true), vec![1, 2, 3, 5]);
    }

    #[test]
    fn list_below_is_strictly_below_or_falls_back_to_current() {
        let m = keyword_line_numbers(BUF, "foo"); // [1,2,3,5]
        assert_eq!(keyword_list(&m, 1, false), vec![2, 3, 5]);
        assert_eq!(keyword_list(&m, 3, false), vec![5]);
        // On the last match (line 5): nothing strictly below → fall back to the current line.
        assert_eq!(keyword_list(&m, 5, false), vec![5]);
    }
}
