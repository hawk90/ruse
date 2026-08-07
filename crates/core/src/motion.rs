//! Motions — the geometry half of the editing grammar (C-EDITLANG). A motion resolves, from the cursor, to
//! a target position (a bare move like `w`) or to a byte range (an operator like `dw`). It also owns the
//! shared char-boundary / line helpers used across the editor.
//!
//! v0 word motions are **WORD-style** (whitespace-delimited); Vim's small-word punctuation split is a later
//! refinement. All positions land on char boundaries.

/// A motion in the editing grammar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    WordFwd,
    WordBack,
    WordEnd,
    /// Linewise (only meaningful under an operator: `dd` / `cc`).
    Line,
}

// --- shared byte / char-boundary / line helpers (one home; editor.rs reuses these) ---

pub(crate) fn is_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || i == b.len() || (b[i] & 0xC0) != 0x80
}

pub(crate) fn prev_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i > 0 {
        i -= 1;
        if is_boundary(b, i) {
            return i;
        }
    }
    0
}

pub(crate) fn next_boundary(b: &[u8], pos: usize) -> usize {
    let mut i = (pos + 1).min(b.len());
    while i < b.len() && !is_boundary(b, i) {
        i += 1;
    }
    i
}

pub(crate) fn snap(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    if is_boundary(b, p) {
        p
    } else {
        prev_boundary(b, p)
    }
}

pub(crate) fn line_start(b: &[u8], pos: usize) -> usize {
    b[..pos.min(b.len())]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1)
}

pub(crate) fn line_end(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    b[p..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |i| p + i)
}

pub(crate) fn col_of(b: &[u8], start: usize, pos: usize) -> usize {
    std::str::from_utf8(&b[start..pos])
        .map(|s| s.chars().count())
        .unwrap_or(0)
}

pub(crate) fn at_col(b: &[u8], start: usize, col: usize) -> usize {
    let end = line_end(b, start);
    let mut i = start;
    for _ in 0..col {
        if i >= end {
            break;
        }
        i = next_boundary(b, i);
    }
    i.min(end)
}

fn is_ws(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n'
}

fn next_word_start(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    while i < b.len() && !is_ws(b[i]) {
        i += 1;
    }
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    i
}

fn prev_word_start(b: &[u8], pos: usize) -> usize {
    let mut i = pos.min(b.len());
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && is_ws(b[i]) {
        i -= 1;
    }
    while i > 0 && !is_ws(b[i - 1]) {
        i -= 1;
    }
    i
}

/// One past the last non-ws byte of the current/next word (the exclusive end for `de`).
fn word_end_excl(b: &[u8], pos: usize) -> usize {
    let mut i = (pos + 1).min(b.len());
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    while i < b.len() && !is_ws(b[i]) {
        i += 1;
    }
    i
}

fn up(b: &[u8], cur: usize) -> usize {
    let ls = line_start(b, cur);
    if ls == 0 {
        cur
    } else {
        at_col(b, line_start(b, ls - 1), col_of(b, ls, cur))
    }
}

fn down(b: &[u8], cur: usize) -> usize {
    let le = line_end(b, cur);
    if le >= b.len() {
        cur
    } else {
        at_col(b, le + 1, col_of(b, line_start(b, cur), cur))
    }
}

/// The cursor target for a bare move (`w`, `3l`, `k`, …), applying `count`.
#[must_use]
pub fn target(b: &[u8], cursor: usize, m: Motion, count: u32) -> usize {
    let n = count.max(1);
    let mut c = cursor.min(b.len());
    for _ in 0..n {
        c = match m {
            Motion::Left => prev_boundary(b, c),
            Motion::Right => next_boundary(b, c),
            Motion::Up => up(b, c),
            Motion::Down => down(b, c),
            Motion::LineStart => line_start(b, c),
            Motion::LineEnd => line_end(b, c),
            Motion::WordFwd => next_word_start(b, c),
            Motion::WordBack => prev_word_start(b, c),
            Motion::WordEnd => prev_boundary(b, word_end_excl(b, c)), // land ON the last char
            Motion::Line => c,
        };
    }
    c
}

/// The charwise byte range `[start, end)` an operator (`d`/`c`) covers for a motion + count. Backward
/// motions produce a range ending at the cursor; `WordEnd` is inclusive (Vim `de`).
#[must_use]
pub fn char_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize) {
    let cur = cursor.min(b.len());
    let n = count.max(1);
    match m {
        // forward / rightward → [cursor, target)
        Motion::Right | Motion::WordFwd | Motion::LineEnd => (cur, target(b, cur, m, n)),
        // inclusive end-of-word (Vim `de`)
        Motion::WordEnd => {
            let mut e = cur;
            for _ in 0..n {
                e = word_end_excl(b, e);
            }
            (cur, e)
        }
        // backward / leftward → [target, cursor)
        Motion::Left | Motion::WordBack | Motion::LineStart => (target(b, cur, m, n), cur),
        // vertical / linewise motions are not charwise — callers handle Line specially
        Motion::Up | Motion::Down | Motion::Line => (cur, cur),
    }
}
