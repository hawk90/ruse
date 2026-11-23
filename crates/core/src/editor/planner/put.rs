//! Puts and registers: `p`/`P` and their indent and block variants, a put over a Visual selection, and
//! setting a register; with the put planners [`paste`], `paste_block` and [`plan_paste_selection`] (#1350).
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{captured, indent_cols, nop, one, MAX_PUT_BYTES};
use crate::editor::{
    at_vcol, block_geom, cluster_cells, line_cells, selection_range, text_cells, vcols_at, Action,
    RegWrite, MAXCOL,
};
use crate::motion::{self, line_end, line_start, next_boundary, next_grapheme, prev_boundary};
use crate::{
    BytePos, CaretGravity, Command, Edit, EditList, EditorState, GroupHint, IndentStyle, Mode,
    Plan, RegKind, Register, SelectKind, Tabs,
};

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, cmd: &Command) -> Plan {
    match cmd {
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
            true,
            st.view.indent.tabs(),
        ),
        // `zp` / `zP` — `p` / `P`, but a block's short rows get no trailing spaces (nvim `PUT_BLOCK_INNER`).
        Command::PasteBlockInner { after, count } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
            false,
            false,
            st.view.indent.tabs(),
        ),
        // `]p`/`[p` — indent-adjusting paste. For a linewise register, reindent its lines to the current
        // line's indent before pasting; a charwise/blockwise register pastes unchanged (Vim `]p` = `p`).
        Command::PasteIndent { after, count } => {
            let reg = st.view.registers.get(st.view.pending_register);
            if reg.is_linewise() {
                let (target_cols, _) = indent_cols(
                    &b[line_start(b, cur)..line_end(b, cur)],
                    st.view.indent.tabs(),
                );
                let adjusted = reindent_register(
                    reg.text(),
                    target_cols,
                    st.view.indent.tabs(),
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
                    true,
                    st.view.indent.tabs(),
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
                    true,
                    st.view.indent.tabs(),
                )
            }
        }
        // `"x` — install the one-shot pending register. A pure state set: no edit, no cursor/mode change.
        Command::SetRegister(name) => Plan {
            action: Action::SetPending(*name),
            ..nop(cur, st.view.mode)
        },
        // `"=<expr><CR>` — evaluate the expression, store the formatted result in the `"=` slot, and arm `=`
        // as the pending register so the following `p`/`P` pastes it. An empty result (malformed/unsupported
        // expression) makes that paste a no-op (Vim's degrade). Like `SetRegister`, a pure state set.
        Command::SetExprRegister(e) => Plan {
            action: Action::SetExprPending(crate::expr::eval_or_empty(e)),
            ..nop(cur, st.view.mode)
        },
        _ => unreachable!("planner::put plans only its own commands"),
    }
}

