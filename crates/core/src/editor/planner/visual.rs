//! Visual and Select mode: entering, reselecting, and the selection operators.
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::*;

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
                let edits: Vec<Edit> = block
                    .rows
                    .iter()
                    .filter_map(|r| recase_span(b, r.s, r.e, *case, recase).map(|new| (r, new)))
                    .filter(|(r, new)| new[..] != b[r.s..r.e])
                    .map(|(r, new)| Edit::replace(BytePos(r.s), r.e - r.s, new))
                    .collect();
                let at = block.rows[0].ds;
                return match EditList::new(edits) {
                    Ok(list) if !list.is_empty() => edit(list, at, Mode::Normal, hint),
                    _ => nop(at, Mode::Normal),
                };
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
