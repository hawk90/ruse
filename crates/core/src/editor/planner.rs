use super::*;

/// Enter a blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`): compute the top-row insert point
/// and, for `c`, the block-delete edits + captured blockwise register. The typed text is replicated down
/// the block by [`block_replicate`] when the session's `<Esc>` closes it.
fn plan_block_insert(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    kind: &BlockInsertKind,
) -> Plan {
    let Some(anchor) = st.view.anchor else {
        // Not in a selection — degrade to a plain Insert entry, no session.
        return nop(cur, Mode::Insert);
    };
    let (rows, col_lo, col_hi) = block_rows(b, anchor, cur);
    let append = matches!(kind, BlockInsertKind::Append);
    let change = matches!(kind, BlockInsertKind::Change);
    // `$A` (`` <C-v>$A ``): `$` set curswant to MAXCOL, so the block is ragged — append at EACH row's own
    // line-end, not a fixed column. (curswant is still MAXCOL here; `update_curswant` runs after commit.)
    let to_eol = append && st.view.curswant == crate::editor::range::MAXCOL;
    let target_col = if append { col_hi + 1 } else { col_lo };
    let top_start = rows.first().map_or(cur, |&(s, _)| s);
    let top_ls = line_start(b, top_start);
    let rows_below = rows.len().saturating_sub(1);

    let mut edits: Vec<Edit> = Vec::new();
    let mut reg: Option<Register> = None;
    // `c`: delete the block on every row first (capture it blockwise), then insert at the left edge
    // — where the top-row delete began, which stays valid after the delete.
    if change {
        let mut text: Vec<u8> = Vec::new();
        for (i, &(s, e)) in rows.iter().enumerate() {
            if i > 0 {
                text.push(b'\n');
            }
            text.extend_from_slice(&b[s..e]);
        }
        reg = Some(Register::blockwise(text));
        for &(s, e) in &rows {
            if e > s {
                edits.push(Edit::delete(s, e - s));
            }
        }
    }

    // Where the insert begins on the top row. For `A` on a top row shorter than the append column,
    // pad it out with spaces so the cursor sits at the append column.
    let top_le = line_end(b, top_ls);
    let toplen = col_of(b, top_ls, top_le);
    let insert_start = if change {
        top_start
    } else if to_eol {
        top_le // `$A`: append at the top row's own end (ragged), no padding
    } else if append && toplen < target_col {
        edits.push(Edit::insert(top_le, vec![b' '; target_col - toplen]));
        top_le + (target_col - toplen)
    } else {
        at_col(b, top_ls, target_col.min(toplen))
    };

    let session = BlockInsert {
        insert_start,
        top_left: at_col(b, top_ls, col_lo),
        top_line_start: top_ls,
        target_col,
        rows_below,
        append,
        to_eol,
    };
    let list = EditList::new(edits).expect("block-insert enter edits are disjoint (one per line)");
    let is_edit = !list.is_empty();
    Plan {
        action: Action::BlockInsertArm {
            edits: list,
            hint,
            session,
        },
        cursor: insert_start,
        mode: Mode::Insert,
        is_edit,
        effects: Vec::new(),
        set_register: reg.map(RegWrite::Edit),
        set_anchor: None,
        set_mark: None,
    }
}

/// The shared `cc`-style linewise-change body over a whole-line span `[ls, content_end)` where
/// `content_end` is the CONTENT end (the trailing newline EXCLUDED). Vim keeps the first line's leading
/// indent, deletes the rest of the content, KEEPS the trailing newline (so exactly one empty indented line
/// remains — the separator to the following line is never eaten), captures the whole lines linewise
/// (indent + trailing newline included), and enters Insert at the end of the kept indent. Shared by
/// `cc`/`S`, the paragraph objects, the linewise inner block ([`plan_change`]), AND visual-linewise change
/// (`V…c` — [`Command::ChangeSelection`] over a linewise selection), so all four agree with nvim.
fn plan_linewise_change(b: &[u8], ls: usize, content_end: usize, hint: GroupHint) -> Plan {
    let indent_end = motion::first_non_blank(b, ls).min(content_end);
    // Register span: whole lines including the terminating newline where one is present.
    let reg_end = if content_end < b.len() && b[content_end] == b'\n' {
        content_end + 1
    } else {
        content_end
    };
    let reg = captured(b, ls, reg_end, true);
    if indent_end >= content_end {
        // Nothing after the indent to delete (empty/blank line): keep the buffer, but still capture the
        // register linewise and drop into Insert at the indent end.
        Plan {
            action: Action::Nop,
            cursor: indent_end,
            mode: Mode::Insert,
            is_edit: false,
            effects: Vec::new(),
            set_register: Some(RegWrite::Edit(reg)),
            set_anchor: None,
            set_mark: None,
        }
    } else {
        edit_yank(
            one(Edit::delete(indent_end, content_end - indent_end)),
            indent_end,
            Mode::Insert,
            hint,
            reg,
        )
    }
}

/// `c{motion}` / `cc`: delete the change span, capture it to a register, and enter Insert. `cc`/`S` is
/// the linewise case that preserves the leading indent (Vim autoindent-like).
fn plan_change(b: &[u8], cur: usize, count: u32, m: &Motion, hint: GroupHint) -> Plan {
    // A change is LINEWISE for `cc`/`S` (Motion::Line), the paragraph objects (`cip`/`cap` — paragraphs are
    // linewise), and an inner block whose braces sit on their own lines (`ci(`/`ci{`). All keep the first
    // line's indent, collapse the rest to one empty line, keep the trailing newline, and enter Insert after
    // the indent — identical machinery, differing only in which line range they act on.
    let linewise_range = if *m == Motion::Line {
        Some(change_range(b, cur, *m, count))
    } else if matches!(m, Motion::InnerParagraph | Motion::AParagraph) {
        let (s, e, _) = crate::editor::range::op_span(b, cur, *m, count); // whole paragraph lines
        let content_end = if e > s && b.get(e - 1) == Some(&b'\n') {
            e - 1
        } else {
            e
        };
        Some((s, content_end))
    } else {
        crate::editor::range::linewise_inner_block(b, cur, *m).map(|(s, e)| (s + 1, e - 1))
    };
    if let Some((ls, content_end)) = linewise_range {
        plan_linewise_change(b, ls, content_end, hint)
    } else {
        let (s, e) = change_range(b, cur, *m, count);
        if s >= e {
            Plan {
                action: Action::Nop,
                cursor: s,
                mode: Mode::Insert,
                is_edit: false,
                effects: Vec::new(),
                set_register: None,
                set_anchor: None,
                set_mark: None,
            }
        } else {
            // The register captures the removed content charwise (a partial-line change like `c$` pastes
            // inline).
            let reg = captured(b, s, e, false);
            edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
        }
    }
}

/// `[op]/pat` (forward) or `[op]?pat` (backward) — search `count` times, then apply the pending operator
/// (move/delete/change/yank) over the charwise-EXCLUSIVE span between the cursor and the match: `[cur, m)`
/// forward, `[m, cur)` backward. An operator that finds no match on the correct side of the cursor is a
/// no-op (Vim rings the bell). Matches nvim v0.12.4 (`d?pat` geometry verified via the oracle harness).
#[allow(clippy::too_many_arguments)]
fn plan_search(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    backward: bool,
    hint: GroupHint,
) -> Plan {
    let opts = st.view.search_options();
    let mut pos = cur;
    for _ in 0..count.max(1) {
        let step = if backward {
            search_bwd(b, pattern, pos, opts)
        } else {
            search_fwd(b, pattern, pos + 1, opts)
        };
        match step {
            Some(m) => pos = m,
            None => break,
        }
    }
    // The operator span is always `[lo, hi)` with `lo < hi`; the direction only decides which end the
    // match is. Bare `Move` just relocates the cursor to the match.
    let (lo, hi) = if backward { (pos, cur) } else { (cur, pos) };
    match op {
        SearchOp::Move => nop(pos, st.view.mode),
        // No match on the operative side of the cursor (forward: match not past cursor; backward: match
        // not before cursor) — abort cleanly rather than emit a reversed/empty edit.
        _ if hi <= lo => nop(cur, st.view.mode),
        SearchOp::Delete => {
            let reg = captured(b, lo, hi, false);
            edit_yank(one(Edit::delete(lo, hi - lo)), lo, st.view.mode, hint, reg)
        }
        SearchOp::Change => {
            let reg = captured(b, lo, hi, false);
            edit_yank(one(Edit::delete(lo, hi - lo)), lo, Mode::Insert, hint, reg)
        }
        SearchOp::Yank => {
            let reg = captured(b, lo, hi, false);
            Plan {
                action: Action::Nop,
                cursor: lo,
                mode: st.view.mode,
                is_edit: false,
                effects: Vec::new(),
                set_register: Some(RegWrite::Yank(reg, (lo, hi))),
                set_anchor: None,
                set_mark: None,
            }
        }
    }
}

/// `gn` / `gN` (the search-match text object, Vim `:help gn`). Selects/operates on the WHOLE match under the
/// cursor, or — when the cursor is not on a match — the next (`gn`) or previous (`gN`) one; `count` advances
/// that many matches (wrapping). The bare form ([`SearchOp::Move`]) enters charwise Visual with the match
/// selected; `Delete`/`Change`/`Yank` operate on the match span `[start, end)` directly.
#[allow(clippy::too_many_arguments)]
fn plan_search_object(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    backward: bool,
    hint: GroupHint,
) -> Plan {
    let spans = match_spans(b, pattern, st.view.search_options());
    if spans.is_empty() {
        return nop(cur, st.view.mode);
    }
    let len = spans.len();
    // The match to start from: forward, the first whose end is past the cursor (the one containing it, or
    // the next), wrapping to the first; backward, the one containing the cursor, else the last that starts
    // before it, wrapping to the last.
    let start_idx = if backward {
        spans
            .iter()
            .rposition(|&(s, e)| s <= cur && cur < e)
            .or_else(|| spans.iter().rposition(|&(s, _)| s < cur))
            .unwrap_or(len - 1)
    } else {
        spans.iter().position(|&(_, e)| e > cur).unwrap_or(0)
    };
    // Advance `count-1` further matches in the direction of travel, wrapping around the document.
    let steps = i64::from(count.max(1)) - 1;
    let delta = if backward { -steps } else { steps };
    let idx = (start_idx as i64 + delta).rem_euclid(len as i64) as usize;
    let (s, e) = spans[idx];
    match op {
        // Bare `gn`/`gN`: enter charwise Visual with the match selected (anchor at its start, cursor on its
        // last char). `commit` applies `set_anchor` after its enter-selection default, so this span wins.
        SearchOp::Move => Plan {
            action: Action::Nop,
            cursor: prev_boundary(b, e),
            mode: Mode::Visual {
                kind: SelectKind::Charwise,
            },
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: Some(s),
            set_mark: None,
        },
        SearchOp::Delete => {
            let reg = captured(b, s, e, false);
            edit_yank(one(Edit::delete(s, e - s)), s, Mode::Normal, hint, reg)
        }
        SearchOp::Change => {
            let reg = captured(b, s, e, false);
            edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
        }
        SearchOp::Yank => {
            let reg = captured(b, s, e, false);
            Plan {
                action: Action::Nop,
                cursor: s,
                mode: st.view.mode,
                is_edit: false,
                effects: Vec::new(),
                set_register: Some(RegWrite::Yank(reg, (s, e))),
                set_anchor: None,
                set_mark: None,
            }
        }
    }
}