/// Visual `p`/`P` (nvim `nv_put_opt`): delete the selection, then put the register where it was, shaped by
/// the selection and register kinds:
/// - a linewise selection puts any register as whole lines (a charwise or block register becomes lines);
/// - a charwise selection with a linewise register splits the line and puts the lines between the halves;
/// - a block selection with a one-line charwise register puts that text on every row of the block (rows
///   shorter than the block's left edge are skipped); with a linewise register `p` puts the lines below the
///   block and `P` above it;
/// - anything else is an ordinary put at the selection's start.
///
/// The put goes before the cursor, or after it when the delete left the cursor on the last character of
/// the line or file (Vim's "cursor at the end" test). The register is read before the delete, so `p` of the
/// unnamed register still puts the old text; `p` then leaves the deleted text in the unnamed register, `P`
/// leaves the registers as they were. A selection with nothing in it to delete (nvim's `op_delete` returns
/// first) only puts and leaves the registers as they were too: any selection on the emptied buffer's line
/// (`ML_EMPTY`), a charwise one on the empty last line, and a one-row block on an empty line (#1346). The
/// whole replacement is one edit and one undo step.
pub(super) fn plan_paste_selection(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    swap: bool,
    count: u32,
) -> Plan {
    let (Some(kind), Some(anchor)) = (st.view.mode.selection(), st.view.anchor) else {
        return nop(cur, Mode::Normal);
    };
    let ts = st.view.indent.tabs();
    let reg = st.view.registers.get(st.view.pending_register).clone();
    // 1. The delete: its edits, the captured text, and where the cursor sits afterwards. The emptied
    //    buffer's line is no text to delete.
    let del = if st.on_emptied_line {
        SelectionDelete::nothing(0, kind == SelectKind::Blockwise)
    } else {
        delete_selection(b, kind, anchor, cur, ts, st.view.curswant == MAXCOL)
    };
    let b2 = EditList::new(del.edits)
        .expect("selection deletes are disjoint (one per line)")
        .apply_to(b);
    // 2. Where the put goes on the post-delete text.
    let (c2, forward) = settle_cursor(&b2, kind, del.cursor);
    // 3. The put, on the post-delete text.
    let put = SelectionPut {
        b2: &b2,
        c2,
        forward,
        count,
        caret: st.view.caret,
        ts,
    };
    let (mut b3, cursor) = put.place(kind, &reg, swap, del.block_lines);
    // Putting lines into a buffer the delete emptied leaves no extra empty line behind (Vim deletes it).
    if kind == SelectKind::Linewise
        && b2.is_empty()
        && b.last() != Some(&b'\n')
        && b3.ends_with(b"\n")
    {
        b3.pop();
    }
    // Lines put on the emptied buffer's line leave it below them; nvim deletes that empty last line.
    if st.on_emptied_line && b3.ends_with(b"\n\n") {
        b3.pop();
    }
    // 4. One edit from the old text to the new. The replaced text goes to the unnamed register, not the one
    //    being put (nvim deletes with no register named, #707); a delete of nothing writes no register.
    let set_register = del.deleted.filter(|_| swap).map(RegWrite::Replaced);
    replacement_plan(b, &b3, cursor, set_register)
}

/// The delete half of a Visual put: the edits that remove the selection, the text they capture, the cursor
/// right after the delete (before [`settle_cursor`]), and a block selection's first and last line.
struct SelectionDelete {
    edits: Vec<Edit>,
    /// The deleted text, or `None` when the selection had nothing to delete.
    deleted: Option<Register>,
    cursor: usize,
    block_lines: Option<(usize, usize)>,
}

impl SelectionDelete {
    /// A delete of nothing with the cursor at `at` (a one-row block's line when `block`).
    fn nothing(at: usize, block: bool) -> SelectionDelete {
        SelectionDelete {
            edits: Vec::new(),
            deleted: None,
            cursor: at,
            block_lines: block.then_some((0, 0)),
        }
    }
}

