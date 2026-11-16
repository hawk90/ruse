//! Paragraph (`{` `}` `ip` `ap`) and section (`[[` `]]` `[]` `][`) geometry.

use crate::pos::raw::{line_end, line_start};

use super::boundary::prev_grapheme;
use super::line::{content_end, last_line_start};

/// Whether the line starting at `ls` is blank — empty or (a start pointing at) a newline / end of buffer.
/// Paragraph boundaries in Vim are truly-empty lines (a whitespace-only line is *not* a boundary).
fn is_blank_line(b: &[u8], ls: usize) -> bool {
    ls >= b.len() || b[ls] == b'\n'
}

/// The starts of the buffer's lines in Vim's model (the slot after a final newline is not a line).
fn line_starts(b: &[u8]) -> Vec<usize> {
    let end = content_end(b);
    let mut v = vec![0];
    v.extend(
        b.iter()
            .enumerate()
            .filter(|&(i, &c)| c == b'\n' && i < end)
            .map(|(i, _)| i + 1),
    );
    v
}

/// The 0-based line of `pos` among `starts`.
fn line_of_starts(starts: &[usize], pos: usize) -> usize {
    starts.partition_point(|&s| s <= pos).saturating_sub(1)
}

/// The paragraph object (`ip` / `ap`) with `count`, as Vim's `current_par` resolves it for an operator: the
/// 0-based first and last line, or `None` when it fails (`count` paragraphs do not fit before the end, or
/// `ap` starts on blank lines with no paragraph after them, #1089). A white line holds only blanks; a
/// non-white run also stops at a line starting with a form feed (`startPS`).
fn paragraph_lines(b: &[u8], cur: usize, count: u32, include: bool) -> Option<(usize, usize)> {
    let starts = line_starts(b);
    let last = starts.len() - 1;
    let white = |l: usize| {
        let ls = starts[l];
        b[ls..line_end(b, ls)]
            .iter()
            .all(|&c| c == b' ' || c == b'\t')
    };
    let start_ps = |l: usize| {
        let ls = starts[l];
        ls >= b.len() || b[ls] == b'\n' || b[ls] == 0x0C
    };
    let mut start = line_of_starts(&starts, cur);
    let white_in_front = white(start);
    while start > 0 {
        if white_in_front {
            if !white(start - 1) {
                break;
            }
        } else if white(start - 1) || start_ps(start) {
            break;
        }
        start -= 1;
    }
    // `next` is the first line not taken yet (Vim's `end_lnum + 1`): past the white lines at the start.
    let mut next = start;
    while next <= last && white(next) {
        next += 1;
    }
    let mut i = count.max(1);
    if !include && white_in_front {
        i -= 1;
    }
    while i > 0 {
        i -= 1;
        if next > last {
            return None;
        }
        let do_white = !include && white(next);
        if include || !do_white {
            next += 1;
            while next <= last && !white(next) && !start_ps(next) {
                next += 1;
            }
        }
        if i == 0 && white_in_front && include {
            break;
        }
        if include || do_white {
            while next <= last && white(next) {
                next += 1;
            }
        }
    }
    let end = next - 1;
    // `ap` with no white lines after it takes the ones before it.
    if !white_in_front && !white(end) && include {
        while start > 0 && white(start - 1) {
            start -= 1;
        }
    }
    Some((start, end))
}

/// The paragraph object as a linewise byte range `[s, e)` covering whole lines ([`paragraph_lines`]); a
/// failed object is the empty range at the cursor.
pub(super) fn paragraph_span(b: &[u8], cur: usize, count: u32, around: bool) -> (usize, usize) {
    if b.is_empty() {
        return (0, 0);
    }
    let Some((first, last)) = paragraph_lines(b, cur, count, around) else {
        return (cur, cur);
    };
    let starts = line_starts(b);
    let le = line_end(b, starts[last]);
    (starts[first], if le < b.len() { le + 1 } else { le })
}

/// Whether the paragraph object fails ([`paragraph_lines`]).
pub(super) fn paragraph_fails(b: &[u8], cur: usize, count: u32, around: bool) -> bool {
    !b.is_empty() && paragraph_lines(b, cur, count, around).is_none()
}

/// One `}` step (Vim `findpar`): the start of the next blank line after the cursor's line that follows a
/// non-blank one — from a blank line the run of blanks is skipped first (#748) — or `None` when the scan
/// runs off the last line.
fn next_para_boundary(b: &[u8], pos: usize) -> Option<usize> {
    let mut ls = line_start(b, pos);
    let mut did_skip = false;
    loop {
        if !is_blank_line(b, ls) {
            did_skip = true;
        }
        let le = line_end(b, ls);
        if le + 1 >= b.len() {
            return None;
        }
        ls = le + 1;
        if did_skip && is_blank_line(b, ls) {
            return Some(ls);
        }
    }
}

