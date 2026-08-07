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
    /// Text object: the word under the cursor (`iw`). Only meaningful under an operator.
    InnerWord,
    /// Text object: the word plus its adjacent whitespace (`aw`). Only meaningful under an operator.
    AWord,
    /// Linewise (only meaningful under an operator: `dd` / `cc`).
    Line,
    /// Char-search within the current line: `f`/`F` land on the `count`-th `ch`; `t`/`T` stop one char
    /// short of it. `forward` picks the direction, `till` picks find-vs-till. Repeated by `;`/`,`.
    FindChar {
        ch: char,
        forward: bool,
        till: bool,
    },
    /// Go to the `count`-th line (1-based), landing on its first non-blank char (`gg`, `{count}G`). Linewise
    /// under an operator.
    GotoLine,
    /// Go to the last line's first non-blank char (bare `G`). Linewise under an operator.
    LastLine,
    /// Jump to the matching bracket of `()`, `[]`, or `{}` (`%`). Nesting-aware; the match may be on another
    /// line. If the cursor is not on a bracket, the first bracket forward on the line is matched instead.
    MatchBracket,
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

/// The run of same-class (word vs whitespace) bytes containing the cursor — Vim `iw`.
fn inner_word_span(b: &[u8], cur: usize) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let c = cur.min(b.len() - 1);
    let ws = is_ws(b[c]);
    let mut s = c;
    while s > 0 && is_ws(b[s - 1]) == ws {
        s -= 1;
    }
    let mut e = c;
    while e < b.len() && is_ws(b[e]) == ws {
        e += 1;
    }
    (s, e)
}

