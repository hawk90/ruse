//! Quote objects `i"`/`a"` (and `'`/`` ` ``): nvim `current_quote` (`textobject.c`) with
//! `find_next_quote` / `find_prev_quote`, confined to the cursor's line.

use super::{is_white, Buf};
use crate::motion;

/// Vim's `'quoteescape'` default: the character that escapes a quote inside a quoted string.
const QUOTE_ESCAPE: u8 = b'\\';

/// Vim `find_next_quote` on the line `[ls, le)`: the first unescaped `q` at or after `col` (absolute
/// offsets), honouring `escape` only when `escaped` is set.
fn find_next_quote(b: &[u8], le: usize, mut col: usize, q: u8, escape: bool) -> Option<usize> {
    loop {
        if col >= le {
            return None;
        }
        let c = b[col];
        if escape && c == QUOTE_ESCAPE {
            col += 1;
            if col >= le {
                return None;
            }
        } else if c == q {
            return Some(col);
        }
        col = motion::next_boundary(b, col);
    }
}

/// Vim `find_prev_quote`: the last unescaped `q` before `col` on the line starting at `ls`, or `ls` when
/// there is none (the caller re-checks the byte, as Vim does).
fn find_prev_quote(b: &[u8], ls: usize, mut col: usize, q: u8, escape: bool) -> usize {
    while col > ls {
        col = motion::prev_boundary(b, col);
        let mut n = 0;
        if escape {
            while col - n > ls && b[col - n - 1] == QUOTE_ESCAPE {
                n += 1;
            }
        }
        if n % 2 == 1 {
            col -= n; // an odd run of escapes: the char is escaped, skip it
        } else if b[col] == q {
            break;
        }
    }
    col
}

/// The cursor's line `[ls, le)` a quote object is confined to.
#[derive(Clone, Copy)]
struct QuoteLine<'a> {
    b: &'a [u8],
    ls: usize,
    le: usize,
}

impl QuoteLine<'_> {
    /// The byte at `i`, `0` at or past the line end (Vim's `line[col]` reading the `NUL`).
    fn at(&self, i: usize) -> u8 {
        if i < self.le {
            self.b[i]
        } else {
            0
        }
    }
}

/// What `current_quote` learns from the Visual area before it searches (`VIsual` is `v`).
struct QuoteSel {
    v: usize,
    vis_empty: bool,
    vis_bef_curs: bool,
    inside_quotes: bool,
    selected_quote: bool,
}

/// Vim `current_quote`, confined to the cursor's line. Returns `(start, cursor, inclusive)` like
/// [`current_word`](super::word::current_word).
pub(super) fn current_quote(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    count: u32,
    include: bool,
    q: u8,
) -> Option<(usize, usize, bool)> {
    let b = buf.b;
    let ls = buf.line_start(cursor);
    let le = motion::line_end(b, cursor).min(buf.last);
    let line = QuoteLine { b, ls, le };
    let at = |i: usize| line.at(i);
    let mut vis = visual;
    let sel = quote_sel(buf, &line, cursor, visual, q)?;
    let (mut col_start, mut col_end) = quote_pair(&line, cursor, &sel, q)?;
    let QuoteSel {
        v,
        vis_empty,
        vis_bef_curs,
        inside_quotes,
        selected_quote,
    } = sel;

    if include {
        // `a"`: the blanks after the closing quote, else those before the opening one.
        if is_white(at(col_end + 1)) {
            while is_white(at(col_end + 1)) {
                col_end += 1;
            }
        } else {
            while col_start > ls && is_white(b[col_start - 1]) {
                col_start -= 1;
            }
        }
    }

    // After `vi"` another `i"` includes the quotes; so does `v2i"` / `d2i"`.
    if !include && count < 2 && (vis_empty || !inside_quotes) {
        col_start += 1;
    }
    let mut cur = col_start;
    if vis.is_some()
        && (vis_empty
            || (vis_bef_curs
                && !selected_quote
                && (inside_quotes || (at(v) != q && (v == ls || b[v - 1] != q)))))
    {
        vis = Some(cur);
    }
    let start = cur;

    cur = col_end;
    let mut inclusive = false;
    if (include || count > 1 || (!vis_empty && inside_quotes)) && buf.inc(&mut cur) == 2 {
        inclusive = true;
    }
    if let Some(va) = vis {
        if vis_empty || vis_bef_curs {
            buf.dec(&mut cur);
        } else {
            let mut va = va;
            if inside_quotes || (!selected_quote && at(v) != q && (at(v) == 0 || at(v + 1) != q)) {
                buf.dec(&mut cur);
                va = cur;
            }
            cur = col_start;
            return Some((va, cur, true));
        }
        return Some((va, cur, true));
    }
    Some((start, cur, inclusive))
}

