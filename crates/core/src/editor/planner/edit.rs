//! The single-key Normal edits: `X`/`x`/`<Del>`, `r`, `~`, `J`/`gJ` and `CTRL-A`/`CTRL-X`.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{beep, captured, edit, edit_yank, nop, one, plan_join, recase_bytes};
use crate::editor::{advance_graphemes, advance_n, block_geom, selection_range};
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
    // `CTRL-A`/`CTRL-X`: adjust the number at or after the cursor on the current line. A `0x`-prefixed
    // hex literal increments in hex (`0x1f`→`0x20`); otherwise a decimal (with optional `-` sign).
    let ls = crate::pos::raw::line_start(b, cur);
    let le = line_end(b, cur);
    match incr_number(b, ls, le, cur, delta) {
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
/// The unifying rule across all three shapes: on each line, search from the SELECTION'S LEFT EDGE
/// (`incr_number` finds the first number at/after it, expanding to the whole number). This makes a
/// column of numbers that are NOT the first on their line increment correctly — the whole point of the
/// feature — for both `CTRL-V` (block left column) and `v` (the start column on the first line):
///   - BLOCKWISE (`CTRL-V`): the first character each row touches (`block_geom`).
///   - LINEWISE (`V`): the line start (whole lines).
///   - CHARWISE (`v`): the selection start on the first line, the line start on continuation lines.
///
/// (Charwise does not yet CLIP a number sitting past the selection end on the last line — a rare edge,
/// noted as a follow-up.)
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
    // Per selected line: (line_start, search_from).
    let lines: Vec<(usize, usize)> = match st.view.mode.selection() {
        Some(SelectKind::Blockwise) => block_geom(b, anchor, cur, st.view.indent.tabs(), false)
            .rows
            .iter()
            .map(|r| (r.ls, r.ds))
            .collect(),
        kind => {
            let linewise = kind == Some(SelectKind::Linewise);
            let (s, e) = selection_range(b, anchor, cur, linewise);
            let mut ls = line_start(b, s.min(b.len()));
            let mut out = Vec::new();
            // Charwise starts its FIRST line at the selection start `s`; every other line (and every
            // linewise line) starts at its own line start.
            let mut from = if linewise { ls } else { s.max(ls) };
            while ls < e && ls <= b.len() {
                out.push((ls, from));
                let le = line_end(b, ls);
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
    let caret = lines.first().map_or(cur, |&(_ls, from)| from);
    let mut steps: i64 = 0; // numbered lines seen so far (the sequence multiplier)
    for (ls, from) in lines {
        let le = line_end(b, ls);
        steps += 1;
        let this = if sequential { delta * steps } else { delta };
        if let Some((start, old_len, bytes, _cursor)) = incr_number(b, ls, le, from, this) {
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

/// Add `delta` to the first number at or after `from` on the line `ls..le`, matching Neovim `nrformats`
/// (decimal, and `0x`/`0b`/`0o` based literals). Returns `(start, old_len, new_bytes, cursor)` — an
/// absolute-offset replacement plus the offset of its last digit (where Vim leaves the caret) — or `None`
/// when the rest of the line holds no number. This is the shared engine for `CTRL-A`/`CTRL-X` (one call at
/// the cursor) and Visual increment (one call per selected line).
fn incr_number(
    b: &[u8],
    ls: usize,
    le: usize,
    from: usize,
    delta: i64,
) -> Option<(usize, usize, Vec<u8>, usize)> {
    // First decimal digit at or after `from` (Vim searches forward). A based literal always has one (its
    // `0`), so this anchors every base.
    let mut d = from.max(ls);
    while d < le && !b[d].is_ascii_digit() {
        d += 1;
    }
    if d >= le {
        return None; // no number on the rest of the line
    }
    // Detect a based literal `0x`/`0X` (hex), `0b`/`0B` (binary), or `0o`/`0O` (octal). Two ways `d` (the
    // first decimal digit) can land on one: the `0` prefix starts AT `d`, or `d` sits inside the digit run
    // (walk left over base-max hex digits — the widest alphabet — to a base letter, and check the `0`).
    let radix_of = |c: u8| match c {
        b'x' | b'X' => Some(16u32),
        b'o' | b'O' => Some(8),
        b'b' | b'B' => Some(2),
        _ => None,
    };
    let is_digit_of = |c: u8, radix: u32| (c as char).is_digit(radix);
    let based_at_d = b[d] == b'0'
        && b.get(d + 1)
            .copied()
            .and_then(radix_of)
            .is_some_and(|r| b.get(d + 2).copied().is_some_and(|c| is_digit_of(c, r)));
    let mut hleft = d;
    while hleft > ls && b[hleft - 1].is_ascii_hexdigit() {
        hleft -= 1;
    }
    let based_inside = hleft >= ls + 2 && b[hleft - 2] == b'0' && radix_of(b[hleft - 1]).is_some();
    if based_at_d || based_inside {
        let prefix = if based_at_d { d } else { hleft - 2 };
        let radix = radix_of(b[prefix + 1]).unwrap_or(16);
        let letter = b[prefix + 1]; // keep the original `x`/`X`/`b`/`B`/`o`/`O` case (Vim does)
        let mut end = prefix + 2;
        while end < le && is_digit_of(b[end], radix) {
            end += 1;
        }
        let val: i128 = std::str::from_utf8(&b[prefix + 2..end])
            .ok()
            .and_then(|s| i128::from_str_radix(s, radix).ok())
            .unwrap_or(0);
        // Based literals stay non-negative (Vim wraps at 0 for the default unsigned view); the numeric body
        // is rendered in its own base (lowercase digits), keeping the original `0{letter}` prefix.
        let n = (val + i128::from(delta)).max(0);
        let body = match radix {
            2 => format!("{n:b}"),
            8 => format!("{n:o}"),
            _ => format!("{n:x}"),
        };
        let new_text = format!("0{}{body}", letter as char);
        let bytes = new_text.into_bytes();
        let cursor = prefix + bytes.len().saturating_sub(1);
        return Some((prefix, end - prefix, bytes, cursor));
    }
    // Decimal: the maximal digit run around `d`, plus a leading `-` sign if directly before it.
    let mut start = d;
    while start > ls && b[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let mut end = d;
    while end < le && b[end].is_ascii_digit() {
        end += 1;
    }
    let num_start = if start > ls && b[start - 1] == b'-' {
        start - 1
    } else {
        start
    };
    // Parse (i128 to absorb any i64 span), add the delta, and re-render. An unparseable/overflowing run is
    // left untouched (returned as a no-op replacement of itself).
    let val: i128 = std::str::from_utf8(&b[num_start..end])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let result = val + i128::from(delta);
    // Vim preserves a leading-zero field WIDTH: `007`→`008`, `099`→`100` (grows only on carry), `-007`→
    // `-006`. Only when the original magnitude (digits, excluding sign) had a leading zero — a plain `42`
    // is never padded. The width is measured on the magnitude and re-applied after sign handling.
    let orig_mag_width = end - start;
    let had_leading_zero = orig_mag_width > 1 && b[start] == b'0';
    let mag = result.unsigned_abs().to_string();
    let mag = if had_leading_zero && mag.len() < orig_mag_width {
        format!("{mag:0>orig_mag_width$}")
    } else {
        mag
    };
    let new_text = if result < 0 { format!("-{mag}") } else { mag };
    let bytes = new_text.into_bytes();
    let cursor = num_start + bytes.len().saturating_sub(1); // land on the last digit (Vim)
    Some((num_start, end - num_start, bytes, cursor))
}
