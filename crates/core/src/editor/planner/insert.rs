//! Mode entry and the text commands of Insert and Replace mode: `i`/`a`/`o`/`R`/`gR` and
//! their variants, `<Esc>`, and what is typed, put, completed or deleted while inserting.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{edit, eval_expr_reporting, indent_cols, nop, one, replace_selection_with};
use crate::editor::{MarkWrite, RegWrite};
use crate::motion::{
    self, at_col, line_end, line_start, next_boundary, next_grapheme, prev_boundary, prev_grapheme,
};
use crate::{
    BytePos, CaretGravity, Command, Edit, EditorState, GroupHint, Mode, Motion, Plan, Register,
    SelectKind,
};

mod block_replicate;
mod replace;
mod typed;

use block_replicate::block_replicate;
use replace::plan_replace_typed;
use typed::plan_typed;

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
        // `<CR>` in Replace keeps replacing on the new line, and `<BS>` can take it back (#727).
        Command::InsertNewline if matches!(st.view.mode, Mode::Replace | Mode::VirtualReplace) => {
            plan_replace_typed(st, b, cur, hint, cmd)
        }
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
        // or unsupported expression evaluates to the empty string and inserts nothing (Vim's degrade),
        // showing nvim's error (#1570).
        Command::InsertEval(e) => {
            let (result, effects) = eval_expr_reporting(e);
            let mut plan = insert_or_nop(cur, result, hint);
            plan.effects.extend(effects);
            plan
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
        // `i_CTRL-W` — delete the word before the caret (within the current line), staying in Insert. At
        // the line start it joins the line to the one above, as `<BS>` does (nvim v0.12.6, default
        // 'backspace' "indent,eol,start"; #726).
        Command::InsertDeleteWordBack => {
            if motion::at_bol(b, cur) {
                return join_line_above(b, cur, hint);
            }
            // Clamped to the line's start, found only when the word motion crossed a `\n` (#1231).
            let wb = motion::target(b, cur, Motion::WordBack, 1);
            let wb = match b[wb.min(cur)..cur].iter().rposition(|&c| c == b'\n') {
                Some(i) => wb.max(wb.min(cur) + i + 1),
                None => wb,
            };
            // If the word motion did not move (e.g. only whitespace precedes), delete one grapheme instead.
            let start = if wb < cur { wb } else { prev_boundary(b, cur) };
            // It stops once where this Insert began (Vim's `Insstart_orig`, 'backspace' "start"), but
            // only after deleting at least one char: from the start itself it deletes on past it.
            let start = match st.view.insstart {
                Some(s) if start < s && s < cur => s,
                _ => start,
            };
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

/// `i_CTRL-W` / `i_CTRL-U` at the line start `ls`: delete the `\n` before it, joining the line to the one
/// above with the caret at the join, staying in Insert; on the buffer's first line, nothing to do.
fn join_line_above(b: &[u8], ls: usize, hint: GroupHint) -> Plan {
    if ls == 0 {
        return nop(ls, Mode::Insert);
    }
    debug_assert_eq!(b[ls - 1], b'\n');
    edit(
        one(Edit::delete(BytePos(ls - 1), 1)),
        ls - 1,
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
        return join_line_above(b, ls, hint);
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

/// Insert `text` at the caret, leaving the cursor after it — what typing, `i_CTRL-R` and a paste do.
fn insert_text(cur: usize, text: Vec<u8>, mode: Mode, hint: GroupHint) -> Plan {
    let n = text.len();
    edit(one(Edit::insert(BytePos(cur), text)), cur + n, mode, hint)
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
