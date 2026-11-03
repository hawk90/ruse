//! Vim's character-wise text objects — `iw`/`aw`/`iW`/`aW`, `is`/`as`, the bracket blocks `i(`/`a(` (and
//! `{`/`[`/`<`), and the quotes `i"`/`a"` (and `'`/`` ` ``) — for both operator-pending and Visual mode.
//!
//! Each object is a port of the corresponding Neovim v0.12 routine (`current_word`, `current_sent`,
//! `current_block`, `current_quote` in `textobject.c`) onto ruse's flat byte buffer. The port keeps Vim's
//! cursor model rather than re-deriving the geometry, because the corner cases (#740, #751, #752, #753,
//! #703) all live in how Vim's position primitives step over a line break:
//!
//! - A position is a byte offset. A line's `NUL` slot (Vim's `col == STRLEN`) is the offset of its `\n`,
//!   or the buffer's content end for the last line. The terminating `\n` of a newline-ended buffer is the
//!   last line's terminator, not a phantom empty line (see [`motion::content_end`]).
//! - [`Buf::inc`] / [`Buf::dec`] / [`Buf::incl`] / [`Buf::decl`] are Vim's `inc`/`dec`/`incl`/`decl`,
//!   with the same return codes (`0` moved within the line, `2` landed on the `NUL`, `1` crossed a line,
//!   `-1` at a buffer edge).
//! - Character classes are Vim's (`cls()` over [`crate::charclass::vim_class`], #1136), the same ones the
//!   `w`/`b`/`e` motions use, and a step crosses one grapheme cluster, as nvim's `inc()`/`dec()` do.
//!
//! An operator result is Vim's `(oap->start, cursor, oap->inclusive)`; [`op_span`] turns it into a byte
//! range and applies the exclusive-linewise rule. A Visual result is the new `(VIsual, cursor)` pair.
//! Bracket matching ignores quotes, as Vim's text objects do (`cpo` is forced to `%` there), but skips a
//! bracket escaped by an odd number of backslashes. Quote objects honour the default `quoteescape` (`\`).

use crate::motion::{self, Motion};

/// Vim's `'quoteescape'` default: the character that escapes a quote inside a quoted string.
const QUOTE_ESCAPE: u8 = b'\\';

/// The buffer seen through Vim's position primitives. `last` is the `NUL` slot of the last line.
struct Buf<'a> {
    b: &'a [u8],
    last: usize,
}

impl<'a> Buf<'a> {
    fn new(b: &'a [u8]) -> Self {
        Buf {
            b,
            last: motion::content_end(b),
        }
    }

    /// Whether `p` is a line's `NUL` slot (its newline, or the end of the last line).
    fn is_nul(&self, p: usize) -> bool {
        p >= self.last || self.b[p] == b'\n'
    }

    /// The byte under `p`, `0` on a `NUL` slot. Non-ASCII chars report their lead byte (`>= 0x80`), which is
    /// never one of the ASCII delimiters or blanks the objects test for.
    fn gchar(&self, p: usize) -> u8 {
        if self.is_nul(p) {
            0
        } else {
            self.b[p]
        }
    }

    fn col0(&self, p: usize) -> bool {
        p == 0 || self.b[p - 1] == b'\n'
    }

    fn line_start(&self, p: usize) -> usize {
        motion::line_start(self.b, p)
    }

    /// `p` sits at column 0 of an empty line.
    fn line_empty(&self, p: usize) -> bool {
        self.col0(p) && self.is_nul(p)
    }

    /// Whether the line holding `p` is an empty line (Vim `LINEEMPTY(lnum)`).
    fn line_of_is_empty(&self, p: usize) -> bool {
        self.line_empty(self.line_start(p))
    }

    fn is_last_line(&self, p: usize) -> bool {
        motion::line_end(self.b, p) >= self.last
    }

    /// Vim `inc`: one char forward; `2` when that lands on the `NUL`, `1` when it crosses into the next
    /// line, `-1` (no move) on the last line's `NUL`.
    fn inc(&self, p: &mut usize) -> i32 {
        if !self.is_nul(*p) {
            let q = motion::next_grapheme(self.b, *p);
            *p = q;
            return if self.is_nul(q) { 2 } else { 0 };
        }
        if *p >= self.last {
            return -1;
        }
        *p += 1;
        1
    }

    /// Vim `dec`: one char back; at column 0 it moves to the previous line's `NUL` (`1`), `-1` at offset 0.
    fn dec(&self, p: &mut usize) -> i32 {
        if !self.col0(*p) {
            *p = motion::prev_grapheme(self.b, *p);
            return 0;
        }
        if *p == 0 {
            return -1;
        }
        *p -= 1;
        1
    }