/// Plans the delete of the `kind` selection between `anchor` and `cur` (`to_eol`: a block selection runs
/// to each line's end, `$`). A charwise selection on the empty last line and a one-row block on an empty
/// line delete nothing (nvim's `op_delete` deletes and yanks nothing there).
fn delete_selection(
    b: &[u8],
    kind: SelectKind,
    anchor: usize,
    cur: usize,
    ts: Tabs,
    to_eol: bool,
) -> SelectionDelete {
    match kind {
        SelectKind::Blockwise => {
            let block = block_geom(b, anchor, cur, ts, to_eol);
            if let [row] = block.rows[..] {
                if row.ls == line_end(b, row.ls) {
                    let line = crate::pos::raw::line_of(b, row.ls);
                    return SelectionDelete {
                        block_lines: Some((line, line)),
                        ..SelectionDelete::nothing(row.ls, true)
                    };
                }
            }
            let dels: Vec<Edit> = block
                .rows
                .iter()
                .filter(|r| r.de > r.ds)
                .map(|r| Edit::replace(BytePos(r.ds), r.de - r.ds, vec![b' '; r.out_l + r.out_r]))
                .collect();
            let reg = Register::blockwise_width(block.yank_text(b), block.width());
            let top = block.rows[0];
            let first = crate::pos::raw::line_of(b, top.ls);
            SelectionDelete {
                edits: dels,
                deleted: Some(reg),
                cursor: top.ds + top.out_l,
                block_lines: Some((first, first + block.rows.len() - 1)),
            }
        }
        SelectKind::Linewise => {
            let (s, e) = selection_range(b, anchor, cur, true);
            // Lines through the end of an unterminated buffer also take the newline before them (as `Vd`).
            let s0 = if e == b.len() && b.last() != Some(&b'\n') && s > 0 {
                s - 1
            } else {
                s
            };
            SelectionDelete {
                edits: vec![Edit::delete(BytePos(s0), e - s0)],
                deleted: Some(captured(b, s, e, true)),
                cursor: s0,
                block_lines: None,
            }
        }
        SelectKind::Charwise => {
            let (s, e) = selection_range(b, anchor, cur, false);
            if s >= e {
                return SelectionDelete::nothing(s, false);
            }
            SelectionDelete {
                edits: vec![Edit::delete(BytePos(s), e - s)],
                deleted: Some(captured(b, s, e, false)),
                cursor: s,
                block_lines: None,
            }
        }
    }
}

/// Normal mode cannot rest past a line's last character: a delete that emptied the end of its line (or took
/// the last lines) leaves the cursor on the character (line) before, and the put goes after it. Returns the
/// settled cursor and whether the put goes forward.
fn settle_cursor(b2: &[u8], kind: SelectKind, mut c2: usize) -> (usize, bool) {
    let mut forward = false;
    if kind == SelectKind::Linewise {
        if c2 >= b2.len() && !b2.is_empty() {
            c2 = line_start(b2, b2.len() - usize::from(b2.ends_with(b"\n")));
            forward = true;
        } else {
            c2 = line_start(b2, c2);
        }
    } else if c2 > line_start(b2, c2) && c2 >= line_end(b2, c2) {
        c2 = prev_boundary(b2, c2);
        forward = true;
    }
    (c2, forward)
}

/// The put half of a Visual put, on the post-delete text `b2` with the settled cursor `c2`.
struct SelectionPut<'a> {
    b2: &'a [u8],
    c2: usize,
    forward: bool,
    count: u32,
    caret: CaretGravity,
    ts: Tabs,
}

