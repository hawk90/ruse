//! The single-key Normal edits: `X`/`x`/`<Del>`, `r`, `~`, `J`/`gJ` and `CTRL-A`/`CTRL-X`.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{beep, captured, edit, edit_yank, nop, one, plan_join, recase_bytes};
use crate::editor::{advance_graphemes, advance_n, block_geom, selection_range, MAXCOL};
use crate::motion::{line_end, line_start, prev_boundary, prev_grapheme};
use crate::{
    BytePos, CaretGravity, Command, Edit, EditList, EditorState, GroupHint, Mode, Plan, SelectKind,
    WordCase,
};

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::DeleteBack | Command::DeleteUnder(_) | Command::DeleteForward(_) => {
            plan_delete(st, b, cur, hint, cmd)
        }
        Command::ReplaceChar(..) | Command::ToggleCase(_) => {
            plan_rewrite_chars(st, b, cur, hint, cmd)
        }
        Command::JoinLines(count) => plan_join(st, cur, *count, st.view.mode, hint, false),
        Command::JoinLinesNoSpace(count) => plan_join(st, cur, *count, st.view.mode, hint, true),
        Command::IncrementNumber(delta) => plan_increment_number(st, b, cur, hint, *delta),
        Command::IncrementSelection { delta, sequential } => {
            plan_increment_selection(st, b, cur, hint, *delta, *sequential)
        }
        _ => unreachable!("planner::edit plans only its own commands"),
    }
}

/// `<BS>` (`DeleteBack`), Vim `x` (`DeleteUnder`) and Emacs `delete-char` (`DeleteForward`).
fn plan_delete(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::DeleteBack => {
            let ls = line_start(b, cur);
            let stepped = (st.view.caret == CaretGravity::OnChar && st.view.mode == Mode::Insert)
                .then(|| st.view.indent.backspace(b, ls, cur, st.view.typed_space))
                .flatten();
            if let Some((from, pad)) = stepped {
                // `'smarttab'` in the indent / `'softtabstop'`: back to the previous stop (nvim's `ins_bs`).
                return edit(
                    one(Edit::replace(BytePos(from), cur - from, vec![b' '; pad])),
                    from + pad,
                    Mode::Insert,
                    hint,
                );
            }
            if cur == 0 {
                nop(cur, st.view.mode)
            } else {
                // Vim's `<BS>` deletes a whole character with its composing marks, and nvim v0.12.6 a whole
                // cluster (a flag, a ZWJ sequence), so the cursor never lands inside one (#909). The Emacs
                // profile keeps deleting one code point.
                let p = if st.view.caret == CaretGravity::OnChar {
                    prev_grapheme(b, cur)
                } else {
                    prev_boundary(b, cur)
                };
                edit(
                    one(Edit::delete(BytePos(p), cur - p)),
                    p,
                    st.view.mode,
                    hint,
                )
            }
        }
        Command::DeleteUnder(count) => {
            // `{count}x`: delete `count` chars from the cursor, clamped at end-of-line (Vim). Fewer than
            // `count` chars left deletes to EOL. The removed span fills the unnamed register (charwise). A
            // char is a grapheme cluster: a base with its composing marks, a ZWJ emoji sequence (#696).
            let le = line_end(b, cur);
            let (end, _) = advance_graphemes(b, cur, *count, le);
            if end <= cur {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, cur, end, false);
                edit_yank(
                    one(Edit::delete(BytePos(cur), end - cur)),
                    cur,
                    st.view.mode,
                    hint,
                    reg,
                )
            }
        }
        Command::DeleteForward(count) => {
            // Emacs `delete-char`: delete `count` chars forward from point, clamped at BUFFER end (crosses
            // newlines — Emacs has no end-of-line boundary), and WITHOUT writing to the kill ring (D-026).
            // Uses the no-register `edit` path, unlike Vim `x` (`DeleteUnder`, which yanks via `edit_yank`).
            let end = advance_n(b, cur, *count, b.len());
            if end <= cur {
                nop(cur, st.view.mode)
            } else {
                edit(
                    one(Edit::delete(BytePos(cur), end - cur)),
                    cur,
                    st.view.mode,
                    hint,
                )
            }
        }
        _ => unreachable!("plan_delete plans only the delete commands"),
    }
}

