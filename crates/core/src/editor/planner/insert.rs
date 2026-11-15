//! Mode entry and the text commands of Insert and Replace mode: `i`/`a`/`o`/`R`/`gR` and
//! their variants, `<Esc>`, and what is typed, put, completed or deleted while inserting.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::*;

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
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
        // `o`/`O` under `'autoindent'`/`'smartindent'` (Vim profile): the new line takes the indent nvim's
        // `open_line` gives it ([`IndentConfig::open_indent`]).
        Command::OpenBelow | Command::OpenAbove if auto_indents(st) => {
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
        Command::OpenBelow => {
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
        Command::OpenAbove => {
            let ls = line_start(b, cur);
            edit(
                one(Edit::insert(BytePos(ls), b"\n".to_vec())),
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
        Command::VirtualReplaceChar(count, c) => {
            plan_virtual_replace_char(st, b, cur, *count, *c, hint)
        }
        Command::InsertChar(c) => {
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
        // `i_CTRL-R{reg}` — splice the named register's raw bytes at the caret, staying in Insert with the
        // cursor after the inserted text. An empty register is a no-op. `"` reads the unnamed register (via
        // `get`'s fallback). The bytes are inserted verbatim (linewise registers keep their trailing `\n`).
        Command::InsertRegister(name) => {
            let bytes = st.view.registers.get(Some(*name)).text().to_vec();
            if bytes.is_empty() {
                return nop(cur, Mode::Insert);
            }
            let n = bytes.len();
            edit(
                one(Edit::insert(BytePos(cur), bytes)),
                cur + n,
                Mode::Insert,
                hint,
            )
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
            edit(
                one(Edit::insert(BytePos(cur), bytes)),
                cur + n,
                Mode::Insert,
                hint,
            )
        }
        // A bracketed paste (#600): one literal splice at the caret, mode unchanged.
        Command::InsertPaste(text) => {
            if text.is_empty() {
                return nop(cur, st.view.mode);
            }
            // Over a charwise/linewise selection a paste replaces it, as a typed Select key does
            // (`vim.paste`: `<Del>` the selection, then put the text where it was), entering Insert.
            if matches!(
                st.view.mode.selection(),
                Some(SelectKind::Charwise | SelectKind::Linewise)
            ) {
                return replace_selection_with(st, b, cur, hint, text.as_bytes().to_vec());
            }
            let n = text.len();
            edit(
                one(Edit::insert(BytePos(cur), text.as_bytes().to_vec())),
                cur + n,
                st.view.mode,
                hint,
            )
        }
        // A Replace-mode paste: overwrite one char of the current line per pasted char (newlines excluded,
        // as nvim's `vim.paste` counts them), then splice the text. nvim counts BYTES and can split a
        // multi-byte char; this counts characters.
        Command::ReplacePaste(text) => {
            if text.is_empty() {
                return nop(cur, st.view.mode);
            }
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
        Command::EmacsPaste(text) => {
            if text.is_empty() {
                return nop(cur, st.view.mode);
            }
            let n = text.len();
            Plan {
                set_register: Some(RegWrite::Write(Register::charwise(
                    text.as_bytes().to_vec(),
                ))),
                set_mark: Some(MarkWrite::Set(cur)),
                ..edit(
                    one(Edit::insert(BytePos(cur), text.as_bytes().to_vec())),
                    cur + n,
                    st.view.mode,
                    hint,
                )
            }
        }
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
        // `i_CTRL-U` (nvim v0.12.6, default 'backspace' "indent,eol,start" and 'autoindent'): delete the text
        // entered before the caret on this line; with none, delete back to the first non-blank, and from
        // there the indent too. At the line start it joins the line to the one above, as `<BS>` does.
        Command::InsertDeleteToLineStart => {
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
        // `i_CTRL-T` — indent the current line to the NEXT multiple of the shiftwidth (Vim rounds: 2 cols at
        // sw=4 → 4); the caret rides right with the text. The leading whitespace is rebuilt at the new width
        // in the configured style (Vim's `change_indent`).
        Command::InsertIndent => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tabs());
            let sw = st.shift_width();
            let want = st
                .view
                .indent
                .rebuilt(&b[ls..ls + ws], (cols / sw + 1) * sw);
            let cursor = reindented_caret(cur, ls, ws, want.len());
            edit(
                one(Edit::replace(BytePos(ls), ws, want)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-D` — dedent the current line by one shiftwidth; the caret rides left (never before the line
        // start). A no-op when there is no leading whitespace.
        Command::InsertDedent => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tabs());
            if ws == 0 {
                return nop(cur, Mode::Insert);
            }
            // Down to the PREVIOUS multiple of the shiftwidth (Vim rounds: 6 cols at sw=4 → 4).
            let sw = st.shift_width();
            let want = st
                .view
                .indent
                .rebuilt(&b[ls..ls + ws], cols.saturating_sub(1) / sw * sw);
            let cursor = reindented_caret(cur, ls, ws, want.len());
            edit(
                one(Edit::replace(BytePos(ls), ws, want)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
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
        // An Insert `<CR>` under `'autoindent'`/`'smartindent'` (Vim profile): the blanks after the caret
        // go, the new line takes the indent of the text before it, and a line holding only the indent the
        // last open gave it is emptied (nvim's `open_line` with `did_ai`).
        Command::InsertNewline if auto_indents(st) && st.view.mode == Mode::Insert => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let (mut pad, _) =
                st.view
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
        Command::InsertNewline => edit(
            one(Edit::insert(BytePos(cur), b"\n".to_vec())),
            cur + 1,
            Mode::Insert,
            hint,
        ),
        // `o`/`O`/`<CR>` with a tree-suggested indent (F-015 Phase 2): open a line whose leading whitespace
        // is `level × unit`, cursor after it. `Above` inserts `<indent>\n` before the line (cursor on the new
        // line above); `Below`/`Split` insert `\n<indent>` (cursor on the new line below / after the split).
        // `level: 0` degrades to a plain open, so this also serves as the non-tree fallback.
        Command::OpenLineIndent { kind, level } => {
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
        _ => unreachable!("planner::insert plans only its own commands"),
    }
}
