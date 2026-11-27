//! Blockwise (`CTRL-V`) operators and the insert point of a block insert.
//!
//! Split out of `planner/mod.rs` (#1392); the items are as they were there.

use super::{edit, edit_yank, nop, recase, recase_span};
use crate::editor::{
    cluster_cells, Action, Block, BlockInsert, BlockRow, MarkAt, MarkCol, OpMarks, RegWrite, MAXCOL,
};
use crate::motion::{line_end, prev_grapheme};
use crate::undo_cursor::UndoEntry;
use crate::{BytePos, Edit, EditList, GroupHint, Mode, OpKind, Plan, Register, Tabs, WordCase};

/// Blockwise recase (Vim `op_tilde` over `block_prep`): each row's characters wholly inside the block take
/// `case`, the cursor goes to the block's top-left, back to Normal. Shared by a Visual-block `u`/`U`/`~`/
/// `g?` and the forced-blockwise case operator (`gU<C-v>j`, #1595).
pub(super) fn block_recase(b: &[u8], block: &Block, case: WordCase, hint: GroupHint) -> Plan {
    let edits: Vec<Edit> = block
        .rows
        .iter()
        .filter_map(|r| recase_span(b, r.s, r.e, case, recase).map(|new| (r, new)))
        .filter(|(r, new)| new[..] != b[r.s..r.e])
        .map(|(r, new)| Edit::replace(BytePos(r.s), r.e - r.s, new))
        .collect();
    let at = block.rows[0].ds;
    match EditList::new(edits) {
        // nvim's `op_tilde` (#1302): `'[` `']` on the block's corners, `'.` at column 0 of the top line
        // (`changed_lines(start, 0, ..)`), which it calls only when something changed.
        Ok(list) if !list.is_empty() => {
            let (start, end) = block_corners(b, block);
            Plan {
                op_marks: Some(OpMarks {
                    start,
                    end,
                    dot: MarkAt::new(start.line, MarkCol::Byte(0)),
                }),
                undo_entry: UndoEntry::One,
                ..edit(list, at, Mode::Normal, hint)
            }
        }
        _ => nop(at, Mode::Normal),
    }
}

/// A block's corners as nvim's `do_pending_operator` sets `oap->start` / `oap->end` for a blockwise
/// operator (#1302), each a line and a byte column of the text before the operator: the top line at the
/// char covering the left edge, the bottom line at the char covering the right edge. A line that ends
/// before the edge gets its length (Visual's `coladvance` stops one past the last char), and so does the
/// bottom line of a `$` block. nvim's end is the last BYTE of a multibyte char (`mb_adjust_opend`); ruse's
/// marks stay on a char's first byte.
pub(super) fn block_corners(b: &[u8], block: &Block) -> (MarkAt, MarkAt) {
    let line = |r: &BlockRow| crate::pos::raw::line_of(b, r.ls);
    let (top, bottom) = (&block.rows[0], &block.rows[block.rows.len() - 1]);
    let end_col = if block.hi != MAXCOL && bottom.cells > block.hi {
        prev_grapheme(b, bottom.de) - bottom.ls
    } else {
        bottom.le - bottom.ls
    };
    (
        MarkAt::new(line(top), MarkCol::Byte(top.ds - top.ls)),
        MarkAt::new(line(bottom), MarkCol::Byte(end_col)),
    )
}

/// The change marks of a block delete, as nvim's `op_delete` (#1302): `'[` the block's top-left, `']` the
/// bottom line at the same byte column (`b_op_end = (end.lnum, start.col)`), and `'.` where the cursor
/// rests on the top line once `check_cursor_col` has moved it off the line's end.
/// A block change (`c`) sets them too as its Insert begins; `'.` stays there when nothing is typed.
pub(super) fn block_delete_marks(b: &[u8], block: &Block) -> OpMarks {
    let (start, end) = block_corners(b, block);
    let top = &block.rows[0];
    let col = top.ds - top.ls;
    // The top line after the delete: `b[ls..ds]`, the straddlers' outside cells as spaces, `b[de..le]`.
    let spaces = top.out_l + top.out_r;
    let new_len = (top.le - top.ls) - (top.de - top.ds) + spaces;
    let cursor = col + top.out_l;
    let dot = if cursor < new_len {
        cursor
    } else if top.de < top.le {
        prev_grapheme(b, top.le) - top.de + col + spaces
    } else if spaces > 0 {
        col + spaces - 1
    } else if top.ds > top.ls {
        prev_grapheme(b, top.ds) - top.ls
    } else {
        0
    };
    OpMarks {
        start,
        end: MarkAt::new(end.line, MarkCol::Byte(col)),
        dot: MarkAt::new(start.line, MarkCol::Byte(dot)),
    }
}

