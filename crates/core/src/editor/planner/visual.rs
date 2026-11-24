//! Visual and Select mode: entering, reselecting, and the selection operators.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{
    block_insert_point, block_op, block_recase, captured, edit, edit_yank, nop, one, plan_comment,
    plan_format_lines, plan_join, plan_linewise_change, plan_paste_selection, plan_reindent,
    plan_shift_times, recase_bytes, replace_selection_with,
};
use crate::editor::{
    at_vcol, block_at, block_geom, cluster_cells, range, selection_range, vcols_at, want_col,
    Action, Block, BlockInsert, RegWrite, MAXCOL,
};
use crate::motion::{self, at_col, col_of, line_end, line_start, prev_grapheme};
use crate::{
    BlockInsertKind, BytePos, CaretGravity, Command, Edit, EditList, EditorState, GroupHint,
    IndentStyle, Mode, OpKind, Plan, Register, SelectKind, VisualOp,
};

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        Command::EnterVisual { kind } => nop(cur, Mode::Visual { kind: *kind }),
        // CTRL-G toggle: enter Select over the same selection. The anchor is preserved because both are
        // selection modes, so `commit` keeps it (see the (true, true) arm there). Outside a selection it is
        // inert: `gh` enters Visual first, and the `v_CTRL-O` return lands after a command that may have
        // ended the selection (nvim returns to Select only while Visual is still active).
        Command::EnterSelect { kind } if st.view.mode.selection().is_some() => {
            nop(cur, Mode::Select { kind: *kind })
        }
        Command::EnterSelect { .. } => nop(cur, st.view.mode),
        // A Visual area `:mark <`/`:mark >` made with only one end (#1241) is not reselected either.
        Command::ReselectVisual if st.view.visual_one_end.is_some() => nop(cur, st.view.mode),
        Command::ReselectVisual => match st.view.last_visual {
            // Restore the remembered selection: re-enter Visual with the stored kind, put the cursor on the
            // active end, and install the stored anchor (via `set_anchor`, which `commit` applies after its
            // enter-selection bookkeeping). No prior selection → a clean no-op (Vim rings the bell).
            Some((anchor, active, kind)) => Plan {
                set_anchor: Some(anchor.at),
                ..nop(active.at, Mode::Visual { kind })
            },
            None => nop(cur, st.view.mode),
        },
        Command::ReplaceSelection(c) => {
            // Select's `open/replace-selection`: delete the selection, insert the char, enter Insert.
            let mut buf = [0u8; 4];
            // A block is a block `c` whose first typed char is this one, so `<Esc>` replicates the whole run
            // down the block (nvim: `g CTRL-H` + `<S-Down>` + `X` leaves `X` on both rows).
            if st.view.mode.selection() == Some(SelectKind::Blockwise) && st.view.anchor.is_some() {
                let seed = c.encode_utf8(&mut buf).as_bytes();
                return plan_block_insert(st, b, cur, hint, &BlockInsertKind::Change, seed);
            }
            let ins = c.encode_utf8(&mut buf).as_bytes().to_vec();
            replace_selection_with(st, b, cur, hint, ins)
        }
        Command::BlockInsert(kind) => plan_block_insert(st, b, cur, hint, kind, &[]),
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
                let ts = st.view.indent.tabs();
                let block = block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL);
                return block_op(b, &block, op, hint);
            }
            let line = st.view.mode.selection() == Some(SelectKind::Linewise);
            let op = match cmd {
                Command::YankSelection => OpKind::Yank,
                Command::ChangeSelection => OpKind::Change,
                _ => OpKind::Delete,
            };
            plan_selection_span(st, b, anchor, cur, op, line, hint)
        }
        Command::VisualOp(op) => plan_visual_op(st, b, cur, *op, hint),
        // Visual `u`/`U`/`~`/`g?` — recase the selection in place, cursor at its start, back to Normal. A
        // block recases each row's characters wholly inside it (Vim `op_tilde` over `block_prep`).
        Command::CaseSelection(case) => {
            if let Some(block) = selection_block(st, b, cur) {
                return block_recase(b, &block, *case, hint);
            }
            let (s, e, _line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            // A byte that is not UTF-8 is kept as it is (#706, #1243).
            let new = recase_bytes(&b[s..e], *case);
            edit(
                one(Edit::replace(BytePos(s), e - s, new)),
                s,
                Mode::Normal,
                hint,
            )
        }
        // Visual `r{char}` — overwrite every non-newline char in the selection with `c`, keeping line
        // breaks and the byte length's char count; cursor to the span start, back to Normal. A block fills
        // each row's cells with `c` (Vim `op_replace`), so a `<Tab>` or wide char inside becomes several.
        Command::ReplaceSelectionChar(c) => {
            if let Some(block) = selection_block(st, b, cur) {
                return block_replace(&block, *c, hint);
            }
            let (s, e, _line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            // A byte that is not UTF-8 is one char (#1243).
            let new: String = crate::pattern::spell(&b[s..e])
                .chars()
                .map(|ch| if ch == '\n' { '\n' } else { *c })
                .collect();
            edit(
                one(Edit::replace(BytePos(s), e - s, new.into_bytes())),
                s,
                Mode::Normal,
                hint,
            )
        }
        // Visual `p`/`P` — replace the selection with the register; `swap` puts the deleted text into the
        // unnamed register (Vim `p`), `P` preserves the register (so it can overwrite successive selections).
        Command::PasteSelection { swap, count } => plan_paste_selection(st, b, cur, *swap, *count),
        _ => unreachable!("planner::visual plans only its own commands"),
    }
}

