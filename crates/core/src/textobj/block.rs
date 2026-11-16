//! Bracket-block objects `i(`/`a(` (and `{`/`[`/`<`): nvim `current_block` (`textobject.c`) over a
//! `findmatchlimit` (`search.c`) reduced to one bracket pair.

use super::Buf;

/// Whether the bracket at `p` is escaped by an odd number of backslashes on its line (Vim skips those when
/// `cpo` lacks `M`).
fn escaped(buf: &Buf, p: usize) -> bool {
    let ls = buf.line_start(p);
    let mut n = 0;
    let mut i = p;
    while i > ls && buf.b[i - 1] == b'\\' {
        n += 1;
        i -= 1;
    }
    n % 2 == 1
}

/// Vim `findmatchlimit` for a bracket pair, scanning from (not including) `from`. `forward == false` finds
/// the unmatched `open` before `from`; `forward == true` finds the unmatched `close` after it. `seek_open`
/// (forward only) instead looks for the next `open` that is not preceded by an unmatched `close`.
fn find_bracket(
    buf: &Buf,
    from: usize,
    open: u8,
    close: u8,
    forward: bool,
    seek_open: bool,
) -> Option<usize> {
    let b = buf.b;
    let (find, other) = if forward && !seek_open {
        (close, open)
    } else {
        (open, close)
    };
    let mut depth = 0u32;
    let mut i = from;
    loop {
        if forward {
            i += 1;
            if i >= b.len() {
                return None;
            }
        } else {
            if i == 0 {
                return None;
            }
            i -= 1;
        }
        let c = b[i];
        if (c == find || c == other) && !escaped(buf, i) {
            if c == other {
                depth += 1;
            } else if depth == 0 {
                return Some(i);
            } else {
                depth -= 1;
            }
        }
    }
}

/// Vim `current_block`. Returns `(start, cursor, inclusive)` like
/// [`current_word`](super::word::current_word).
pub(super) fn current_block(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    count: u32,
    include: bool,
    open: u8,
    close: u8,
) -> Option<(usize, usize, bool)> {
    let (cur, old_start, old_end) = block_search_from(buf, cursor, visual, open);
    let mut start_pos = block_opener(buf, cur, open, close, count)?;
    let mut end_pos = find_bracket(buf, start_pos, open, close, true, false)?;
    let mut cur = end_pos;

    // `i(`: leave out the brackets; a closer preceded only by indent also leaves out that indent. In Visual,
    // a result no bigger than the current area grows to the enclosing block.
    let mut sol = false;
    if !include {
        loop {
            buf.incl(&mut start_pos);
            sol = buf.col0(cur);
            buf.decl(&mut cur);
            while buf.inindent(cur, 1) {
                sol = true;
                if buf.decl(&mut cur) != 0 {
                    break;
                }
            }
            if visual.is_some() && start_pos == end_pos {
                return None; // no inner block to select
            }
            if visual.is_some() && start_pos >= old_start && old_end >= cur && start_pos != cur {
                cur = old_start;
                buf.decl(&mut cur);
                start_pos = find_bracket(buf, cur, open, close, false, false)?;
                end_pos = find_bracket(buf, start_pos, open, close, true, false)?;
                cur = end_pos;
            } else {
                break;
            }
        }
    }

    if visual.is_some() {
        if sol && buf.gchar(cur) != 0 {
            buf.inc(&mut cur); // include the line break
        }
        return Some((start_pos, cur, true));
    }
    if sol {
        buf.incl(&mut cur);
        Some((start_pos, cur, false))
    } else if start_pos <= cur {
        Some((start_pos, cur, true))
    } else {
        // Nothing between the brackets: operate on no text, at the inner start.
        Some((start_pos, start_pos, false))
    }
}

/// `current_block`'s search start: `(cur, old_start, old_end)`. A Visual area wider than one char searches
/// from its low end and remembers its bounds; otherwise the cursor, moved past a `{` indent and an opener
/// under it.
fn block_search_from(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    open: u8,
) -> (usize, usize, usize) {
    let mut cur = cursor;
    let mut old_start = cursor;
    let mut old_end = cursor;
    match visual {
        Some(v) if v != cur => {
            if v < cur {
                old_start = v;
                cur = v; // search from the low end of the Visual area
            } else {
                old_end = v;
            }
        }
        _ => {
            if open == b'{' {
                // `{` ignores the indent: a cursor in the leading blanks uses the line's first brace.
                while buf.inindent(cur, 1) {
                    if buf.inc(&mut cur) != 0 {
                        break;
                    }
                }
            }
            if buf.gchar(cur) == open {
                cur += 1; // on the opener: search from just after it, so it is the first match
            }
        }
    }
    (cur, old_start, old_end)
}

/// `current_block`: the `count`-th enclosing opener, or — when the cursor is in no block — the `count`-th
/// next one.
fn block_opener(buf: &Buf, mut cur: usize, open: u8, close: u8, count: u32) -> Option<usize> {
    let mut count = count.max(1);
    let enclosed = find_bracket(buf, cur, open, close, false, false).is_some();
    let mut start_pos = None;
    while count > 0 {
        count -= 1;
        let found = if enclosed {
            find_bracket(buf, cur, open, close, false, false)
        } else {
            find_bracket(buf, cur, open, close, true, true)
        };
        match found {
            Some(p) => {
                cur = p;
                start_pos = Some(p);
            }
            None => {
                start_pos = None;
                break;
            }
        }
    }
    start_pos
}
