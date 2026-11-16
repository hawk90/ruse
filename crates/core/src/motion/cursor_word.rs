//! The word or keyword under or before the cursor: `*` / `#`, `<cword>`, the `iw` span.

use crate::pos::raw::line_end;

use super::boundary::{at_bol, at_nul, next_boundary, prev_boundary, snap, Steps};
use super::word::{char_cls, class, cluster_class, Class};

/// The word-object class of a byte: for `iW`/`aW` (`big`) only whitespace-vs-not matters, so punctuation
/// joins the word; for `iw`/`aw` the three small-word classes stay distinct. Class changes only fall on char
/// boundaries (non-ASCII bytes are all `Word`), so a span built from runs of equal `obj_class` is boundary-safe.
fn obj_class(c: u8, big: bool) -> u8 {
    match class(c) {
        Class::Space => 0,
        Class::Word => 1,
        Class::Punct => {
            if big {
                1
            } else {
                2
            }
        }
    }
}

/// The run of same-object-class bytes containing the cursor — Vim `iw` (small) / `iW` (big).
fn inner_word_span(b: &[u8], cur: usize, big: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let c = cur.min(b.len() - 1);
    let cls = obj_class(b[c], big);
    let mut s = c;
    while s > 0 && obj_class(b[s - 1], big) == cls {
        s -= 1;
    }
    let mut e = c;
    while e < b.len() && obj_class(b[e], big) == cls {
        e += 1;
    }
    (s, e)
}

/// The keyword span under (or, failing that, forward on the current line from) the cursor — Vim `*`/`#`/
/// `gd` (nvim's `find_ident_at_pos`). A keyword is a run of characters of one Vim class of 2 or more
/// ([`crate::charclass::vim_class`]), so "API를" holds two keywords, "API" and "를", as for nvim (#745).
/// Returns `None` when the current line has no keyword at or after the cursor.
pub(crate) fn word_under_cursor(b: &[u8], cur: usize) -> Option<(usize, usize)> {
    use crate::charclass::WORD;
    if b.is_empty() {
        return None;
    }
    // The chars of the line, a byte that is not UTF-8 one char with its Latin-1 class (#1243).
    let class = |p: usize| cluster_class(b, p);
    // 1. The first keyword char at or after the cursor, on its line.
    let mut at = snap(b, cur.min(b.len() - 1));
    while class(at) < WORD {
        if at_nul(b, at) {
            return None;
        }
        at = next_boundary(b, at);
    }
    let this = class(at);
    // 2. Back to the start of the run of that class, 3. on to its end.
    let mut start = at;
    while !at_bol(b, start) && class(prev_boundary(b, start)) == this {
        start = prev_boundary(b, start);
    }
    let mut end = at;
    while !at_nul(b, end) && class(end) == this {
        end = next_boundary(b, end);
    }
    Some((start, end))
}

/// The `<cword>` (`big == false`) or `<cWORD>` (`big == true`) span under the cursor — the buffer text
/// Vim's command-line `c_CTRL-R_CTRL-W` / `c_CTRL-R_CTRL-A` splice at the cmdline caret
/// (`:help c_CTRL-R_CTRL-W`, `:help expand()` `<cword>`/`<cWORD>`). Distinct from [`word_under_cursor`]
/// (Vim `*`): that returns `None` when the line has no keyword, whereas `<cword>` falls back to the
/// non-blank (punctuation) run — so `C-r C-w` on `foo ...` at the first `.` yields `...`, where `*` bells.
///
/// `<cword>`: the keyword (alnum/`_`/non-ASCII run) under the cursor; if the cursor is not on a keyword,
/// the search scans FORWARD on the line for the next keyword char (crossing blanks and punctuation). When
/// the rest of the line has no keyword, it falls back to the non-blank run under/forward of the cursor.
///
/// `<cWORD>`: the whitespace-delimited WORD (maximal non-blank run) containing the cursor; if the cursor
/// is on whitespace, the next such run forward on the line.
///
/// Returns `None` when the rest of the line (from the cursor) is blank. Char-boundary safe (spans are
/// built from equal-class runs). Verified against nvim v0.12.4.
pub(crate) fn cword_under_cursor(b: &[u8], cur: usize, big: bool) -> Option<(usize, usize)> {
    if b.is_empty() {
        return None;
    }
    let i = cur.min(b.len() - 1);
    let le = line_end(b, i);
    // The next non-blank position at or forward of the cursor, within the current line (`None` if the
    // rest of the line is blank). Both variants fall back to this run when there is no keyword.
    let first_nonblank = |from: usize| {
        let mut k = from;
        while k < le && class(b[k]) == Class::Space {
            k += 1;
        }
        (k < le && class(b[k]) != Class::Space).then_some(k)
    };
    if big {
        // `<cWORD>`: skip leading blanks, then the maximal non-blank run (word+punct merge under `big`).
        return first_nonblank(i).map(|k| inner_word_span(b, k, true));
    }
    // `<cword>`: prefer the next keyword char forward on the line (crossing blanks and punctuation).
    // A char's class is its Vim class, a byte that is not UTF-8 the one of its Latin-1 char (#1243, #1252).
    let mut j = i;
    while j < le && char_cls(b, j, false) < crate::charclass::WORD {
        j = next_boundary(b, j);
    }
    if j < le {
        return Some(inner_word_span(b, j, false));
    }
    // No keyword on the rest of the line: fall back to the non-blank (punctuation) run.
    first_nonblank(i).map(|k| inner_word_span(b, k, false))
}

/// The keyword (Word-class run) ending exactly AT the caret — the "base" Vim's insert-mode keyword
/// completion (`i_CTRL-N` / `i_CTRL-P`) matches against. Returns `(start, base)`: `start` is the byte
/// offset where the run begins, `base` its text (empty, with `start == cur`, when the char before the
/// caret is not keyword-class or the caret is at a line/buffer start). A keyword is a run of grapheme
/// clusters of one Vim class of 2 or more ([`crate::charclass::vim_class`]), as nvim's
/// `get_normal_compl_info` reads it, so the base of "API를" is "를" (#1136).
pub(crate) fn keyword_before_cursor(b: &[u8], cur: usize) -> (usize, &[u8]) {
    let cur = cur.min(b.len());
    let steps = Steps::new(b);
    let mut start = cur;
    let mut class = None;
    while !at_bol(b, start) {
        let p = steps.prev(start);
        let k = cluster_class(b, p);
        if k < crate::charclass::WORD || class.is_some_and(|c| c != k) {
            break;
        }
        class = Some(k);
        start = p;
    }
    (start, &b[start..cur])
}