/// Enter a blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`): compute the top-row insert point
/// and, for `c`, the block-delete edits + captured blockwise register. The typed text is replicated down
/// the block by [`block_replicate`] when the session's `<Esc>` closes it. `seed` is text already typed into
/// the session: blockwise Select's replace-selection policy (`g CTRL-H` then a printable key) is a block `c`
/// whose first char is the key, so it replicates with the rest of the run.
fn plan_block_insert(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    kind: &BlockInsertKind,
    seed: &[u8],
) -> Plan {
    let append = matches!(kind, BlockInsertKind::Append);
    let change = matches!(kind, BlockInsertKind::Change);
    let ts = st.view.indent.tabs();
    let block = if let Some(anchor) = st.view.anchor {
        // Live block insert from a Visual-block selection: the two corners are `anchor` and `cur`. `$`
        // (curswant at MAXCOL) makes the block ragged: `$A` appends at EACH row's own line end.
        block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL)
    } else if let Some(geom) = st.view.last_block_geom {
        // Dot-repeat (`.`): no live selection. Rebuild an equivalent block of the stored WIDTH x HEIGHT with
        // its top-left at the caret — nvim positions the repeat at the cursor and reuses the original geometry
        // (verified vs nvim v0.12.4). The recorded command carries the kind (I/A/c); `geom` carries the size
        // and the `$`-ragged flag (only meaningful with `A`). Rows clamp at end-of-buffer like the live path.
        block_at(b, cur, geom.width, geom.height, ts, geom.to_eol)
    } else {
        // Neither a live selection nor a prior block insert to repeat — degrade to a plain Insert entry.
        return nop(cur, Mode::Insert);
    };
    let to_eol = append && block.hi == MAXCOL;
    let width = block.width().max(1);
    let lo = block.lo;
    let target_col = if append { lo + width } else { lo };
    let top = block.rows[0];
    let top_ls = top.ls;
    let rows_below = block.rows.len().saturating_sub(1);

    let mut edits: Vec<Edit> = Vec::new();
    let mut reg: Option<Register> = None;
    // `c`: delete the block on every row first (capture it blockwise), then insert at the left edge.
    let (insert_start, top_left) = if change {
        reg = Some(Register::blockwise_width(block.yank_text(b), width));
        for (i, r) in block.rows.iter().enumerate() {
            let mut keep = vec![b' '; r.out_l + r.out_r];
            if i == 0 {
                keep.splice(r.out_l..r.out_l, seed.iter().copied());
            }
            if r.de > r.ds || !keep.is_empty() {
                edits.push(Edit::replace(BytePos(r.ds), r.de - r.ds, keep));
            }
        }
        (top.ds + top.out_l, top.ds + top.out_l)
    } else if to_eol {
        (top.le, top.ds) // `$A`: append at the top row's own end (ragged), no padding
    } else {
        // `I` at the left edge, `A` past the right edge; a short `A` row is padded out to the column. On
        // this row the caret cannot sit inside a `<Tab>` or wide char crossing the column, so the insert goes
        // before it (`I`) or after it (`A`), as in nvim; the other rows split it (`block_replicate`).
        let (at, _, pre, _) = block_insert_point(b, top_ls, target_col, ts, append);
        let at = if at < top.le && pre > 0 {
            if append {
                cluster_cells(b, at, top.le, 0, ts).0
            } else {
                at
            }
        } else {
            if pre > 0 {
                edits.push(Edit::insert(BytePos(at), vec![b' '; pre]));
            }
            at + pre
        };
        (at, if append { top.ds } else { at })
    };

    // The top row's length once a change has deleted its part of the block, before anything is typed.
    let top_len = if change {
        (top.le - top.ls) - (top.de - top.ds) + top.out_l + top.out_r
    } else {
        top.le - top.ls
    };
    let session = BlockInsert {
        insert_start,
        top_left,
        top_line_start: top_ls,
        top_len,
        target_col,
        rows_below,
        append,
        to_eol,
        change,
        width,
    };
    let list = EditList::new(edits).expect("block-insert enter edits are disjoint (one per line)");
    let is_edit = !list.is_empty();
    Plan {
        action: Action::BlockInsertArm {
            edits: list,
            hint,
            session,
        },
        is_edit,
        set_register: reg.map(RegWrite::Edit),
        ..nop(insert_start + seed.len(), Mode::Insert)
    }
}