/// Blockwise (`CTRL-V`) yank/delete/change over the rectangle whose corners are the byte offsets `c1` and
/// `c2` (a Visual-block selection's anchor+cursor, or an operator's cursor+motion-target — e.g.
/// `c<C-v>{motion}`). Yank and Delete capture the per-row slices into a blockwise [`Register`]; the cursor
/// lands at the block's top-left corner (Vim). Change ALSO replicates: it deletes the block on every row
/// (capturing it blockwise), then ARMS a [`BlockInsert`] replicate session at the top-left exactly like
/// [`plan_block_insert`], so the text typed before `<Esc>` is replicated down every block row by
/// [`block_replicate`] (short rows follow the same skip rule). Visual-block `c`/`I`/`A` reach that
/// machinery through [`plan_block_insert`] directly; this arm handles the operator-forced `c<C-v>` path,
/// which is verified against nvim to match Visual-block change byte-for-byte. Dot-repeat of the
/// operator-forced block change is NOT yet wired (the same open gap as Visual-block-insert dot-repeat) and
/// is intentionally not oracle-tested; the replicate + single-undo-unit ARE.
pub(super) fn block_op(b: &[u8], block: &Block, op: OpKind, hint: GroupHint) -> Plan {
    let top = block.rows[0];
    // The cursor lands on the block's top-left corner, the character covering the left edge — after a
    // delete, past the spaces a split `<Tab>` leaves there.
    let top_left = if op == OpKind::Yank {
        top.ds
    } else {
        top.ds + top.out_l
    };
    let reg = Register::blockwise_width(block.yank_text(b), block.width());
    // The `[`/`]` change-mark span for a blockwise yank: top-left corner → bottom-right corner (the end of
    // the last row's slice). An approximation of Vim's rectangular bracketing, adequate for the mark jump.
    let block_span = (top.ds, block.rows.last().map_or(top.ds, |r| r.de));
    let nop = |mode: Mode| nop(top_left, mode);
    // One edit per row (rows on different lines are inherently disjoint): remove every character the block
    // touches, leaving the outside cells of a split `<Tab>` or wide char as spaces (Vim `op_delete`).
    let deletes = || -> Vec<Edit> {
        block
            .rows
            .iter()
            .filter(|r| r.de > r.ds)
            .map(|r| Edit::replace(BytePos(r.ds), r.de - r.ds, vec![b' '; r.out_l + r.out_r]))
            .collect()
    };
    match op {
        // Yank leaves the buffer unchanged, cursor at the block's top-left, back to Normal.
        // nvim's `op_yank` puts `'[` `']` on the block's corners (#1302). A yank changes nothing, so commit
        // records no `'.` for it.
        OpKind::Yank => {
            let (start, end) = block_corners(b, block);
            Plan {
                set_register: Some(RegWrite::Yank(reg, block_span)),
                op_marks: Some(OpMarks {
                    start,
                    end,
                    dot: start,
                }),
                ..nop(Mode::Normal)
            }
        }
        OpKind::Delete => {
            // nvim marks the block even when it removes nothing: `op_delete` saves for undo and calls
            // `changed_lines` whatever it deleted.
            let op_marks = Some(block_delete_marks(b, block));
            let edits = deletes();
            if edits.is_empty() {
                // The block sits entirely past every line's end — nothing to remove.
                return Plan {
                    op_marks,
                    ..nop(Mode::Normal)
                };
            }
            let list = EditList::new(edits).expect("block-row deletes are disjoint (one per line)");
            Plan {
                op_marks,
                undo_entry: UndoEntry::One,
                ..edit_yank(list, top_left, Mode::Normal, hint, reg)
            }
        }
        // Operator-forced blockwise change (`c<C-v>{motion}`): delete the block on every row (captured
        // blockwise, above), then ARM a replicate session at the top-left — the SAME machinery
        // `plan_block_insert` uses for Visual-block `c`, so the text typed before `<Esc>` is replicated
        // down every block row by `block_replicate`. Even when nothing is deleted (the block sits past
        // every line's end) we still arm the session so a typed run replicates; `is_edit` tracks whether
        // the delete transaction is non-empty. `append`/`to_eol` are false — `c` inserts at the left edge.
        OpKind::Change => {
            let session = BlockInsert {
                insert_start: top_left,
                top_left,
                top_line_start: top.ls,
                top_len: (top.le - top.ls) - (top.de - top.ds) + top.out_l + top.out_r,
                target_col: block.lo,
                rows_below: block.rows.len().saturating_sub(1),
                append: false,
                to_eol: false,
                change: true,
                width: block.width().max(1),
            };
            let list =
                EditList::new(deletes()).expect("block-row deletes are disjoint (one per line)");
            let is_edit = !list.is_empty();
            Plan {
                action: Action::BlockInsertArm {
                    edits: list,
                    hint,
                    session,
                },
                is_edit,
                set_register: Some(RegWrite::Edit(reg)),
                op_marks: Some(block_delete_marks(b, block)),
                undo_entry: UndoEntry::One,
                ..self::nop(top_left, Mode::Insert)
            }
        }
    }
}

/// Where text inserted at virtual column `t` goes on the line starting at `ls` (Vim `block_insert`):
/// `(at, del, pre, post)` — replace `del` bytes at `at` with `pre` spaces, the text, then `post` spaces. A
/// `<Tab>` crossing `t` is split into the spaces on either side; another wide character crossing it stays
/// after the text, with `pre` spaces before. A line ending before `t` is padded out to it when `pad` (`A`).
pub(super) fn block_insert_point(
    b: &[u8],
    ls: usize,
    t: usize,
    ts: Tabs,
    pad: bool,
) -> (usize, usize, usize, usize) {
    let le = line_end(b, ls);
    let (mut i, mut v) = (ls, 0);
    while i < le {
        if v >= t {
            return (i, 0, 0, 0);
        }
        let (j, w) = cluster_cells(b, i, le, v, ts);
        if v + w > t {
            let pre = t - v;
            return if b[i] == b'\t' {
                (i, 1, pre, w - pre)
            } else {
                (i, 0, pre, 0)
            };
        }
        i = j;
        v += w;
    }
    (le, 0, if pad { t.saturating_sub(v) } else { 0 }, 0)
}