/// One `{` step: the start of the previous blank line before the cursor's line that precedes a non-blank
/// one (the mirror of [`next_para_boundary`]: a run of blanks above the cursor is skipped first), or `None`
/// when the scan runs off the first line.
fn prev_para_boundary(b: &[u8], pos: usize) -> Option<usize> {
    let mut ls = line_start(b, pos);
    let mut did_skip = !is_blank_line(b, ls);
    while ls > 0 {
        let prev_ls = line_start(b, ls - 1);
        if did_skip && is_blank_line(b, prev_ls) {
            return Some(prev_ls);
        }
        if !is_blank_line(b, prev_ls) {
            did_skip = true;
        }
        ls = prev_ls;
    }
    None
}

/// `{count}}` / `{count}{`: `count` paragraph steps. A step running off the buffer lands on its edge —
/// for `}` the LAST CHAR of the last line (never its newline or the phantom slot after it, #677; Vim's `}`
/// turns inclusive there), for `{` the buffer start — but only as the final step: with count left over the
/// motion fails and stays put (Vim `findpar` returns false, so `d2}` over one paragraph is a no-op).
/// Returns the landing and whether it is that end-of-buffer edge, or `None` when the motion fails.
pub(super) fn paragraph_landing(
    b: &[u8],
    cur: usize,
    forward: bool,
    n: u32,
) -> Option<(usize, bool)> {
    let mut c = cur;
    for k in 0..n {
        let step = if forward {
            next_para_boundary(b, c)
        } else {
            prev_para_boundary(b, c)
        };
        match step {
            Some(t) => c = t,
            None if k + 1 < n => return None,
            None if forward => {
                let le = content_end(b); // the last line's end
                let ls = line_start(b, le);
                return Some((if le > ls { prev_grapheme(b, le) } else { le }, true));
            }
            None => return Some((0, true)),
        }
    }
    Some((c, false))
}

pub(super) fn paragraph_target(b: &[u8], cur: usize, forward: bool, n: u32) -> usize {
    paragraph_landing(b, cur, forward, n).map_or(cur, |(t, _)| t)
}

/// Whether the line starting at `ls` is a SECTION boundary line. A form-feed (`\x0C`) in column 0 is a
/// boundary for both the start (`]]`/`[[`) and end (`][`/`[]`) motions; otherwise the first byte must be the
/// brace for the direction — `{` for section starts, `}` for section ends. (Vim's default: `{`/`}` in the
/// first column plus form-feed; the nroff `sections` macros are a documented follow-up.)
fn is_section_line(b: &[u8], ls: usize, ends: bool) -> bool {
    match b.get(ls) {
        Some(&0x0C) => true,
        Some(&c) => c == if ends { b'}' } else { b'{' },
        None => false,
    }
}

/// The line start of the NEXT section boundary strictly after `ls`, or `None` when the run reaches the last
/// content line without finding one (the caller clamps to the last line, matching nvim's EOF landing).
fn scan_section_fwd(b: &[u8], ls: usize, ends: bool) -> Option<usize> {
    let mut cur = ls;
    loop {
        let le = line_end(b, cur);
        if le >= content_end(b) {
            return None; // `cur` is the last content line — no next line to test
        }
        cur = le + 1;
        if is_section_line(b, cur, ends) {
            return Some(cur);
        }
    }
}

/// The line start of the PREVIOUS section boundary strictly before `ls`, or `None` when none exists (the
/// caller clamps to the first line).
fn scan_section_back(b: &[u8], ls: usize, ends: bool) -> Option<usize> {
    let mut cur = ls;
    while cur > 0 {
        cur = line_start(b, cur - 1);
        if is_section_line(b, cur, ends) {
            return Some(cur);
        }
    }
    None
}

/// The `count`-th section boundary line-start forward (`]]`/`][`), clamping to the last content line when the
/// sections run out (nvim rests the cursor on the last line at that point).
pub(super) fn next_section_line(b: &[u8], pos: usize, count: u32, ends: bool) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..count.max(1) {
        match scan_section_fwd(b, ls, ends) {
            Some(f) => ls = f,
            None => return last_line_start(b),
        }
    }
    ls
}

/// The `count`-th section boundary line-start backward (`[[`/`[]`), clamping to the first line.
pub(super) fn prev_section_line(b: &[u8], pos: usize, count: u32, ends: bool) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..count.max(1) {
        match scan_section_back(b, ls, ends) {
            Some(f) => ls = f,
            None => return 0,
        }
    }
    ls
}