    /// Vim `incl`: [`Buf::inc`], skipping over a `NUL` it lands on.
    fn incl(&self, p: &mut usize) -> i32 {
        let r = self.inc(p);
        if r >= 1 && !self.col0(*p) {
            return self.inc(p);
        }
        r
    }

    /// Vim `decl`: [`Buf::dec`], skipping over the `NUL` of the line it crosses back into.
    fn decl(&self, p: &mut usize) -> i32 {
        let r = self.dec(p);
        if r == 1 && !self.col0(*p) {
            return self.dec(p);
        }
        r
    }

    /// Vim `oneleft`: one char left within the line; `false` at column 0.
    fn oneleft(&self, p: &mut usize) -> bool {
        if self.col0(*p) {
            return false;
        }
        *p = motion::prev_grapheme(self.b, *p);
        true
    }

    /// Vim `inindent(extra)`: the line's leading blanks reach at least `col(p) + extra`.
    fn inindent(&self, p: usize, extra: usize) -> bool {
        let ls = self.line_start(p);
        let mut i = ls;
        while i < self.b.len() && is_white(self.b[i]) {
            i += 1;
        }
        i - ls >= p - ls + extra
    }

    /// Vim `cls()`: `0` for a blank or `NUL`, else the char's Vim class; for WORD objects (`big`) every
    /// non-blank is class `1`.
    fn cls(&self, p: usize, big: bool) -> u32 {
        if self.is_nul(p) {
            0
        } else {
            motion::char_cls(self.b, p, big)
        }
    }
}