impl SelectionPut<'_> {
    /// The text after the put and the cursor, shaped by the selection and register kinds.
    fn place(
        &self,
        kind: SelectKind,
        reg: &Register,
        swap: bool,
        block_lines: Option<(usize, usize)>,
    ) -> (Vec<u8>, usize) {
        let (b2, c2, forward) = (self.b2, self.c2, self.forward);
        if reg.is_empty() {
            return (b2.to_vec(), c2.min(b2.len()));
        }
        match (kind, reg.kind()) {
            (SelectKind::Linewise, RegKind::Linewise) => self.put(reg, c2, forward),
            (SelectKind::Linewise, _) => {
                self.put(&Register::linewise(reg.text().to_vec()), c2, forward)
            }
            (SelectKind::Charwise, RegKind::Linewise) => self.lines_into_line(reg),
            (SelectKind::Blockwise, RegKind::Linewise) if swap => {
                let (_, last) = block_lines.expect("a block selection records its lines");
                let at = crate::pos::raw::nth_line_start(b2, last).min(b2.len());
                self.put(reg, at, true)
            }
            (SelectKind::Blockwise, RegKind::Charwise) if !reg.text().contains(&b'\n') => {
                let (first, last) = block_lines.expect("a block selection records its lines");
                self.text_on_block_rows(reg, first, last)
            }
            _ => self.put(reg, c2, forward),
        }
    }

    /// An ordinary put of `reg` at `at`, applied to the post-delete text.
    fn put(&self, reg: &Register, at: usize, after: bool) -> (Vec<u8>, usize) {
        let p = paste(
            self.b2,
            at,
            Mode::Normal,
            reg,
            after,
            self.count.max(1),
            self.caret,
            false,
            true,
            self.ts,
        );
        match p.action {
            Action::Txn { edits, .. } => (edits.apply_to(self.b2), p.cursor),
            _ => (self.b2.to_vec(), p.cursor),
        }
    }

    /// A linewise register over a charwise selection: split the line and put the lines between the halves.
    fn lines_into_line(&self, reg: &Register) -> (Vec<u8>, usize) {
        let b2 = self.b2;
        let sp = if self.forward {
            next_boundary(b2, self.c2)
        } else {
            self.c2
        };
        let mut out = b2[..sp].to_vec();
        out.push(b'\n');
        // Bounded like every other put ([`MAX_PUT_BYTES`]), so a hostile count cannot exhaust memory.
        let reps =
            (self.count.max(1) as usize).min((MAX_PUT_BYTES / reg.text().len().max(1)).max(1));
        for _ in 0..reps {
            out.extend_from_slice(reg.text());
        }
        out.extend_from_slice(&b2[sp..]);
        let fnb = reg
            .text()
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count();
        (out, sp + 1 + fnb)
    }

    /// A one-line charwise register over a block selection: put the text on every row of the block's lines
    /// `first..=last`, skipping rows shorter than the block's left edge.
    fn text_on_block_rows(&self, reg: &Register, first: usize, last: usize) -> (Vec<u8>, usize) {
        let (b2, c2, ts) = (self.b2, self.c2, self.ts);
        let (v0, v1) = vcols_at(b2, c2, ts);
        let vcol = if self.forward { v1 + 1 } else { v0 };
        let mut edits = Vec::new();
        let mut cursor = c2;
        for line in first..=last {
            let ls = crate::pos::raw::nth_line_start(b2, line);
            if ls > b2.len() || line_cells(b2, ls, ts) < vcol {
                continue;
            }
            let le = line_end(b2, ls);
            let at = if vcol >= line_cells(b2, ls, ts) {
                le
            } else {
                at_vcol(b2, ls, vcol, ts)
            };
            if line == first {
                cursor = at + reg.text().len() - 1;
            }
            edits.push(Edit::insert(BytePos(at), reg.text().to_vec()));
        }
        let list = EditList::new(edits).expect("one insert per line");
        (list.apply_to(b2), cursor)
    }
}