/// Visual `d`/`c`/`y` over a charwise (`line == false`) or linewise selection `anchor`..`cur`; also the
/// linewise uppercase forms `D`/`X`/`Y`/`C`/`S`/`R` ([`VisualOp::Lines`]), which force `line`.
fn plan_selection_span(
    st: &EditorState,
    b: &[u8],
    anchor: usize,
    cur: usize,
    op: OpKind,
    line: bool,
    hint: GroupHint,
) -> Plan {
    let (s, e) = selection_range(b, anchor, cur, line);
    // Visual-LINEWISE change (`V…c`) behaves exactly like `cc` over the selected line range:
    // keep the first line's indent, replace the rest with ONE empty line, and — crucially — KEEP
    // the trailing separator to the following line (never merge the next line in). Route it through
    // the shared cc-logic; `selection_range` gives whole lines incl. the trailing '\n', so drop
    // that one byte to get the CONTENT end the helper expects. Charwise/blockwise change stays the
    // delete-then-Insert path below.
    if line && op == OpKind::Change && s < e {
        let content_end = if e > s && b[e - 1] == b'\n' { e - 1 } else { e };
        return plan_linewise_change(b, s, content_end, st.view.indent.autoindent, hint);
    }
    // A charwise Visual `d`/`c` cannot take the final newline: Vim has no line after the last one to join,
    // so `v}d` at the end of `a\nb\n` leaves one empty line rather than an empty buffer (#677).
    let e = if !line && op != OpKind::Yank && st.view.caret == CaretGravity::OnChar {
        e.min(motion::content_end(b)).max(s)
    } else {
        e
    };
    let reg = captured(b, s, e, line);
    match op {
        // Yank leaves the buffer unchanged, cursor at the selection start (Vim), back to Normal: on a char,
        // so a start on a line's break (`$vk` reaches past the end, #1176) is its last char.
        OpKind::Yank => Plan {
            set_register: Some(RegWrite::Yank(reg, (s, e))),
            ..nop(
                {
                    let (ls, le) = (line_start(b, s), line_end(b, s));
                    if s >= le && le > ls && st.view.caret == CaretGravity::OnChar {
                        prev_grapheme(b, le)
                    } else {
                        s
                    }
                },
                Mode::Normal,
            )
        },
        OpKind::Delete if s < e => {
            // A linewise delete keeps the wanted column on the line that moves up under the cursor (nvim's
            // `nostartofline`: `lVjd` lands on column 1); `at_col` clamps it to that line's end.
            // Lines through the end of the buffer also take the newline before them (no blank line is left
            // behind) and the cursor moves up onto the new last line; in a newline-terminated buffer only
            // the empty slot after the final newline is left below, which is no line (#677, #1167).
            let (s, cursor) = if !line {
                (s, s)
            } else if e < b.len() {
                (
                    s,
                    want_col(b, e, st.view.curswant.get(), st.view.indent.tabs()) - (e - s),
                )
            } else if s > 0 {
                (
                    if b[e - 1] == b'\n' { s } else { s - 1 },
                    want_col(
                        b,
                        line_start(b, s - 1),
                        st.view.curswant.get(),
                        st.view.indent.tabs(),
                    ),
                )
            } else {
                (s, s)
            };
            edit_yank(
                one(Edit::delete(BytePos(s), e - s)),
                cursor,
                Mode::Normal,
                hint,
                reg,
            )
        }
        OpKind::Change if s < e => edit_yank(
            one(Edit::delete(BytePos(s), e - s)),
            s,
            Mode::Insert,
            hint,
            reg,
        ),
        // Empty selection: just leave Visual (Change still opens Insert).
        OpKind::Change => nop(s, Mode::Insert),
        OpKind::Delete => nop(s, Mode::Normal),
    }
}