fn is_white(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

// --- words (`iw`/`aw`/`iW`/`aW`) ----------------------------------------------------------------------

/// Vim `fwd_word` for one word. `eol` stops at the end of the line (operator use). `false` = buffer end.
fn fwd_word(buf: &Buf, p: &mut usize, big: bool, eol: bool) -> bool {
    let sclass = buf.cls(*p, big);
    let last_line = buf.is_last_line(*p);
    let i = buf.inc(p);
    if i == -1 || (i >= 1 && last_line) {
        return false;
    }
    if i >= 1 && eol {
        return true;
    }
    if sclass != 0 {
        while buf.cls(*p, big) == sclass {
            let i = buf.inc(p);
            if i == -1 || (i >= 1 && eol) {
                return true;
            }
        }
    }
    while buf.cls(*p, big) == 0 {
        if buf.line_empty(*p) {
            break;
        }
        let i = buf.inc(p);
        if i == -1 || (i >= 1 && eol) {
            return true;
        }
    }
    true
}

/// Vim `bck_word` for one word, with `stop` (an already-at-start cursor moves one word less).
fn bck_word(buf: &Buf, p: &mut usize, big: bool, stop: bool) -> bool {
    let sclass = buf.cls(*p, big);
    if buf.dec(p) == -1 {
        return false;
    }
    if !stop || sclass == buf.cls(*p, big) || sclass == 0 {
        while buf.cls(*p, big) == 0 {
            if buf.line_empty(*p) {
                return true;
            }
            if buf.dec(p) == -1 {
                return true;
            }
        }
        if skip_chars(buf, p, buf.cls(*p, big), big, false) {
            return true;
        }
    }
    buf.inc(p);
    true
}

/// Vim `end_word` for one word. `stop`: already on a word end moves one word less; `empty`: stop on an
/// empty line. `false` = buffer end.
fn end_word(buf: &Buf, p: &mut usize, big: bool, stop: bool, empty: bool) -> bool {
    let sclass = buf.cls(*p, big);
    if buf.inc(p) == -1 {
        return false;
    }
    if buf.cls(*p, big) == sclass && sclass != 0 {
        if skip_chars(buf, p, sclass, big, true) {
            return false;
        }
    } else if !stop || sclass == 0 {
        while buf.cls(*p, big) == 0 {
            if empty && buf.line_empty(*p) {
                return true;
            }
            if buf.inc(p) == -1 {
                return false;
            }
        }
        if skip_chars(buf, p, buf.cls(*p, big), big, true) {
            return false;
        }
    }
    buf.dec(p);
    true
}

/// Vim `bckend_word` for one word; `eol` stops at a line break.
fn bckend_word(buf: &Buf, p: &mut usize, big: bool, eol: bool) -> bool {
    let sclass = buf.cls(*p, big);
    let i = buf.dec(p);
    if i == -1 {
        return false;
    }
    if eol && i == 1 {
        return true;
    }
    if sclass != 0 {
        while buf.cls(*p, big) == sclass {
            let i = buf.dec(p);
            if i == -1 || (eol && i == 1) {
                return true;
            }
        }
    }
    while buf.cls(*p, big) == 0 {
        if buf.line_empty(*p) {
            break;
        }
        let i = buf.dec(p);
        if i == -1 || (eol && i == 1) {
            return true;
        }
    }
    true
}

/// Vim `skip_chars`: step over a run of class `cclass`; `true` when the buffer edge stops it.
fn skip_chars(buf: &Buf, p: &mut usize, cclass: u32, big: bool, forward: bool) -> bool {
    while buf.cls(*p, big) == cclass {
        let r = if forward { buf.inc(p) } else { buf.dec(p) };
        if r == -1 {
            return true;
        }
    }
    false
}

/// Vim `back_in_line`: back to the start of the word or blank run under `p`, within the line.
fn back_in_line(buf: &Buf, p: &mut usize, big: bool) {
    let sclass = buf.cls(*p, big);
    loop {
        if buf.col0(*p) {
            break;
        }
        buf.dec(p);
        if buf.cls(*p, big) != sclass {
            buf.inc(p);
            break;
        }
    }
}

/// Vim `current_word`. `visual` is the Visual anchor (`VIsual`) when a selection is active. Returns
/// `(start, cursor, inclusive)`: for an operator the span start, its end and whether the end char is
/// included; in Visual the new anchor and cursor.
fn current_word(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    count: u32,
    include: bool,
    big: bool,
) -> Option<(usize, usize, bool)> {
    current_word_at(buf, cursor, visual, count, include, big).ok()
}

/// [`current_word`], reporting a failure with where it left the cursor: Vim moves `curwin->w_cursor` while
/// it scans and does not put it back, so `daw` on a blank line rests at the blanks' end (#1089).
fn current_word_at(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    count: u32,
    include: bool,
    big: bool,
) -> Result<(usize, usize, bool), usize> {
    let mut cur = cursor;
    let mut start = cursor;
    let mut vis = visual;
    let mut inclusive = true;
    let mut include_white = false;
    let mut count = count.max(1);

    if vis.is_none() || vis == Some(cur) {
        back_in_line(buf, &mut cur, big);
        start = cur;
        if (buf.cls(cur, big) == 0) == include {
            if !end_word(buf, &mut cur, big, true, true) {
                return Err(cur);
            }
        } else {
            // A word with trailing blanks ("word   "), or a blank run without them: go to the next word
            // start and back up one — over the line break when that start is the next line's first char.
            fwd_word(buf, &mut cur, big, true);
            if buf.col0(cur) {
                buf.decl(&mut cur);
            } else {
                buf.oneleft(&mut cur);
            }
            if include {
                include_white = true;
            }
        }
        if vis.is_some() {
            vis = Some(start);
        }
        count -= 1;
    }

    // A remaining count (or a Visual area wider than one char) extends by more words / blank runs.
    while count > 0 {
        inclusive = true;
        if vis.is_some_and(|v| cur < v) {
            if buf.decl(&mut cur) == -1 {
                return Err(cur);
            }
            if include != (buf.cls(cur, big) != 0) {
                if !bck_word(buf, &mut cur, big, true) {
                    return Err(cur);
                }
            } else {
                if !bckend_word(buf, &mut cur, big, true) {
                    return Err(cur);
                }
                buf.incl(&mut cur);
            }
        } else {
            if buf.incl(&mut cur) == -1 {
                return Err(cur);
            }
            if include != (buf.cls(cur, big) == 0) {
                if !fwd_word(buf, &mut cur, big, true) && count > 1 {
                    return Err(cur);
                }
                // Ending just past a line break must not take the next line's first char.
                if !buf.oneleft(&mut cur) {
                    inclusive = false;
                }
            } else if !end_word(buf, &mut cur, big, true, true) {
                return Err(cur);
            }
        }
        count -= 1;
    }

    // `aw` without trailing blanks takes the leading ones instead (not the indent at the line start).
    if include_white && (buf.cls(cur, big) != 0 || (buf.col0(cur) && !inclusive)) {
        let mut p = start;
        if buf.oneleft(&mut p) {
            back_in_line(buf, &mut p, big);
            if buf.cls(p, big) == 0 && !buf.col0(p) {
                if vis.is_some() {
                    vis = Some(p);
                } else {
                    start = p;
                }
            }
        }
    }

    Ok((vis.unwrap_or(start), cur, inclusive))
}

// --- sentences (`is`/`as`) ----------------------------------------------------------------------------

/// Vim `startPS(lnum, NUL, false)` at a line start: an empty line or a form feed opens a paragraph. (The
/// nroff `'paragraphs'`/`'sections'` macros are not modeled, matching ruse's `{`/`}` motions.)
fn start_ps(buf: &Buf, ls: usize) -> bool {
    buf.is_nul(ls) || buf.b[ls] == 0x0C
}

const SENT_END: &[u8] = b".!?";
const SENT_CLOSE: &[u8] = b")]\"'";

/// Vim `findsent(dir, count)` on `p`: move to the start of the `count`-th next (or previous) sentence.
fn findsent(buf: &Buf, p: &mut usize, forward: bool, mut count: u32) -> bool {
    let step = |q: &mut usize| if forward { buf.incl(q) } else { buf.decl(q) };
    let mut pos = *p;
    let mut noskip = false;
    while count > 0 {
        count -= 1;
        let prev_pos = pos;
        'found: {
            if buf.gchar(pos) == 0 {
                // On an empty line (or a NUL): skip up to a non-empty line.
                loop {
                    if step(&mut pos) == -1 {
                        break;
                    }
                    if buf.gchar(pos) != 0 {
                        break;
                    }
                }
                if forward {
                    break 'found;
                }
            } else if forward && buf.col0(pos) && start_ps(buf, pos) {
                if buf.is_last_line(pos) {
                    return false;
                }
                pos = motion::line_end(buf.b, pos) + 1;
                break 'found;
            } else if !forward {
                buf.decl(&mut pos);
            }

            // Back over blanks and sentence punctuation to the previous ordinary char.
            let mut found_dot = false;
            loop {
                let c = buf.gchar(pos);
                if !(is_white(c) || (c != 0 && (SENT_END.contains(&c) || SENT_CLOSE.contains(&c))))
                {
                    break;
                }
                let mut tpos = pos;
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
                buf.decl(&mut pos);
            }

            // Find the end of the sentence.
            let startline = buf.line_start(pos);
            loop {
                let c = buf.gchar(pos);
                if c == 0 || (buf.col0(pos) && start_ps(buf, pos)) {
                    if !forward && buf.line_start(pos) != startline {
                        pos = motion::line_end(buf.b, pos) + 1;
                    }
                    break;
                }
                if SENT_END.contains(&c) {
                    let mut tpos = pos;
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
                        pos = tpos;
                        if buf.gchar(pos) == 0 {
                            buf.inc(&mut pos); // skip the NUL at the end of the line
                        }
                        break;
                    }
                }
                if step(&mut pos) == -1 {
                    if count > 0 {
                        return false;
                    }
                    noskip = true;
                    break;
                }
            }
        }
        // Skip blanks.
        while !noskip && is_white(buf.gchar(pos)) {
            if buf.incl(&mut pos) == -1 {
                break;
            }
        }
        if pos == prev_pos {
            // Did not move: advance one char and try again.
            if step(&mut pos) == -1 {
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

/// Vim `current_sent`. Returns `(start, cursor, inclusive)` like [`current_word`].
fn current_sent(
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

// --- bracket blocks (`i(`/`a(`, `{`, `[`, `<`) ---------------------------------------------------------

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

/// Vim `current_block`. Returns `(start, cursor, inclusive)` like [`current_word`].
fn current_block(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    count: u32,
    include: bool,
    open: u8,
    close: u8,
) -> Option<(usize, usize, bool)> {
    let mut count = count.max(1);
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

    // The `count`-th enclosing opener, or — when the cursor is in no block — the `count`-th next one.
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
    let mut start_pos = start_pos?;
    let mut end_pos = find_bracket(buf, start_pos, open, close, true, false)?;
    cur = end_pos;

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

// --- quotes (`i"`/`a"`, `'`, `` ` ``) -------------------------------------------------------------------

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

/// Vim `current_quote`, confined to the cursor's line. Returns `(start, cursor, inclusive)` like
/// [`current_word`].
fn current_quote(
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
    let at = |i: usize| if i < le { b[i] } else { 0 };
    let mut vis = visual;
    let mut cur = cursor;
    let col_start = cursor;
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

    let (mut col_start, mut col_end) = if !vis_empty && at(col_start) == q {
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
    cur = col_start;
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

// --- entry points --------------------------------------------------------------------------------------

fn delim(c: char) -> bool {
    c.is_ascii_graphic()
}

/// Dispatch a text-object motion to its Vim routine. `None` for a motion this module does not own
/// (paragraphs and tags live in `motion`) or an object that fails (Vim beeps and does nothing).
fn object(
    buf: &Buf,
    cursor: usize,
    visual: Option<usize>,
    m: Motion,
    count: u32,
) -> Option<(usize, usize, bool)> {
    match m {
        Motion::InnerWord => current_word(buf, cursor, visual, count, false, false),
        Motion::AWord => current_word(buf, cursor, visual, count, true, false),
        Motion::InnerBigWord => current_word(buf, cursor, visual, count, false, true),
        Motion::ABigWord => current_word(buf, cursor, visual, count, true, true),
        Motion::InnerSentence => current_sent(buf, cursor, visual, count, false),
        Motion::ASentence => current_sent(buf, cursor, visual, count, true),
        // Delimiters are printable ASCII (the input engine only produces those); a non-ASCII, NUL or
        // newline delimiter is no object — it would alias the `NUL` slot or a line break.
        Motion::Pair {
            open,
            close,
            around,
        } if delim(open) && delim(close) => {
            current_block(buf, cursor, visual, count, around, open as u8, close as u8)
        }
        Motion::Quote { ch, around } if delim(ch) => {
            current_quote(buf, cursor, visual, count, around, ch as u8)
        }
        _ => None,
    }
}

/// Where a failed word object (`aw`/`iw`/`aW`/`iW` under an operator, or in Visual with the anchor
/// `visual`) leaves the cursor, or `None` when it does not fail (#1089). In Visual the selection then
/// reaches that spot, as Vim keeps the moved cursor: `vaw` on a blank line selects to its end (#1111).
pub(crate) fn word_object_fails(
    b: &[u8],
    cursor: usize,
    visual: Option<usize>,
    m: Motion,
    count: u32,
) -> Option<usize> {
    let (include, big) = match m {
        Motion::InnerWord => (false, false),
        Motion::AWord => (true, false),
        Motion::InnerBigWord => (false, true),
        Motion::ABigWord => (true, true),
        _ => return None,
    };
    let buf = Buf::new(b);
    let cur = cursor.min(b.len()).min(buf.last);
    let visual = visual.map(|v| v.min(buf.last));
    current_word_at(&buf, cur, visual, count, include, big).err()
}

/// Whether a block, quote or sentence object (`i(`, `a"`, `is`) finds nothing to select from `cursor` under
/// an operator, where Vim beeps and cancels the operator (#1179). Word objects fail through
/// [`word_object_fails`], which also places the cursor.
pub(crate) fn object_fails(b: &[u8], cursor: usize, m: Motion, count: u32) -> bool {
    if !matches!(
        m,
        Motion::Pair { .. } | Motion::Quote { .. } | Motion::InnerSentence | Motion::ASentence
    ) {
        return false;
    }
    let buf = Buf::new(b);
    let cur = cursor.min(b.len()).min(buf.last);
    object(&buf, cur, None, m, count).is_none()
}

/// Whether `m` is one of the objects this module computes.
pub(crate) fn owns(m: Motion) -> bool {
    matches!(
        m,
        Motion::InnerWord
            | Motion::AWord
            | Motion::InnerBigWord
            | Motion::ABigWord
            | Motion::InnerSentence
            | Motion::ASentence
            | Motion::Pair { .. }
            | Motion::Quote { .. }
    )
}

/// The operator-pending object as Vim leaves it: a byte range `[s, e)` and whether it is EXCLUSIVE with
/// its end at a line start (the case Vim's exclusive-linewise rule reshapes). A failed object is the empty
/// range at the cursor; an object that selects nothing (`di(` on `()`) is the empty range at its start.
pub(crate) fn op_range(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    let cur = cursor.min(b.len());
    let buf = Buf::new(b);
    let cur = cur.min(buf.last);
    let Some((start, end, inclusive)) = object(&buf, cur, None, m, count) else {
        return (cur, cur, false);
    };
    let e = if inclusive && !buf.is_nul(end) {
        motion::next_grapheme(b, end)
    } else {
        end
    };
    if e <= start {
        return (start, start, false);
    }
    (start, e, !inclusive && buf.col0(e))
}

/// The operator span of an object: [`op_range`] reduced by the exclusive-linewise rule (an exclusive span
/// ending at a line start drops that line break, and goes linewise when it starts in the indent). Returns
/// `(s, e, linewise)`. The editor's operator path computes the same through `editor::op_span_full`.
#[cfg(test)]
pub(crate) fn op_span(b: &[u8], cursor: usize, m: Motion, count: u32) -> (usize, usize, bool) {
    let (s, e, exclusive_at_col0) = op_range(b, cursor, m, count);
    if exclusive_at_col0 {
        crate::editor::exclusive_linewise(b, s, e)
    } else {
        (s, e, false)
    }
}

/// Vi's `d` quirk (nvim `op_delete`): a charwise delete `[s, e)` spanning several lines, that starts in its
/// line's indent and leaves only blanks after its end, deletes whole lines instead. Returns `(s, e,
/// linewise)`; not applied to Visual deletes, `c` or `y`.
pub(crate) fn delete_reshape(b: &[u8], s: usize, e: usize) -> (usize, usize, bool) {
    let buf = Buf::new(b);
    if s >= e || e > buf.last || buf.line_start(e) == buf.line_start(s) {
        return (s, e, false);
    }
    let mut i = e;
    while !buf.is_nul(i) && is_white(b[i]) {
        i += 1;
    }
    if !buf.is_nul(i) || !buf.inindent(s, 0) {
        return (s, e, false);
    }
    let le = motion::line_end(b, e);
    (
        buf.line_start(s),
        if le < b.len() { le + 1 } else { le },
        true,
    )
}

/// The object applied to an active Visual selection (`anchor`..`cursor`): the new `(anchor, cursor)`, the
/// cursor on the last selected char. A repeated object extends the area (`viwiw`, `va(a(`, `visis`).
/// `None` when the object fails (the selection is left alone).
pub(crate) fn visual(
    b: &[u8],
    anchor: usize,
    cursor: usize,
    m: Motion,
    count: u32,
) -> Option<(usize, usize)> {
    let buf = Buf::new(b);
    let anchor = anchor.min(buf.last);
    let cursor = cursor.min(buf.last);
    let (a, c, _) = object(&buf, cursor, Some(anchor), m, count)?;
    Some((a, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(open: char, close: char, around: bool) -> Motion {
        Motion::Pair {
            open,
            close,
            around,
        }
    }

    fn quote(ch: char, around: bool) -> Motion {
        Motion::Quote { ch, around }
    }

    /// The text an operator over `m` would take (`d`/`y` register content), after the
    /// exclusive-linewise rule, plus whether it is linewise.
    fn take(b: &str, cur: usize, m: Motion, count: u32) -> (String, bool) {
        let (s, e, line) = op_span(b.as_bytes(), cur, m, count);
        (b[s..e].to_string(), line)
    }

    fn sel(b: &str, anchor: usize, cur: usize, m: Motion, count: u32) -> Option<String> {
        let (a, c) = visual(b.as_bytes(), anchor, cur, m, count)?;
        let (lo, hi) = (a.min(c), a.max(c));
        let hi = if hi < b.len() {
            motion::next_boundary(b.as_bytes(), hi)
        } else {
            hi
        };
        Some(b[lo..hi].to_string())
    }

    #[test]
    fn words_stop_at_the_line_break() {
        let iw = Motion::InnerWord;
        let aw = Motion::AWord;
        // #740: trailing blanks, empty line, last word on a line, only word on a line.
        assert_eq!(take("foo  \n  bar", 4, iw, 1), ("  ".into(), false));
        assert_eq!(take("a\n\nb", 2, iw, 1), ("".into(), false));
        assert_eq!(take("foo bar\nbaz", 4, aw, 1), (" bar".into(), false));
        assert_eq!(take("foo\nbar", 0, aw, 1), ("foo".into(), false));
        assert_eq!(take("  foo\nbar", 2, aw, 1), ("foo".into(), false));
        assert_eq!(take("foo  \nbar", 4, aw, 1), ("  \nbar".into(), false));
        // Counts do cross lines, but never take the next line's first char by accident.
        assert_eq!(
            take("foo bar\nbaz qux", 4, iw, 2),
            ("bar\nbaz".into(), false)
        );
        assert_eq!(
            take("foo bar\nbaz qux", 4, iw, 3),
            ("bar\nbaz ".into(), false)
        );
        assert_eq!(
            take("foo bar\nbaz qux", 4, aw, 2),
            ("bar\nbaz ".into(), false)
        );
        assert_eq!(
            take("foo\n\nbar baz", 0, iw, 4),
            ("foo\n\nbar ".into(), false)
        );
        // Multibyte words; the empty buffer.
        assert_eq!(take("한글 abc\ndef", 0, aw, 1), ("한글 ".into(), false));
        assert_eq!(take("abc 한글\ndef", 4, aw, 1), (" 한글".into(), false));
        // Vim classes split a Latin word from a Hangul particle; a cluster keeps its marks (#1136).
        assert_eq!(take("API를 x", 0, iw, 1), ("API".into(), false));
        assert_eq!(take("API를 x", 3, iw, 1), ("를".into(), false));
        assert_eq!(take("x API를 y", 2, aw, 1), (" API".into(), false));
        assert_eq!(take("x 가\u{301}b", 7, iw, 1), ("b".into(), false));
        assert_eq!(take("x 가\u{301}b", 2, iw, 1), ("가\u{301}".into(), false));
        assert_eq!(take("", 0, iw, 1), ("".into(), false));
        assert_eq!(take("", 0, aw, 1), ("".into(), false));
        // A newline-terminated buffer: the final `\n` is the last line's terminator.
        assert_eq!(take("foo bar\n", 6, aw, 1), (" bar".into(), false));
    }

    #[test]
    fn repeated_word_objects_extend_a_visual_area() {
        let b = "foo bar baz";
        // `viw` then `iw`: the anchor stays, the cursor grows by one word / blank run each time.
        assert_eq!(sel(b, 0, 2, Motion::InnerWord, 1).as_deref(), Some("foo "));
        assert_eq!(
            sel(b, 0, 3, Motion::InnerWord, 1).as_deref(),
            Some("foo bar")
        );
        assert_eq!(
            sel(b, 0, 0, Motion::InnerWord, 3).as_deref(),
            Some("foo bar")
        );
        assert_eq!(sel(b, 0, 1, Motion::InnerWord, 1).as_deref(), Some("foo"));
        // Cursor before the anchor: extend backward.
        assert_eq!(sel(b, 10, 9, Motion::InnerWord, 1).as_deref(), Some("baz"));
        assert_eq!(sel(b, 10, 8, Motion::InnerWord, 1).as_deref(), Some(" baz"));
    }

    #[test]
    fn quotes_honour_backslash_escapes() {
        let i = quote('"', false);
        assert_eq!(take(r#"a"b\"c"d"#, 0, i, 1).0, r#"b\"c"#);
        assert_eq!(take(r#"a"b\"c"d"#, 4, i, 1).0, r#"b\"c"#);
        assert_eq!(take(r#""a\"b" "c""#, 9, i, 1).0, "c");
        assert_eq!(
            take(r#"x "a\"b" y"#, 0, quote('"', true), 1).0,
            r#""a\"b" "#
        );
        assert_eq!(take(r#""a\\" b "c""#, 0, i, 1).0, r"a\\");
        assert_eq!(take(r#"x "한\"글" y"#, 0, i, 1).0, r#"한\"글"#);
        assert_eq!(take(r#""a" x "b""#, 4, i, 1).0, " x ");
        assert_eq!(take(r#"x "ab" y"#, 0, i, 2).0, r#""ab""#);
        assert_eq!(take("no quotes", 3, i, 1), ("".into(), false));
        // `vi"` then `i"` takes the quotes.
        assert_eq!(sel(r#"x "ab" y"#, 3, 4, i, 1).as_deref(), Some(r#""ab""#));
    }

    #[test]
    fn sentences_pick_the_sentence_or_gap_under_the_cursor() {
        let is = Motion::InnerSentence;
        let as_ = Motion::ASentence;
        assert_eq!(take("Hi. Bye.", 3, is, 1).0, " ");
        assert_eq!(take("Hi. Bye.", 3, as_, 1).0, " Bye.");
        assert_eq!(take("Hi there. Bye.", 13, as_, 1).0, " Bye.");
        assert_eq!(take("Hi there. Bye.", 0, as_, 1).0, "Hi there. ");
        assert_eq!(take("A b. C d. E f.", 0, is, 2).0, "A b. ");
        assert_eq!(take("A b. C d. E f.", 0, as_, 2).0, "A b. C d. ");
        assert_eq!(take("가나. 다라. 마바.", 8, as_, 1).0, "다라. ");
        // A sentence ending its line is linewise under an operator (exclusive end at the next line start).
        assert_eq!(take("Hi.\nBye.", 0, is, 1), ("Hi.\n".into(), true));
        assert_eq!(take("", 0, is, 1), ("".into(), false));
    }

    #[test]
    fn block_counts_climb_outward_and_search_forward() {
        let i = pair('(', ')', false);
        assert_eq!(take("((a))", 2, i, 2).0, "(a)");
        assert_eq!(take("((a))", 0, i, 2).0, "");
        assert_eq!(take("((a))", 2, i, 3).0, "");
        assert_eq!(take("x((a)b)y", 3, pair('(', ')', true), 2).0, "((a)b)");
        assert_eq!(take("foo\n(bar)", 0, i, 1).0, "bar");
        assert_eq!(take("(a(b)c)", 4, i, 1).0, "b");
        assert_eq!(take("(한(글)x)", 7, i, 2).0, "한(글)x");
        assert_eq!(take(r"(a \) b)", 1, i, 1).0, r"a \) b");
        // Visual: a repeated `a(` grows to the enclosing block; `i(` too once the area is the whole inner.
        assert_eq!(
            sel("x((a) b)y", 3, 3, pair('(', ')', true), 1).as_deref(),
            Some("(a)")
        );
        assert_eq!(
            sel("x((a) b)y", 2, 4, pair('(', ')', true), 1).as_deref(),
            Some("((a) b)")
        );
        assert_eq!(sel("((ab))", 2, 3, i, 1).as_deref(), Some("(ab)"));
        // No enclosing block left to grow into: the object fails and the selection stays. (nvim then beeps
        // and flushes typeahead, which is why this is a unit test rather than an oracle fixture.)
        assert_eq!(sel("(x(ab)y)", 1, 6, i, 1), None);
    }

    fn objects() -> Vec<Motion> {
        let mut v = vec![
            Motion::InnerWord,
            Motion::AWord,
            Motion::InnerBigWord,
            Motion::ABigWord,
            Motion::InnerSentence,
            Motion::ASentence,
        ];
        for (o, c) in [('(', ')'), ('{', '}'), ('[', ']'), ('<', '>')] {
            v.push(pair(o, c, false));
            v.push(pair(o, c, true));
        }
        for q in ['"', '\'', '`'] {
            v.push(quote(q, false));
            v.push(quote(q, true));
        }
        v
    }

    #[test]
    fn hostile_counts_terminate() {
        let b = "Hi. Bye.\n\n(a (b)) \"q\" w";
        for m in objects() {
            for cur in [0, 4, 11, b.len()] {
                let _ = op_span(b.as_bytes(), cur, m, u32::MAX);
                let _ = visual(b.as_bytes(), 0, cur, m, u32::MAX);
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// Every object, from every cursor, with any count, in operator-pending and Visual mode, yields
        /// in-bounds ranges on char boundaries (the buffer mixes blanks, line breaks, sentence and bracket
        /// punctuation, escapes and multibyte text) — and never spins.
        #[test]
        fn object_ranges_stay_in_bounds_on_char_boundaries(
            text in "[a .!?\"'`()\\[\\]{}<>\\\\\t\n한]{0,24}",
            count in 1u32..4,
        ) {
            let b = text.as_bytes();
            let ok = |p: usize| p <= b.len() && motion::is_boundary(b, p);
            for m in objects() {
                for cur in (0..=b.len()).filter(|&p| ok(p)) {
                    let (s, e, _) = op_span(b, cur, m, count);
                    proptest::prop_assert!(s <= e && ok(s) && ok(e), "{m:?} at {cur}: {s}..{e}");
                    let (s, e) = motion::char_span(b, cur, m, count);
                    proptest::prop_assert!(s <= e && ok(s) && ok(e), "{m:?} at {cur}: {s}..{e}");
                    for anchor in [cur, 0, b.len()] {
                        if let Some((a, c)) = visual(b, anchor, cur, m, count) {
                            proptest::prop_assert!(ok(a) && ok(c), "{m:?} visual {anchor}..{cur}: {a}..{c}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn indented_closer_keeps_its_line() {
        let i = pair('{', '}', false);
        // #703: a closer preceded only by indent leaves that indent (and its line) alone.
        let b = "    if x {\n        foo\n    }";
        assert_eq!(take(b, 12, i, 1), ("        foo\n".into(), true));
        assert_eq!(take("{\n\tfoo\n\t}", 2, i, 1), ("\tfoo\n".into(), true));
        assert_eq!(
            take("{ foo\n  bar\n  }", 7, i, 1),
            (" foo\n  bar".into(), false)
        );
        assert_eq!(take("{  \nfoo\n}", 4, i, 1), ("  \nfoo".into(), false));
        assert_eq!(take("{\n  foo }", 2, i, 1), ("  foo ".into(), false));
        assert_eq!(
            take("(\n)", 0, pair('(', ')', false), 1),
            ("".into(), false)
        );
        assert_eq!(
            op_span(b"(\n)", 0, pair('(', ')', false), 1).0,
            2,
            "empty inner sits after the break"
        );
        // Visual `i{` is charwise and takes the last inner line break.
        assert_eq!(sel(b, 12, 12, i, 1).as_deref(), Some("        foo\n"));
    }
}