/// The in-place character rewrites: `{count}r{ch}` and `{count}~`.
fn plan_rewrite_chars(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
    match cmd {
        Command::ReplaceChar(count, c) => {
            // `{count}r{ch}`: replace `count` chars with `ch`. Per Vim it is a NO-OP if fewer than
            // `count` chars remain on the line (never a partial replace). Cursor lands on the last one.
            let le = line_end(b, cur);
            let (end, reached) = advance_graphemes(b, cur, *count, le);
            if !reached {
                // Vim beeps (`nv_replace`), so a macro or `:normal` stops here (#1111).
                beep(cur, st.view.mode)
            } else if *c == '\n' {
                // `{count}r<CR>`: replace the count chars with a SINGLE line break (Vim splits the line),
                // leaving the cursor on the first char of the new line — never `count` newlines.
                edit(
                    one(Edit::replace(BytePos(cur), end - cur, b"\n".to_vec())),
                    cur + 1,
                    st.view.mode,
                    hint,
                )
            } else {
                let mut buf = [0u8; 4];
                let one_ch = c.encode_utf8(&mut buf).as_bytes();
                let mut bytes = Vec::with_capacity(one_ch.len() * *count as usize);
                for _ in 0..*count {
                    bytes.extend_from_slice(one_ch);
                }
                let last = cur + one_ch.len() * (*count as usize - 1);
                edit(
                    one(Edit::replace(BytePos(cur), end - cur, bytes)),
                    last,
                    st.view.mode,
                    hint,
                )
            }
        }
        Command::ToggleCase(count) => {
            // `{count}~`: toggle the case of `count` chars, clamped at EOL, then leave the cursor past the
            // last toggled char (a Normal-mode edit, so `commit` clamps it back onto the last char at EOL).
            // Case-toggle by Unicode scalar (uppercase→lowercase, else lowercase→uppercase), so non-ASCII
            // letters flip too (`~` on "αβ" → "Αβ"); non-letters are consumed but left unchanged. The
            // toggled UTF-8 may differ in byte length from the source, so the cursor lands at `cur +
            // flipped.len()`, not the source end.
            let le = line_end(b, cur);
            let (end, _) = advance_graphemes(b, cur, *count, le);
            if end <= cur {
                // An empty line: nvim's `n_swapchar` fails (`clearopbeep`), so it is no change for `.` (#1179).
                beep(cur, st.view.mode)
            } else {
                // Invalid UTF-8 bytes are kept as they are (#706).
                let flipped = recase_bytes(&b[cur..end], WordCase::Toggle);
                if flipped == b[cur..end] {
                    nop(end, st.view.mode) // nothing was a letter: `~` just moves right
                } else {
                    let cursor = cur + flipped.len();
                    edit(
                        one(Edit::replace(BytePos(cur), end - cur, flipped)),
                        cursor,
                        st.view.mode,
                        hint,
                    )
                }
            }
        }
        _ => unreachable!("plan_rewrite_chars plans only `r` and `~`"),
    }
}

/// `CTRL-A`/`CTRL-X` at the cursor.
fn plan_increment_number(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    delta: i64,
) -> Plan {
    // `CTRL-A`/`CTRL-X`: adjust the number under or after the cursor on the current line (`incr_number`).
    let ls = crate::pos::raw::line_start(b, cur);
    let le = line_end(b, cur);
    match incr_number(b, ls, le, cur, delta, None) {
        Some((start, old_len, bytes, cursor)) => edit(
            one(Edit::replace(BytePos(start), old_len, bytes)),
            cursor,
            st.view.mode,
            hint,
        ),
        None => nop(cur, st.view.mode), // no number on the rest of the line
    }
}

