//! Sentence objects `is`/`as` and the `)`/`(` motions: nvim `findsent` (`search.c`) and `current_sent`
//! (`textobject.c`) with their helpers (`startPS`, `find_first_blank`, `findsent_forward`).

use super::{is_white, Buf};

/// Vim `startPS(lnum, NUL, false)` at a line start: an empty line or a form feed opens a paragraph. (The
/// nroff `'paragraphs'`/`'sections'` macros are not modeled, matching ruse's `{`/`}` motions.)
fn start_ps(buf: &Buf, ls: usize) -> bool {
    buf.is_nul(ls) || buf.b[ls] == 0x0C
}

const SENT_END: &[u8] = b".!?";
const SENT_CLOSE: &[u8] = b")]\"'";

/// Vim `findsent(dir, count)` on `p`: move to the start of the `count`-th next (or previous) sentence.
fn findsent(buf: &Buf, p: &mut usize, forward: bool, mut count: u32) -> bool {
    let mut pos = *p;
    let mut noskip = false;
    while count > 0 {
        count -= 1;
        let prev_pos = pos;
        match sent_scan(buf, &mut pos, forward, count) {
            SentScan::Found => {}
            SentScan::NoSkip => noskip = true,
            SentScan::Fail => return false,
        }
        // Skip blanks.
        while !noskip && is_white(buf.gchar(pos)) {
            if buf.incl(&mut pos) == -1 {
                break;
            }
        }
        if pos == prev_pos {
            // Did not move: advance one char and try again.
            if sent_step(buf, &mut pos, forward) == -1 {
                if count > 0 {
                    return false;
                }
                break;
            }
            count += 1;
        }
    }
    *p = pos;
    true
}

/// The `findsent` step in its direction: Vim `incl` forward, `decl` backward.
fn sent_step(buf: &Buf, q: &mut usize, forward: bool) -> i32 {
    if forward {
        buf.incl(q)
    } else {
        buf.decl(q)
    }
}

/// How one count of `findsent`'s scan ended (Vim's `goto found` / `return FAIL` / `noskip = true`).
enum SentScan {
    /// Reached the `found:` label: skip the blanks that follow.
    Found,
    /// Ran off the buffer on the last count: stay there, skipping no blanks.
    NoSkip,
    /// Ran off the buffer with counts left, or `)` on the last paragraph line: `findsent` fails.
    Fail,
}

/// One count of `findsent`, up to its `found:` label. `count` is the number of counts still left after this
/// one.
fn sent_scan(buf: &Buf, pos: &mut usize, forward: bool, count: u32) -> SentScan {
    if buf.gchar(*pos) == 0 {
        // On an empty line (or a NUL): skip up to a non-empty line.
        loop {
            if sent_step(buf, pos, forward) == -1 {
                break;
            }
            if buf.gchar(*pos) != 0 {
                break;
            }
        }
        if forward {
            return SentScan::Found;
        }
    } else if forward && buf.col0(*pos) && start_ps(buf, *pos) {
        if buf.is_last_line(*pos) {
            return SentScan::Fail;
        }
        *pos = buf.steps.line_end(*pos) + 1;
        return SentScan::Found;
    } else if !forward {
        buf.decl(pos);
    }
    sent_back_over_punct(buf, pos, forward);
    sent_find_end(buf, pos, forward, count)
}

/// `findsent`: back over blanks and sentence punctuation to the previous ordinary char.
fn sent_back_over_punct(buf: &Buf, pos: &mut usize, forward: bool) {
    let mut found_dot = false;
    loop {
        let c = buf.gchar(*pos);
        if !(is_white(c) || (c != 0 && (SENT_END.contains(&c) || SENT_CLOSE.contains(&c)))) {
            break;
        }
        let mut tpos = *pos;
        if buf.decl(&mut tpos) == -1 || (buf.line_of_is_empty(tpos) && forward) {
            break;
        }
        if found_dot {
            break;
        }
        if SENT_END.contains(&c) {
            found_dot = true;
        }
        let tc = buf.gchar(tpos);
        if SENT_CLOSE.contains(&c)
            && !(tc != 0 && (SENT_END.contains(&tc) || SENT_CLOSE.contains(&tc)))
        {
            break;
        }
        buf.decl(pos);
    }
}

/// `findsent`: find the end of the sentence, landing on the start of the next one.
fn sent_find_end(buf: &Buf, pos: &mut usize, forward: bool, count: u32) -> SentScan {
    let startline = buf.line_start(*pos);
    loop {
        let c = buf.gchar(*pos);
        if c == 0 || (buf.col0(*pos) && start_ps(buf, *pos)) {
            if !forward && buf.line_start(*pos) != startline {
                *pos = buf.steps.line_end(*pos) + 1;
            }
            return SentScan::Found;
        }
        if SENT_END.contains(&c) {
            let mut tpos = *pos;
            let mut r;
            loop {
                r = buf.inc(&mut tpos);
                if r == -1 {
                    break;
                }
                let tc = buf.gchar(tpos);
                if tc == 0 || !SENT_CLOSE.contains(&tc) {
                    break;
                }
            }
            let tc = buf.gchar(tpos);
            if r == -1 || is_white(tc) || tc == 0 {
                *pos = tpos;
                if buf.gchar(*pos) == 0 {
                    buf.inc(pos); // skip the NUL at the end of the line
                }
                return SentScan::Found;
            }
        }
        if sent_step(buf, pos, forward) == -1 {
            if count > 0 {
                return SentScan::Fail;
            }
            return SentScan::NoSkip;
        }
    }
}