/// [`Command::VisualOp`]: a line-oriented Visual operator over the live selection, leaving it (to Normal,
/// or Insert for the changes). Outside a selection it is a clean no-op back to Normal.
fn plan_visual_op(st: &EditorState, b: &[u8], cur: usize, op: VisualOp, hint: GroupHint) -> Plan {
    let (Some(kind), Some(anchor)) = (st.view.mode.selection(), st.view.anchor) else {
        return nop(cur, Mode::Normal);
    };
    let (lo, hi) = (anchor.min(cur), anchor.max(cur));
    let (first, last) = (
        crate::pos::raw::line_of(b, lo),
        crate::pos::raw::line_of(b, hi),
    );
    let lines = (last - first + 1) as u32;
    // The shared line planners keep the mode they were called in; a Visual operator ends the selection.
    let ts = st.view.indent.tabs();
    let normal = |p: Plan| Plan {
        mode: Mode::Normal,
        ..p
    };
    match op {
        VisualOp::Lines(k) => plan_selection_span(st, b, anchor, cur, k, true, hint),
        VisualOp::BlockToEol(k) if kind == SelectKind::Blockwise => {
            block_op(b, &block_geom(b, anchor, cur, ts, true), k, hint)
        }
        VisualOp::BlockToEol(k) => plan_selection_span(st, b, anchor, cur, k, true, hint),
        VisualOp::Shift { left, count } if kind == SelectKind::Blockwise => {
            normal(plan_block_shift(
                st,
                b,
                &block_geom(b, anchor, cur, ts, false),
                left,
                count,
                hint,
            ))
        }
        VisualOp::Shift { left, count } => normal(plan_shift_times(
            st,
            line_start(b, lo),
            lines,
            !left,
            count.max(1),
            hint,
        )),
        VisualOp::Join { spaces } => plan_join(st, lo, lines.max(2), Mode::Normal, hint, !spaces),
        VisualOp::Reindent => normal(plan_reindent(st, first, last, hint)),
        VisualOp::Comment => normal(plan_comment(st, first, last, lo, hint)),
        VisualOp::Format => normal(plan_format_lines(
            st,
            cur,
            line_start(b, lo),
            line_end(b, hi),
            false,
            false,
            hint,
        )),
        // `zy`: a block yank drops each row's trailing white space; any other shape yanks as `y` does.
        VisualOp::YankTrim if kind == SelectKind::Blockwise => {
            let mut p = block_op(
                b,
                &block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL),
                OpKind::Yank,
                hint,
            );
            if let Some(RegWrite::Yank(reg, span)) = p.set_register.take() {
                let rows: Vec<&[u8]> = reg
                    .text()
                    .split(|&c| c == b'\n')
                    .map(|row| row.trim_ascii_end())
                    .collect();
                let reg = Register::blockwise_width(rows.join(&b'\n'), reg.block_width());
                p.set_register = Some(RegWrite::Yank(reg, span));
            }
            p
        }
        VisualOp::YankTrim => plan_selection_span(
            st,
            b,
            anchor,
            cur,
            OpKind::Yank,
            kind == SelectKind::Linewise,
            hint,
        ),
        // Block `O`: the cursor takes the anchor's column on its own row, the anchor the cursor's column on
        // its row — the other corner of the same rectangle, horizontally. Charwise/linewise `O` is `o`.
        // A corner past a row's last char (a `$` block, #1207) rests past it on the other row too, as
        // nvim's Visual `coladvance` may go one past the end.
        VisualOp::OtherCorner if kind == SelectKind::Blockwise => {
            let (a_col, c_col) = (vcols_at(b, anchor, ts).0, vcols_at(b, cur, ts).0);
            let corner = |p: usize, col: usize| {
                let ls = line_start(b, p);
                if col >= range::line_cells(b, ls, ts) {
                    line_end(b, ls)
                } else {
                    at_vcol(b, ls, col, ts)
                }
            };
            Plan {
                set_anchor: Some(corner(anchor, c_col)),
                ..nop(corner(cur, a_col), st.view.mode)
            }
        }
        VisualOp::OtherCorner => Plan {
            set_anchor: Some(cur),
            ..nop(anchor, st.view.mode)
        },
        // `:`/`!`: Vim's colon operator leaves the cursor at the start of the area — a block's top-left
        // corner; for lines, the first line's column 0, unless the cursor is the selection's start already,
        // where it stays (nvim v0.12.6). `'<`/`'>` keep the selection as it was.
        VisualOp::LeaveAtStart => {
            let at = match kind {
                SelectKind::Charwise => lo,
                SelectKind::Blockwise => {
                    let col = |p: usize| col_of(b, line_start(b, p), p);
                    at_col(b, line_start(b, lo), col(anchor).min(col(cur)))
                }
                SelectKind::Linewise if cur < anchor => cur,
                SelectKind::Linewise => line_start(b, anchor),
            };
            nop(at, Mode::Normal)
        }
    }
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