/// Visual `CTRL-A`/`CTRL-X` (and `g CTRL-A`/`g CTRL-X`): increment the first number per selected line.
/// Plain form adds `delta` to every line; `sequential` (the `g` form) adds `delta`, `2·delta`, `3·delta`…
/// to successive numbered lines — the classic "turn a column of 1s into 1,2,3…".
///
/// Each line contributes its selected part, as nvim's `op_addsub` hands `do_addsub` a start column and a
/// length: the number must START inside that part and is read no further than its end (#717). This
/// makes a column of numbers that are NOT the first on their line increment correctly:
///   - BLOCKWISE (`CTRL-V`): the characters each row touches (`block_geom`; `$` runs to each line end).
///   - LINEWISE (`V`): the whole line.
///   - CHARWISE (`v`): from the selection start on the first line (the line start on the others) to the
///     selection end on the last line (the line end on the others).
fn plan_increment_selection(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    delta: i64,
    sequential: bool,
) -> Plan {
    let Some(anchor) = st.view.anchor else {
        return nop(cur, Mode::Normal);
    };
    // Per selected line: (line_start, from, until) — the selected bytes `from..until`.
    let lines: Vec<(usize, usize, usize)> = match st.view.mode.selection() {
        Some(SelectKind::Blockwise) => {
            let to_eol = st.view.curswant == MAXCOL;
            block_geom(b, anchor, cur, st.view.indent.tabs(), to_eol)
                .rows
                .iter()
                .map(|r| (r.ls, r.ds.min(r.le), r.de.min(r.le)))
                .collect()
        }
        kind => {
            let linewise = kind == Some(SelectKind::Linewise);
            let (s, e) = selection_range(b, anchor, cur, linewise);
            let mut ls = line_start(b, s.min(b.len()));
            let mut out = Vec::new();
            // Charwise starts its FIRST line at the selection start `s`; every other line (and every
            // linewise line) starts at its own line start.
            let mut from = if linewise { ls } else { s.max(ls) };
            while ls < e && ls <= b.len() {
                let le = line_end(b, ls);
                out.push((ls, from, if linewise { le } else { e.min(le) }));
                if le >= b.len() {
                    break;
                }
                ls = le + 1;
                from = ls;
            }
            out
        }
    };
    let mut edits: Vec<Edit> = Vec::new();
    // Vim leaves the caret on the FIRST SELECTED line at the selection's LEFT-EDGE column — the
    // first line's search-`from` (linewise: column 0; charwise: the selection start; blockwise: the
    // block's left column). It is INDEPENDENT of which line changed (or whether any did) and of the
    // number's own last digit — so a multi-digit result (`007`→`008`) or a numberless first line
    // (`abc` above the numbers) still homes here. Verified against nvim v0.12.4 (parity fixtures).
    let caret = lines.first().map_or(cur, |&(_ls, from, _until)| from);
    let mut steps: i64 = 0; // numbered lines seen so far (the sequence multiplier)
    for (ls, from, until) in lines {
        let le = line_end(b, ls);
        steps += 1;
        let this = if sequential {
            delta.saturating_mul(steps)
        } else {
            delta
        };
        if let Some((start, old_len, bytes, _cursor)) =
            incr_number(b, ls, le, from, this, Some(until))
        {
            edits.push(Edit::replace(BytePos(start), old_len, bytes));
        } else {
            steps -= 1; // a line with no number does not advance the sequence
        }
    }
    if edits.is_empty() {
        return nop(caret, Mode::Normal);
    }
    let list = EditList::new(edits).expect("one replacement per line ⇒ disjoint");
    edit(list, caret, Mode::Normal, hint)
}

