//! Mode entry and the text commands of Insert and Replace mode: `i`/`a`/`o`/`R`/`gR` and
//! their variants, `<Esc>`, and what is typed, put, completed or deleted while inserting.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::*;

/// The single entry: route each Insert/Replace-mode command to its family's planner. The variants are
/// disjoint, so the families are independent; each keeps its arms (and their guards) in the old order.
pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::EnterInsert
        | Command::InsertAtLastInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::InsertColumnZero
        | Command::AppendLineEnd
        | Command::OpenBelow
        | Command::OpenAbove
        | Command::EnterNormal
        | Command::EnterTerminal
        | Command::EnterTerminalNormal
        | Command::EnterReplace
        | Command::EnterVirtualReplace => plan_mode_entry(st, b, cur, hint, cmd),
        Command::ReplaceType(..)
        | Command::ReplaceBackspace
        | Command::VirtualReplaceType(..)
        | Command::VirtualReplaceChar(..) => plan_replace_typed(st, b, cur, hint, cmd),
        Command::InsertChar(..)
        | Command::InsertNewline
        | Command::OpenLineIndent { .. }
        | Command::InsertCloser { .. } => plan_typed(st, b, cur, hint, cmd),
        Command::InsertRegister(..)
        | Command::InsertEval(..)
        | Command::InsertPaste(..)
        | Command::ReplacePaste(..)
        | Command::EmacsPaste(..) => plan_splice(st, b, cur, hint, cmd),
        Command::InsertDeleteWordBack
        | Command::InsertDeleteToLineStart
        | Command::CompleteWord { .. } => plan_delete_back(st, b, cur, hint, cmd),
        Command::InsertIndent
        | Command::InsertDedent
        | Command::InsertDeleteIndent
        | Command::InsertDeleteIndentRestore
        | Command::InsertTab => plan_indent(st, b, cur, hint, cmd),
        Command::InsertStartColumnLine { .. } | Command::InsertCaretStep { .. } => {
            plan_caret(st, b, cur, cmd)
        }
        _ => unreachable!("planner::insert plans only its own commands"),
    }
}

/// Mode entry: `i`/`a`/`I`/`gI`/`A`/`gi`, `o`/`O`, `<Esc>`, and the Terminal/Replace mode switches.
fn plan_mode_entry(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::EnterInsert => nop(cur, Mode::Insert),
        // `gi` — resume Insert at the last-insert position (snapped), or the buffer start if none yet.
        Command::InsertAtLastInsert => {
            let at = st
                .view
                .last_insert()
                .map_or(0, |p| motion::snap(b, p.min(b.len())));
            nop(at, Mode::Insert)
        }
        // `a`: Insert after the cursor's char — on an empty line (or past the last char) there is none, so
        // the caret stays rather than crossing into the next line (#701).
        Command::EnterInsertAfter => {
            let at = if cur < line_end(b, cur) {
                next_grapheme(b, cur)
            } else {
                cur
            };
            nop(at, Mode::Insert)
        }
        Command::InsertLineStart => nop(motion::first_non_blank(b, cur), Mode::Insert),
        // `gI` — insert at byte column 0 (line start), BEFORE all indentation, unlike `I` (first non-blank).
        Command::InsertColumnZero => nop(line_start(b, cur), Mode::Insert),
        Command::AppendLineEnd => nop(line_end(b, cur), Mode::Insert),
        Command::OpenBelow | Command::OpenAbove if auto_indents(st) => {
            open_line_indented(st, b, cur, hint, cmd)
        }
        Command::OpenBelow => open_below(b, cur, hint),
        Command::OpenAbove => {
            let ls = line_start(b, cur);
            edit(
                one(Edit::insert(BytePos(ls), b"\n".to_vec())),
                ls,
                Mode::Insert,
                hint,
            )
        }
        Command::EnterNormal => enter_normal(st, b, cur, hint),
        // F-011: mode-only transitions; the frontend owns the PTY + scrollback (the core never edits a
        // terminal buffer). Cursor is unchanged — a terminal buffer's document is an empty placeholder.
        Command::EnterTerminal => nop(cur, Mode::Terminal),
        Command::EnterTerminalNormal => nop(cur, Mode::TerminalNormal),
        Command::EnterReplace => nop(cur, Mode::Replace),
        Command::EnterVirtualReplace => nop(cur, Mode::VirtualReplace),
        _ => unreachable!("plan_mode_entry plans only the mode-entry commands"),
    }
}

/// What is typed (or backspaced) in Replace and Virtual Replace mode.
fn plan_replace_typed(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
    match cmd {
        Command::ReplaceType(c) => replace_type(b, cur, hint, c),
        Command::ReplaceBackspace => replace_backspace(st, b, cur, hint),
        Command::VirtualReplaceType(c) => plan_virtual_replace_type(st, b, cur, *c, hint),
        Command::VirtualReplaceChar(count, c) => {
            plan_virtual_replace_char(st, b, cur, *count, *c, hint)
        }
        _ => unreachable!("plan_replace_typed plans only the Replace-mode typing commands"),
    }
}