/// Whether `count` `)`/`(` steps from `p` fail (Vim `findsent` returns FAIL: the count ran past the buffer).
pub(crate) fn sentence_motion_fails(b: &[u8], p: usize, forward: bool, count: u32) -> bool {
    let buf = Buf::new(b);
    let mut pos = p.min(buf.last);
    !findsent(&buf, &mut pos, forward, count.max(1))
}

/// The landing of the `)` / `(` motions (Vim `nv_brace` → `findsent`): an empty line is a sentence of its
/// own. A count that runs off the buffer fails and the cursor stays at `p`.
pub(crate) fn sentence_motion(b: &[u8], p: usize, forward: bool, count: u32) -> usize {
    let buf = Buf::new(b);
    let mut pos = p.min(buf.last);
    if findsent(&buf, &mut pos, forward, count.max(1)) {
        pos
    } else {
        p
    }
}

/// Vim `find_first_blank`: back to the first of the blanks before `p`.
fn find_first_blank(buf: &Buf, p: &mut usize) {
    while buf.decl(p) != -1 {
        if !is_white(buf.gchar(*p)) {
            buf.incl(p);
            break;
        }
    }
}

/// Vim `findsent_forward`: skip `count / 2` sentences and `count / 2` separating blank runs.
fn findsent_forward(buf: &Buf, p: &mut usize, mut count: u32, mut at_start_sent: bool) {
    while count > 0 {
        count -= 1;
        findsent(buf, p, true, 1);
        if at_start_sent {
            find_first_blank(buf, p);
        }
        if count == 0 || at_start_sent {
            buf.decl(p);
        }
        at_start_sent = !at_start_sent;
    }
}

/// The Visual "extend by sentences" half of Vim `current_sent` (the selection is already wider than one
/// char, or `is` would select nothing new). `cur` is the cursor after the initial forward `findsent`.
fn sent_extend(
    buf: &Buf,
    cur: &mut usize,
    mut pos: usize,
    start_pos: usize,
    vis: usize,
    mut count: u32,
    include: bool,
) {
    if start_pos < vis {
        // Cursor at the start of the Visual area: extend backward.
        let mut at_start_sent = true;
        buf.decl(&mut pos);
        while pos < *cur {
            if !is_white(buf.gchar(pos)) {
                at_start_sent = false;
                break;
            }
            buf.incl(&mut pos);
        }
        if !at_start_sent {
            findsent(buf, cur, false, 1);
            if *cur == start_pos {
                at_start_sent = true;
            } else {
                findsent(buf, cur, true, 1);
            }
        }
        if include {
            count = count.saturating_mul(2);
        }
        while count > 0 {
            count -= 1;
            if at_start_sent {
                find_first_blank(buf, cur);
            }
            let c = buf.gchar(*cur);
            if !at_start_sent || (!include && !is_white(c)) {
                findsent(buf, cur, false, 1);
            }
            at_start_sent = !at_start_sent;
        }
    } else {
        // Cursor at the end of the Visual area: extend forward.
        buf.incl(&mut pos);
        let mut at_start_sent = true;
        if pos != *cur {
            at_start_sent = false;
            while pos < *cur {
                if !is_white(buf.gchar(pos)) {
                    at_start_sent = true;
                    break;
                }
                buf.incl(&mut pos);
            }
            if at_start_sent {
                findsent(buf, cur, false, 1);
            } else {
                *cur = start_pos;
            }
        }
        if include {
            count = count.saturating_mul(2);
        }
        findsent_forward(buf, cur, count, at_start_sent);
    }
}

/// Vim `current_sent`. Returns `(start, cursor, inclusive)` like
/// [`current_word`](super::word::current_word).
pub(super) fn current_sent(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    count: u32,
    include: bool,
) -> Option<(usize, usize, bool)> {
    // Every sentence (and gap) step advances at least one char, so a count past twice the buffer length
    // only re-walks the buffer end: cap it, so a hostile `999999999das` cannot spin (#907).
    let cap = u32::try_from(buf.b.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(2)
        .saturating_add(4);
    let count = count.clamp(1, cap);
    let mut start_pos = cursor;
    let mut pos = start_pos;
    let mut cur = cursor;
    findsent(buf, &mut cur, true, 1);

    if let Some(v) = visual {
        if start_pos != v {
            sent_extend(buf, &mut cur, pos, start_pos, v, count, include);
            return Some((v, cur, true));
        }
    }

    // Starting on a blank: is it the blank run just before the next sentence?
    while is_white(buf.gchar(pos)) {
        buf.incl(&mut pos);
    }
    let start_blank = pos == cur;
    if start_blank {
        find_first_blank(buf, &mut start_pos);
    } else {
        findsent(buf, &mut cur, false, 1);
        start_pos = cur;
    }
    let ncount = if include {
        count.saturating_mul(2)
    } else {
        count - u32::from(start_blank)
    };
    if ncount > 0 {
        findsent_forward(buf, &mut cur, ncount, true);
    } else {
        buf.decl(&mut cur);
    }

    if include {
        // With the leading blanks included, leave out the trailing ones; with no trailing blanks, take the
        // leading ones.
        if start_blank {
            find_first_blank(buf, &mut cur);
            if is_white(buf.gchar(cur)) {
                buf.decl(&mut cur);
            }
        } else if !is_white(buf.gchar(cur)) {
            find_first_blank(buf, &mut start_pos);
        }
    }

    if let Some(v) = visual {
        if start_pos == cur {
            // `is` on a single blank before a sentence: extend instead of getting stuck.
            sent_extend(buf, &mut cur, pos, start_pos, v, count, include);
            return Some((v, cur, true));
        }
        return Some((start_pos, cur, true));
    }
    // Take the line break after the sentence, when there is one.
    let inclusive = buf.incl(&mut cur) == -1;
    Some((start_pos, cur, inclusive))
}