/// Add `delta` to a number on the line `ls..le`, as nvim's `do_addsub` (ops.c) does with the default
/// `nrformats=bin,hex` (ruse has no `nrformats` option, so octal, alpha and unsigned are never on).
/// Returns `(start, old_len, new_bytes, cursor)` — an absolute-offset replacement plus the offset of its
/// last character (where Vim leaves the caret) — or `None` when no number is found. The shared engine
/// for `CTRL-A`/`CTRL-X` (`visual: None`, `from` the cursor) and Visual increment (`visual: Some(until)`,
/// one call per selected line over `from..until`).
///
/// The number model (#717) is nvim's: a decimal is an unsigned 64-bit magnitude plus a sign, which
/// saturates at `u64::MAX` when its digits overflow (`vim_str2nr`) and then stays unchanged; a `0x`/`0b`
/// literal is unsigned and wraps modulo 2^64. Leading zeros keep the width, and hex renders in the case of
/// the number's LAST letter (`0xAB`→`0xAC`, `0x9F`→`0xa0`). A `-` before a based literal is not a sign;
/// `0o17` and `017` are decimals, as octal is not in the default formats.
fn incr_number(
    b: &[u8],
    ls: usize,
    le: usize,
    from: usize,
    delta: i64,
    visual: Option<usize>,
) -> Option<(usize, usize, Vec<u8>, usize)> {
    let p = &b[ls..le];
    let pos = from.max(ls) - ls; // nvim's `pos->col`
    if pos >= p.len() {
        return None; // the cursor is at or past the line end
    }
    let isbdigit = |c: u8| c == b'0' || c == b'1';
    // A `0x`/`0X` + hex digit, or `0b`/`0B` + binary digit, with its base letter at `c`.
    let hex_at = |c: usize| {
        c > 0
            && matches!(p[c], b'x' | b'X')
            && p[c - 1] == b'0'
            && p.get(c + 1).is_some_and(u8::is_ascii_hexdigit)
    };
    let bin_at = |c: usize| {
        c > 0
            && matches!(p[c], b'b' | b'B')
            && p[c - 1] == b'0'
            && p.get(c + 1).copied().is_some_and(isbdigit)
    };
    let mut col = pos;
    let mut negative = false;
    let mut was_positive = true;
    match visual {
        None => {
            // Is the cursor on a based literal after its `0x`/`0b`? Walk left over binary, then hex
            // digits to the base letter; when that found no `0x`, rescan over decimal digits only.
            while col > 0 && isbdigit(p[col]) {
                col -= 1;
            }
            while col > 0 && p[col].is_ascii_hexdigit() {
                col -= 1;
            }
            if !hex_at(col) {
                col = pos;
                while col > 0 && p[col].is_ascii_digit() {
                    col -= 1;
                }
            }
            if hex_at(col) || bin_at(col) {
                col -= 1; // to the literal's `0`
            } else {
                // Search forward, then backward, for the start of a decimal.
                col = pos;
                while col < p.len() && !p[col].is_ascii_digit() {
                    col += 1;
                }
                while col > 0 && p[col - 1].is_ascii_digit() {
                    col -= 1;
                }
            }
        }
        Some(until) => {
            // Visual searches forward only, within the selection; a `-` is a sign only when selected too.
            let mut length = until.saturating_sub(ls + pos);
            while col < p.len() && length > 0 && !p[col].is_ascii_digit() {
                let next = crate::motion::next_grapheme(p, col).max(col + 1);
                length = length.saturating_sub(next - col);
                col = next;
            }
            if length == 0 {
                return None;
            }
            if col > pos && p[col - 1] == b'-' {
                negative = true;
                was_positive = false;
            }
        }
    }
    let firstdigit = *p.get(col)?;
    if !firstdigit.is_ascii_digit() {
        return None; // no number
    }
    if visual.is_none() && col > 0 && p[col - 1] == b'-' {
        col -= 1; // a negative number
        negative = true;
    }
    // A Visual number is read no further than the selection (nvim's `maxlen`).
    let maxlen = visual
        .map_or(p.len(), |until| until.min(le))
        .saturating_sub(ls + col);
    let (pre, mut length, mut n, overflow) = str2nr(&p[col..col + maxlen]);
    if pre != 0 && negative {
        // A `-` before a hex or binary literal is not a sign.
        col += 1;
        length -= 1;
        negative = false;
    }
    let subtract = (delta < 0) ^ negative;
    let amount = delta.unsigned_abs();
    let oldn = n;
    if !overflow {
        // A number too big to read is not changed.
        n = if subtract {
            n.wrapping_sub(amount)
        } else {
            n.wrapping_add(amount)
        };
    }
    if pre == 0 {
        // A decimal that wraps flips its sign (nvim's arithmetic, asymmetric as it is).
        if subtract {
            if n > oldn {
                n = 1u64.wrapping_add(n ^ u64::MAX);
                negative = !negative;
            }
        } else if n < oldn {
            n ^= u64::MAX;
            negative = !negative;
        }
        if n == 0 {
            negative = false;
        }
    }
    if visual.is_some() && !was_positive && !negative && col > 0 {
        // The selected `-` goes with the sign.
        col -= 1;
        length += 1;
    }
    let old = &p[col..col + length];
    let todel = length;
    if old[0] == b'-' {
        length -= 1; // the kept width excludes the sign
    }
    // Hex digits take the case of the old number's last letter (the `x`/`X` included).
    let hexupper = old
        .iter()
        .rev()
        .find(|c| c.is_ascii_alphabetic())
        .is_some_and(u8::is_ascii_uppercase);
    let mut out: Vec<u8> = Vec::with_capacity(length + 24);
    if negative && (visual.is_none() || was_positive) {
        out.push(b'-');
    }
    if pre != 0 {
        out.extend_from_slice(&[b'0', pre]);
        length -= 2;
    }
    let digits = match pre {
        b'b' | b'B' if n == 0 => String::new(), // no binary digits for 0: the zero padding supplies them
        b'b' | b'B' => format!("{n:b}"),
        b'x' | b'X' if hexupper => format!("{n:X}"),
        b'x' | b'X' => format!("{n:x}"),
        _ => n.to_string(),
    };
    // Leading zeros keep the number's width (`007`→`008`, `0x00ff`→`0x0100`).
    if firstdigit == b'0' {
        out.resize(out.len() + length.saturating_sub(digits.len()), b'0');
    }
    out.extend_from_slice(digits.as_bytes());
    let start = ls + col;
    let cursor = start + out.len() - 1;
    Some((start, todel, out, cursor))
}