/// What is typed in Insert mode: a char, `<CR>`, an opened line at an indent level, an auto-closer.
fn plan_typed(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::InsertChar(c) => insert_char(st, b, cur, hint, c),
        Command::InsertNewline if auto_indents(st) && st.view.mode == Mode::Insert => {
            insert_newline_indented(st, b, cur, hint)
        }
        Command::InsertNewline => insert_text(cur, b"\n".to_vec(), Mode::Insert, hint),
        Command::OpenLineIndent { kind, level } => {
            open_line_at_level(st, b, cur, hint, kind, level)
        }
        Command::InsertCloser { ch } => insert_closer(b, cur, hint, ch),
        _ => unreachable!("plan_typed plans only the Insert-mode typing commands"),
    }
}

/// Text spliced at the caret in one step: a register, an expression result, or a paste.
fn plan_splice(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        // `i_CTRL-R{reg}` — splice the named register's raw bytes at the caret, staying in Insert with the
        // cursor after the inserted text. An empty register is a no-op. `"` reads the unnamed register (via
        // `get`'s fallback). The bytes are inserted verbatim (linewise registers keep their trailing `\n`).
        Command::InsertRegister(name) => insert_or_nop(
            cur,
            st.view.registers.get(Some(*name)).text().to_vec(),
            hint,
        ),
        // `i_CTRL-R=<expr><CR>` — evaluate the expression (a minimal-honest arithmetic/string calculator,
        // NOT full Vimscript) and splice the formatted result at the caret, staying in Insert. A malformed
        // or unsupported expression evaluates to the empty string and inserts nothing (Vim's degrade).
        Command::InsertEval(e) => {
            insert_or_nop(cur, crate::expr::eval_or_empty(e).into_bytes(), hint)
        }
        // An empty paste of any kind is a no-op in the current mode.
        Command::InsertPaste(text) | Command::ReplacePaste(text) | Command::EmacsPaste(text)
            if text.is_empty() =>
        {
            nop(cur, st.view.mode)
        }
        // A bracketed paste (#600): one literal splice at the caret, mode unchanged.
        Command::InsertPaste(text) => {
            // Over a charwise/linewise selection a paste replaces it, as a typed Select key does
            // (`vim.paste`: `<Del>` the selection, then put the text where it was), entering Insert.
            if matches!(
                st.view.mode.selection(),
                Some(SelectKind::Charwise | SelectKind::Linewise)
            ) {
                return replace_selection_with(st, b, cur, hint, text.as_bytes().to_vec());
            }
            insert_text(cur, text.as_bytes().to_vec(), st.view.mode, hint)
        }
        // A Replace-mode paste: overwrite one char of the current line per pasted char (newlines excluded,
        // as nvim's `vim.paste` counts them), then splice the text. nvim counts BYTES and can split a
        // multi-byte char; this counts characters.
        Command::ReplacePaste(text) => {
            let nchars = text.chars().filter(|&c| c != '\n').count();
            let le = line_end(b, cur);
            // A byte that is not UTF-8 is one char (#1243).
            let end = (0..nchars).fold(cur, |p, _| if p < le { next_boundary(b, p) } else { le });
            let n = text.len();
            edit(
                one(Edit::replace(
                    BytePos(cur),
                    end - cur,
                    text.as_bytes().to_vec(),
                )),
                cur + n,
                st.view.mode,
                hint,
            )
        }
        // Emacs `xterm-paste`: kill-new the text, then yank it (point after, mark at the start).
        Command::EmacsPaste(text) => Plan {
            set_register: Some(RegWrite::Write(Register::charwise(
                text.as_bytes().to_vec(),
            ))),
            set_mark: Some(MarkWrite::Set(cur)),
            ..insert_text(cur, text.as_bytes().to_vec(), st.view.mode, hint)
        },
        _ => unreachable!("plan_splice plans only the splice commands"),
    }
}

/// Splice `bytes` at the caret staying in Insert, or stay put when there are none.
fn insert_or_nop(cur: usize, bytes: Vec<u8>, hint: GroupHint) -> Plan {
    if bytes.is_empty() {
        return nop(cur, Mode::Insert);
    }
    insert_text(cur, bytes, Mode::Insert, hint)
}

/// Deleting back from the caret: `i_CTRL-W`, `i_CTRL-U`, and a completion step replacing its region.
fn plan_delete_back(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
    match cmd {
        // `i_CTRL-W` — delete the word before the caret (within the current line), staying in Insert.
        Command::InsertDeleteWordBack => {
            if motion::at_bol(b, cur) {
                return nop(cur, Mode::Insert);
            }
            // Clamped to the line's start, found only when the word motion crossed a `\n` (#1231).
            let wb = motion::target(b, cur, Motion::WordBack, 1);
            let wb = match b[wb.min(cur)..cur].iter().rposition(|&c| c == b'\n') {
                Some(i) => wb.max(wb.min(cur) + i + 1),
                None => wb,
            };
            // If the word motion did not move (e.g. only whitespace precedes), delete one grapheme instead.
            let start = if wb < cur { wb } else { prev_boundary(b, cur) };
            edit(
                one(Edit::delete(BytePos(start), cur - start)),
                start,
                Mode::Insert,
                hint,
            )
        }
        Command::InsertDeleteToLineStart => delete_to_line_start(st, b, cur, hint),
        // `i_CTRL-N` / `i_CTRL-P` — one keyword-completion cycle step: replace the `back` CHARACTERS before
        // the caret (the current completion region) with the candidate `text`, staying in Insert with the
        // caret after it. `back` walks char boundaries so multibyte keywords delete cleanly; it is clamped
        // at the buffer start. Applying repeated steps in one session chains cleanly (each deletes the last
        // applied text and inserts the next), so `.` reproduces the final accepted text.
        Command::CompleteWord { back, text } => {
            let mut start = cur;
            for _ in 0..*back {
                if start == 0 {
                    break;
                }
                start = prev_boundary(b, start);
            }
            // A candidate holding a byte that is not UTF-8 spells it as its escape char (#1252).
            let bytes = crate::pattern::unspell(text).into_owned();
            let n = bytes.len();
            edit(
                one(Edit::replace(BytePos(start), cur - start, bytes)),
                start + n,
                Mode::Insert,
                hint,
            )
        }
        _ => unreachable!("plan_delete_back plans only the delete-back commands"),
    }
}