/// `gR`-mode typing (Virtual Replace): tab-aware overwrite — over a multi-column tab, insert before it
/// (it shrinks) until its last column, then replace it; at end-of-line, append; else overwrite one char,
/// remembering the original for `<BS>`.
fn plan_virtual_replace_type(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    c: char,
    hint: GroupHint,
) -> Plan {
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes().to_vec();
    let tn = typed.len();
    let le = line_end(b, cur);
    let (edits, push, cursor) = if cur >= le {
        // End-of-line: nothing to overwrite — append (backspace deletes it).
        (one(Edit::insert(cur, typed)), Some(None), cur + tn)
    } else if b[cur] == b'\t' {
        // Over a TAB: it spans `w` virtual columns to the next tabstop. While more than one column
        // remains, INSERT before the tab (it shrinks, backspace regrows it); on the LAST column, replace
        // the tab with the typed char (backspace restores the tab).
        let ls = line_start(b, cur);
        let vcol = motion::vcol_of(b, ls, cur, st.view.indent.tab_width);
        let w = st.view.indent.tab_width.max(1) - (vcol % st.view.indent.tab_width.max(1));
        if w > 1 {
            (one(Edit::insert(cur, typed)), Some(None), cur + tn)
        } else {
            (
                one(Edit::replace(cur, 1, typed)),
                Some(Some(vec![b'\t'])),
                cur + tn,
            )
        }
    } else {
        // Over a normal char: overwrite it (remember the original for `<BS>`), like Replace.
        let nb = next_boundary(b, cur);
        (
            one(Edit::replace(cur, nb - cur, typed)),
            Some(Some(b[cur..nb].to_vec())),
            cur + tn,
        )
    };
    Plan {
        action: Action::ReplaceTxn {
            edits,
            hint,
            push,
            pop: false,
        },
        cursor,
        mode: Mode::VirtualReplace,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

fn nop(cursor: usize, mode: Mode) -> Plan {
    Plan {
        action: Action::Nop,
        cursor,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// A non-editing move (like [`nop`]) that also SETS the Emacs mark at `mark`. Shared by the mark commands
/// (`SetMark` / `ExchangePointMark` / `EmacsMarkWord` / `EmacsBufferEdge`).
fn nop_mark(cursor: usize, mode: Mode, mark: usize) -> Plan {
    Plan {
        set_mark: Some(MarkWrite::Set(mark)),
        ..nop(cursor, mode)
    }
}

/// A non-editing plan carrying an arbitrary `action` — the jumplist / changelist / named-mark steps whose
/// resolution happens in `commit` (the planner supplies a placeholder cursor the action then overrides).
/// Same field shape as [`nop`] but the action is not forced to `Action::Nop`.
fn moved(action: Action, cursor: usize, mode: Mode) -> Plan {
    Plan {
        action,
        ..nop(cursor, mode)
    }
}

fn edit(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// An `edit` that also captures the removed span into a register (`d`/`c`/`x` fill the unnamed slot).
fn edit_yank(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: Some(RegWrite::Edit(reg)),
        set_anchor: None,
        set_mark: None,
    }
}

fn one(e: Edit) -> EditList {
    EditList::new(vec![e]).expect("single edit is always valid")
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

/// Resolve the current Visual/Select selection into a NON-EMPTY byte span `(s, e)` plus its linewise flag
/// (charwise/linewise; blockwise is handled separately by its callers). Shared by the in-place selection
/// operators (`CaseSelection`, `ReplaceSelectionChar`, `PasteSelection`) whose only difference is what they
/// do with the span. On failure it returns the cursor those arms should drop back to Normal at — `cur` when
/// there is no anchor, `s` on an empty span (the collapse point) — matching each arm's prior inline `nop`.
fn visual_span(st: &EditorState, b: &[u8], cur: usize) -> Result<(usize, usize, bool), usize> {
    let anchor = st.view.anchor.ok_or(cur)?;
    let line = st.view.mode.selection() == Some(SelectKind::Linewise);
    let (s, e) = selection_range(b, anchor, cur, line);
    if s >= e {
        return Err(s);
    }
    Ok((s, e, line))
}

/// Like [`edit_yank`] but routes the captured span through [`RegWrite::KillAppend`] — an Emacs kill that
/// accumulates onto the current unnamed entry when it follows another kill (kill-ring behaviour).
fn edit_kill(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: Some(RegWrite::KillAppend(reg)),
        set_anchor: None,
        set_mark: None,
    }
}

/// Recase a word span for [`Command::EmacsCaseWord`]. `Capitalize` upper-cases the first alphanumeric of
/// each word (a run of alphanumerics) and lower-cases the rest, mirroring Emacs `capitalize-region`;
/// `Upcase`/`Downcase` map every character. Unicode-aware (`char::to_uppercase` can widen, e.g. `ß`→`SS`).
fn recase(text: &str, case: WordCase) -> String {
    match case {
        WordCase::Upcase => text.to_uppercase(),
        WordCase::Downcase => text.to_lowercase(),
        WordCase::Toggle => {
            let mut out = String::with_capacity(text.len());
            for ch in text.chars() {
                if ch.is_uppercase() {
                    out.extend(ch.to_lowercase());
                } else if ch.is_lowercase() {
                    out.extend(ch.to_uppercase());
                } else {
                    out.push(ch);
                }
            }
            out
        }
        WordCase::Rot13 => text
            .chars()
            .map(|ch| match ch {
                'a'..='z' => (((ch as u8 - b'a' + 13) % 26) + b'a') as char,
                'A'..='Z' => (((ch as u8 - b'A' + 13) % 26) + b'A') as char,
                other => other,
            })
            .collect(),
        WordCase::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut at_word_start = true;
            for ch in text.chars() {
                if ch.is_alphanumeric() {
                    if at_word_start {
                        out.extend(ch.to_uppercase());
                    } else {
                        out.extend(ch.to_lowercase());
                    }
                    at_word_start = false;
                } else {
                    out.push(ch);
                    at_word_start = true;
                }
            }
            out
        }
    }
}

/// Capture `b[s..e]` as a register value with the given paste geometry.
fn captured(b: &[u8], s: usize, e: usize, linewise: bool) -> Register {
    let bytes = b[s..e].to_vec();
    if linewise {
        Register::linewise(bytes)
    } else {
        Register::charwise(bytes)
    }
}

/// Whether `byte` is horizontal whitespace (a space or tab) — never a newline. The shared predicate for
/// the whitespace-fixup commands (`J`, `just-one-space`, `delete-horizontal-space`, `delete-indentation`).
fn is_hspace(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// The first index `<= from` such that `b[i..from]` is all horizontal whitespace (scan left).
fn hspace_start(b: &[u8], from: usize) -> usize {
    let mut s = from;
    while s > 0 && is_hspace(b[s - 1]) {
        s -= 1;
    }
    s
}

/// The first index `>= from` that is not horizontal whitespace (scan right).
fn hspace_end(b: &[u8], from: usize) -> usize {
    let mut e = from;
    while e < b.len() && is_hspace(b[e]) {
        e += 1;
    }
    e
}

/// The apostrophe-mark (`'{a-z}` / `'.`) target for a mark at `pos`: the first non-blank of that position's
/// LINE (Vim's linewise mark jump), clamped into range.
fn mark_line_target(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    motion::first_non_blank(b, crate::pos::line_start(b, p))
}

/// A charwise mark jump target: snap `pos` to a char boundary, then apply Vim's Normal-mode end-of-line
/// clamp — a mark that lands on (or past) the line's newline pulls back onto the line's last char. This is
/// what makes `` `] `` land on the last inserted/changed char even when the stored mark is the exclusive
/// insert end-caret or the trailing '\n' of a linewise span (matching Neovim's on-jump clamp).
fn mark_char_target(b: &[u8], pos: usize) -> usize {
    let p = motion::snap(b, pos.min(b.len()));
    let le = line_end(b, p);
    let ls = line_start(b, p);
    if p >= le && ls < le {
        prev_boundary(b, le)
    } else {
        p
    }
}

/// The active Emacs region as an ordered `[s, e)` byte span, or `None` when no mark is set or the mark
/// coincides with point (a degenerate region — the region commands are inert then). Shared by
/// `KillRegion` / `CopyRegion` / `EmacsCaseRegion`.
fn active_region(st: &EditorState, cur: usize) -> Option<(usize, usize)> {
    match st.view.mark {
        Some(m) if m != cur => Some((cur.min(m), cur.max(m))),
        _ => None,
    }
}

/// Recase `b[s..e]` with `case`, or `None` when the span is empty or not valid UTF-8 (a grapheme-boundary
/// span always is). Shared by `EmacsCaseWord` (forward-word span) and `EmacsCaseRegion` (the region).
fn recase_span(b: &[u8], s: usize, e: usize, case: WordCase) -> Option<Vec<u8>> {
    if e <= s {
        return None;
    }
    std::str::from_utf8(&b[s..e])
        .ok()
        .map(|span| recase(span, case).into_bytes())
}

/// Plan a `[count]J` / `[count]gJ` join. `count` joins `count-1` seams (Vim: `J`/`2J` = one join, `3J` =
/// two), leaving the cursor on the LAST join. `J` inserts one space per seam (suppressed before `)`, after
/// trailing whitespace, or on an empty line) and strips the next line's leading blanks; `gJ` removes only
/// the newline, keeping leading whitespace. The seams are disjoint in original coordinates, so they apply as
/// one `EditList`; `removed` maps each seam's original offset to its post-edit position for the cursor.
fn plan_join(
    b: &[u8],
    cur: usize,
    count: u32,
    mode: Mode,
    hint: GroupHint,
    no_space: bool,
) -> Plan {
    let joins = count.max(2) - 1;
    let mut edits: Vec<Edit> = Vec::new();
    let mut cursor = cur;
    let mut removed = 0usize;
    let mut le = line_end(b, cur);
    for _ in 0..joins {
        if le >= b.len() {
            break; // no next line to join
        }
        let (del_start, del_len, sep): (usize, usize, Vec<u8>) = if no_space {
            (le, 1, Vec::new()) // gJ: remove only the newline, keep the next line's leading whitespace
        } else {
            let ws_end = hspace_end(b, le + 1);
            let next_is_close = ws_end < b.len() && b[ws_end] == b')';
            let cur_ends_ws = le > line_start(b, le) && is_hspace(b[le - 1]);
            let cur_empty = le == line_start(b, le);
            let sep = if next_is_close || cur_ends_ws || cur_empty {
                Vec::new()
            } else {
                b" ".to_vec()
            };
            (le, ws_end - le, sep)
        };
        cursor = del_start - removed; // Vim rests the cursor on the join seam
        removed += del_len - sep.len();
        edits.push(Edit::replace(del_start, del_len, sep));
        le = line_end(b, del_start + del_len); // end of the next line to join (original coords)
    }
    if edits.is_empty() {
        return nop(cur, mode);
    }
    edit(
        EditList::new(edits).expect("join seams are disjoint and ordered"),
        cursor,
        mode,
        hint,
    )
}

pub fn plan(st: &EditorState, cmd: &Command) -> Plan {
    let b = st.bytes();
    let cur = st.view.cursor;
    // Undo/redo restore DOCUMENT state, not mode. The engine only reaches them from Normal or from
    // Insert via `i_CTRL-O` (Vim's `u` in Visual is not an undo), so preserving Insert makes
    // `i_CTRL-O u` return to Insert — the one-shot's resume (KL-OBL-5) — while everything else
    // collapses to Normal exactly as before.
    let undo_resume_mode = if st.view.mode == Mode::Insert {
        Mode::Insert
    } else {
        Mode::Normal
    };
    let hint = if st.view.last_was_edit {
        GroupHint::Continue
    } else {
        GroupHint::BreakBefore
    };

    match cmd {
        // `h`/`l` move by whole grapheme cluster so the cursor never lands mid-emoji/ZWJ/combining
        // (F-002 #2). Operator/selection internals below still step by char boundary — grapheme-aware
        // deletion is a follow-up; this slice makes the CURSOR stay synced to user-perceived chars.
        Command::MoveLeft => nop(prev_grapheme(b, cur), st.view.mode),
        Command::MoveRight => nop(next_grapheme(b, cur), st.view.mode),
        Command::MoveLineStart => nop(line_start(b, cur), st.view.mode),
        Command::MoveLineEnd => nop(line_end(b, cur), st.view.mode),
        Command::MoveUp => nop(
            motion::vmove(b, cur, 1, false, st.view.curswant),
            st.view.mode,
        ),
        Command::MoveDown => nop(
            motion::vmove(b, cur, 1, true, st.view.curswant),
            st.view.mode,
        ),
        Command::EnterInsert => nop(cur, Mode::Insert),
        // `gi` — resume Insert at the last-insert position (snapped), or the buffer start if none yet.
        Command::InsertAtLastInsert => {
            let at = st
                .view
                .last_insert()
                .map_or(0, |p| motion::snap(b, p.min(b.len())));
            nop(at, Mode::Insert)
        }
        Command::EnterInsertAfter => nop(next_boundary(b, cur), Mode::Insert),
        Command::InsertLineStart => nop(motion::first_non_blank(b, cur), Mode::Insert),
        Command::AppendLineEnd => nop(line_end(b, cur), Mode::Insert),
        Command::OpenBelow => {
            let le = line_end(b, cur);
            edit(
                one(Edit::insert(le, b"\n".to_vec())),
                le + 1,
                Mode::Insert,
                hint,
            )
        }
        Command::OpenAbove => {
            let ls = line_start(b, cur);
            edit(
                one(Edit::insert(ls, b"\n".to_vec())),
                ls,
                Mode::Insert,
                hint,
            )
        }
        Command::EnterNormal => {
            // `<Esc>` closing a blockwise insert-replicate session replicates the top row's typed text down
            // the block (see `block_replicate`); the session is cleared in `commit` as Insert is left.
            if st.view.mode == Mode::Insert {
                if let Some(session) = st.view.block_insert {
                    return block_replicate(b, cur, session, hint);
                }
                // Autoindent cleanup (Vim): if this Insert session opened a line with auto-indent and
                // nothing non-blank was typed on it, leaving Insert removes the indent so no trailing
                // whitespace is left. Guarded to ONLY a line that is entirely blank with the caret at its
                // end — exactly the auto-indent-leftover shape — so user-typed content is never touched.
                if st.view.auto_indent_pending {
                    let ls = line_start(b, cur);
                    let le = line_end(b, cur);
                    if cur == le && le > ls && b[ls..le].iter().all(|&c| c == b' ' || c == b'\t') {
                        return edit(one(Edit::delete(ls, le - ls)), ls, Mode::Normal, hint);
                    }
                }
            }
            // Vim: leaving Insert OR Replace nudges the cursor left one, but never before the line start.
            // Leaving Visual (Esc) just collapses the selection in place — no nudge.
            if matches!(
                st.view.mode,
                Mode::Insert | Mode::Replace | Mode::VirtualReplace
            ) {
                let ls = line_start(b, cur);
                let c = if cur > ls { prev_boundary(b, cur) } else { cur };
                nop(c, Mode::Normal)
            } else {
                nop(cur, Mode::Normal)
            }
        }
        // F-011: mode-only transitions; the frontend owns the PTY + scrollback (the core never edits a
        // terminal buffer). Cursor is unchanged — a terminal buffer's document is an empty placeholder.
        Command::EnterTerminal => nop(cur, Mode::Terminal),
        Command::EnterTerminalNormal => nop(cur, Mode::TerminalNormal),
        Command::EnterReplace => nop(cur, Mode::Replace),
        Command::ReplaceType(c) => {
            let mut buf = [0u8; 4];
            let typed = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let tn = typed.len();
            let le = line_end(b, cur);
            let (edits, push) = if cur < le {
                // Overwrite the char under the cursor; remember its original bytes for `<BS>` restore.
                let nb = next_boundary(b, cur);
                (
                    one(Edit::replace(cur, nb - cur, typed)),
                    Some(Some(b[cur..nb].to_vec())),
                )
            } else {
                // At end-of-line there is nothing to overwrite: append, remembered as `None` (backspace deletes).
                (one(Edit::insert(cur, typed)), Some(None))
            };
            Plan {
                action: Action::ReplaceTxn {
                    edits,
                    hint,
                    push,
                    pop: false,
                },
                cursor: cur + tn,
                mode: Mode::Replace,
                is_edit: true,
                effects: Vec::new(),
                set_register: None,
                set_anchor: None,
                set_mark: None,
            }
        }
        // Shared by Replace (`R`) and Virtual Replace (`gR`) — the restore stack is identical; the mode is
        // preserved so `<BS>` stays in whichever replace mode is active.
        Command::ReplaceBackspace => match st.view.replace_stack.last() {
            // At the session start `<BS>` only moves the cursor left (Vim does not restore past the start).
            None => {
                let ls = line_start(b, cur);
                let c = if cur > ls { prev_boundary(b, cur) } else { cur };
                nop(c, st.view.mode)
            }
            Some(entry) => {
                let start = prev_boundary(b, cur); // the last typed char occupies [start, cur)
                let edits = match entry {
                    Some(orig) => one(Edit::replace(start, cur - start, orig.clone())),
                    None => one(Edit::delete(start, cur - start)),
                };
                Plan {
                    action: Action::ReplaceTxn {
                        edits,
                        hint,
                        push: None,
                        pop: true,
                    },
                    cursor: start,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: None,
                    set_mark: None,
                }
            }
        },
        Command::EnterVirtualReplace => nop(cur, Mode::VirtualReplace),
        Command::VirtualReplaceType(c) => plan_virtual_replace_type(st, b, cur, *c, hint),
        Command::InsertChar(c) => {
            let mut buf = [0u8; 4];
            let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = bytes.len();
            edit(one(Edit::insert(cur, bytes)), cur + n, Mode::Insert, hint)
        }
        // `i_CTRL-R{reg}` — splice the named register's raw bytes at the caret, staying in Insert with the
        // cursor after the inserted text. An empty register is a no-op. `"` reads the unnamed register (via
        // `get`'s fallback). The bytes are inserted verbatim (linewise registers keep their trailing `\n`).
        Command::InsertRegister(name) => {
            let bytes = st.view.registers.get(Some(*name)).text().to_vec();
            if bytes.is_empty() {
                return nop(cur, Mode::Insert);
            }
            let n = bytes.len();
            edit(one(Edit::insert(cur, bytes)), cur + n, Mode::Insert, hint)
        }
        // `i_CTRL-R=<expr><CR>` — evaluate the expression (a minimal-honest arithmetic/string calculator,
        // NOT full Vimscript) and splice the formatted result at the caret, staying in Insert. A malformed
        // or unsupported expression evaluates to the empty string and inserts nothing (Vim's degrade).
        Command::InsertEval(e) => {
            let bytes = crate::expr::eval_or_empty(e).into_bytes();
            if bytes.is_empty() {
                return nop(cur, Mode::Insert);
            }
            let n = bytes.len();
            edit(one(Edit::insert(cur, bytes)), cur + n, Mode::Insert, hint)
        }
        // `i_CTRL-W` — delete the word before the caret (within the current line), staying in Insert.
        Command::InsertDeleteWordBack => {
            let ls = line_start(b, cur);
            if cur <= ls {
                return nop(cur, Mode::Insert);
            }
            let wb = motion::target(b, cur, Motion::WordBack, 1).max(ls);
            // If the word motion did not move (e.g. only whitespace precedes), delete one grapheme instead.
            let start = if wb < cur { wb } else { prev_boundary(b, cur) };
            edit(
                one(Edit::delete(start, cur - start)),
                start,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-U` — delete to the line's first non-blank; if already at/before it, delete the indent too.
        Command::InsertDeleteToLineStart => {
            let ls = line_start(b, cur);
            if cur <= ls {
                return nop(cur, Mode::Insert);
            }
            let mut fnb = ls;
            while fnb < cur && matches!(b[fnb], b' ' | b'\t') {
                fnb += 1;
            }
            let start = if cur > fnb { fnb } else { ls };
            edit(
                one(Edit::delete(start, cur - start)),
                start,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-T` — indent the current line by one shiftwidth; the caret rides right with the text.
        Command::InsertIndent => {
            let ls = line_start(b, cur);
            let unit = st.indent_unit();
            edit(
                one(Edit::insert(ls, unit.clone())),
                cur + unit.len(),
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-D` — dedent the current line by one shiftwidth; the caret rides left (never before the line
        // start). A no-op when there is no leading whitespace.
        Command::InsertDedent => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let remove = shift_left_remove(b, ls, le, st.view.indent.tab_width);
            if remove == 0 {
                return nop(cur, Mode::Insert);
            }
            let cursor = cur.saturating_sub(remove).max(ls);
            edit(one(Edit::delete(ls, remove)), cursor, Mode::Insert, hint)
        }
        // `<Tab>` in Insert: insert whitespace at the caret to the next tabstop. Space style (`expandtab`)
        // inserts `tab_width - (vcol % tab_width)` spaces so the caret lands on a tabstop column even when
        // it started mid-line; tab style inserts one `\t`. Unlike `i_CTRL-T` this is caret-relative.
        Command::InsertTab => {
            let ins: Vec<u8> = match st.view.indent.style {
                IndentStyle::Tab => vec![b'\t'],
                IndentStyle::Space => {
                    let ts = st.view.indent.tab_width.max(1);
                    let vcol = crate::motion::vcol_of(b, line_start(b, cur), cur, ts);
                    vec![b' '; ts - (vcol % ts)]
                }
            };
            let cursor = cur + ins.len();
            edit(one(Edit::insert(cur, ins)), cursor, Mode::Insert, hint)
        }
        Command::InsertNewline => edit(
            one(Edit::insert(cur, b"\n".to_vec())),
            cur + 1,
            Mode::Insert,
            hint,
        ),
        // `o`/`O`/`<CR>` with a tree-suggested indent (F-015 Phase 2): open a line whose leading whitespace
        // is `level × unit`, cursor after it. `Above` inserts `<indent>\n` before the line (cursor on the new
        // line above); `Below`/`Split` insert `\n<indent>` (cursor on the new line below / after the split).
        // `level: 0` degrades to a plain open, so this also serves as the non-tree fallback.
        Command::OpenLineIndent { kind, level } => {
            let unit = st.indent_unit();
            let pad: Vec<u8> = unit
                .iter()
                .cycle()
                .take(unit.len() * level)
                .copied()
                .collect();
            let (at, mut ins, cursor) = match kind {
                OpenKind::Above => {
                    let ls = line_start(b, cur);
                    (ls, Vec::with_capacity(pad.len() + 1), ls + pad.len())
                }
                OpenKind::Below => {
                    let le = line_end(b, cur);
                    (le, Vec::with_capacity(pad.len() + 1), le + 1 + pad.len())
                }
                OpenKind::Split => (cur, Vec::with_capacity(pad.len() + 1), cur + 1 + pad.len()),
            };
            if matches!(kind, OpenKind::Above) {
                ins.extend_from_slice(&pad);
                ins.push(b'\n');
            } else {
                ins.push(b'\n');
                ins.extend_from_slice(&pad);
            }
            edit(one(Edit::insert(at, ins)), cursor, Mode::Insert, hint)
        }
        // A closer (`}`/`)`/`]`) typed as the line's sole leading content realigns the line to its matching
        // opener's indent, then inserts the closer — smartindent-like (F-015 Phase 3a). Deterministic bytes
        // bracket-match; with content already before the cursor, or no matching opener, it is a plain insert.
        Command::InsertCloser { ch } => {
            let mut buf = [0u8; 4];
            let typed = ch.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = typed.len();
            let ls = line_start(b, cur);
            let all_ws = b[ls..cur].iter().all(|&x| x == b' ' || x == b'\t');
            let pair = match ch {
                '}' => Some((b'{', b'}')),
                ')' => Some((b'(', b')')),
                ']' => Some((b'[', b']')),
                _ => None,
            };
            let pad = if all_ws {
                pair.and_then(|(open, close)| matching_opener_pad(b, cur, open, close))
            } else {
                None
            };
            match pad {
                Some(pad) => {
                    // One edit: replace the leading whitespace [ls, cur) with `pad ++ closer`, so any content
                    // after the cursor is untouched and the whole thing is a single insert-coalesced group.
                    let cursor = ls + pad.len() + n;
                    let mut repl = pad;
                    repl.extend_from_slice(&typed);
                    edit(
                        one(Edit::replace(ls, cur - ls, repl)),
                        cursor,
                        Mode::Insert,
                        hint,
                    )
                }
                None => edit(one(Edit::insert(cur, typed)), cur + n, Mode::Insert, hint),
            }
        }
        Command::DeleteBack => {
            if cur == 0 {
                nop(cur, st.view.mode)
            } else {
                let p = prev_boundary(b, cur);
                edit(one(Edit::delete(p, cur - p)), p, st.view.mode, hint)
            }
        }
        Command::DeleteUnder(count) => {
            // `{count}x`: delete `count` chars from the cursor, clamped at end-of-line (Vim). Fewer than
            // `count` chars left deletes to EOL. The removed span fills the unnamed register (charwise).
            let le = line_end(b, cur);
            let end = advance_n(b, cur, *count, le);
            if end <= cur {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, cur, end, false);
                edit_yank(
                    one(Edit::delete(cur, end - cur)),
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
                edit(one(Edit::delete(cur, end - cur)), cur, st.view.mode, hint)
            }
        }
        Command::ReplaceChar(count, c) => {
            // `{count}r{ch}`: replace `count` chars with `ch`. Per Vim it is a NO-OP if fewer than
            // `count` chars remain on the line (never a partial replace). Cursor lands on the last one.
            let le = line_end(b, cur);
            let (end, reached) = advance_n_checked(b, cur, *count, le);
            if !reached {
                nop(cur, st.view.mode)
            } else if *c == '\n' {
                // `{count}r<CR>`: replace the count chars with a SINGLE line break (Vim splits the line),
                // leaving the cursor on the first char of the new line — never `count` newlines.
                edit(
                    one(Edit::replace(cur, end - cur, b"\n".to_vec())),
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
                    one(Edit::replace(cur, end - cur, bytes)),
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
            let end = advance_n(b, cur, *count, le);
            if end <= cur {
                nop(cur, st.view.mode)
            } else {
                // `[cur, end)` is on char boundaries (`advance_n` walks boundaries), so it is valid UTF-8.
                let src =
                    std::str::from_utf8(&b[cur..end]).expect("cursor span is on char boundaries");
                let mut flipped: Vec<u8> = Vec::with_capacity(end - cur);
                for ch in src.chars() {
                    if ch.is_uppercase() {
                        for c in ch.to_lowercase() {
                            let mut buf = [0u8; 4];
                            flipped.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                    } else if ch.is_lowercase() {
                        for c in ch.to_uppercase() {
                            let mut buf = [0u8; 4];
                            flipped.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
                    } else {
                        let mut buf = [0u8; 4];
                        flipped.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
                if flipped == b[cur..end] {
                    nop(end, st.view.mode) // nothing was a letter: `~` just moves right
                } else {
                    let cursor = cur + flipped.len();
                    edit(
                        one(Edit::replace(cur, end - cur, flipped)),
                        cursor,
                        st.view.mode,
                        hint,
                    )
                }
            }
        }
        Command::JoinLines(count) => plan_join(b, cur, *count, st.view.mode, hint, false),
        Command::JoinLinesNoSpace(count) => plan_join(b, cur, *count, st.view.mode, hint, true),
        Command::IncrementNumber(delta) => {
            // `CTRL-A`/`CTRL-X`: adjust the number at or after the cursor on the current line. A `0x`-prefixed
            // hex literal increments in hex (`0x1f`→`0x20`); otherwise a decimal (with optional `-` sign).
            let ls = crate::pos::line_start(b, cur);
            let le = line_end(b, cur);
            match incr_number(b, ls, le, cur, *delta) {
                Some((start, old_len, bytes, cursor)) => edit(
                    one(Edit::replace(start, old_len, bytes)),
                    cursor,
                    st.view.mode,
                    hint,
                ),
                None => nop(cur, st.view.mode), // no number on the rest of the line
            }
        }
        // Visual `CTRL-A`/`CTRL-X` (and `g CTRL-A`/`g CTRL-X`): increment the first number per selected line.
        // Plain form adds `delta` to every line; `sequential` (the `g` form) adds `delta`, `2·delta`, `3·delta`…
        // to successive numbered lines — the classic "turn a column of 1s into 1,2,3…".
        //
        // The unifying rule across all three shapes: on each line, search from the SELECTION'S LEFT EDGE
        // (`incr_number` finds the first number at/after it, expanding to the whole number). This makes a
        // column of numbers that are NOT the first on their line increment correctly — the whole point of the
        // feature — for both `CTRL-V` (block left column) and `v` (the start column on the first line):
        //   - BLOCKWISE (`CTRL-V`): each row's left-column byte offset (`block_rows`).
        //   - LINEWISE (`V`): the line start (whole lines).
        //   - CHARWISE (`v`): the selection start on the first line, the line start on continuation lines.
        // (Charwise does not yet CLIP a number sitting past the selection end on the last line — a rare edge,
        // noted as a follow-up.)
        Command::IncrementSelection { delta, sequential } => {
            let Some(anchor) = st.view.anchor else {
                return nop(cur, Mode::Normal);
            };
            // Per selected line: (line_start, search_from).
            let lines: Vec<(usize, usize)> = match st.view.mode.selection() {
                Some(SelectKind::Blockwise) => block_rows(b, anchor, cur)
                    .0
                    .into_iter()
                    .map(|(from, _end)| (line_start(b, from), from))
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
                let this = if *sequential { delta * steps } else { *delta };
                if let Some((start, old_len, bytes, _cursor)) = incr_number(b, ls, le, from, this) {
                    edits.push(Edit::replace(start, old_len, bytes));
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
        Command::GotoLastChange => {
            // `` `. `` — move to the last change position (snapped into range). No-op before any edit.
            match st.view.last_change() {
                Some(pos) => nop(motion::snap(b, pos), st.view.mode),
                None => nop(cur, st.view.mode),
            }
        }
        // `'.` — LINEWISE to the first non-blank of the last change's line. No-op before any edit.
        Command::GotoLastChangeLine => match st.view.last_change() {
            Some(pos) => nop(mark_line_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `` `[ `` — to the FIRST char of the last changed/yanked text. No-op before any change/yank.
        Command::GotoChangeMarkStart => match st.view.change_mark_start() {
            Some(pos) => nop(mark_char_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `` `] `` — to the LAST char of the last changed/yanked text (EOL-clamped). No-op if unset.
        Command::GotoChangeMarkEnd => match st.view.change_mark_end() {
            Some(pos) => nop(mark_char_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `'[` — LINEWISE to the first non-blank of the `[` mark's line. No-op if unset.
        Command::GotoChangeMarkStartLine => match st.view.change_mark_start() {
            Some(pos) => nop(mark_line_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `']` — LINEWISE to the first non-blank of the `]` mark's line. No-op if unset.
        Command::GotoChangeMarkEndLine => match st.view.change_mark_end() {
            Some(pos) => nop(mark_line_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `` `` `` — jump to the context mark (position before the latest jump). No-op before any jump. It is
        // itself a jump (`apply_command` records the leave position AFTER commit), so a repeat toggles.
        Command::GotoContextMark => match st.view.context_mark() {
            Some(pos) => nop(motion::snap(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `''` — LINEWISE to the first non-blank of the context mark's line. No-op before any jump.
        Command::GotoContextMarkLine => match st.view.context_mark() {
            Some(pos) => nop(mark_line_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `g;`/`g,` — step the change list. The cursor here is a placeholder; `commit` steps `change_idx`
        // (a mutation the pure planner cannot make) and overrides it with the resolved change position.
        Command::GotoOlderChange => moved(Action::JumpChange { older: true }, cur, st.view.mode),
        Command::GotoNewerChange => moved(Action::JumpChange { older: false }, cur, st.view.mode),
        // `CTRL-O`/`CTRL-I` — step the jumplist. `commit` mutates it (the planner is pure); the placeholder
        // cursor is overridden with the resolved jump position.
        Command::GotoOlderJump => moved(Action::JumpList { older: true }, cur, st.view.mode),
        Command::GotoNewerJump => moved(Action::JumpList { older: false }, cur, st.view.mode),
        // `m{a-z}` — install a named mark at the cursor. The cursor stays; `commit` writes the mark table.
        Command::SetNamedMark(ch) => moved(Action::SetNamedMark { ch: *ch }, cur, st.view.mode),
        // `` `{a-z} `` — jump to a named mark (snapped). No-op if unset.
        Command::GotoNamedMark(ch) => match st.view.named_mark(*ch) {
            Some(pos) => nop(motion::snap(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `'{a-z}` — LINEWISE to the first non-blank of a named mark's line. No-op if unset.
        Command::GotoNamedMarkLine(ch) => match st.view.named_mark(*ch) {
            Some(pos) => nop(mark_line_target(b, pos), st.view.mode),
            None => nop(cur, st.view.mode),
        },
        // `` d`{a-z} `` / `d'{a-z}` (and `c`/`y`): operate from the cursor to a named mark. `` ` `` is
        // exclusive charwise; `'` is linewise over the line range. No-op if the mark is unset.
        Command::OpToMark { op, name, linewise } => {
            let Some(mark) = st.view.named_mark(*name) else {
                return nop(cur, st.view.mode);
            };
            let mark = mark.min(b.len());
            let (s, e) = if *linewise {
                let start = line_start(b, cur.min(mark));
                let le = line_end(b, cur.max(mark));
                (start, if le < b.len() { le + 1 } else { le })
            } else {
                (cur.min(mark), cur.max(mark))
            };
            let reg = captured(b, s, e, *linewise);
            match op {
                OpKind::Yank => Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: Mode::Normal,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg, (s, e))),
                    set_anchor: None,
                    set_mark: None,
                },
                OpKind::Delete if s < e => {
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Normal, hint, reg)
                }
                OpKind::Change if s < e => {
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
                }
                OpKind::Change => nop(s, Mode::Insert),
                OpKind::Delete => nop(s, Mode::Normal),
            }
        }
        // `CTRL-G u`: break the undo group. A pure nop (is_edit = false), so `commit` sets
        // `last_was_edit = false` and the NEXT edit's `GroupHint` becomes `BreakBefore` — a fresh undo
        // group starts here mid-insert-session (Vim `i_CTRL-G_u`). Cursor and mode are untouched.
        Command::BreakUndo => nop(cur, st.view.mode),
        Command::Move(count, m) => {
            // A text object issued in a selection mode (`viw`, `vi(`) sets BOTH ends: anchor at the object's
            // start, cursor on its last char (inclusive selection). A bare motion only moves the cursor.
            if st.view.mode.selection().is_some() && is_text_object(*m) {
                let (s, e) = motion::char_span(b, cur, *m, *count);
                if s >= e {
                    return nop(cur, st.view.mode);
                }
                // `ip`/`ap` are LINEWISE objects: selecting one in Visual switches the selection to
                // linewise (Vim), so a following `d`/`y` gets linewise register geometry. Other objects
                // (`iw`, `i(`, `is`, …) keep the current visual kind.
                let obj_mode = if matches!(m, Motion::InnerParagraph | Motion::AParagraph) {
                    match st.view.mode {
                        Mode::Select { .. } => Mode::Select {
                            kind: SelectKind::Linewise,
                        },
                        _ => Mode::Visual {
                            kind: SelectKind::Linewise,
                        },
                    }
                } else {
                    st.view.mode
                };
                return Plan {
                    action: Action::Nop,
                    cursor: prev_boundary(b, e),
                    mode: obj_mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: Some(s),
                    set_mark: None,
                };
            }
            // Vertical motions honour the sticky desired column (curswant) so `j`/`k` keep the wanted
            // column through shorter interior lines; every other motion computes its own landing.
            let target = match m {
                Motion::Up => motion::vmove(b, cur, *count, false, st.view.curswant),
                Motion::Down => motion::vmove(b, cur, *count, true, st.view.curswant),
                _ => motion::target(b, cur, *m, *count),
            };
            // Normal mode never rests on a non-empty line's trailing newline: `j`/`k` overshooting a short
            // line (or `$`'s MAXCOL), AND `w`/`W` past the last word (which move to end-of-line when there
            // is no next word), pull back onto the last char. Restricted to these motions — `^` on an
            // all-blank line legitimately rests at the line end, and the Emacs word motions are separate
            // variants (`EmacsWord*`) with between-char gravity. Insert/Visual keep the past-end column.
            let target = if matches!(
                m,
                Motion::Up | Motion::Down | Motion::WordFwd | Motion::BigWordFwd
            ) && matches!(st.view.mode, Mode::Normal)
            {
                let ls = line_start(b, target);
                let le = line_end(b, target);
                if target == le && le > ls {
                    prev_boundary(b, le)
                } else {
                    target
                }
            } else {
                target
            };
            nop(target, st.view.mode)
        }
        Command::Delete(count, m) => {
            let (s, e, linewise) = op_span(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else if linewise && e == b.len() && s > 0 && b[e - 1] != b'\n' {
                // Deleting the buffer's LAST line while earlier lines remain, where that line has no
                // trailing newline (so the span itself does not already eat one): Vim removes the line
                // entirely (not blank it in place), which means also dropping the newline that ends the
                // previous line, then moving the cursor up to the new last line. The register still holds
                // the line content linewise ("beta\n"), not the leading newline we splice away.
                // (`dG` on a newline-terminated buffer keeps its own trailing newline and takes the plain
                // branch, since the span already ends in `\n`.)
                let reg = captured(b, s, e, true);
                let del_start = s - 1; // the '\n' terminating the previous line (s is a line start, s > 0)
                let cursor = line_start(b, del_start);
                edit_yank(
                    one(Edit::delete(del_start, e - del_start)),
                    cursor,
                    st.view.mode,
                    hint,
                    reg,
                )
            } else {
                let reg = captured(b, s, e, linewise);
                edit_yank(one(Edit::delete(s, e - s)), s, st.view.mode, hint, reg)
            }
        }
        Command::Change(count, m) => plan_change(b, cur, *count, m, hint),
        Command::Yank(count, m) => {
            let (s, e, linewise) = op_span(b, cur, *m, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                // Yank captures without editing; Vim leaves the cursor at the start of the yanked span.
                let reg = captured(b, s, e, linewise);
                Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: st.view.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg, (s, e))),
                    set_anchor: None,
                    set_mark: None,
                }
            }
        }
        Command::CaseMotion {
            count,
            motion,
            case,
        } => {
            let (s, e, _linewise) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                // `op_span` walks char boundaries, so `[s, e)` is valid UTF-8.
                let src =
                    std::str::from_utf8(&b[s..e]).expect("operator span is on char boundaries");
                let recased = recase(src, *case).into_bytes();
                if recased == b[s..e] {
                    nop(s, st.view.mode) // no letters in range: `gu`/`gU`/`g~` just move to the start
                } else {
                    edit(one(Edit::replace(s, e - s, recased)), s, st.view.mode, hint)
                }
            }
        }
        // Forced-wise operator (`dvj`, `dVe`, `yv}`): compute the reshaped span, then apply the operator
        // like its plain form. Change uses the charwise change shape (delete span + Insert); the linewise
        // indent-preserving `cc` special-case is not reused here (forced-linewise change is vanishingly rare).
        Command::OpForced {
            op,
            count,
            motion,
            wise,
        } => {
            // Forced blockwise (`d<C-v>j`): the cursor and the motion target are the block's two corners.
            if *wise == ForcedWise::Blockwise {
                let target = motion::target(b, cur, *motion, *count);
                return block_op(b, cur, target, *op, hint);
            }
            let (s, e, linewise) = forced_span(b, cur, *motion, *count, *wise);
            match op {
                OpKind::Delete if s < e => {
                    let reg = captured(b, s, e, linewise);
                    edit_yank(one(Edit::delete(s, e - s)), s, st.view.mode, hint, reg)
                }
                OpKind::Yank if s < e => {
                    let reg = captured(b, s, e, linewise);
                    Plan {
                        action: Action::Nop,
                        cursor: s,
                        mode: st.view.mode,
                        is_edit: false,
                        effects: Vec::new(),
                        set_register: Some(RegWrite::Yank(reg, (s, e))),
                        set_anchor: None,
                        set_mark: None,
                    }
                }
                OpKind::Change if s < e => {
                    let reg = captured(b, s, e, linewise);
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
                }
                // Empty span: delete/yank are a clean no-op; change still drops into Insert (Vim).
                OpKind::Change => nop(s, Mode::Insert),
                _ => nop(cur, st.view.mode),
            }
        }
        Command::ShiftRight(count) => plan_shift(st, cur, *count, true, hint),
        Command::ShiftLeft(count) => plan_shift(st, cur, *count, false, hint),
        Command::ShiftMotion {
            left,
            count,
            motion,
        } => {
            // Resolve the motion's byte span, then shift every LINE it touches (Vim `>` is always linewise).
            let (s, e, _) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let first_line = crate::pos::line_of(b, s);
                let last_line = crate::pos::line_of(b, e - 1);
                let lines = (last_line - first_line + 1) as u32;
                plan_shift(st, line_start(b, s), lines, !*left, hint)
            }
        }
        Command::Reindent { count, motion } => {
            // `=` over a motion — reindent every LINE it touches (always linewise, like `>`).
            let (s, e, _) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let first = crate::pos::line_of(b, s);
                let last = crate::pos::line_of(b, e - 1);
                plan_reindent(st, first, last, hint)
            }
        }
        Command::Format {
            count,
            motion,
            keep_cursor,
        } => plan_format(st, cur, *count, *motion, *keep_cursor, hint),
        Command::SetIndents {
            first_line,
            last_line,
            levels,
        } => plan_set_indents(st, *first_line, *last_line, levels, hint),
        // Paste reads the pending register (`"xp`) or the unnamed slot; `commit` clears the pending slot.
        Command::Paste {
            after,
            count,
            move_after,
        } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
            *move_after,
        ),
        // `]p`/`[p` — indent-adjusting paste. For a linewise register, reindent its lines to the current
        // line's indent before pasting; a charwise/blockwise register pastes unchanged (Vim `]p` = `p`).
        Command::PasteIndent { after, count } => {
            let reg = st.view.registers.get(st.view.pending_register);
            if reg.is_linewise() {
                let (target_cols, _) = indent_cols(
                    &b[line_start(b, cur)..line_end(b, cur)],
                    st.view.indent.tab_width,
                );
                let adjusted = reindent_register(
                    reg.text(),
                    target_cols,
                    st.view.indent.tab_width,
                    st.view.indent.style,
                );
                let tmp = Register::linewise(adjusted);
                paste(
                    b,
                    cur,
                    st.view.mode,
                    &tmp,
                    *after,
                    *count,
                    st.view.caret,
                    false,
                )
            } else {
                paste(
                    b,
                    cur,
                    st.view.mode,
                    reg,
                    *after,
                    *count,
                    st.view.caret,
                    false,
                )
            }
        }
        // `"x` — install the one-shot pending register. A pure state set: no edit, no cursor/mode change.
        Command::SetRegister(name) => Plan {
            action: Action::SetPending(*name),
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        // `"=<expr><CR>` — evaluate the expression, store the formatted result in the `"=` slot, and arm `=`
        // as the pending register so the following `p`/`P` pastes it. An empty result (malformed/unsupported
        // expression) makes that paste a no-op (Vim's degrade). Like `SetRegister`, a pure state set.
        Command::SetExprRegister(e) => Plan {
            action: Action::SetExprPending(crate::expr::eval_or_empty(e)),
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::EnterVisual { kind } => nop(cur, Mode::Visual { kind: *kind }),
        // CTRL-G toggle: enter Select over the same selection. The anchor is preserved because both are
        // selection modes, so `commit` keeps it (see the (true, true) arm there).
        Command::EnterSelect { kind } => nop(cur, Mode::Select { kind: *kind }),
        Command::ReselectVisual => match st.view.last_visual {
            // Restore the remembered selection: re-enter Visual with the stored kind, put the cursor on the
            // active end, and install the stored anchor (via `set_anchor`, which `commit` applies after its
            // enter-selection bookkeeping). No prior selection → a clean no-op (Vim rings the bell).
            Some((anchor, active, kind)) => Plan {
                action: Action::Nop,
                cursor: active,
                mode: Mode::Visual { kind },
                is_edit: false,
                effects: Vec::new(),
                set_register: None,
                set_anchor: Some(anchor),
                set_mark: None,
            },
            None => nop(cur, st.view.mode),
        },
        Command::ReplaceSelection(c) => {
            // Select's `open/replace-selection`: delete the selection, insert the char, enter Insert.
            // Blockwise Select-replace is out of scope for this slice; a block here is treated charwise.
            let line = matches!(
                st.view.mode,
                Mode::Visual {
                    kind: SelectKind::Linewise
                } | Mode::Select {
                    kind: SelectKind::Linewise
                }
            );
            let mut buf = [0u8; 4];
            let ins = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = ins.len();
            match st.view.anchor {
                Some(anchor) => {
                    let (s, e) = selection_range(b, anchor, cur, line);
                    if s < e {
                        // The removed span fills the unnamed register, as a Visual/Normal delete does.
                        let reg = captured(b, s, e, line);
                        edit_yank(
                            one(Edit::replace(s, e - s, ins)),
                            s + n,
                            Mode::Insert,
                            hint,
                            reg,
                        )
                    } else {
                        edit(one(Edit::insert(s, ins)), s + n, Mode::Insert, hint)
                    }
                }
                // No anchor (not really in a selection): degrade to a plain insert-and-enter-Insert.
                None => edit(one(Edit::insert(cur, ins)), cur + n, Mode::Insert, hint),
            }
        }
        Command::BlockInsert(kind) => plan_block_insert(st, b, cur, hint, kind),
        Command::SwapSelectionEnds => {
            // Visual/Select `o`: exchange the two ends. The cursor jumps to the anchor; the anchor becomes
            // the old cursor (`set_anchor`, which `commit` installs). The SAME text stays selected, but a
            // later bare motion now extends the OTHER end (it re-plans against the new anchor). Involutive.
            // Outside a selection (no anchor) it is a clean no-op.
            match (st.view.mode.selection(), st.view.anchor) {
                (Some(_), Some(anchor)) => Plan {
                    set_anchor: Some(cur),
                    ..nop(anchor, st.view.mode)
                },
                _ => nop(cur, st.view.mode),
            }
        }
        Command::YankSelection | Command::DeleteSelection | Command::ChangeSelection => {
            let Some(anchor) = st.view.anchor else {
                // Not in a selection (or no anchor) — drop back to Normal, do nothing.
                return nop(cur, Mode::Normal);
            };
            // Blockwise (`CTRL-V`) selections operate on a rectangle of per-row slices, not one span.
            if st.view.mode.selection() == Some(SelectKind::Blockwise) {
                let op = match cmd {
                    Command::YankSelection => OpKind::Yank,
                    Command::ChangeSelection => OpKind::Change,
                    _ => OpKind::Delete,
                };
                return block_op(b, anchor, cur, op, hint);
            }
            let line = st.view.mode.selection() == Some(SelectKind::Linewise);
            let (s, e) = selection_range(b, anchor, cur, line);
            // Visual-LINEWISE change (`V…c`) behaves exactly like `cc` over the selected line range:
            // keep the first line's indent, replace the rest with ONE empty line, and — crucially — KEEP
            // the trailing separator to the following line (never merge the next line in). Route it through
            // the shared cc-logic; `selection_range` gives whole lines incl. the trailing '\n', so drop
            // that one byte to get the CONTENT end the helper expects. Charwise/blockwise change stays the
            // delete-then-Insert path below.
            if line && matches!(cmd, Command::ChangeSelection) && s < e {
                let content_end = if e > s && b[e - 1] == b'\n' { e - 1 } else { e };
                return plan_linewise_change(b, s, content_end, hint);
            }
            let reg = captured(b, s, e, line);
            match cmd {
                // Yank leaves the buffer unchanged, cursor at the selection start (Vim), back to Normal.
                Command::YankSelection => Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: Mode::Normal,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg, (s, e))),
                    set_anchor: None,
                    set_mark: None,
                },
                Command::DeleteSelection if s < e => {
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Normal, hint, reg)
                }
                Command::ChangeSelection if s < e => {
                    edit_yank(one(Edit::delete(s, e - s)), s, Mode::Insert, hint, reg)
                }
                // Empty selection: just leave Visual (Change still opens Insert).
                Command::ChangeSelection => nop(s, Mode::Insert),
                _ => nop(s, Mode::Normal),
            }
        }
        // Visual `u`/`U`/`~` — recase the selection in place, cursor at its start, back to Normal.
        Command::CaseSelection(case) => {
            let (s, e, _line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            match std::str::from_utf8(&b[s..e]) {
                Ok(text) => {
                    let new = recase(text, *case).into_bytes();
                    edit(one(Edit::replace(s, e - s, new)), s, Mode::Normal, hint)
                }
                Err(_) => nop(s, Mode::Normal),
            }
        }
        // Visual `r{char}` — overwrite every non-newline char in the selection with `c`, keeping line
        // breaks and the byte length's char count; cursor to the span start, back to Normal.
        Command::ReplaceSelectionChar(c) => {
            let (s, e, _line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            match std::str::from_utf8(&b[s..e]) {
                Ok(text) => {
                    let new: String = text
                        .chars()
                        .map(|ch| if ch == '\n' { '\n' } else { *c })
                        .collect();
                    edit(
                        one(Edit::replace(s, e - s, new.into_bytes())),
                        s,
                        Mode::Normal,
                        hint,
                    )
                }
                Err(_) => nop(s, Mode::Normal),
            }
        }
        // Visual `p`/`P` — replace the selection with the register; `swap` puts the deleted text into the
        // unnamed register (Vim `p`), `P` preserves the register (so it can overwrite successive selections).
        Command::PasteSelection { swap } => {
            let (s, e, line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            let deleted = captured(b, s, e, line);
            let reg = st.view.registers.get(st.view.pending_register).clone();
            let repl = reg.text().to_vec();
            // Cursor: charwise register → on the last pasted byte; linewise/empty → at the span start.
            let cursor = if repl.is_empty() || reg.is_linewise() {
                s
            } else {
                s + repl.len() - 1
            };
            let set_register = swap.then_some(RegWrite::Edit(deleted));
            Plan {
                action: Action::Txn {
                    edits: one(Edit::replace(s, e - s, repl)),
                    hint: GroupHint::BreakBefore,
                },
                cursor,
                mode: Mode::Normal,
                is_edit: true,
                effects: Vec::new(),
                set_register,
                set_anchor: None,
                set_mark: None,
            }
        }
        Command::SearchNext(pat) => {
            let m = search_fwd(b, pat, cur + 1, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        Command::SearchPrev(pat) => {
            let m = search_bwd(b, pat, cur, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        // `/pat` / `?pat` as a motion: step to the `count`-th match in the search direction (each step
        // resumes from just past/before the last), then either move there (`Move`) or fold the exclusive
        // span into an edit (`d/pat`, `c?pat`, `y?pat`). No match on the operative side aborts (a clean
        // no-op, never a reversed/empty edit). See `plan_search` for the direction geometry.
        Command::Search {
            op,
            count,
            pattern,
            backward,
        } => plan_search(st, b, cur, op, *count, pattern, *backward, hint),
        Command::SearchObject {
            op,
            count,
            pattern,
            backward,
        } => plan_search_object(st, b, cur, op, *count, pattern, *backward, hint),
        // `*`/`#` are resolved by the frontend (it reads the word under the cursor from the buffer and
        // rewrites this to a concrete `SearchNext`/`SearchPrev`), so the pure core never acts on it.
        Command::SearchWordUnder { .. } => nop(cur, st.view.mode),
        // `g&` — resolved in the frontend against its last-substitute state; a no-op in the pure core.
        Command::RepeatSubstituteGlobal => nop(cur, st.view.mode),
        Command::Undo => Plan {
            action: Action::Undo,
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::Redo => Plan {
            action: Action::Redo,
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::UndoOlder => Plan {
            action: Action::UndoChrono { older: true },
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::UndoNewer => Plan {
            action: Action::UndoChrono { older: false },
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::Save => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: vec![Effect::Save],
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::Quit => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: vec![Effect::Quit],
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        // Emacs-profile commands: the depth-1 region ops (D-027) and the distinct Emacs commands
        // (D-051), grouped into `plan_emacs` to keep this match readable.
        Command::SetMark
        | Command::ExchangePointMark
        | Command::CopyRegion
        | Command::KillRegion
        | Command::EmacsYank { .. }
        | Command::EmacsKillLine
        | Command::EmacsKillWord { .. }
        | Command::EmacsBackwardKillWord { .. }
        | Command::EmacsTransposeChars
        | Command::EmacsTransposeWords
        | Command::EmacsCaseWord { .. }
        | Command::EmacsHorizontalSpace { .. }
        | Command::EmacsOpenLine
        | Command::EmacsMarkWord
        | Command::EmacsKillWholeLine
        | Command::EmacsCaseRegion { .. }
        | Command::EmacsDeleteIndentation
        | Command::EmacsBufferEdge { .. } => plan_emacs(st, b, cur, hint, cmd),
    }
}

/// Plan the Emacs-profile commands split out of [`plan`]: the depth-1 region ops (D-027 —
/// SetMark / KillRegion / CopyRegion / ExchangePointMark) and the distinct Emacs commands (D-051).
/// The core stays profile-agnostic; the profile decides which of these its keymap resolves to.
fn plan_emacs(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        // Emacs region (D-027 depth-1). `C-SPC` drops the mark at point.
        Command::SetMark => nop_mark(cur, st.view.mode, cur),
        // `C-x C-x` swaps point and mark; the mark takes the old point. No mark set → inert.
        Command::ExchangePointMark => match st.view.mark {
            Some(m) => nop_mark(m, st.view.mode, cur),
            None => nop(cur, st.view.mode),
        },
        // `M-w` copies the region `[min,max)` charwise into the register; point and mark are untouched. An
        // empty region or no mark is inert.
        Command::CopyRegion => match active_region(st, cur) {
            Some((s, e)) => {
                let reg = captured(b, s, e, false);
                Plan {
                    action: Action::Nop,
                    cursor: cur,
                    mode: st.view.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg, (s, e))),
                    set_anchor: None,
                    set_mark: None,
                }
            }
            None => nop(cur, st.view.mode),
        },
        // `C-w` kills the region into the register (the kill ring) and leaves point and mark together at its
        // start — Emacs keeps the mark at the region's lower bound `s` (where point also lands after the
        // deletion collapses the span), it does NOT clear it. An empty region or no mark is inert.
        Command::KillRegion => match active_region(st, cur) {
            Some((s, e)) => {
                let reg = captured(b, s, e, false);
                Plan {
                    action: Action::Txn {
                        edits: one(Edit::delete(s, e - s)),
                        hint,
                    },
                    cursor: s,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    // A kill: accumulates onto the current unnamed entry when it follows another kill.
                    set_register: Some(RegWrite::KillAppend(reg)),
                    set_anchor: None,
                    set_mark: Some(MarkWrite::Set(s)),
                }
            }
            None => nop(cur, st.view.mode),
        },
        // `C-y` (Emacs yank, D-051): the same gravity-aware charwise paste as `Paste{after:false}`, plus it
        // SETS the mark at the insertion start (point before the paste). An empty register is inert (no mark
        // write either — `paste` returns a Nop).
        Command::EmacsYank { count } => {
            let mut p = paste(
                b,
                cur,
                st.view.mode,
                st.view.registers.get(st.view.pending_register),
                false,
                *count,
                st.view.caret,
                false, // Emacs yank is not `gp` — normal cursor placement
            );
            if !matches!(p.action, Action::Nop) {
                p.set_mark = Some(MarkWrite::Set(cur));
            }
            p
        }
        // `C-k` (Emacs kill-line, D-051): kill from point into the register. With text before the line end,
        // kill that text (NOT the newline). With point already at the line end, kill the terminating newline
        // instead — joining the next line up. At end-of-buffer (no newline to take) it is inert. This is the
        // no-prefix `kill-whole-line`-nil default; unlike Vim `D` (`Delete(1, LineEnd)`) it never no-ops at EOL.
        Command::EmacsKillLine => {
            let le = line_end(b, cur);
            if cur < le {
                let reg = captured(b, cur, le, false);
                edit_kill(
                    one(Edit::delete(cur, le - cur)),
                    cur,
                    st.view.mode,
                    hint,
                    reg,
                )
            } else if le < b.len() {
                // Point at EOL: kill the single newline byte, joining the following line.
                let reg = captured(b, le, le + 1, false);
                edit_kill(one(Edit::delete(le, 1)), le, st.view.mode, hint, reg)
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `M-d` / `kill-word` (Emacs kill-word, D-051): kill the `EmacsWordFwd` span (the word only, not Vim
        // `dw`'s trailing space) into the register, accumulating like every Emacs kill. Distinct from Vim
        // `Delete(count, EmacsWordFwd)` ONLY in that accumulation — but that difference is enough to warrant
        // its own command (D-051), so Vim deletes never accumulate.
        Command::EmacsKillWord { count } => {
            let (s, e, _) = op_span(b, cur, Motion::EmacsWordFwd, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, s, e, false);
                edit_kill(one(Edit::delete(s, e - s)), s, st.view.mode, hint, reg)
            }
        }
        // `M-DEL` (Emacs backward-kill-word, D-051): kill `count` words backward (the `WordBack` span) into
        // the register. A BACKWARD kill, so on a kill run it PREPENDS onto the current entry (via
        // `RegWrite::KillPrepend`). Distinct from Vim `db` (no accumulation).
        Command::EmacsBackwardKillWord { count } => {
            let target = motion::target(b, cur, Motion::EmacsWordBack, *count);
            if target >= cur {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, target, cur, false);
                Plan {
                    action: Action::Txn {
                        edits: one(Edit::delete(target, cur - target)),
                        hint,
                    },
                    cursor: target,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::KillPrepend(reg)),
                    set_anchor: None,
                    set_mark: None,
                }
            }
        }
        // `C-t` (Emacs transpose-chars, D-051): swap the char before point with the char at point, then
        // advance point past the pair. `j` is the right char (it slides left), `i = j - 1` the left char;
        // at end of line Emacs steps back one so the two chars ENDING the line are transposed. Inert when the
        // line has no pair to transpose (point at buffer/line start, or a one-char line). No kill-ring write.
        Command::EmacsTransposeChars => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let j = if cur >= le {
                le.checked_sub(1).filter(|_| le >= ls + 2)
            } else if cur > ls {
                Some(cur)
            } else {
                None
            };
            match j {
                Some(j) if j > ls && j < le => Plan {
                    action: Action::Txn {
                        edits: one(Edit::replace(j - 1, 2, vec![b[j], b[j - 1]])),
                        hint,
                    },
                    cursor: j + 1,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: None,
                    set_mark: None,
                },
                _ => nop(cur, st.view.mode),
            }
        }
        // `M-t` (Emacs transpose-words, D-051): swap the word before/around point with the following word,
        // preserving the separator, and leave point past the moved second word. Mirrors Emacs `transpose-subr`
        // for `forward-word`: `start1` = backward-word, `end1` = forward-word from there, `end2` =
        // forward-word again, `start2` = backward-word from there. Inert without a real pair. No kill write.
        Command::EmacsTransposeWords => {
            let start1 = motion::target(b, cur, Motion::EmacsWordBack, 1);
            let end1 = motion::target(b, start1, Motion::EmacsWordFwd, 1);
            let end2 = motion::target(b, end1, Motion::EmacsWordFwd, 1);
            let start2 = motion::target(b, end2, Motion::EmacsWordBack, 1);
            if start1 < end1 && end1 <= start2 && start2 < end2 {
                let mut new = Vec::with_capacity(end2 - start1);
                new.extend_from_slice(&b[start2..end2]); // second word moves to the front
                new.extend_from_slice(&b[end1..start2]); // the original separator
                new.extend_from_slice(&b[start1..end1]); // first word moves to the back
                edit(
                    one(Edit::replace(start1, end2 - start1, new)),
                    end2,
                    st.view.mode,
                    hint,
                )
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `M-u`/`M-l`/`M-c` (Emacs upcase-/downcase-/capitalize-word, D-051): recase the `forward-word` span
        // (point to the end of the next word) and leave point at that end. Inert with no word ahead; no
        // kill-ring write. The span sits on grapheme boundaries (EmacsWordFwd lands on one), so it is valid
        // UTF-8 to recase; point lands at `cur + new_len` in case the recased bytes changed length.
        Command::EmacsCaseWord { case } => {
            let end = motion::target(b, cur, Motion::EmacsWordFwd, 1);
            match recase_span(b, cur, end, *case) {
                Some(recased) => {
                    let cursor = cur + recased.len();
                    Plan {
                        action: Action::Txn {
                            edits: one(Edit::replace(cur, end - cur, recased)),
                            hint,
                        },
                        cursor,
                        mode: st.view.mode,
                        is_edit: true,
                        effects: Vec::new(),
                        set_register: None,
                        set_anchor: None,
                        set_mark: None,
                    }
                }
                None => nop(cur, st.view.mode),
            }
        }
        // `M-SPC` (just-one-space) / `M-\` (delete-horizontal-space, D-051): collapse the run of spaces/tabs
        // around point (never crossing a newline). `keep_one` leaves exactly one space with point after it
        // (inserting one when there was none); otherwise it deletes them all. No kill-ring write.
        Command::EmacsHorizontalSpace { keep_one } => {
            let (s, e) = (hspace_start(b, cur), hspace_end(b, cur));
            if *keep_one {
                // Already exactly a single space? Leave the buffer, rest point after it (a pure move).
                if e - s == 1 && b[s] == b' ' {
                    nop(s + 1, st.view.mode)
                } else {
                    edit(
                        one(Edit::replace(s, e - s, b" ".to_vec())),
                        s + 1,
                        st.view.mode,
                        hint,
                    )
                }
            } else if e > s {
                edit(one(Edit::delete(s, e - s)), s, st.view.mode, hint)
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `C-o` (Emacs open-line, D-051): insert a newline at point but leave point BEFORE it (opens a blank
        // line below without moving onto it). Unlike Vim `o`, no mode change and no register write.
        Command::EmacsOpenLine => edit(
            one(Edit::insert(cur, b"\n".to_vec())),
            cur,
            st.view.mode,
            hint,
        ),
        // `M-@` (Emacs mark-word, D-051): set the mark at the end of the next word (`forward-word`) without
        // moving point, activating the region point→word-end. No edit, no kill-ring write.
        Command::EmacsMarkWord => nop_mark(
            cur,
            st.view.mode,
            motion::target(b, cur, Motion::EmacsWordFwd, 1),
        ),
        // `C-S-DEL` (Emacs kill-whole-line, D-051): kill the whole line INCLUDING its trailing newline,
        // regardless of point column, into the register (accumulating), leaving point at the line start.
        Command::EmacsKillWholeLine => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let e = if le < b.len() { le + 1 } else { le }; // include the terminating newline when present
            if e > ls {
                let reg = captured(b, ls, e, false);
                edit_kill(one(Edit::delete(ls, e - ls)), ls, st.view.mode, hint, reg)
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `C-x C-u` / `C-x C-l` (Emacs upcase-/downcase-/capitalize-region, D-051): recase the active region
        // `[min(point,mark), max)` and leave point and mark where they are. Inert without a mark. No kill write.
        Command::EmacsCaseRegion { case } => match active_region(st, cur) {
            Some((s, e)) => match recase_span(b, s, e, *case) {
                Some(recased) => Plan {
                    action: Action::Txn {
                        edits: one(Edit::replace(s, e - s, recased)),
                        hint,
                    },
                    cursor: cur,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: None,
                    set_mark: None,
                },
                None => nop(cur, st.view.mode),
            },
            None => nop(cur, st.view.mode),
        },
        // `M-^` (Emacs delete-indentation / join-line, D-051): join this line to the previous one — delete
        // the preceding newline plus the previous line's trailing whitespace and this line's leading
        // indentation, then fix up whitespace to ONE space, or NONE when the join lands at beginning-of-line
        // (an empty previous line). Point rests at the join. No kill-ring write. Distinct from Vim `J`.
        Command::EmacsDeleteIndentation => {
            let ls = line_start(b, cur);
            if ls == 0 {
                nop(cur, st.view.mode) // no previous line to join to
            } else {
                // Eat the previous line's trailing whitespace (back from the newline) and this line's
                // leading indentation (forward from its start).
                let s = hspace_start(b, ls - 1);
                let e = hspace_end(b, ls);
                // fixup-whitespace: one space, unless the join sits at the start of its line (empty prev).
                let repl: Vec<u8> = if s == line_start(b, s) {
                    Vec::new()
                } else {
                    vec![b' ']
                };
                edit(one(Edit::replace(s, e - s, repl)), s, st.view.mode, hint)
            }
        }
        // `M-<` / `M->` (Emacs beginning/end-of-buffer, D-051): move point to the ABSOLUTE buffer start/end
        // (not Vim `gg`/`G`'s first-non-blank line) and PUSH the mark at the old point.
        Command::EmacsBufferEdge { start } => {
            nop_mark(if *start { 0 } else { b.len() }, st.view.mode, cur)
        }
        _ => unreachable!("plan_emacs handles only Emacs-profile commands"),
    }
}

/// How many leading whitespace bytes one `<<` removes from the line at `ls`: a single leading tab, else up
/// to `tab_width` leading spaces — one indent level, never crossing a non-blank char or the line end
/// (`le`). Style-agnostic on purpose: a space-configured buffer that happens to start with a tab still
/// unindents by that tab, matching Vim's "remove one shiftwidth of indent" for the common cases ruse models.
fn shift_left_remove(b: &[u8], ls: usize, le: usize, tab_width: usize) -> usize {
    if ls < le && b[ls] == b'\t' {
        return 1;
    }
    let mut n = 0;
    while n < tab_width && ls + n < le && b[ls + n] == b' ' {
        n += 1;
    }
    n
}

/// Plan a linewise shift (`>>` / `<<`) over `count` lines from the cursor's line down. `right` adds one
/// indent level to each line; `!right` removes up to one. Empty lines are never indented (Vim); the cursor
/// lands on the first non-blank of the cursor's (first) line, exactly as Vim leaves it. The register is
/// untouched. Edits are one-per-line at distinct line starts, so the [`EditList`] is disjoint by construction.
/// `=` reindent (bracket-depth model): set each line in `[first_line, last_line]` (0-based, inclusive) to
/// `depth × indent_unit`, where `depth` is the net unclosed `([{` before the line; a line whose first
/// non-blank is a closer dedents one level, and blank lines are left empty. Structural / language-agnostic:
/// brackets inside strings or comments are NOT excluded (documented). One transaction (edits are disjoint,
/// ascending line starts). Cursor → the first reindented line's new first-non-blank.
/// The leading whitespace of the line holding the opener that matches the closer at `cur` (F-015 Phase 3a).
/// Scans backwards from `cur` counting the `(open, close)` pair; when the nesting depth returns to zero the
/// matching opener is found and its line's leading whitespace is returned as the closer's target indent.
/// `None` when no matching opener exists. Deterministic; not string/comment-aware (as with the `=` fallback).
fn matching_opener_pad(b: &[u8], cur: usize, open: u8, close: u8) -> Option<Vec<u8>> {
    let mut depth = 1i32;
    let mut i = cur;
    while i > 0 {
        i -= 1;
        let x = b[i];
        if x == close {
            depth += 1;
        } else if x == open {
            depth -= 1;
            if depth == 0 {
                let ls = line_start(b, i);
                let fnb = motion::first_non_blank(b, ls);
                return Some(b[ls..fnb].to_vec());
            }
        }
    }
    None
}

/// Apply explicit indent `levels` (one per line from `first_line`) to `[first_line, last_line]`: each
/// non-blank line's leading whitespace becomes `levels[i] × indent_unit`; blank lines are emptied. One
/// transaction. The frontend supplies tree-derived levels (tree-aware `=`); the core stays view-free.
fn plan_set_indents(
    st: &EditorState,
    first_line: usize,
    last_line: usize,
    levels: &[usize],
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let unit = st.indent_unit();
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let last = last_line.min(line_starts.len().saturating_sub(1));
    let first = first_line.min(last);
    let mut edits: Vec<Edit> = Vec::new();
    let mut first_cursor = line_starts[first];
    #[allow(clippy::needless_range_loop)]
    for li in first..=last {
        let ls = line_starts[li];
        let le = line_end(b, ls);
        let fnb = motion::first_non_blank(b, ls);
        let want: Vec<u8> = if fnb >= le {
            Vec::new() // blank line → no indent
        } else {
            let n = unit.len() * levels.get(li - first).copied().unwrap_or(0);
            unit.iter().cycle().take(n).copied().collect()
        };
        if li == first {
            first_cursor = ls + want.len();
        }
        if b[ls..fnb] != want[..] {
            edits.push(Edit::replace(ls, fnb - ls, want));
        }
    }
    if edits.is_empty() {
        return nop(first_cursor.min(b.len()), st.view.mode);
    }
    let edits =
        EditList::new(edits).expect("indent edits sit at distinct line starts, so disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor: first_cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Re-wrap a block of text to `width` columns for `gq`/`gw`. Each blank-line-separated PARAGRAPH is joined
/// into a single word stream and greedily re-broken so no line exceeds `width` (always ≥1 word/line); the
/// paragraph's FIRST line's leading whitespace is used as the indent of every wrapped line. Blank lines are
/// preserved as paragraph separators. Width is measured in `char`s (an MVP approximation of display width).
fn reflow(block: &str, width: usize) -> String {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<String>| {
        if para.is_empty() {
            return;
        }
        let indent: String = para[0]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let words: Vec<&str> = para.iter().flat_map(|l| l.split_whitespace()).collect();
        para.clear();
        if words.is_empty() {
            return;
        }
        let mut line = indent.clone();
        let mut have_word = false;
        for w in words {
            let wlen = w.chars().count();
            if !have_word {
                line.push_str(w);
                have_word = true;
            } else if line.chars().count() + 1 + wlen > width {
                out.push(std::mem::replace(&mut line, {
                    let mut s = indent.clone();
                    s.push_str(w);
                    s
                }));
            } else {
                line.push(' ');
                line.push_str(w);
            }
        }
        out.push(line);
    };
    for l in block.split('\n') {
        if l.trim().is_empty() {
            flush(&mut para, &mut out);
            out.push(String::new()); // preserve the blank separator line
        } else {
            para.push(l);
        }
    }
    flush(&mut para, &mut out);
    out.join("\n")
}

/// `gq`/`gw` {motion} — reflow the motion's whole lines to `'textwidth'` (or 79 when tw=0). `gw`
/// (`keep_cursor`) restores the caret; `gq` leaves it at the start of the last reformatted line.
fn plan_format(
    st: &EditorState,
    cur: usize,
    count: u32,
    motion: Motion,
    keep_cursor: bool,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let (s, e, _) = op_span(b, cur, motion, count);
    if s >= e {
        return nop(cur, st.view.mode);
    }
    let start = line_start(b, s);
    let end = line_end(b, e - 1); // through the last touched line (exclusive of its trailing newline)
    let Ok(block) = std::str::from_utf8(&b[start..end]) else {
        return nop(cur, Mode::Normal);
    };
    let width = if st.view.text_width > 0 {
        st.view.text_width
    } else {
        79
    };
    let new = reflow(block, width);
    if new.as_bytes() == &b[start..end] {
        // Already wrapped — no edit; still move the cursor per gq (Vim) unless gw.
        let cursor = if keep_cursor { cur } else { start };
        return nop(cursor, Mode::Normal);
    }
    let new_bytes = new.into_bytes();
    // gw restores the caret (clamped into the resized buffer); gq lands on the last reformatted line's start.
    let cursor = if keep_cursor {
        cur.min(start + new_bytes.len())
    } else {
        let last_line_off = new_bytes
            .iter()
            .rposition(|&c| c == b'\n')
            .map_or(0, |i| i + 1);
        start + last_line_off
    };
    edit(
        one(Edit::replace(start, end - start, new_bytes)),
        cursor,
        Mode::Normal,
        hint,
    )
}

fn plan_reindent(st: &EditorState, first_line: usize, last_line: usize, hint: GroupHint) -> Plan {
    let b = st.bytes();
    let unit = st.indent_unit();
    // Per-line start offset, and the net bracket depth AT each line's start.
    let mut line_starts: Vec<usize> = Vec::new();
    let mut depth_at: Vec<i32> = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0usize;
    line_starts.push(0);
    depth_at.push(0);
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.max(1) - 1,
            b'\n' => {
                line_starts.push(i + 1);
                depth_at.push(depth);
            }
            _ => {}
        }
        i += 1;
    }

    // Target indent (in bytes) for line `li`: `level × unit`, level from depth (closer-first dedents one).
    let indent_len = |li: usize, fnb: usize, ls: usize| -> usize {
        let closer = ls < b.len() && fnb < b.len() && matches!(b[fnb], b')' | b']' | b'}');
        let level = (depth_at[li] - i32::from(closer)).max(0) as usize;
        unit.len() * level
    };

    let mut edits: Vec<Edit> = Vec::new();
    let last = last_line.min(line_starts.len().saturating_sub(1));
    let mut first_cursor = line_starts[first_line];
    // `li` indexes the two parallel vecs (`line_starts` + `depth_at`) and marks the first line, so a
    // range loop is the clear form here.
    #[allow(clippy::needless_range_loop)]
    for li in first_line..=last {
        let ls = line_starts[li];
        let le = line_end(b, ls);
        let fnb = motion::first_non_blank(b, ls);
        let want: Vec<u8> = if fnb >= le {
            Vec::new() // blank line → no indent
        } else {
            let n = indent_len(li, fnb, ls);
            unit.iter().cycle().take(n).copied().collect()
        };
        if li == first_line {
            first_cursor = ls + want.len();
        }
        if b[ls..fnb] != want[..] {
            edits.push(Edit::replace(ls, fnb - ls, want));
        }
    }

    if edits.is_empty() {
        return nop(first_cursor.min(b.len()), st.view.mode);
    }
    let edits =
        EditList::new(edits).expect("reindent edits sit at distinct line starts, so disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor: first_cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

fn plan_shift(st: &EditorState, cur: usize, count: u32, right: bool, hint: GroupHint) -> Plan {
    let b = st.bytes();
    let first_ls = line_start(b, cur);
    let first_le = line_end(b, first_ls);
    let old_fnb = motion::first_non_blank(b, first_ls);
    let unit = st.indent_unit();

    let mut edits: Vec<Edit> = Vec::new();
    let mut first_removed = 0usize;
    let mut ls = first_ls;
    for i in 0..count.max(1) {
        let le = line_end(b, ls);
        if right {
            // Vim indents a whitespace-only line but never a truly EMPTY one (`ls == le`).
            if ls < le {
                edits.push(Edit::insert(ls, unit.clone()));
            }
        } else {
            let remove = shift_left_remove(b, ls, le, st.view.indent.tab_width);
            if i == 0 {
                first_removed = remove;
            }
            if remove > 0 {
                edits.push(Edit::delete(ls, remove));
            }
        }
        if le >= b.len() {
            break; // no more lines — shifting fewer than `count` is fine (Vim clamps too).
        }
        ls = le + 1;
    }

    // Cursor: first non-blank of the FIRST shifted line, computed against the POST-edit buffer. Prepending
    // `unit` (all blanks) shifts that line's first non-blank right by `unit.len()`; a `<<` shifts it left by
    // the bytes removed. An empty line got no indent, so the cursor stays at its start. The all-blank-line
    // case (fnb past the last char) is pulled back onto the last char by `commit`'s Normal-mode clamp.
    let cursor = if right {
        if first_ls < first_le {
            old_fnb + unit.len()
        } else {
            first_ls
        }
    } else {
        old_fnb - first_removed
    };

    if edits.is_empty() {
        // Nothing to indent/unindent (e.g. `<<` at column 0, or `>>` on an empty line): a pure cursor move.
        return Plan {
            action: Action::Nop,
            cursor,
            mode: st.mode(),
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        };
    }
    let edits = EditList::new(edits)
        .expect("shift edits sit at distinct line starts, so they are disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Build the paste plan for `p` (after) / `P` (before) from the unnamed register. Charwise pastes insert
/// inline next to the cursor; linewise pastes open a whole new line below/above. An empty register is a
/// no-op. This is the paste-geometry semantic D-026 pins down for v0.
/// Blockwise (`CTRL-V`) yank/delete/change over the rectangle whose corners are the byte offsets `c1` and
/// `c2` (the selection's anchor+cursor, or an operator's cursor+motion-target). Yank and Delete capture
/// the per-row slices into a blockwise [`Register`]; the cursor lands at the block's top-left corner (Vim).
/// Change deletes the block and enters Insert at the top-left — but only the SINGLE-ROW partial: block
/// `c`/`I`/`A`'s replicate-typed-text-to-every-row behaviour is deferred to a later slice, so block change
/// is intentionally NOT oracle-tested here.
fn block_op(b: &[u8], c1: usize, c2: usize, op: OpKind, hint: GroupHint) -> Plan {
    let (rows, _col_lo, _col_hi) = block_rows(b, c1, c2);
    let top_left = rows.first().map_or(c1.min(c2), |&(s, _)| s);
    // The blockwise register: each row's slice, joined by '\n' (ragged rows, no trailing newline).
    let mut text: Vec<u8> = Vec::new();
    for (i, &(s, e)) in rows.iter().enumerate() {
        if i > 0 {
            text.push(b'\n');
        }
        text.extend_from_slice(&b[s..e]);
    }
    let reg = Register::blockwise(text);
    // The `[`/`]` change-mark span for a blockwise yank: top-left corner → bottom-right corner (the end of
    // the last row's slice). An approximation of Vim's rectangular bracketing, adequate for the mark jump.
    let block_span = (top_left, rows.last().map_or(top_left, |&(_, e)| e));
    let nop = |mode: Mode| Plan {
        action: Action::Nop,
        cursor: top_left,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };
    match op {
        // Yank leaves the buffer unchanged, cursor at the block's top-left, back to Normal.
        OpKind::Yank => Plan {
            set_register: Some(RegWrite::Yank(reg, block_span)),
            ..nop(Mode::Normal)
        },
        OpKind::Delete | OpKind::Change => {
            let after_mode = if op == OpKind::Change {
                Mode::Insert
            } else {
                Mode::Normal
            };
            // One delete per row (rows on different lines are inherently disjoint); empty rows contribute
            // nothing. `EditList::new` sorts + validates disjointness.
            let edits: Vec<Edit> = rows
                .iter()
                .filter(|&&(s, e)| e > s)
                .map(|&(s, e)| Edit::delete(s, e - s))
                .collect();
            if edits.is_empty() {
                // The block sits entirely past every line's end — nothing to remove.
                return nop(after_mode);
            }
            let list = EditList::new(edits).expect("block-row deletes are disjoint (one per line)");
            Plan {
                action: Action::Txn { edits: list, hint },
                cursor: top_left,
                mode: after_mode,
                is_edit: true,
                effects: Vec::new(),
                set_register: Some(RegWrite::Edit(reg)),
                set_anchor: None,
                set_mark: None,
            }
        }
    }
}

/// Finish a blockwise insert-replicate session (`<Esc>` after `CTRL-V` `I`/`A`/`c`): take the text typed
/// on the top row since `session.insert_start` and insert it at `session.target_col` on each of the
/// `rows_below` rows beneath the top line. `append` rows pad short lines with spaces; non-append (`I`/`c`)
/// rows shorter than the target column are skipped (Vim). A newline typed on the top row aborts the
/// replicate (Vim). The cursor returns to the block's top-left corner (`session.top_left`).
fn block_replicate(b: &[u8], cur: usize, session: BlockInsert, hint: GroupHint) -> Plan {
    let nop = |cursor: usize| Plan {
        action: Action::Nop,
        cursor,
        mode: Mode::Normal,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };
    let start = session.insert_start;
    // The cursor rests at the block's top-left when the session ends (Vim), for both `I` and `A`.
    let end_cursor = session.top_left.min(b.len());
    // The text typed on the top row. Empty (or containing a newline) → nothing to replicate.
    if cur <= start || b[start..cur].contains(&b'\n') {
        return nop(end_cursor);
    }
    let typed = &b[start..cur];

    let mut edits: Vec<Edit> = Vec::new();
    let mut rs = session.top_line_start;
    for _ in 0..session.rows_below {
        let le = line_end(b, rs);
        if le >= b.len() {
            break; // no further line to replicate onto
        }
        rs = le + 1;
        let row_le = line_end(b, rs);
        let rowlen = col_of(b, rs, row_le);
        if session.to_eol {
            // `$A`: append at THIS row's own end (ragged), never a fixed column.
            edits.push(Edit::insert(row_le, typed.to_vec()));
        } else if session.append {
            if rowlen < session.target_col {
                let mut ins = vec![b' '; session.target_col - rowlen];
                ins.extend_from_slice(typed);
                edits.push(Edit::insert(row_le, ins));
            } else {
                edits.push(Edit::insert(
                    at_col(b, rs, session.target_col),
                    typed.to_vec(),
                ));
            }
        } else if rowlen >= session.target_col {
            edits.push(Edit::insert(
                at_col(b, rs, session.target_col),
                typed.to_vec(),
            ));
        }
        // non-append + short row: skipped (Vim leaves lines shorter than the block's left edge untouched).
    }
    if edits.is_empty() {
        return nop(end_cursor);
    }
    let list = EditList::new(edits).expect("block-replicate inserts are disjoint (one per line)");
    Plan {
        action: Action::Txn { edits: list, hint },
        cursor: end_cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Blockwise paste (`p`/`P` of a `CTRL-V` register): drop the register's rows as a rectangle. Row `i`
/// lands at the target column on the line `i` rows below the cursor's line; the target column is the
/// cursor's column for `P` and one past it for `p` (Vim). Lines shorter than the target column are padded
/// with spaces; rows past the last line are appended as new lines. The cursor ends at the block's
/// top-left. `{count}p` repeats each row `count` times horizontally.
fn paste_block(b: &[u8], cur: usize, reg: &Register, after: bool, count: usize) -> Plan {
    let rows: Vec<Vec<u8>> = reg
        .text()
        .split(|&c| c == b'\n')
        .map(|row| row.repeat(count))
        .collect();
    let cls = line_start(b, cur);
    let ccol = col_of(b, cls, cur);
    let has_char = cur < line_end(b, cls);
    let target_col = if after && has_char { ccol + 1 } else { ccol };

    let mut edits: Vec<Edit> = Vec::new();
    let mut trailing: Vec<u8> = Vec::new();
    let mut cursor = cur;
    let mut cursor_set = false;
    let mut rs = cls;
    let mut past_eof = false;
    for (i, row) in rows.iter().enumerate() {
        if !past_eof && rs < b.len() {
            let le = line_end(b, rs);
            let linelen = col_of(b, rs, le);
            let (at, pad) = if target_col <= linelen {
                (at_col(b, rs, target_col), 0usize)
            } else {
                (le, target_col - linelen)
            };
            if i == 0 {
                cursor = at + pad; // the start of the pasted text on the cursor's line
                cursor_set = true;
            }
            if pad > 0 || !row.is_empty() {
                let mut ins = vec![b' '; pad];
                ins.extend_from_slice(row);
                edits.push(Edit::insert(at, ins));
            }
            if le < b.len() {
                rs = le + 1;
            } else {
                past_eof = true;
            }
        } else {
            // Past the last line: append the remaining rows as fresh lines at the buffer end.
            past_eof = true;
            trailing.push(b'\n');
            trailing.extend(std::iter::repeat_n(b' ', target_col));
            trailing.extend_from_slice(row);
        }
    }
    if !trailing.is_empty() {
        // Merge into a bottom-most edit already sitting at the buffer end (avoids two inserts at one
        // position, which the disjoint-edit invariant forbids); otherwise append as its own edit.
        match edits.last_mut() {
            Some(last) if last.end() == b.len() => last.ins.extend_from_slice(&trailing),
            _ => edits.push(Edit::insert(b.len(), trailing)),
        }
        if !cursor_set {
            // The whole block landed past EOF: cursor at the first appended row's text start.
            cursor = b.len() + 1 + target_col;
        }
    }
    if edits.is_empty() {
        return Plan {
            action: Action::Nop,
            cursor: cursor.min(b.len()),
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        };
    }
    let list = EditList::new(edits).expect("block paste inserts are disjoint (one per line)");
    Plan {
        action: Action::Txn {
            edits: list,
            hint: GroupHint::BreakBefore,
        },
        cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Leading-whitespace measure of a single line (no trailing `\n`): its indent width in display COLUMNS
/// (a tab advances to the next `tab_width` multiple) and the BYTE length of that leading whitespace.
fn indent_cols(line: &[u8], tab_width: usize) -> (usize, usize) {
    let tw = tab_width.max(1);
    let (mut cols, mut n) = (0usize, 0usize);
    for &c in line {
        match c {
            b' ' => {
                cols += 1;
                n += 1;
            }
            b'\t' => {
                cols += tw - (cols % tw);
                n += 1;
            }
            _ => break,
        }
    }
    (cols, n)
}

/// Re-indent a linewise register's bytes for `]p`/`[p`: the first line takes `target_cols` of indent and
/// every other line shifts by the same column delta (Vim). Indent is rebuilt in the editor's style (spaces,
/// or tabs + a spaces remainder); a blank line stays blank. Input/output both end each line with `\n`.
fn reindent_register(
    text: &[u8],
    target_cols: usize,
    tab_width: usize,
    style: IndentStyle,
) -> Vec<u8> {
    let tw = tab_width.max(1);
    let first_end = text.iter().position(|&c| c == b'\n').unwrap_or(text.len());
    let (first_cols, _) = indent_cols(&text[..first_end], tw);
    let delta = target_cols as isize - first_cols as isize;
    let build = |cols: usize| -> Vec<u8> {
        match style {
            IndentStyle::Space => vec![b' '; cols],
            IndentStyle::Tab => {
                let mut v = vec![b'\t'; cols / tw];
                v.extend(std::iter::repeat_n(b' ', cols % tw));
                v
            }
        }
    };
    let mut out = Vec::with_capacity(text.len());
    for line in text.split_inclusive(|&c| c == b'\n') {
        let (body, nl): (&[u8], &[u8]) = match line.strip_suffix(b"\n") {
            Some(b) => (b, b"\n"),
            None => (line, b""),
        };
        let (cols, ws) = indent_cols(body, tw);
        let content = &body[ws..];
        if content.is_empty() {
            out.extend_from_slice(nl); // a blank line stays blank (no indent)
        } else {
            let new_cols = (cols as isize + delta).max(0) as usize;
            out.extend_from_slice(&build(new_cols));
            out.extend_from_slice(content);
            out.extend_from_slice(nl);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)] // paste legitimately needs the full register + gravity + gp context
fn paste(
    b: &[u8],
    cur: usize,
    mode: Mode,
    reg: &Register,
    after: bool,
    count: u32,
    gravity: CaretGravity,
    move_after: bool,
) -> Plan {
    let nop = Plan {
        action: Action::Nop,
        cursor: cur,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };
    if reg.is_empty() {
        return nop;
    }
    // Bound the repetition so a pathological `{count}p` (e.g. a digit-spam count that saturates to
    // `u32::MAX`) cannot request a multi-gigabyte allocation. 64 MiB of pasted bytes is far beyond any
    // interactive intent; clamping the count keeps the editor from OOM-ing on absurd input.
    const MAX_PASTE_BYTES: usize = 1 << 26;
    let unit_len = reg.text().len().max(1);
    let count = (count.max(1) as usize).min((MAX_PASTE_BYTES / unit_len).max(1));
    if reg.is_blockwise() {
        return paste_block(b, cur, reg, after, count);
    }
    // `{count}p` pastes the register `count` times (Vim); the register itself is unchanged. The repeated
    // bytes are one contiguous insert, so the cursor math below (last pasted byte) still holds.
    let repeat = |unit: &[u8]| unit.repeat(count);
    let one = |e: Edit| EditList::new(vec![e]).expect("single edit is always valid");
    let mk = |at: usize, bytes: Vec<u8>, cursor: usize| Plan {
        action: Action::Txn {
            edits: one(Edit::insert(at, bytes)),
            hint: GroupHint::BreakBefore,
        },
        cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };

    if reg.is_linewise() {
        // Linewise content is normalized to end with '\n'; `{count}p` stacks that many whole-line copies.
        // `gp`/`gP` (move_after) leave the cursor on the line AFTER the pasted block instead of on it.
        let text = repeat(reg.text());
        let tlen = text.len();
        // Vim rests the cursor on the FIRST NON-BLANK of the first pasted line (not column 0). The leading
        // whitespace never contains a newline, so counting spaces/tabs stays within that first line.
        let fnb = text
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count();
        if after {
            let le = line_end(b, cur);
            if le < b.len() {
                // Insert after the current line's newline: the stored "...\n" becomes a fresh line below.
                let c = if move_after {
                    le + 1 + tlen
                } else {
                    le + 1 + fnb
                };
                mk(le + 1, text, c)
            } else {
                // Last line has no trailing newline: prepend one and drop the stored trailing newline so no
                // dangling blank line is created. Cursor lands on the first non-blank of the pasted line.
                let mut bytes = vec![b'\n'];
                bytes.extend_from_slice(text.strip_suffix(b"\n").unwrap_or(&text));
                let end = le + bytes.len();
                mk(le, bytes, if move_after { end } else { le + 1 + fnb })
            }
        } else {
            let ls = line_start(b, cur);
            mk(ls, text, if move_after { ls + tlen } else { ls + fnb })
        }
    } else {
        // `{count}p` inserts that many copies inline. Vim's charwise-paste cursor rule splits on whether the
        // pasted text spans lines: single-line content leaves the cursor on the LAST pasted byte, but content
        // carrying a newline (e.g. a charwise Visual delete across a line boundary) leaves it on the FIRST
        // pasted byte — the start of the inserted run — not the last.
        let text = repeat(reg.text());
        let n = text.len();
        let multiline = text.contains(&b'\n');
        // Single-line charwise paste: Vim (OnChar) rests the cursor ON the LAST pasted byte; the Emacs
        // profile (BetweenChar, D-050) rests point AFTER the pasted text — Emacs `yank` leaves point past
        // what it inserted. `end` is the after-last boundary; back it up one grapheme only under OnChar.
        // `end` is a position in the POST-insert buffer, so back up by one byte (Vim's existing rule) and
        // let `commit`'s `snap` land it on a char boundary — byte-identical to the pre-D-050 path.
        let tail = |end: usize| match gravity {
            CaretGravity::BetweenChar => end,
            CaretGravity::OnChar => end.saturating_sub(1),
        };
        if after {
            // Insert after the cursor char; cursor lands on the first pasted byte for multi-line content,
            // else per gravity on/after the last pasted byte. `gp` (move_after) rests just past the paste.
            let at = if cur < b.len() {
                next_boundary(b, cur)
            } else {
                cur
            };
            let cursor = if move_after {
                at + n
            } else if multiline {
                at
            } else {
                tail(at + n)
            };
            mk(at, text, cursor)
        } else {
            // Insert before the cursor; cursor lands on the first pasted byte for multi-line content, else
            // per gravity on/after the last pasted byte. `gP` (move_after) rests just past the paste.
            let cursor = if move_after {
                cur + n
            } else if multiline {
                cur
            } else {
                tail(cur + n)
            };
            mk(cur, text, cursor)
        }
    }
}