/// The live `CTRL-V` selection's [`Block`], or `None` when the selection is not blockwise. `$` (curswant at
/// MAXCOL) makes it ragged, each row running to its own line end.
fn selection_block(st: &EditorState, b: &[u8], cur: usize) -> Option<Block> {
    if st.view.mode.selection() != Some(SelectKind::Blockwise) {
        return None;
    }
    let anchor = st.view.anchor?;
    let to_eol = st.view.curswant == MAXCOL;
    Some(block_geom(b, anchor, cur, st.view.indent.tabs(), to_eol))
}

/// Block `r{char}` (Vim `op_replace`): every cell of the block on each row becomes `c` — a `<Tab>` or a
/// wide character inside turns into one `c` per cell, a straddler's outside cells stay as spaces, and a
/// double-width `c` fills two cells each (an odd cell left over becomes a space). Rows the block does not
/// reach are left alone. The cursor goes to the block's top-left.
fn block_replace(block: &Block, c: char, hint: GroupHint) -> Plan {
    use unicode_width::UnicodeWidthChar;
    let cw = c.width().unwrap_or(1).max(1);
    let mut buf = [0u8; 4];
    let cb = c.encode_utf8(&mut buf).as_bytes();
    let edits: Vec<Edit> = block
        .rows
        .iter()
        .filter(|r| r.de > r.ds)
        .map(|r| {
            let short = block.hi == MAXCOL || r.cells <= block.hi;
            let end = if block.hi == MAXCOL {
                r.cells
            } else {
                (block.hi + 1).min(r.cells)
            };
            let cells = end.saturating_sub(block.lo);
            let extra = if short { 0 } else { cells % cw };
            let mut ins = vec![b' '; r.out_l];
            for _ in 0..cells / cw {
                ins.extend_from_slice(cb);
            }
            if !short {
                ins.extend(std::iter::repeat_n(b' ', r.out_r + extra));
            }
            Edit::replace(BytePos(r.ds), r.de - r.ds, ins)
        })
        .collect();
    let at = block.rows[0].ds;
    match EditList::new(edits) {
        Ok(list) if !list.is_empty() => edit(list, at, Mode::Normal, hint),
        _ => nop(at, Mode::Normal),
    }
}