/// The plan that turns `b` into `b3` as one edit: the changed middle between the common ends, kept on char
/// boundaries. An unchanged text is a no-op that still writes `set_register`.
fn replacement_plan(b: &[u8], b3: &[u8], cursor: usize, set_register: Option<RegWrite>) -> Plan {
    let max = b.len().min(b3.len());
    let mut p = b
        .iter()
        .zip(b3)
        .take_while(|(x, y)| x == y)
        .count()
        .min(max);
    while p > 0 && p < b.len() && (b[p] & 0xC0) == 0x80 {
        p -= 1;
    }
    let mut q = b
        .iter()
        .rev()
        .zip(b3.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
        .min(max - p);
    while q > 0 && (b[b.len() - q] & 0xC0) == 0x80 {
        q -= 1;
    }
    if b3 == b {
        return Plan {
            set_register,
            ..nop(cursor.min(b.len()), Mode::Normal)
        };
    }
    Plan {
        action: Action::Txn {
            edits: one(Edit::replace(
                BytePos(p),
                b.len() - p - q,
                b3[p..b3.len() - q].to_vec(),
            )),
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

/// Blockwise paste (`p`/`P` of a `CTRL-V` register, nvim `do_put`): drop the register's rows as a
/// rectangle. Row `i` lands on the line `i` rows below the cursor's, at the virtual column of the cursor's
/// first cell for `P` and one past its last cell for `p`. A line shorter than that column is padded with
/// spaces; a `<Tab>` crossing it is split into spaces around the block (another wide character crossing it
/// moves right, as in Vim); rows past the last line open new lines. The cursor ends at the block's top-left.
/// `{count}p` repeats each row `count` times horizontally. With `pad`, a row narrower than the block is
/// filled with spaces to the block's width between copies and before any text that follows it, so that text
/// stays in one column; `zp`/`zP` paste without (`pad` false).
fn paste_block(
    b: &[u8],
    cur: usize,
    reg: &Register,
    after: bool,
    count: usize,
    pad: bool,
    ts: Tabs,
) -> Plan {
    let units: Vec<&[u8]> = reg.text().split(|&c| c == b'\n').collect();
    let block_width = reg.block_width();
    let cls = line_start(b, cur);
    let (v0, v1) = vcols_at(b, cur, ts);
    let col = if after && cur < line_end(b, cls) {
        v1 + 1
    } else {
        v0
    };
    // The cursor's line is real even when it is the empty slot after a final newline (`Go<Esc>` in a
    // buffer without one, #1080): the first row lands in it. Lines opened past the end then follow it with
    // a separator before each; after a terminated last line each new line carries its own newline (#695).
    let in_end_slot = cls == b.len();
    let terminated = !in_end_slot && b.last() == Some(&b'\n');
    let mut edits: Vec<Edit> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut cursor = cur;
    let mut line = Some(cls);
    for (i, unit) in units.iter().enumerate() {
        let fill = if pad {
            block_width.saturating_sub(text_cells(unit, ts))
        } else {
            0
        };
        // Row `i` repeated `count` times; the fill goes between copies, and after the last one only when
        // text follows it on its line.
        let build = |shortline: bool| {
            let mut out = Vec::new();
            for j in 0..count {
                out.extend_from_slice(unit);
                if j + 1 < count || !shortline {
                    out.extend(std::iter::repeat_n(b' ', fill));
                }
            }
            out
        };
        let Some(ls) = line else {
            let mut row = vec![b' '; col];
            row.extend(build(true));
            if terminated {
                tail.extend(row);
                tail.push(b'\n');
            } else {
                tail.push(b'\n');
                tail.extend(row);
            }
            continue;
        };
        let le = line_end(b, ls);
        let (mut p, mut prev, mut v, mut w) = (ls, ls, 0, 0);
        while v < col && p < le {
            prev = p;
            let (j, cw) = cluster_cells(b, p, le, v, ts);
            p = j;
            w = cw;
            v += w;
        }
        let (mut at, mut del, mut pre, mut post) = (p, 0, 0, 0);
        if v < col {
            pre = col - v; // line too short: pad with spaces
        } else if v > col {
            // The last character crosses the column: split a `<Tab>` into spaces on both sides; anything
            // else moves to after the block.
            post = v - col;
            pre = w - post;
            at = prev;
            if b[prev] == b'\t' {
                del = p - prev;
            } else {
                post = 0;
            }
        }
        let shortline = v < col || (v == col && p >= le);
        let mut ins = vec![b' '; pre];
        ins.extend(build(shortline));
        ins.extend(std::iter::repeat_n(b' ', post));
        if i == 0 {
            cursor = at + pre;
        }
        if del > 0 || !ins.is_empty() {
            edits.push(Edit::replace(BytePos(at), del, ins));
        }
        line = (le + 1 < b.len()).then_some(le + 1);
    }
    if !tail.is_empty() {
        // Merge into a bottom-most edit already sitting at the buffer end (avoids two inserts at one
        // position, which the disjoint-edit invariant forbids); otherwise append as its own edit.
        match edits.last_mut() {
            Some(last) if last.end().0 == b.len() => last.ins.extend_from_slice(&tail),
            _ => edits.push(Edit::insert(BytePos(b.len()), tail)),
        }
    }
    if edits.is_empty() {
        return nop(cursor.min(b.len()), Mode::Normal);
    }
    let list = EditList::new(edits).expect("block paste edits are disjoint (one per line)");
    Plan {
        action: Action::Txn {
            edits: list,
            hint: GroupHint::BreakBefore,
        },
        is_edit: true,
        ..nop(cursor, Mode::Normal)
    }
}

#[allow(clippy::too_many_arguments)] // paste legitimately needs the full register + gravity + gp context
pub(super) fn paste(
    b: &[u8],
    cur: usize,
    mode: Mode,
    reg: &Register,
    after: bool,
    count: u32,
    gravity: CaretGravity,
    move_after: bool,
    pad_block: bool,
    ts: Tabs,
) -> Plan {
    let nop = nop(cur, mode);
    if reg.is_empty() {
        return nop;
    }
    // Bound the repetition so a pathological `{count}p` (e.g. a digit-spam count that saturates to
    // `u32::MAX`) cannot request a multi-gigabyte allocation ([`MAX_PUT_BYTES`]).
    let unit_len = reg.text().len().max(1);
    let count = (count.max(1) as usize).min((MAX_PUT_BYTES / unit_len).max(1));
    if reg.is_blockwise() {
        return paste_block(b, cur, reg, after, count, pad_block, ts);
    }
    // `{count}p` pastes the register `count` times (Vim); the register itself is unchanged. The repeated
    // bytes are one contiguous insert, so the cursor math below (last pasted byte) still holds.
    let repeat = |unit: &[u8]| unit.repeat(count);
    let one = |e: Edit| EditList::new(vec![e]).expect("single edit is always valid");
    // A paste run from Insert (`i_CTRL-O p`) returns to Insert; every other paste ends in Normal.
    let end_mode = if mode == Mode::Insert {
        Mode::Insert
    } else {
        Mode::Normal
    };
    let mk = |at: usize, bytes: Vec<u8>, cursor: usize| Plan {
        action: Action::Txn {
            edits: one(Edit::insert(BytePos(at), bytes)),
            hint: GroupHint::BreakBefore,
        },
        is_edit: true,
        ..self::nop(cursor, end_mode)
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
        // what it inserted. `end` is the after-last boundary; back it up one grapheme only under OnChar: the
        // start of the text's last cluster, so `ylP` on a char with combining marks rests on its base (#1132).
        let last = if multiline || n == 0 {
            1
        } else {
            n - motion::prev_grapheme(&text, n)
        };
        let tail = |end: usize| match gravity {
            CaretGravity::BetweenChar => end,
            CaretGravity::OnChar => end.saturating_sub(last),
        };
        if after {
            // Insert after the cursor char; cursor lands on the first pasted byte for multi-line content,
            // else per gravity on/after the last pasted byte. `gp` (move_after) rests just past the paste.
            // On an empty line, or past the last char (`A<C-o>p`), there is no char to paste after: the
            // text goes in at the cursor, never across the line break (#701).
            let at = if cur < b.len() && b[cur] != b'\n' {
                next_grapheme(b, cur)
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

/// Re-indent a linewise register's bytes for `]p`/`[p`: the first line takes `target_cols` of indent and
/// every other line shifts by the same column delta (Vim). Indent is rebuilt in the editor's style (spaces,
/// or tabs + a spaces remainder); a blank line stays blank. Input/output both end each line with `\n`.
fn reindent_register(
    text: &[u8],
    target_cols: usize,
    tab_width: Tabs,
    style: IndentStyle,
) -> Vec<u8> {
    let tw = tab_width;
    let first_end = text.iter().position(|&c| c == b'\n').unwrap_or(text.len());
    let (first_cols, _) = indent_cols(&text[..first_end], tw);
    let delta = target_cols as isize - first_cols as isize;
    let build = |cols: usize| -> Vec<u8> {
        match style {
            IndentStyle::Space => vec![b' '; cols],
            IndentStyle::Tab => tw.fill(cols),
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
