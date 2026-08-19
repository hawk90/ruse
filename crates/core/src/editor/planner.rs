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

/// `c{motion}` / `cc`: delete the change span, capture it to a register, and enter Insert. `cc`/`S` is
/// the linewise case that preserves the leading indent (Vim autoindent-like).
fn plan_change(b: &[u8], cur: usize, count: u32, m: &Motion, hint: GroupHint) -> Plan {
    if *m == Motion::Line {
        // `cc` / `{count}cc` / `S`: a LINEWISE change. Vim keeps the leading indent of the first line
        // (the existing indent TEXT is preserved), deletes the rest of the line content down through
        // `count` lines, keeps the trailing newline, and enters Insert at the end of the kept indent.
        let (ls, content_end) = change_range(b, cur, *m, count);
        let indent_end = motion::first_non_blank(b, ls).min(content_end);
        // Register span: whole lines including the terminating newline where one is present.
        let reg_end = if content_end < b.len() && b[content_end] == b'\n' {
            content_end + 1
        } else {
            content_end
        };
        let reg = captured(b, ls, reg_end, true);
        if indent_end >= content_end {
            // Nothing after the indent to delete (empty/blank line): keep the buffer, but still capture
            // the register linewise and drop into Insert at the indent end.
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

/// `[op]/pat` — search forward `count` times, then apply the pending operator (move/delete/change/yank)
/// over `[cur, match)`. An operator with no match (or a backward match) is a no-op.
fn plan_search(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    hint: GroupHint,
) -> Plan {
    let opts = st.view.search_options();
    let mut pos = cur;
    for _ in 0..count.max(1) {
        match search_fwd(b, pattern, pos + 1, opts) {
            Some(m) => pos = m,
            None => break,
        }
    }
    match op {
        SearchOp::Move => nop(pos, st.view.mode),
        _ if pos <= cur => nop(cur, st.view.mode),
        SearchOp::Delete => {
            let reg = captured(b, cur, pos, false);
            edit_yank(
                one(Edit::delete(cur, pos - cur)),
                cur,
                st.view.mode,
                hint,
                reg,
            )
        }
        SearchOp::Change => {
            let reg = captured(b, cur, pos, false);
            edit_yank(
                one(Edit::delete(cur, pos - cur)),
                cur,
                Mode::Insert,
                hint,
                reg,
            )
        }
        SearchOp::Yank => {
            let reg = captured(b, cur, pos, false);
            Plan {
                action: Action::Nop,
                cursor: cur,
                mode: st.view.mode,
                is_edit: false,
                effects: Vec::new(),
                set_register: Some(RegWrite::Yank(reg)),
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
        Command::InsertNewline => edit(
            one(Edit::insert(cur, b"\n".to_vec())),
            cur + 1,
            Mode::Insert,
            hint,
        ),
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
        Command::JoinLines => {
            // Join the current line with the next on a single space (Vim `J`). No-op on the last line.
            let le = line_end(b, cur);
            if le >= b.len() {
                nop(cur, st.view.mode)
            } else {
                // Delete the newline plus the next line's leading blanks, insert one space.
                let ws_end = hspace_end(b, le + 1);
                edit(
                    one(Edit::replace(le, ws_end - le, b" ".to_vec())),
                    le,
                    st.view.mode,
                    hint,
                )
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
                return Plan {
                    action: Action::Nop,
                    cursor: prev_boundary(b, e),
                    mode: st.view.mode,
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
            // Normal mode never rests on a non-empty line's trailing newline: when the wanted column
            // overshoots a short target line (or `$`'s MAXCOL), pull back onto its last char. Insert/Visual
            // keep the past-end column (append position / block right edge), so this is Normal-only.
            let target =
                if matches!(m, Motion::Up | Motion::Down) && matches!(st.view.mode, Mode::Normal) {
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
                    set_register: Some(RegWrite::Yank(reg)),
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
                        set_register: Some(RegWrite::Yank(reg)),
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
        Command::SetIndents {
            first_line,
            last_line,
            levels,
        } => plan_set_indents(st, *first_line, *last_line, levels, hint),
        // Paste reads the pending register (`"xp`) or the unnamed slot; `commit` clears the pending slot.
        Command::Paste { after, count } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
        ),
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
                    action: Action::Nop,
                    cursor: anchor,
                    mode: st.view.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: Some(cur),
                    set_mark: None,
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
            let reg = captured(b, s, e, line);
            match cmd {
                // Yank leaves the buffer unchanged, cursor at the selection start (Vim), back to Normal.
                Command::YankSelection => Plan {
                    action: Action::Nop,
                    cursor: s,
                    mode: Mode::Normal,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg)),
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
        Command::SearchNext(pat) => {
            let m = search_fwd(b, pat, cur + 1, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        Command::SearchPrev(pat) => {
            let m = search_bwd(b, pat, cur, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        // `/pat` as a motion: step forward to the `count`-th match (each step searches from just past the
        // last), then either move there (`Move`) or fold `[cursor, match)` into a charwise-exclusive edit
        // (`d/pat`/`c/pat`/`y/pat`). If no forward match lands past the cursor the operator aborts (Vim
        // rings the bell) — a clean no-op, never a reversed/empty edit.
        Command::Search { op, count, pattern } => {
            plan_search(st, b, cur, op, *count, pattern, hint)
        }
        // `*`/`#` are resolved by the frontend (it reads the word under the cursor from the buffer and
        // rewrites this to a concrete `SearchNext`/`SearchPrev`), so the pure core never acts on it.
        Command::SearchWordUnder { .. } => nop(cur, st.view.mode),
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
                    set_register: Some(RegWrite::Yank(reg)),
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
            let target = motion::target(b, cur, Motion::WordBack, *count);
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
            let start1 = motion::target(b, cur, Motion::WordBack, 1);
            let end1 = motion::target(b, start1, Motion::EmacsWordFwd, 1);
            let end2 = motion::target(b, end1, Motion::EmacsWordFwd, 1);
            let start2 = motion::target(b, end2, Motion::WordBack, 1);
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
            set_register: Some(RegWrite::Yank(reg)),
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
        if session.append {
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

fn paste(
    b: &[u8],
    cur: usize,
    mode: Mode,
    reg: &Register,
    after: bool,
    count: u32,
    gravity: CaretGravity,
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
        let text = repeat(reg.text());
        if after {
            let le = line_end(b, cur);
            if le < b.len() {
                // Insert after the current line's newline: the stored "...\n" becomes a fresh line below.
                mk(le + 1, text, le + 1)
            } else {
                // Last line has no trailing newline: prepend one and drop the stored trailing newline so no
                // dangling blank line is created. Cursor lands at the start of the pasted line.
                let mut bytes = vec![b'\n'];
                bytes.extend_from_slice(text.strip_suffix(b"\n").unwrap_or(&text));
                mk(le, bytes, le + 1)
            }
        } else {
            let ls = line_start(b, cur);
            mk(ls, text, ls)
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
            // else per gravity on/after the last pasted byte.
            let at = if cur < b.len() {
                next_boundary(b, cur)
            } else {
                cur
            };
            let cursor = if multiline { at } else { tail(at + n) };
            mk(at, text, cursor)
        } else {
            // Insert before the cursor; cursor lands on the first pasted byte for multi-line content, else
            // per gravity on/after the last pasted byte.
            let cursor = if multiline { cur } else { tail(cur + n) };
            mk(cur, text, cursor)
        }
    }
}