/// nvim's `vim_str2nr` (charset.c) as `do_addsub` calls it with `STR2NR_BIN | STR2NR_HEX`: an optional
/// `-`, a `0x`/`0X`/`0b`/`0B` prefix when a digit of that base follows, then the digits. Returns `(prefix
/// letter or 0, bytes read, unsigned magnitude, overflowed)`; an overflow saturates at `u64::MAX`.
fn str2nr(s: &[u8]) -> (u8, usize, u64, bool) {
    let mut i = usize::from(s.first() == Some(&b'-'));
    let mut pre = 0u8;
    if s.get(i) == Some(&b'0') {
        match (s.get(i + 1).copied(), s.get(i + 2).copied()) {
            (Some(c @ (b'x' | b'X')), Some(d)) if d.is_ascii_hexdigit() => pre = c,
            (Some(c @ (b'b' | b'B')), Some(b'0' | b'1')) => pre = c,
            _ => {}
        }
    }
    let radix: u32 = match pre {
        b'x' | b'X' => 16,
        b'b' | b'B' => 2,
        _ => 10,
    };
    if pre != 0 {
        i += 2;
    }
    let (mut un, mut overflow) = (0u64, false);
    while let Some(d) = s.get(i).and_then(|&c| char::from(c).to_digit(radix)) {
        match un
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) if !overflow => un = v,
            _ => {
                un = u64::MAX;
                overflow = true;
            }
        }
        i += 1;
    }
    (pre, i, un, overflow)
}