/// `current_quote`'s look at the Visual area `vis`..`cur`; `None` when it leaves the cursor's line.
fn quote_sel(
    buf: &Buf,
    line: &QuoteLine,
    cur: usize,
    vis: Option<usize>,
    q: u8,
) -> Option<QuoteSel> {
    let QuoteLine { b, ls, .. } = *line;
    let at = |i: usize| line.at(i);
    let mut vis_empty = true;
    let mut vis_bef_curs = false;
    let mut inside_quotes = false;
    let mut selected_quote = false;

    if let Some(v) = vis {
        if buf.line_start(v) != ls {
            return None; // quote objects work within one line
        }
        vis_bef_curs = v < cur;
        vis_empty = v == cur;
    }
    let v = vis.unwrap_or(cur);
    if !vis_empty {
        // Does the selection exactly span the text inside quotes (an `i"` done before)?
        let (lo, hi) = if vis_bef_curs { (v, cur) } else { (cur, v) };
        inside_quotes = lo > ls && b[lo - 1] == q && at(hi) != 0 && at(hi + 1) == q;
        let mut i = lo;
        while i <= hi {
            if at(i) == 0 {
                break;
            }
            if b[i] == q {
                selected_quote = true;
                break;
            }
            i += 1;
        }
    }
    Some(QuoteSel {
        v,
        vis_empty,
        vis_bef_curs,
        inside_quotes,
        selected_quote,
    })
}

/// `current_quote`: the `(opening, closing)` quote columns of the string the object takes, searched from
/// `cursor`; `None` when the line has no such pair.
fn quote_pair(line: &QuoteLine, cursor: usize, sel: &QuoteSel, q: u8) -> Option<(usize, usize)> {
    let QuoteLine { b, ls, le } = *line;
    let at = |i: usize| line.at(i);
    let (cur, col_start) = (cursor, cursor);
    let (vis_empty, vis_bef_curs) = (sel.vis_empty, sel.vis_bef_curs);
    let pair = if !vis_empty && at(col_start) == q {
        // Already selecting and on a quote: take the next quoted string.
        if vis_bef_curs {
            let s = find_next_quote(b, le, col_start + 1, q, false)?;
            match find_next_quote(b, le, s + 1, q, true) {
                Some(e) => (s, e),
                None => (cur, s), // on an opening quote after all
            }
        } else {
            let e = find_prev_quote(b, ls, col_start, q, false);
            if at(e) != q {
                return None;
            }
            let s = find_prev_quote(b, ls, e, q, true);
            if at(s) != q {
                (e, cur) // on a closing quote after all
            } else {
                (s, e)
            }
        }
    } else if at(col_start) == q || !vis_empty {
        let first_col = if vis_empty {
            col_start
        } else if vis_bef_curs {
            find_next_quote(b, le, col_start, q, false)?
        } else {
            find_prev_quote(b, ls, col_start, q, false)
        };
        // On a quote (opening or closing?): pair the quotes up from the line start to find out.
        let mut s = ls;
        loop {
            let open = find_next_quote(b, le, s, q, false)?;
            if open > first_col {
                return None;
            }
            let close = find_next_quote(b, le, open + 1, q, true)?;
            if open <= first_col && first_col <= close {
                break (open, close);
            }
            s = close + 1;
        }
    } else {
        // Back to an opening quote, else forward to the first quote after the cursor.
        let mut s = find_prev_quote(b, ls, col_start, q, true);
        if at(s) != q {
            s = find_next_quote(b, le, s, q, false)?;
        }
        (s, find_next_quote(b, le, s + 1, q, true)?)
    };
    Some(pair)
}
