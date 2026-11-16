//! Word objects `iw`/`aw`/`iW`/`aW`: nvim `current_word` (`textobject.c`) with the word walkers it
//! calls (`fwd_word`, `bck_word`, `end_word`, `bckend_word`, `skip_chars`, `back_in_line`).

use super::Buf;

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
pub(super) fn current_word(
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
pub(super) fn current_word_at(
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