/// Visual-block `>`/`<` (`:help v_b_>`, `v_b_<`, #1180): each row is shifted from the block's LEFT column
/// by `count` shiftwidths, as nvim's `shift_block`; the text left of the block and the block's right edge
/// play no part. A row that ends before the block, or an empty one, is left alone.
///
/// - `>`: the white space just left of the block (a `<Tab>` the left edge cuts included) and the white
///   space from the left edge up to the first non-blank are rebuilt as one run, `count × sw` columns wider,
///   from where that run starts: tabs and spaces under `noexpandtab`, spaces under `expandtab`.
/// - `<`: the white space from the left edge up to the first non-blank shrinks by `count × sw` columns, or
///   to nothing; what of it still fits stays as it was, and a `<Tab>` that no longer fits becomes spaces.
///
/// The cursor goes to the top row at the byte column the block's top-left character had (nvim's
/// `block_col`).
fn plan_block_shift(
    st: &EditorState,
    b: &[u8],
    block: &Block,
    left: bool,
    count: u32,
    hint: GroupHint,
) -> Plan {
    let ind = st.view.indent;
    let ts = ind.tabs();
    let total = ind.shift_width().saturating_mul(count.max(1) as usize);
    let lo = block.lo;
    let is_white = |c: u8| c == b' ' || c == b'\t';
    let mut edits: Vec<Edit> = Vec::new();
    for row in &block.rows {
        let (ls, le) = (row.ls, row.le);
        if ls == le || row.short(lo) {
            continue;
        }
        // The characters that start before the left edge (a straddler included) and, among them, where the
        // run of white space they end with starts (`ws`, at virtual column `ws_v`).
        let (mut i, mut v) = (ls, 0usize);
        let (mut ws, mut ws_v) = (ls, 0usize);
        let (mut prev, mut prev_v) = (ls, 0usize);
        while i < le && v < lo {
            let (j, w) = range::cluster_cells(b, i, le, v, ts);
            if !is_white(b[i]) {
                (ws, ws_v) = (j, v + w);
            }
            (prev, prev_v) = (i, v);
            i = j;
            v += w;
        }
        // `i` (at `v`): the first character at or right of the edge, past a straddler. `nw` (at `nw_v`): the
        // first non-blank from there.
        let (mut nw, mut nw_v) = (i, v);
        while nw < le && is_white(b[nw]) {
            let (j, w) = range::cluster_cells(b, nw, le, nw_v, ts);
            nw = j;
            nw_v += w;
        }
        // nvim's `tabstop_fromto`: the white space filling virtual columns `from..to`, tabs first under
        // `noexpandtab`.
        let blanks = |from: usize, to: usize| {
            let mut out = Vec::new();
            let mut c = from;
            if ind.style == IndentStyle::Tab {
                while c + ts.padding(c) <= to {
                    out.push(b'\t');
                    c += ts.padding(c);
                }
            }
            out.extend(std::iter::repeat_n(b' ', to - c));
            out
        };
        let (from, to, fill) = if left {
            let dest = nw_v - total.min(nw_v.saturating_sub(lo));
            // Keep what stands from the edge (from a straddler the edge cuts) while it fits before `dest`.
            let (mut k, mut kv) = if v > lo { (prev, prev_v) } else { (i, v) };
            while k < nw && kv < dest {
                let (j, w) = range::cluster_cells(b, k, le, kv, ts);
                if kv + w > dest {
                    break;
                }
                k = j;
                kv += w;
            }
            (k, nw, vec![b' '; dest - kv])
        } else if v > lo && b[prev] != b'\t' {
            // The edge cuts a wide character: the shift goes in before it, its tabs laid out as from
            // column 0 (nvim's `ws_vcol = 0`).
            (prev, prev, blanks(0, total))
        } else {
            (ws, nw, blanks(ws_v, nw_v + total))
        };
        if b[from..to] != fill[..] {
            edits.push(Edit::replace(BytePos(from), to - from, fill));
        }
    }
    // The top row's byte column at the edge, kept on the shifted line (clamped onto it).
    let top = block.rows[0];
    let col = range::at_vcol(b, top.ls, lo, ts) - top.ls;
    let delta: isize = edits
        .iter()
        .take_while(|e| e.pos.0 < top.le)
        .map(|e| e.ins.len() as isize - e.del as isize)
        .sum();
    let new_len = (top.le - top.ls).saturating_add_signed(delta);
    let cursor = top.ls + col.min(new_len.saturating_sub(1));
    if edits.is_empty() {
        return nop(cursor, st.mode());
    }
    let edits = EditList::new(edits).expect("one edit per block row, top to bottom: disjoint");
    edit(edits, cursor, st.mode(), hint)
}