/// The word plus its trailing whitespace (or leading, if there is no trailing) — Vim `aw`.
fn a_word_span(b: &[u8], cur: usize) -> (usize, usize) {
    let (s, e) = inner_word_span(b, cur);
    let mut e2 = e;
    while e2 < b.len() && is_ws(b[e2]) {
        e2 += 1;
    }
    if e2 > e {
        (s, e2)
    } else {
        let mut s2 = s;
        while s2 > 0 && is_ws(b[s2 - 1]) {
            s2 -= 1;
        }
        (s2, e)
    }
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

/// The bare-move landing for a char-search (`f`/`F`/`t`/`T`), or `None` if the `count`-th match does not
/// exist on the current line. `f`/`F` land on the char; `t`/`T` land one boundary short of it. Search is
/// confined to the current line (Vim never crosses a newline for these).
fn find_char_target(
    b: &[u8],
    cursor: usize,
    ch: char,
    forward: bool,
    till: bool,
    count: u32,
) -> Option<usize> {
    let n = count.max(1) as usize;
    let ls = line_start(b, cursor);
    let le = line_end(b, cursor); // newline / end position for this line
    let line = std::str::from_utf8(b.get(ls..le)?).ok()?;
    if forward {
        // Matches strictly after the cursor, in order; take the n-th.
        let hit = line
            .char_indices()
            .map(|(i, c)| (ls + i, c))
            .filter(|&(pos, c)| pos > cursor && c == ch)
            .nth(n - 1)?
            .0;
        Some(if till { prev_boundary(b, hit) } else { hit })
    } else {
        // Matches strictly before the cursor; take the n-th counting backward.
        let hit = line
            .char_indices()
            .map(|(i, c)| (ls + i, c))
            .filter(|&(pos, c)| pos < cursor && c == ch)
            .rev()
            .nth(n - 1)?
            .0;
        Some(if till { next_boundary(b, hit) } else { hit })
    }
}

/// First non-blank (non-space, non-tab) byte position on the line containing `line_start_pos`, or the line
/// start itself if the whole line is blank. Where `gg`/`G` land (Vim).
pub(crate) fn first_non_blank(b: &[u8], line_start_pos: usize) -> usize {
    let ls = line_start(b, line_start_pos);
    let le = line_end(b, ls);
    let mut i = ls;
    while i < le && (b[i] == b' ' || b[i] == b'\t') {
        i = next_boundary(b, i);
    }
    i
}

/// Byte start of the `n`-th line (1-based), clamped to the last line.
pub(crate) fn nth_line_start(b: &[u8], n: u32) -> usize {
    let target = n.max(1) - 1; // 0-based
    let mut seen = 0u32;
    let mut ls = 0usize;
    while seen < target {
        let le = line_end(b, ls);
        if le >= b.len() {
            break; // no more lines; clamp to the last one
        }
        ls = le + 1;
        seen += 1;
    }
    ls
}

/// Byte start of the last line.
pub(crate) fn last_line_start(b: &[u8]) -> usize {
    line_start(b, b.len())
}

/// The bracket pairs `%` matches (v0: the three ASCII pairs; `matchpairs` config is deferred).
const BRACKET_PAIRS: [(u8, u8); 3] = [(b'(', b')'), (b'[', b']'), (b'{', b'}')];

/// The matching-bracket position for `%`, or `None` if there is nothing to match. If the byte at `cursor`
/// is a bracket, its pair is found (nesting-aware, possibly on another line); otherwise the first bracket
/// forward on the current line is matched (Vim). Brackets are ASCII, so byte scanning stays on boundaries.
fn match_bracket(b: &[u8], cursor: usize) -> Option<usize> {
    // Pick the bracket to match: the one under the cursor, else the first forward on this line.
    let mut bp = cursor.min(b.len());
    if bp >= b.len() || !BRACKET_PAIRS.iter().any(|&(o, c)| b[bp] == o || b[bp] == c) {
        let le = line_end(b, cursor);
        bp = cursor.min(b.len());
        while bp < le && !BRACKET_PAIRS.iter().any(|&(o, c)| b[bp] == o || b[bp] == c) {
            bp += 1;
        }
        if bp >= le {
            return None;
        }
    }
    let ch = b[bp];
    // Opener → scan forward for the matching closer; closer → scan backward for the opener. Depth counts
    // only the same pair type, so `([)]` matches `(`↔`)` exactly as Vim does.
    if let Some(&(open, close)) = BRACKET_PAIRS.iter().find(|&&(o, _)| o == ch) {
        let mut depth = 0i32;
        for (i, &byte) in b.iter().enumerate().skip(bp) {
            if byte == open {
                depth += 1;
            } else if byte == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    } else if let Some(&(open, close)) = BRACKET_PAIRS.iter().find(|&&(_, c)| c == ch) {
        let mut depth = 0i32;
        for i in (0..=bp).rev() {
            if b[i] == close {
                depth += 1;
            } else if b[i] == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    } else {
        None
    }
}

/// The cursor target for a bare move (`w`, `3l`, `k`, …), applying `count`.
#[must_use]
pub fn target(b: &[u8], cursor: usize, m: Motion, count: u32) -> usize {
    let n = count.max(1);
    let cur0 = cursor.min(b.len());
    // Char-search resolves the count-th match directly (repeating a single `t` step would stick in place).
    if let Motion::FindChar { ch, forward, till } = m {
        return find_char_target(b, cur0, ch, forward, till, count).unwrap_or(cur0);
    }
    // Line jumps use the count as an absolute line number (or the last line), not a repeat count.
    if m == Motion::GotoLine {
        return first_non_blank(b, nth_line_start(b, count));
    }
    if m == Motion::LastLine {
        return first_non_blank(b, last_line_start(b));
    }
    // Bracket match jumps to a single computed position (count is not a repeat; `count%` is deferred).
    if m == Motion::MatchBracket {
        return match_bracket(b, cur0).unwrap_or(cur0);
    }
    let mut c = cur0;
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
            // FindChar / line-jumps are resolved by the early returns above; objects/linewise have no
            // bare-move target.
            Motion::FindChar { .. }
            | Motion::GotoLine
            | Motion::LastLine
            | Motion::MatchBracket
            | Motion::InnerWord
            | Motion::AWord
            | Motion::Line => c,
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
        // char-search: forward includes through the landing char (`dfx` incl. x, `dtx` up to x); backward
        // spans from the landing to the cursor. A missing match is a no-op range.
        Motion::FindChar { ch, forward, till } => {
            match find_char_target(b, cur, ch, forward, till, n) {
                None => (cur, cur),
                Some(t) if forward => (cur, next_boundary(b, t)),
                Some(t) => (t, cur),
            }
        }
        // bracket match: inclusive of both brackets and everything between (Vim `d%`). No match → no-op.
        Motion::MatchBracket => match match_bracket(b, cur) {
            Some(m) if m != cur => {
                let (lo, hi) = (cur.min(m), cur.max(m));
                (lo, next_boundary(b, hi))
            }
            _ => (cur, cur),
        },
        // text objects: a range around the cursor (count ignored in v0)
        Motion::InnerWord => inner_word_span(b, cur),
        Motion::AWord => a_word_span(b, cur),
        // vertical / linewise motions are not charwise — callers handle Line / line-jumps specially
        Motion::Up | Motion::Down | Motion::Line | Motion::GotoLine | Motion::LastLine => {
            (cur, cur)
        }
    }
}