/// The line's indent from Insert mode: `i_CTRL-T`, `i_CTRL-D`, `i_0_CTRL-D`, and `<Tab>`.
fn plan_indent(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        // `i_CTRL-T` — indent the current line to the NEXT multiple of the shiftwidth (Vim rounds: 2 cols at
        // sw=4 → 4); the caret rides right with the text. The leading whitespace is rebuilt at the new width
        // in the configured style (Vim's `change_indent`).
        Command::InsertIndent => shift_line_indent(st, b, cur, hint, false),
        // `i_CTRL-D` — dedent the current line by one shiftwidth; the caret rides left (never before the line
        // start). A no-op when there is no leading whitespace.
        Command::InsertDedent => shift_line_indent(st, b, cur, hint, true),
        // `i_0_CTRL-D` / `i_^_CTRL-D` — delete all leading whitespace of the line; the caret rides left.
        Command::InsertDeleteIndent | Command::InsertDeleteIndentRestore => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let remove = b[ls..le]
                .iter()
                .take_while(|c| matches!(c, b' ' | b'\t'))
                .count();
            if remove == 0 {
                return nop(cur, Mode::Insert);
            }
            let cursor = cur.saturating_sub(remove).max(ls);
            edit(
                one(Edit::delete(BytePos(ls), remove)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // `<Tab>` in Insert: insert whitespace at the caret to the next stop. Space style (`expandtab`)
        // inserts spaces up to the next tabstop column even when it started mid-line; tab style inserts one
        // `\t`. Within the line's leading whitespace the stop is the SHIFTWIDTH instead ('smarttab', on by
        // default in nvim): spaces under `expandtab`, else the leading whitespace is rebuilt as tabs + spaces
        // at the new width. With `ts == sw` both rules agree. Unlike `i_CTRL-T` this is caret-relative.
        Command::InsertTab => {
            let ls = line_start(b, cur);
            let (from, run) = st.view.indent.tab(b, ls, cur);
            let cursor = from + run.len();
            edit(
                one(Edit::replace(BytePos(from), cur - from, run)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        _ => unreachable!("plan_indent plans only the Insert-mode indent commands"),
    }
}

/// `i_CTRL-T` (`dedent == false`) or `i_CTRL-D` (`dedent == true`): rebuild the line's leading whitespace
/// one shiftwidth wider, or narrower, at a multiple of the shiftwidth. A dedent with no leading whitespace
/// is a no-op.
fn shift_line_indent(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    dedent: bool,
) -> Plan {
    let ls = line_start(b, cur);
    let le = line_end(b, ls);
    let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tabs());
    if dedent && ws == 0 {
        return nop(cur, Mode::Insert);
    }
    let sw = st.shift_width();
    // Up to the NEXT multiple, or down to the PREVIOUS one (Vim rounds: 2 cols at sw=4 → 4; 6 → 4).
    let width = if dedent {
        cols.saturating_sub(1) / sw * sw
    } else {
        (cols / sw + 1) * sw
    };
    let want = st.view.indent.rebuilt(&b[ls..ls + ws], width);
    let cursor = reindented_caret(cur, ls, ws, want.len());
    edit(
        one(Edit::replace(BytePos(ls), ws, want)),
        cursor,
        Mode::Insert,
        hint,
    )
}

/// Caret moves that stay in the current mode: `i_CTRL-G_j`/`k` and `i_CTRL-G_U` + `<Left>`/`<Right>`.
fn plan_caret(st: &EditorState, b: &[u8], cur: usize, cmd: &Command) -> Plan {
    match cmd {
        // `i_CTRL-G_j` / `i_CTRL-G_k` — the adjacent line, at the column the insert started in (char columns,
        // like the rest of the motion model). No adjacent line: stay put (nvim beeps).
        Command::InsertStartColumnLine { down } => {
            let ls = line_start(b, cur);
            let target = if *down {
                let le = line_end(b, cur);
                (le < crate::motion::content_end(b)).then(|| le + 1)
            } else {
                (ls > 0).then(|| line_start(b, ls - 1))
            };
            let to = target.map_or(cur, |t| at_col(b, t, st.view.insert_col));
            nop(to, st.view.mode)
        }
        // `i_CTRL-G_U` + `<Left>`/`<Right>`: one char within the line (the undo-group keep is in
        // `apply_command`).
        Command::InsertCaretStep { right } => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let to = if *right {
                if cur < le {
                    next_boundary(b, cur)
                } else {
                    cur
                }
            } else if cur > ls {
                prev_boundary(b, cur)
            } else {
                cur
            };
            nop(to, st.view.mode)
        }
        _ => unreachable!("plan_caret plans only the Insert-mode caret steps"),
    }
}

/// `o`/`O` under `'autoindent'`/`'smartindent'` (Vim profile): the new line takes the indent nvim's
/// `open_line` gives it ([`IndentConfig::open_indent`]).
fn open_line_indented(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let below = matches!(cmd, Command::OpenBelow);
    let (pad, _) = st
        .view
        .indent
        .open_indent(b, ls, le, below, &st.view.format.cinwords);
    let mut ins = Vec::with_capacity(pad.len() + 1);
    if below && le == ls && le < b.len() {
        // An empty line: the new line goes in after its `\n`, so marks stay on it (#1125).
        ins.extend_from_slice(&pad);
        ins.push(b'\n');
        edit(
            one(Edit::insert(BytePos(le + 1), ins)),
            le + 1 + pad.len(),
            Mode::Insert,
            hint,
        )
    } else if below {
        ins.push(b'\n');
        ins.extend_from_slice(&pad);
        edit(
            one(Edit::insert(BytePos(le), ins)),
            le + 1 + pad.len(),
            Mode::Insert,
            hint,
        )
    } else {
        ins.extend_from_slice(&pad);
        ins.push(b'\n');
        edit(
            one(Edit::insert(BytePos(ls), ins)),
            ls + pad.len(),
            Mode::Insert,
            hint,
        )
    }
}

/// `o` with no indent rule: open a line below the current one.
fn open_below(b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    let le = line_end(b, cur);
    // On an empty line the new line goes in after its `\n`, the same bytes as before it, so its
    // marks read as staying on it (nvim `o`; `O` there moves them down, #1125).
    let at = if le == line_start(b, cur) && le < b.len() {
        le + 1
    } else {
        le
    };
    edit(
        one(Edit::insert(BytePos(at), b"\n".to_vec())),
        le + 1,
        Mode::Insert,
        hint,
    )
}

/// `<Esc>`: back to Normal. Leaving Insert or Replace moves the cursor one left (never before the line
/// start), after a blockwise insert replicates its text or an unused auto-indent is dropped; leaving a
/// selection keeps the cursor, off the newline.
fn enter_normal(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    // `<Esc>` closing a blockwise insert-replicate session replicates the top row's typed text down
    // the block (see `block_replicate`); the session is cleared in `commit` as Insert is left.
    if st.view.mode == Mode::Insert {
        if let Some(session) = st.view.block_insert {
            return block_replicate(b, cur, session, hint, st.view.indent.tabs());
        }
        // Autoindent cleanup (Vim): if this Insert session opened a line with auto-indent and
        // nothing non-blank was typed on it, leaving Insert removes the indent so no trailing
        // whitespace is left. Guarded to ONLY a line that is entirely blank with the caret at its
        // end — exactly the auto-indent-leftover shape — so user-typed content is never touched.
        if st.view.auto_indent_pending {
            // nvim's `stop_insert` with `did_ai`: the blanks before the caret at the line end go,
            // the whole indent of a line holding nothing else, or the blank after a comment
            // leader (`o<Esc>` on `// a` leaves `//`, #618).
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let mut cut = le;
            while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
                cut -= 1;
            }
            if cur == le && cut < le {
                return edit(
                    one(Edit::delete(BytePos(cut), le - cut)),
                    cut,
                    Mode::Normal,
                    hint,
                );
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
        let c = if cur > ls { prev_grapheme(b, cur) } else { cur };
        nop(c, Mode::Normal)
    } else if st.view.mode.selection().is_some() && st.view.caret == CaretGravity::OnChar {
        // A Visual cursor past the line's last char (`v$`, `<C-v>$`, `$vj`) comes back onto it, as
        // Normal mode never rests on the newline (#1207).
        let (ls, le) = (line_start(b, cur), line_end(b, cur));
        let c = if cur == le && le > ls {
            prev_grapheme(b, cur)
        } else {
            cur
        };
        nop(c, Mode::Normal)
    } else {
        nop(cur, Mode::Normal)
    }
}

/// A char typed in Replace mode: overwrite the char under the cursor, or append at the line end.
fn replace_type(b: &[u8], cur: usize, hint: GroupHint, c: &char) -> Plan {
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes().to_vec();
    let tn = typed.len();
    let le = line_end(b, cur);
    let (edits, push) = if cur < le {
        // Overwrite the char under the cursor; remember its original bytes for `<BS>` restore.
        let nb = next_boundary(b, cur);
        (
            one(Edit::replace(BytePos(cur), nb - cur, typed)),
            Some(Some(b[cur..nb].to_vec())),
        )
    } else {
        // At end-of-line there is nothing to overwrite: append, remembered as `None` (backspace deletes).
        (one(Edit::insert(BytePos(cur), typed)), Some(None))
    };
    Plan {
        action: Action::ReplaceTxn {
            edits,
            hint,
            push,
            pop: false,
        },
        is_edit: true,
        ..nop(cur + tn, Mode::Replace)
    }
}

/// A char typed in Insert mode, with smartindent realignment, comment ending and auto-wrap.
fn insert_char(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, c: &char) -> Plan {
    let mut buf = [0u8; 4];
    let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
    let n = bytes.len();
    // `'smartindent'`: a `{`, `}` or `#` typed first on a line just opened realigns it.
    let ls = line_start(b, cur);
    let si = st.view.si_state;
    if auto_indents(st)
        && st.view.indent.smartindent
        && si.armed()
        && st.view.mode == Mode::Insert
        && b[ls..cur].iter().all(|&x| x == b' ' || x == b'\t')
    {
        if let Some(pad) = st.view.indent.typed_indent(b, ls, cur, si, *c) {
            let cursor = ls + pad.len() + n;
            let mut repl = pad;
            repl.extend_from_slice(&bytes);
            return edit(
                one(Edit::replace(BytePos(ls), cur - ls, repl)),
                cursor,
                Mode::Insert,
                hint,
            );
        }
    }
    if let Some(p) = plan_end_comment(st, b, cur, *c, hint) {
        return p;
    }
    if let Some(p) = auto_wrap(st, b, cur, *c, hint) {
        return p;
    }
    edit(
        one(Edit::insert(BytePos(cur), bytes)),
        cur + n,
        Mode::Insert,
        hint,
    )
}

/// `i_CTRL-U` (nvim v0.12.6, default 'backspace' "indent,eol,start" and 'autoindent'): delete the text
/// entered before the caret on this line; with none, delete back to the first non-blank, and from
/// there the indent too. At the line start it joins the line to the one above, as `<BS>` does.
fn delete_to_line_start(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    let ls = line_start(b, cur);
    if cur <= ls {
        if ls == 0 {
            return nop(cur, Mode::Insert);
        }
        return edit(
            one(Edit::delete(BytePos(ls - 1), 1)),
            ls - 1,
            Mode::Insert,
            hint,
        );
    }
    let mut fnb = ls;
    while fnb < cur && matches!(b[fnb], b' ' | b'\t') {
        fnb += 1;
    }
    let floor = if cur > fnb { fnb } else { ls };
    // Where this Insert began (Vim's `Insstart_orig`): a caret key inside it does not move it.
    let insstart = st.view.insstart.unwrap_or(cur);
    let start = if (ls..cur).contains(&insstart) {
        insstart.max(floor)
    } else {
        floor
    };
    edit(
        one(Edit::delete(BytePos(start), cur - start)),
        start,
        Mode::Insert,
        hint,
    )
}

/// An Insert `<CR>` under `'autoindent'`/`'smartindent'` (Vim profile): the blanks after the caret
/// go, the new line takes the indent of the text before it, and a line holding only the indent the
/// last open gave it is emptied (nvim's `open_line` with `did_ai`).
fn insert_newline_indented(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let (mut pad, _) = st
        .view
        .indent
        .open_indent(b, ls, cur, true, &st.view.format.cinwords);
    // After `^ CTRL-D` an unindented line hands the deleted indent to the next one (`old_indent`).
    if pad.is_empty() && st.view.old_indent > 0 && st.view.indent.autoindent {
        pad = st.view.indent.whitespace(st.view.old_indent);
    }
    let mut cut = cur;
    if st.view.auto_indent_pending {
        while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
            cut -= 1;
        }
    }
    let mut after = cur;
    while after < le && matches!(b[after], b' ' | b'\t') {
        after += 1;
    }
    let mut ins = Vec::with_capacity(pad.len() + 1);
    ins.push(b'\n');
    ins.extend_from_slice(&pad);
    let cursor = cut + ins.len();
    edit(
        one(Edit::replace(BytePos(cut), after - cut, ins)),
        cursor,
        Mode::Insert,
        hint,
    )
}

/// `o`/`O`/`<CR>` with a tree-suggested indent (F-015 Phase 2): open a line whose leading whitespace
/// is `level × unit`, cursor after it. `Above` inserts `<indent>\n` before the line (cursor on the new
/// line above); `Below`/`Split` insert `\n<indent>` (cursor on the new line below / after the split).
/// `level: 0` degrades to a plain open, so this also serves as the non-tree fallback.
fn open_line_at_level(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    kind: &OpenKind,
    level: &usize,
) -> Plan {
    let pad = st.indent_for_level(*level);
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
    edit(
        one(Edit::insert(BytePos(at), ins)),
        cursor,
        Mode::Insert,
        hint,
    )
}

/// A closer (`}`/`)`/`]`) typed as the line's sole leading content realigns the line to its matching
/// opener's indent, then inserts the closer — smartindent-like (F-015 Phase 3a). Deterministic bytes
/// bracket-match; with content already before the cursor, or no matching opener, it is a plain insert.
fn insert_closer(b: &[u8], cur: usize, hint: GroupHint, ch: &char) -> Plan {
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
                one(Edit::replace(BytePos(ls), cur - ls, repl)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        None => edit(
            one(Edit::insert(BytePos(cur), typed)),
            cur + n,
            Mode::Insert,
            hint,
        ),
    }
}

/// Shared by Replace (`R`) and Virtual Replace (`gR`) — the restore stack is identical; the mode is
/// preserved so `<BS>` stays in whichever replace mode is active.
fn replace_backspace(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    match st.view.replace_stack.last() {
        // At the session start `<BS>` only moves the cursor left (Vim does not restore past the start).
        None => {
            let ls = line_start(b, cur);
            let c = if cur > ls { prev_grapheme(b, cur) } else { cur };
            nop(c, st.view.mode)
        }
        Some(entry) => {
            let start = prev_boundary(b, cur); // the last typed char occupies [start, cur)
            let edits = match entry {
                Some(orig) => one(Edit::replace(BytePos(start), cur - start, orig.clone())),
                None => one(Edit::delete(BytePos(start), cur - start)),
            };
            Plan {
                action: Action::ReplaceTxn {
                    edits,
                    hint,
                    push: None,
                    pop: true,
                },
                is_edit: true,
                ..nop(start, st.view.mode)
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
        (one(Edit::insert(BytePos(cur), typed)), Some(None), cur + tn)
    } else if b[cur] == b'\t' {
        // Over a TAB: it spans `w` virtual columns to the next tabstop. While more than one column
        // remains, INSERT before the tab (it shrinks, backspace regrows it); on the LAST column, replace
        // the tab with the typed char (backspace restores the tab).
        let ls = line_start(b, cur);
        let vcol = motion::vcol_of(b, ls, cur, st.view.indent.tabs());
        let w = st.view.indent.tabs().padding(vcol);
        if w > 1 {
            (one(Edit::insert(BytePos(cur), typed)), Some(None), cur + tn)
        } else {
            (
                one(Edit::replace(BytePos(cur), 1, typed)),
                Some(Some(vec![b'\t'])),
                cur + tn,
            )
        }
    } else {
        // Over a normal char: overwrite it (remember the original for `<BS>`), like Replace.
        let nb = next_boundary(b, cur);
        (
            one(Edit::replace(BytePos(cur), nb - cur, typed)),
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
        is_edit: true,
        ..nop(cursor, Mode::VirtualReplace)
    }
}

/// `{count}gr{char}` — CLASSIC-Vim one-shot virtual replace: overwrite `count` chars with `char` using the
/// tab-aware [`plan_virtual_replace_type`] policy, then return to Normal (the one-shot form of `gR`, as `r`
/// is to `R`). Because a single typed char is never `\n`, every step stays on the cursor's own line, so the
/// whole change is one replace of `[cur, line_end)` computed by SIMULATING the `gR`-typing loop on an owned
/// copy of the line. Over a `<Tab>` this shrinks/inserts to preserve the following text's column; when
/// `count` overruns the line it APPENDS the surplus past end-of-line (Vim), unlike `r`. The cursor lands on
/// the last replaced char (Vim leaves Replace with a one-boundary left nudge).
fn plan_virtual_replace_char(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    c: char,
    hint: GroupHint,
) -> Plan {
    let tabw = st.view.indent.tabs();
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    // Work on an owned copy of the whole line so tab virtual-column math sees the evolving text; positions
    // are relative to `ls`. Only bytes from the cursor onward are ever touched.
    let mut line: Vec<u8> = b[ls..le].to_vec();
    let start = cur - ls; // cursor offset within `line`
    let mut p = start;
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes();
    let tn = typed.len();
    for done in 0..count {
        if p >= line.len() {
            // Past end-of-line: append the whole surplus at once (virtual Replace grows the line here),
            // bounded like every put ([`MAX_PUT_BYTES`]) so a hostile count cannot exhaust memory (#683).
            let rest = ((count - done) as usize).min((MAX_PUT_BYTES / tn).max(1));
            line.reserve(rest * tn);
            for _ in 0..rest {
                line.extend_from_slice(typed);
            }
            p += rest * tn;
            break;
        } else if line[p] == b'\t' {
            // Over a TAB spanning `w` virtual columns: insert before it (shrinking it) while more than one
            // column remains; on its last column replace the tab itself — preserving the next char's column.
            let vcol = motion::vcol_of(&line, 0, p, tabw);
            let w = tabw.padding(vcol);
            if w > 1 {
                line.splice(p..p, typed.iter().copied());
            } else {
                line.splice(p..p + 1, typed.iter().copied());
            }
        } else {
            // Over a normal char: overwrite exactly one grapheme boundary's worth.
            let nb = next_boundary(&line, p);
            line.splice(p..nb, typed.iter().copied());
        }
        p += tn;
    }
    // One replace of the line tail from the cursor; everything before `start` is unchanged.
    let new_tail = line[start..].to_vec();
    // Vim leaves Replace nudging left one boundary → the cursor rests on the LAST typed char at `ls + p - tn`
    // (p advanced by at least one `tn` per iteration, so this never precedes the line start).
    let cursor = ls + p - tn;
    edit(
        one(Edit::replace(BytePos(cur), le - cur, new_tail)),
        cursor,
        Mode::Normal,
        hint,
    )
}

/// Insert `text` at the caret, leaving the cursor after it — what typing, `i_CTRL-R` and a paste do.
fn insert_text(cur: usize, text: Vec<u8>, mode: Mode, hint: GroupHint) -> Plan {
    let n = text.len();
    edit(one(Edit::insert(BytePos(cur), text)), cur + n, mode, hint)
}

/// Typing the armed comment-end character first on a line opened inside a three-part comment replaces the
/// middle leader by the end one (nvim's `insertchar` with `end_comment_pending`): ` * ` + `/` is ` */`.
fn plan_end_comment(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    c: char,
    hint: GroupHint,
) -> Option<Plan> {
    let ch = u8::try_from(c).ok()?;
    if !st.view.auto_indent_pending
        || st.view.end_comment != Some(ch)
        || st.view.mode != Mode::Insert
    {
        return None;
    }
    let ls = line_start(b, cur);
    let line = &b[ls..line_end(b, cur)];
    let parts = format::parse_comments(&st.view.format.comments);
    let (n, k) = format::leader_len(line, &parts, false, true);
    let k = k.filter(|&k| n > 0 && parts[k].flags.contains('m'))?;
    let mut middle = parts[k].text.as_slice();
    while middle.last().is_some_and(|&x| x == b' ' || x == b'\t') {
        middle = &middle[..middle.len() - 1];
    }
    let end = &parts.get(k + 1)?.text;
    let mut i = cur - ls;
    while i > 0 && matches!(line[i - 1], b' ' | b'\t') {
        i -= 1;
    }
    let i = i.checked_sub(middle.len())?;
    if end.last() != Some(&ch) {
        return None;
    }
    Some(edit(
        one(Edit::replace(BytePos(ls + i), cur - (ls + i), end.clone())),
        ls + i + end.len(),
        Mode::Insert,
        hint,
    ))
}

/// Plan a linewise shift (`>>` / `<<`) over `count` lines from the cursor's line down. `right` adds one
/// indent level to each line; `!right` removes up to one. Empty lines are never indented (Vim); the cursor
/// lands on the first non-blank of the cursor's (first) line, exactly as Vim leaves it. The register is
/// untouched. Edits are one-per-line at distinct line starts, so the [`EditList`] is disjoint by construction.
/// `=` reindent (bracket-depth model): set each line in `[first_line, last_line]` (0-based, inclusive) to
/// `depth × shiftwidth` of indent, where `depth` is the net unclosed `([{` before the line; a line whose first
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

/// Auto-wrap while typing (nvim's `insertchar` → `internal_format`, #618): typing the non-blank `c` at
/// `cur` in Insert past `'textwidth'` (or the `'wrapmargin'` width) breaks the line at a blank before the
/// cursor, the new line taking the indent and, with `'formatoptions'` `c`, the comment leader; then `c`
/// goes in. `None` when nothing wraps.
fn auto_wrap(st: &EditorState, b: &[u8], cur: usize, c: char, hint: GroupHint) -> Option<Plan> {
    if st.view.caret != CaretGravity::OnChar
        || st.view.mode != Mode::Insert
        || c == ' '
        || c == '\t'
    {
        return None;
    }
    let tw = st.view.text_width_for(false);
    if tw == 0 {
        return None;
    }
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    // 'formatoptions' `l`: a line already longer than 'textwidth' when this insert began is left alone.
    if st.view.format.has('l') {
        if let Some((line, len)) = st.view.insstart_line {
            if line == crate::pos::raw::line_of(b, cur) && len > tw {
                return None;
            }
        }
    }
    let o = fmt_opts(st);
    let mut fb = format::FmtBuf {
        lines: vec![b[ls..le].to_vec()],
        cur: (0, cur - ls),
        saved: None,
        before: None,
        after: None,
    };
    fb.internal_format(tw, -1, false, false, false, Some(c), &o);
    if fb.lines.len() == 1 {
        return None;
    }
    let (l, col) = fb.cur;
    let at: usize = fb.lines[..l].iter().map(|x| x.len() + 1).sum::<usize>() + col;
    let mut text = fb.lines.join(&b'\n');
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes();
    text.splice(at..at, typed.iter().copied());
    Some(edit(
        one(Edit::replace(BytePos(ls), le - ls, text)),
        ls + at + typed.len(),
        Mode::Insert,
        hint,
    ))
}

/// Finish a blockwise insert-replicate session (`<Esc>` after `CTRL-V` `I`/`A`/`c`): take the text typed
/// on the top row since `session.insert_start` and insert it at `session.target_col` on each of the
/// `rows_below` rows beneath the top line. `append` rows pad short lines with spaces; non-append (`I`/`c`)
/// rows shorter than the target column are skipped (Vim). A newline typed on the top row aborts the
/// replicate (Vim). For `I`/`A` the cursor returns to the block's top-left corner (`session.top_left`);
/// for `c` (change) it rests one char left of the top row's typed text — the normal Insert-exit position,
/// which Vim keeps for a block change (matters only when more than one char was typed).
fn block_replicate(b: &[u8], cur: usize, session: BlockInsert, hint: GroupHint, ts: Tabs) -> Plan {
    let nop = |cursor: usize| nop(cursor, Mode::Normal);
    let mut start = session.insert_start;
    // Insert left the block's top line (a `<BS>` at its start joined it onto the line above): nothing is
    // replicated, as Vim's `op_insert` gives up when the cursor's line changed, and the cursor steps left
    // as a plain `<Esc>` does. `insert_start` and `top_left` no longer point into that line.
    let cur_ls = line_start(b, cur);
    let tls = session.top_line_start;
    if session.change && cur_ls > tls && line_start(b, tls) == tls {
        return block_change_split(b, cur, &session, hint, ts);
    }
    if cur_ls != tls {
        return nop(motion::prev_boundary(b, cur).max(cur_ls));
    }
    // The cursor rests at the block's top-left when the session ends — for `I`/`A`, and for a `c` where the
    // typed run is empty/aborted (nothing to sit after). A non-empty block CHANGE instead rests one char
    // left of the top row's typed text (Vim's normal Insert-exit), computed once `typed` is known below.
    let end_cursor = session.top_left.min(b.len());
    // A `<BS>` past the insert start and wider text typed over it (`c<BS><BS>é`) can leave `start` inside a
    // char. nvim copies the net bytes added from the start column regardless and writes a split char onto
    // the rows below (verified vs nvim v0.12.6); here the copy starts at the next char instead (#1149).
    while start < cur && start < b.len() && (b[start] & 0xC0) == 0x80 {
        start += 1;
    }
    // The text typed on the top row. Empty (or containing a newline) → nothing to replicate.
    if cur <= start || b[start..cur].contains(&b'\n') {
        return nop(end_cursor);
    }
    let typed = &b[start..cur];
    let rest_cursor = if session.change {
        motion::prev_boundary(b, cur)
    } else {
        end_cursor
    };

    let mut edits: Vec<Edit> = Vec::new();
    let mut rs = session.top_line_start;
    for _ in 0..session.rows_below {
        let le = line_end(b, rs);
        if le >= b.len() {
            break; // no further line to replicate onto
        }
        rs = le + 1;
        let row_le = line_end(b, rs);
        if session.to_eol {
            // `$A`: append at THIS row's own end (ragged), never a fixed column.
            edits.push(Edit::insert(BytePos(row_le), typed.to_vec()));
            continue;
        }
        // Non-append + short row: skipped (Vim leaves lines shorter than the block's left edge untouched).
        if !session.append && line_cells(b, rs, ts) < session.target_col {
            continue;
        }
        let (at, del, pre, post) =
            block_insert_point(b, rs, session.target_col, ts, session.append);
        let mut ins = vec![b' '; pre];
        ins.extend_from_slice(typed);
        ins.extend(std::iter::repeat_n(b' ', post));
        edits.push(Edit::replace(BytePos(at), del, ins));
    }
    if edits.is_empty() {
        return nop(rest_cursor);
    }
    let list = EditList::new(edits).expect("block-replicate inserts are disjoint (one per line)");
    edit(list, rest_cursor, Mode::Normal, hint)
}

/// A block change (`c`/`C`) whose Insert broke the top row with a `<CR>` (#1142), as nvim's `op_change`:
/// the text the top row GREW by (its length now less its length when Insert began), taken from the insert
/// column, goes into the lines BELOW the top row by line number, the rows the block had: the lines the
/// `<CR>` opened come first, so `x<CR>y` over two rows writes the `x` into the `y` line too. A line that
/// ends before the block's left column is skipped. The cursor stays where `<Esc>` leaves it, at its byte
/// column.
fn block_change_split(
    b: &[u8],
    cur: usize,
    session: &BlockInsert,
    hint: GroupHint,
    ts: Tabs,
) -> Plan {
    let cur_ls = line_start(b, cur);
    let rest = motion::prev_boundary(b, cur).max(cur_ls);
    let tls = session.top_line_start;
    let tle = line_end(b, tls);
    let at = session.insert_start.clamp(tls, tle);
    let grew = (tle - tls).saturating_sub(session.top_len);
    let mut end = (at + grew).min(tle);
    // nvim copies bytes; a character the cut would split is left out.
    while end > at && end < tle && (b[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    if end <= at {
        return nop(rest, Mode::Normal);
    }
    let text = &b[at..end];
    let mut edits: Vec<Edit> = Vec::new();
    let mut rs = tls;
    for _ in 0..session.rows_below {
        let le = line_end(b, rs);
        if le >= b.len() {
            break;
        }
        rs = le + 1;
        if line_cells(b, rs, ts) < session.target_col {
            continue;
        }
        edits.push(Edit::insert(
            BytePos(range::want_col(b, rs, session.target_col, ts)),
            text.to_vec(),
        ));
    }
    if edits.is_empty() {
        return nop(rest, Mode::Normal);
    }
    // The lines above the cursor's grew; its own line keeps the cursor's byte column.
    let shift: usize = edits
        .iter()
        .filter(|e| e.pos.0 < cur_ls)
        .map(|e| e.ins.len())
        .sum();
    let list = EditList::new(edits).expect("one insert per line, top to bottom: disjoint");
    edit(list, rest + shift, Mode::Normal, hint)
}

/// Whether `o`/`O`/`<CR>` indent the line they open: `'autoindent'` or `'smartindent'`, under the Vim
/// profile (an Emacs `newline` never indents).
fn auto_indents(st: &EditorState) -> bool {
    st.view.caret == CaretGravity::OnChar
        && (st.view.indent.autoindent || st.view.indent.smartindent)
}

/// Where an Insert caret at `cur` lands after its line's leading whitespace (`old_ws` bytes from `ls`) is
/// rebuilt as `new_ws` bytes: it moves by the change in length (riding with the text, nvim also inside the
/// indent), never before the line start.
fn reindented_caret(cur: usize, ls: usize, old_ws: usize, new_ws: usize) -> usize {
    (cur + new_ws).saturating_sub(old_ws).max(ls)
}
