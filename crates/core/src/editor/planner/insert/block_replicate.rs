//! `<Esc>` closing a blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`): the top row's typed
//! text replicated down the block, or a block change broken by a `<CR>`.

use super::{
    block_insert_point, edit, line_cells, line_end, line_start, motion, nop, range, BlockInsert,
    BytePos, Edit, EditList, GroupHint, Mode, Plan, Tabs,
};

/// Finish a blockwise insert-replicate session (`<Esc>` after `CTRL-V` `I`/`A`/`c`): take the text typed
/// on the top row since `session.insert_start` and insert it at `session.target_col` on each of the
/// `rows_below` rows beneath the top line. `append` rows pad short lines with spaces; non-append (`I`/`c`)
/// rows shorter than the target column are skipped (Vim). A newline typed on the top row aborts the
/// replicate (Vim). For `I`/`A` the cursor returns to the block's top-left corner (`session.top_left`);
/// for `c` (change) it rests one char left of the top row's typed text — the normal Insert-exit position,
/// which Vim keeps for a block change (matters only when more than one char was typed).
pub(super) fn block_replicate(
    b: &[u8],
    cur: usize,
    session: BlockInsert,
    hint: GroupHint,
    ts: Tabs,
) -> Plan {
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
