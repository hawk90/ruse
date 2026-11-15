//! The commit half of the plan / commit pipeline (RFC-0012): [`commit`] applies a `Plan`, and
//! [`apply_command`] plans and commits one command, with the change marks, registers, jumps, Insert state
//! and `curswant` bookkeeping around them. Moved out of `editor/mod.rs` (#1379); a child module, so it
//! reads the private `EditorState` and `View` fields as before.

use super::*;

/// The `(start, end)` byte span (half-open, POST-edit coordinates) the `[`/`]` change marks bracket for an
/// [`EditList`]: `start` is the first edit's position (nothing before it moved), and `end` is the end of the
/// LAST edit's inserted text — its position shifted by the cumulative length delta of every earlier edit,
/// plus that edit's inserted byte count. A pure delete yields `end == start` (both marks collapse onto the
/// deletion point, as Vim does). `None` for an empty list.
fn edit_bracket(list: &EditList) -> Option<(usize, usize)> {
    let edits = list.edits();
    let first = edits.first()?;
    let last = edits.last()?;
    // Lines put below an unterminated last line arrive as `"\n" + lines` at its end: `[` is the first put
    // line's start, not the old line's end (`yyjp`[`, #1088).
    let start =
        if edits.len() == 1 && first.del == 0 && first.ins.len() > 1 && first.ins[0] == b'\n' {
            first.pos.0 + 1
        } else {
            first.pos.0
        };
    let delta_before_last: isize = edits[..edits.len() - 1].iter().map(Edit::delta).sum();
    let post_last_start = (last.pos.0 as isize + delta_before_last).max(0) as usize;
    Some((start, post_last_start + last.ins.len()))
}

/// Where a change begins for `u`/`C-r` (Vim's `uh_cursor`, #1063): the cursor at the moment Vim saves the
/// first undo entry, which an operator has already moved to the start of its span. Inferred from the mode
/// the change was made in and its first edit, since the plan does not carry the operator span.
fn change_start(
    b: &[u8],
    mode: Mode,
    cursor: usize,
    anchor: Option<usize>,
    edits: &EditList,
) -> usize {
    let Some(first) = edits.edits().first() else {
        return cursor;
    };
    let pos = first.pos.0;
    match (mode.selection(), anchor) {
        // A selection operator starts at the selection's start; a linewise one at column 0 of its top line
        // unless the cursor is the top end (nvim 0.12.6: `jlVj>u` -> col 0, `jjlVk>u` -> col 1).
        (Some(SelectKind::Linewise), Some(a)) if a < cursor => line_start(b, a),
        (Some(SelectKind::Linewise), _) => cursor,
        (Some(SelectKind::Charwise), Some(a)) => a.min(cursor),
        (Some(SelectKind::Blockwise), Some(a)) => {
            let (top, bottom) = (a.min(cursor), a.max(cursor));
            let col =
                col_of(b, line_start(b, top), top).min(col_of(b, line_start(b, bottom), bottom));
            at_col(b, line_start(b, top), col)
        }
        (Some(_), None) => pos.min(cursor),
        // A text-entry session starts where the caret was when its first change landed.
        _ if is_text_entry(mode) => cursor,
        _ => {
            let at_line_start = pos == 0 || b[pos - 1] == b'\n';
            let end = pos + first.del;
            // Deleting whole lines: `dd`/`dj` keep the cursor's column, so only the line can move up.
            // The buffer's end is its last line's end, before the final newline (RFC-0024).
            let at_end = end == b.len() || end == crate::motion::content_end(b);
            let whole_lines = first.del > 0
                && ((at_line_start && (b[end - 1] == b'\n' || at_end))
                    || (b.get(pos) == Some(&b'\n') && end == b.len()));
            if first.del > 0 && !whole_lines && pos < cursor {
                // A charwise operator over a backward span (`db`, `d0`, `X`): its start.
                pos
            } else {
                // A linewise operator reaching up (`dk`, `>k`): its first line, at the cursor's column.
                let top = if whole_lines && !at_line_start {
                    pos + 1
                } else {
                    pos
                };
                if top < line_start(b, cursor) {
                    at_col(
                        b,
                        line_start(b, top),
                        col_of(b, line_start(b, cursor), cursor),
                    )
                } else {
                    cursor
                }
            }
        }
    }
}

/// The byte offset for an undo placement: the column on its line (clamped, and pulled off the newline in
/// Normal mode as Vim's `check_cursor` does), or the sticky column when Vim would `beginline`.
/// Run one undo/redo step `f` with the view's marks tracked through it. Like nvim's `u_undoredo`, a step
/// first records the cursor `from` on the jumplist (`setpcmark`), so the entry follows the step's own line
/// changes; a step that finds nothing to re-apply records nothing (#1168).
fn undo_step(
    st: &mut EditorState,
    from: usize,
    f: impl FnOnce(&mut Document, &mut [Tracked]) -> Option<crate::undo_cursor::UndoCursor>,
) -> Option<crate::undo_cursor::UndoCursor> {
    let saved = (st.view.jumps.clone(), st.view.jump_idx);
    st.view.push_jump(from);
    let placed = st.with_tracked(f);
    if placed.is_none() {
        (st.view.jumps, st.view.jump_idx) = saved;
    }
    placed
}

fn undo_target(st: &EditorState, placed: crate::undo_cursor::UndoCursor, mode: Mode) -> usize {
    let b = st.doc.bytes();
    let ls = crate::pos::raw::nth_line_start(b, placed.line);
    let le = line_end(b, ls);
    let at = match placed.col {
        Some(col) => snap(b, (ls + col).min(le)),
        None => range::want_col(b, ls, st.view.curswant.get(), st.view.indent.tabs()),
    };
    if at == le && ls < le && mode == Mode::Normal && st.view.caret == CaretGravity::OnChar {
        prev_grapheme(b, le)
    } else {
        at
    }
}

/// Whether `mode` is one of the text-entry sessions (Insert / Replace / VirtualReplace) — used to decide how
/// the `[`/`]` change marks are bounded (a whole typed run, tracked from session entry to exit) versus a
/// one-shot Normal edit (bounded directly by its [`EditList`]).
fn is_text_entry(mode: Mode) -> bool {
    matches!(mode, Mode::Insert | Mode::Replace | Mode::VirtualReplace)
}

/// Apply a plan to the state, returning the effects the frontend must perform.
pub fn commit(st: &mut EditorState, plan: Plan) -> Vec<Effect> {
    // `"x` sets the one-shot pending register and nothing else — it must NOT be cleared by the tail below
    // (that clear is what consumes the selection on the FOLLOWING command), so it returns early.
    if let Action::SetPending(name) = plan.action {
        st.view.pending_register = name;
        return plan.effects;
    }
    // `"=<expr><CR>`: store the evaluated result into the `"=` slot and arm `=` as the pending register, so
    // the NEXT paste reads it. Returns early like `SetPending` — it must not fall into the tail that clears
    // the pending register (that clear is what consumes the selection on the FOLLOWING command).
    if let Action::SetExprPending(result) = plan.action {
        st.view.registers.set_expr(result);
        st.view.pending_register = Some('=');
        return plan.effects;
    }
    let entry_selection = st.view.mode.selection();
    let entry = Entry {
        cursor: st.view.cursor,
        anchor: st.view.anchor,
        mode: st.view.mode,
        // Every command outside a text-entry session is a new change for `U`; inside one only the first
        // change is (and the first after an undo break, which the transaction's `BreakBefore` hint carries).
        new_command: !is_text_entry(st.view.mode),
    };
    // Whether the buffer ended in a newline before this command: only then is a cursor left at the end
    // the phantom slot (see the clamp below). `x` on the one char of an unterminated last line leaves that
    // line empty, which the byte model spells the same as a final newline.
    let entry_terminated = st.doc.bytes().last() == Some(&b'\n');
    // The undo group before the command: an edit that leaves a different one began a new undo step.
    let entry_group = st.doc.history().current_group();
    // Leaving a selection remembers it (anchor, active end, kind) for `gv` and `'<`/`'>` — the depth-1
    // slice of D-027's selection history — BEFORE the operator's edit, so the edit carries it as nvim's
    // `mark_adjust` carries `b_visual` (#686).
    if plan.mode.selection().is_none() {
        if let (Some(kind), Some(a)) = (entry_selection, entry.anchor) {
            st.view.last_visual = Some((a, entry.cursor, kind));
            st.view.visual_one_end = None;
        }
    }
    let applied = apply_action(st, plan.action, plan.mode, &entry);
    // The cursor the plan computed is valid for the post-action buffer, except undo/redo which resize the
    // text unpredictably — clamp and snap to a char boundary either way.
    // `g;`/`g,` navigate the change list: the action stepped `change_idx` above and yielded the target
    // (or `None` at an end), which overrides the plan's placeholder cursor for this frame only.
    let resolved_cursor = applied.nav_target.unwrap_or(plan.cursor);
    st.view.cursor = snap(st.doc.bytes(), resolved_cursor);
    // Vim `` `^ ``: leaving Insert for Normal records where the caret was (the pre-clamp insert position),
    // so `gi` resumes Insert exactly there. Checked before the mode is overwritten below.
    if matches!(st.view.mode, Mode::Insert) && matches!(plan.mode, Mode::Normal) {
        st.view.last_insert = Some(entry.cursor);
    }
    st.view.mode = plan.mode;
    st.view.last_was_edit = plan.is_edit;
    // Vim change list: an edit records where it happened (the cursor it left behind) as the newest entry,
    // which is also the `` `. `` mark. Motions/undo/g;/g, don't push. Snapped below with the other offsets.
    if plan.is_edit {
        let new_step = st.doc.history().current_group() != entry_group;
        st.view
            .push_change(st.doc.bytes(), st.view.cursor, new_step);
    }
    let leaving_entry = set_change_marks(st, &entry, &plan.set_register, &applied);
    st.view.snap_offsets(st.doc.bytes());
    // The `<BS>`-restore history lives only while a replace session is active; drop it on any exit.
    if !matches!(st.view.mode, Mode::Replace | Mode::VirtualReplace) {
        st.view.replace_stack.clear();
    }
    // A blockwise insert-replicate session lives only while Insert is held; drop it on any exit (the
    // `<Esc>` that closes it already read the session to build the replicate before this runs).
    if st.view.mode != Mode::Insert {
        st.view.block_insert = None;
    }
    settle_normal_cursor(st, plan.is_edit, leaving_entry, entry_terminated);
    write_register(st, plan.set_register);
    set_anchor_and_mark(
        st,
        entry_selection.is_some(),
        entry.cursor,
        plan.set_anchor,
        plan.set_mark,
    );
    // The pending register (`"x`) is one-shot: any command other than `SetRegister` (which returned early
    // above) consumes it. Cleared here AFTER the register write so a stray `"x` before a non-register
    // command is simply forgotten, and a later plain edit never leaks into the named slot.
    st.view.pending_register = None;
    let mut effects = plan.effects;
    effects.extend(applied.extra);
    effects
}

/// The state a command found, read before [`commit`] runs its action.
struct Entry {
    cursor: usize,
    anchor: Option<usize>,
    mode: Mode,
    /// Whether the command starts a new change for `U` (see [`commit`]).
    new_command: bool,
}

/// What a plan's action left for the rest of [`commit`] to read.
#[derive(Default)]
struct Applied {
    /// The `[`/`]` change-mark span of a Normal-mode edit, read off the plan's edits (post-edit coords)
    /// before they are moved into a Transaction. A yank has no edits; its span rides in `RegWrite::Yank`.
    edit_span: Option<(usize, usize)>,
    /// `g;`/`g,`, the jumplist and undo resolve their destination here (they step state the pure planner
    /// cannot); it overrides the plan's cursor.
    nav_target: Option<usize>,
    /// The lines an undo/redo leaves `[`, `]` and `.` on (#1168).
    undo_marks: Option<crate::undo_cursor::ChangeMarks>,
    /// Effects the commit itself adds (a failed `g;`/`CTRL-O`), after the plan's own.
    extra: Vec<Effect>,
}

/// Run a plan's action: apply its edits, step undo or a list, or arm a session.
fn apply_action(st: &mut EditorState, action: Action, mode: Mode, entry: &Entry) -> Applied {
    let mut out = Applied::default();
    match action {
        Action::Txn { edits, hint } => {
            out.edit_span = apply_planned(st, edits, hint, entry);
        }
        Action::ReplaceTxn {
            edits,
            hint,
            push,
            pop,
        } => {
            out.edit_span = apply_planned(st, edits, hint, entry);
            if pop {
                st.view.replace_stack.pop();
            }
            if let Some(entry) = push {
                st.view.replace_stack.push(entry);
            }
        }
        Action::Undo => undo_nav(st, &mut out, mode, |d, t| {
            UndoStep::Undo.take(d, entry.cursor, t)
        }),
        Action::Redo => undo_nav(st, &mut out, mode, |d, t| {
            UndoStep::Redo.take(d, entry.cursor, t)
        }),
        Action::UndoChrono { older } => undo_nav(st, &mut out, mode, |d, t| {
            UndoStep::Chrono { older }.take(d, entry.cursor, t)
        }),
        Action::LineUndo => out.nav_target = st.line_undo_at(entry.cursor),
        Action::JumpChange { older } => {
            out.nav_target = st.view.nav_change(older);
            if out.nav_target.is_none() {
                // nvim's `nv_pcmark`: each end of the change list has its own error.
                let msg = if st.view.changes.is_empty() {
                    "E664: Changelist is empty"
                } else if older {
                    "E662: At start of changelist"
                } else {
                    "E663: At end of changelist"
                };
                out.extra.push(Effect::Status(msg.to_string()));
                out.extra.push(Effect::Beep);
            }
        }
        Action::JumpList { older } => {
            out.nav_target = st.view.nav_jump(older, entry.cursor, st.doc.bytes());
            if out.nav_target.is_none() {
                out.extra.push(Effect::Beep);
            }
        }
        Action::SetNamedMark { ch } => {
            st.view.set_named_mark(ch, st.view.cursor);
        }
        Action::BlockInsertArm {
            edits,
            hint,
            session,
        } => {
            if !edits.is_empty() {
                out.edit_span = apply_planned(st, edits, hint, entry);
            }
            // Retain this block's geometry so a later `.` (dot-repeat) can rebuild an equivalent block at
            // the caret. Set on ARM (the geometry is known here) so it is available before the session's
            // `<Esc>` closes; the dot-replay re-arms with the SAME geometry, so `..` stays idempotent.
            let kind = if session.change {
                BlockInsertKind::Change
            } else if session.append {
                BlockInsertKind::Append
            } else {
                BlockInsertKind::Insert
            };
            st.view.last_block_geom = Some(BlockGeom {
                height: session.rows_below + 1,
                width: session.width,
                kind,
                to_eol: session.to_eol,
            });
            st.view.block_insert = Some(session);
        }
        Action::Nop => {}
        // Handled by the early return above; the buffer-mutating tail never runs for them.
        Action::SetPending(_) => unreachable!("SetPending is handled before the action match"),
        Action::SetExprPending(_) => {
            unreachable!("SetExprPending is handled before the action match")
        }
    }
    out
}

/// Apply a plan's edits as one transaction whose cursor is where the change began; the `[`/`]` span of
/// the edits, in post-edit coordinates.
fn apply_planned(
    st: &mut EditorState,
    edits: EditList,
    hint: GroupHint,
    entry: &Entry,
) -> Option<(usize, usize)> {
    let span = edit_bracket(&edits);
    st.doc.prime_line_undo(entry.cursor, entry.new_command);
    let start = change_start(
        st.doc.bytes(),
        entry.mode,
        entry.cursor,
        entry.anchor,
        &edits,
    );
    st.apply_planned_txn(edits, hint, start);
    span
}

/// One undo-tree step (`u`, `CTRL-R`, `g-`/`g+`): where it puts the cursor and the marks it leaves.
fn undo_nav(
    st: &mut EditorState,
    out: &mut Applied,
    mode: Mode,
    step: impl FnOnce(&mut Document, &mut [Tracked]) -> Option<crate::undo_cursor::UndoCursor>,
) {
    let placed = undo_step(st, st.view.cursor, step);
    out.nav_target = placed.map(|p| undo_target(st, p, mode));
    out.undo_marks = placed.map(|p| p.marks);
}

/// The `[`/`]` change marks and the `.` entry an undo leaves; whether the command left a text-entry
/// session.
fn set_change_marks(
    st: &mut EditorState,
    entry: &Entry,
    set_register: &Option<RegWrite>,
    applied: &Applied,
) -> bool {
    let (entry_mode, entry_cursor) = (entry.mode, entry.cursor);
    let plan_mode = st.view.mode;
    let undo_marks = applied.undo_marks;
    let edit_span = applied.edit_span;
    // Undo and redo put `[`/`]` at column 0 of the first and last line they re-applied, and `.` (the newest
    // change-list entry, without starting a new one) at column 0 of the top line of the last entry they
    // re-applied: nvim's `u_undoredo` and `changed_lines` (#1168).
    if let Some(m) = undo_marks {
        let b = st.doc.bytes();
        st.view.change_start = Some(crate::pos::raw::nth_line_start(b, m.start_line));
        st.view.change_end = Some(crate::pos::raw::nth_line_start(b, m.end_line));
        if let Some(line) = m.last_change_line {
            let pos = crate::pos::raw::nth_line_start(b, line);
            st.view.push_change(b, pos, false);
        }
    }
    // Vim `[`/`]` change marks: bound the last changed OR yanked text. A text-entry session (Insert/Replace)
    // is ONE change bracketed from where it began to the caret at exit (so `ihello<Esc>` brackets the whole
    // run, not just the last typed char); a one-shot Normal edit is bounded by its `EditList`; a yank by the
    // span carried in `RegWrite::Yank`. `]` is stored as the LAST char (inclusive) for delete/replace/put/
    // yank, but as the exclusive insert END-caret for a session — the jump's EOL clamp reconciles both to
    // match Neovim. Individual keystrokes WITHIN a session don't touch the marks (only entry/exit do).
    let entering_entry = !is_text_entry(entry_mode) && is_text_entry(plan_mode);
    let leaving_entry = is_text_entry(entry_mode) && !is_text_entry(plan_mode);
    if entering_entry {
        st.view.insert_start = Some(st.view.cursor);
    }
    if leaving_entry {
        let start = st.view.insert_start.unwrap_or(entry_cursor);
        st.view.change_start = Some(start.min(entry_cursor));
        st.view.change_end = Some(start.max(entry_cursor));
        st.view.insert_start = None;
    } else if !is_text_entry(plan_mode) {
        // A completed Normal-mode edit, or a yank (which carries its own span since it makes no edits).
        let yank_span = match set_register {
            Some(RegWrite::Yank(_, span)) => Some(*span),
            _ => None,
        };
        if let Some((s, e)) = edit_span.or(yank_span) {
            let b = st.doc.bytes();
            let s = s.min(b.len());
            st.view.change_start = Some(s);
            // A change that reaches the buffer's end ends on its last char: the final newline is no
            // position (`yyjp` on the last line: `']` on the put line's last char, RFC-0024).
            let e = e.min(crate::motion::content_end(b)).max(s);
            st.view.change_end = Some(if e > s { prev_grapheme(b, e) } else { s });
        }
    }
    leaving_entry
}

/// Where an edit leaves the on-char cursor: never on the newline, never inside a cluster, never on the
/// phantom slot after the final newline.
fn settle_normal_cursor(
    st: &mut EditorState,
    is_edit: bool,
    leaving_entry: bool,
    entry_terminated: bool,
) {
    // Vim never rests the Normal-mode cursor on the newline: after an edit that leaves it beyond the final
    // char of a non-empty line, pull it back onto the last char (e.g. `dw` on the last word → the cursor
    // clamps to the trailing char rather than the line end). Scoped to edits in Normal mode so it never
    // touches Insert's legitimate cursor-past-end, and guarded by `ls < le` so an empty line keeps `[n,0]`.
    // Gated on `CaretGravity::OnChar` (D-050): the Emacs profile is BetweenChar, so Emacs point legitimately
    // rests on the after-last slot and must NOT be clamped (`kill-line`/`yank` land point N+1, not N).
    if is_edit && st.view.mode == Mode::Normal && st.view.caret == CaretGravity::OnChar {
        let b = st.doc.bytes();
        let le = line_end(b, st.view.cursor);
        let ls = line_start(b, st.view.cursor);
        if st.view.cursor == le && ls < le {
            st.view.cursor = motion::prev_grapheme(b, le);
        }
        // An edit can join the text around the cursor into one cluster: deleting a byte that is not UTF-8
        // from between a base and a combining mark (#1232). The cursor goes to the cluster's start, as
        // nvim's `mb_adjust_cursor` puts it on a char's head.
        let cur = st.view.cursor;
        if cur < b.len() && b[cur] != b'\n' {
            let head = motion::prev_grapheme(b, motion::next_grapheme(b, cur));
            if head < cur {
                st.view.cursor = head;
            }
        }
    }
    // The phantom slot after the final newline is not a line (#677): an on-char caret outside text entry
    // never rests there, whatever put it there (a motion running off the end, a linewise delete of the last
    // line, undo). Exempt where the byte model cannot tell that slot from a real empty last line: leaving
    // Insert (the caret is on the line just typed into, as after `o` on an unterminated last line), and a
    // buffer that was unterminated before the command (`x` emptying its last line).
    if st.view.caret == CaretGravity::OnChar
        && !is_text_entry(st.view.mode)
        && !leaving_entry
        && entry_terminated
    {
        st.view.cursor = motion::off_phantom_at(st.doc.bytes(), st.view.cursor);
    }
}

/// Write what a plan captured into the pending register (or the unnamed one), and keep the Emacs kill run.
fn write_register(st: &mut EditorState, set_register: Option<RegWrite>) {
    // A yank/delete/change writes its captured span into the pending register (or unnamed when none),
    // mirroring the unnamed slot on a named write; append (`"A`) is handled inside the store. An Emacs kill
    // additionally ACCUMULATES onto the unnamed entry when the previous command was also a kill, and always
    // marks the run so the next kill accumulates onto it; any other command breaks the run.
    let was_kill = st.view.last_was_kill;
    st.view.last_was_kill = false;
    match set_register {
        Some(RegWrite::Edit(reg)) => st.view.registers.delete(st.view.pending_register, reg),
        Some(RegWrite::EditRingOne(reg)) => {
            st.view
                .registers
                .delete_ring(st.view.pending_register, reg, true);
        }
        Some(RegWrite::Replaced(reg)) => st.view.registers.delete(None, reg),
        Some(RegWrite::Yank(reg, _)) => st.view.registers.yank(st.view.pending_register, reg),
        Some(RegWrite::KillAppend(reg)) => {
            if was_kill {
                st.view.registers.kill_append(reg);
            } else {
                st.view.registers.write(st.view.pending_register, reg);
            }
            st.view.last_was_kill = true;
        }
        Some(RegWrite::KillPrepend(reg)) => {
            if was_kill {
                st.view.registers.kill_prepend(reg);
            } else {
                st.view.registers.write(st.view.pending_register, reg);
            }
            st.view.last_was_kill = true;
        }
        Some(RegWrite::Write(reg)) => st.view.registers.write(st.view.pending_register, reg),
        None => {}
    }
}

/// The selection anchor and the Emacs mark after a command, snapped into the text.
fn set_anchor_and_mark(
    st: &mut EditorState,
    was_selection: bool,
    entry_cursor: usize,
    set_anchor: Option<usize>,
    set_mark: Option<MarkWrite>,
) {
    // Maintain the selection anchor: set it when entering a selection mode (Visual/Select; the fixed end
    // is where the cursor was), keep it while staying in one — including across a Visual↔Select CTRL-G
    // toggle, since both are selection modes — and clear it on any exit to Normal/Insert.
    match (was_selection, st.view.mode.selection().is_some()) {
        (false, true) => st.view.anchor = Some(entry_cursor),
        // Leaving a selection (remembered for `gv` above, before the edit) drops the anchor.
        (_, false) => st.view.anchor = None,
        (true, true) => {}
    }
    // A text object in a selection mode overrides the anchor to span the object (both ends move at once).
    if let Some(a) = set_anchor {
        st.view.anchor = Some(a);
    }
    // Keep the raw-offset anchor valid: an edit applied while in Visual mode can resize the buffer under it,
    // and a stale anchor past the new end would make `selection_range` slice out of bounds (a core panic).
    // Snapping clamps it into range and onto a char boundary. The edit-tracking anchor-store position that
    // would move the anchor *semantically* with the edit is deferred (D-027); v0 only guarantees totality.
    if let Some(a) = st.view.anchor {
        st.view.anchor = Some(snap(st.doc.bytes(), a));
    }
    // The Emacs mark (D-027): a plan installs or drops it; otherwise it persists. Snap it into range like the
    // anchor so an edit that resized the buffer under it can never make `KillRegion` slice out of bounds.
    match set_mark {
        Some(MarkWrite::Set(m)) => st.view.mark = Some(m),
        None => {}
    }
    if let Some(m) = st.view.mark {
        st.view.mark = Some(snap(st.doc.bytes(), m));
    }
}

/// Whether `cmd` is a Vim JUMP — a command that sets the `''`/jumplist "before" mark (search, `G`/`gg`, `%`,
/// a mark jump, paragraph/sentence motion). A plain `h`/`j`/`k`/`l`/word motion is NOT a jump. `CTRL-O`/
/// `CTRL-I` themselves are excluded so navigating the jumplist does not record new entries.
fn is_jump(cmd: &Command) -> bool {
    use crate::motion::Motion as M;
    match cmd {
        Command::SearchNext(_)
        | Command::SearchPrev(_)
        // A `/`/`?` search is a jump too, so `''` returns from it (#1088).
        | Command::Search {
            op: crate::SearchOp::Move,
            ..
        }
        | Command::SearchWordUnder {
            op: crate::SearchOp::Move,
            ..
        } => true,
        // `gd`/`gD` (and the `GotoFirstMatch` they resolve to) are jumps — `CTRL-O` returns.
        Command::GotoDeclaration { .. } | Command::GotoFirstMatch(_) => true,
        Command::GotoLastChange
        | Command::GotoLastChangeLine
        | Command::GotoChangeMarkStart
        | Command::GotoChangeMarkEnd
        | Command::GotoChangeMarkStartLine
        | Command::GotoChangeMarkEndLine
        | Command::GotoNamedMark(_)
        | Command::GotoNamedMarkLine(_)
        | Command::GotoContextMark
        | Command::GotoContextMarkLine => true,
        Command::Move(_, m) => matches!(
            m,
            M::GotoLine
                | M::LastLine
                | M::MatchBracket
                | M::GotoPercent
                | M::ParagraphFwd
                | M::ParagraphBack
                | M::SentenceFwd
                | M::SentenceBack
                | M::SectionFwd
                | M::SectionBack
                | M::SectionEndFwd
                | M::SectionEndBack
        ),
        _ => false,
    }
}

/// Convenience: plan then commit one command, then maintain the sticky desired column (curswant).
pub fn apply_command(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    // A command that adds text to the emptied buffer adds it to its one line (RFC-0024 §5).
    if st.makes_the_emptied_line() && adds_text(st, cmd) {
        return st.on_the_emptied_line(|st| apply_command_here(st, cmd));
    }
    apply_command_here(st, cmd)
}

/// Whether `cmd` adds text to the buffer: `o` and `O`, a put of a non-empty register (`p`, `P`, `gp`, `gP`,
/// `]p`, `[p`, `zp`, `zP`), and the text commands of Insert and Replace mode (typed, put or completed).
fn adds_text(st: &EditorState, cmd: &Command) -> bool {
    let register = || {
        !st.view
            .registers
            .get(st.view.pending_register)
            .text()
            .is_empty()
    };
    match cmd {
        Command::OpenBelow | Command::OpenAbove | Command::OpenLineIndent { .. } => true,
        Command::Paste { .. }
        | Command::PasteIndent { .. }
        | Command::PasteBlockInner { .. }
        | Command::PasteSelection { .. } => register(),
        Command::InsertChar(_)
        | Command::InsertNewline
        | Command::InsertTab
        | Command::InsertRegister(_)
        | Command::InsertEval(_)
        | Command::InsertPaste(_)
        | Command::InsertCloser { .. }
        | Command::InsertAtLastInsert
        | Command::InsertIndent
        | Command::CompleteWord { .. }
        | Command::ReplaceType(_)
        | Command::VirtualReplaceType(_)
        | Command::VirtualReplaceChar(..)
        | Command::ReplacePaste(_)
        | Command::BlockInsert(_) => true,
        _ => false,
    }
}

fn apply_command_here(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    // `%` follows the View's `'lisp'` rules while the command runs (#1163).
    let _lisp = crate::findmatch::lisp_scope(st.view.format.lisp);
    // A resolved Ex edit (RFC-0020) runs the same `EditorState` method the live Ex command runs; it has no
    // plan of its own.
    if let Command::Ex(ex) = cmd {
        st.apply_ex(ex);
        return Vec::new();
    }
    // A jump records the position it LEAVES onto the jumplist (Vim), so `CTRL-O` can return there.
    let jump_from = is_jump(cmd).then_some(st.view.cursor);
    prepare_selection_op(st, cmd);
    let (entry_mode, entry_cursor, entry_rev) = (st.view.mode, st.view.cursor, st.doc.revision());
    let entry_was_edit = st.view.last_was_edit;
    // The buffer as the command found it (a refcount, not a copy): the entry column and line below are
    // measured from it only when needed, since finding them walks the line from its start (#1231).
    let entry_text = st.doc.text_arc();
    let entry_col = || {
        col_of(
            &entry_text,
            line_start(&entry_text, entry_cursor),
            entry_cursor,
        )
    };
    let opened = Opened::of(st, cmd);
    let mut p = plan(st, cmd);
    // `'nomodifiable'` (#619): a command that would change the text or step the undo history does
    // nothing but say E21 (nvim checks `MODIFIABLE` before each change), so no register, mark or mode
    // change happens either.
    if !st.doc.is_modifiable()
        && matches!(
            p.action,
            Action::Txn { .. }
                | Action::ReplaceTxn { .. }
                | Action::Undo
                | Action::Redo
                | Action::LineUndo
                | Action::UndoChrono { .. }
        )
    {
        return vec![Effect::Status(crate::document::E21.to_string())];
    }
    // A linewise change (`cc`, `S`, `Vc`) leaves its line with the kept indent and nothing typed: the
    // same autoindent cleanup an opened line gets applies (nvim's `op_change` sets `did_ai`).
    let linewise_change = entry_mode != Mode::Insert
        && p.mode == Mode::Insert
        && matches!(&p.set_register, Some(RegWrite::Edit(r) | RegWrite::EditRingOne(r)) if r.is_linewise());
    if uses_reg_one(cmd) {
        if let Some(RegWrite::Edit(reg)) = p.set_register.take() {
            p.set_register = Some(RegWrite::EditRingOne(reg));
        }
    }
    // A refused register still ran the operator in nvim (`op_delete` fails inside `do_pending_operator`,
    // which has already set the wanted column), so it is not a failure that keeps the column (#1171).
    let refused = refuse_unwritable_register(st, cmd, &p);
    let was_refused = refused.is_some();
    if let Some(refused) = refused {
        p = refused;
    }
    let mut effects = commit(st, p);
    effects.extend(lisp_reindent(
        st,
        cmd,
        linewise_change,
        opened.leader,
        entry_rev,
    ));
    let failed = !was_refused
        && st.view.cursor == entry_cursor
        && st.doc.revision() == entry_rev
        && st.view.mode == entry_mode;
    let kept_col = failed && keeps_curswant_on_failure(st, cmd, effects.contains(&Effect::Beep));
    track_insert_start(st, cmd, entry_mode, entry_cursor, entry_rev, &entry_col);
    drop_opened_line_undo(st, cmd, entry_rev);
    // `CTRL-G U` + `<Left>`/`<Right>` moves without starting a new undo group.
    if matches!(cmd, Command::InsertCaretStep { .. }) {
        st.view.last_was_edit = entry_was_edit;
    }
    // A failed jump (a search that matches nowhere) records nothing.
    if let Some(from) = jump_from.filter(|_| !effects.contains(&Effect::Beep)) {
        // A jump edits nothing, except the untouched autoindent an Insert one-shot leaves behind
        // (`leave_autoindent`): the jump then leaves from where that removal put the cursor (#1368).
        let from = if st.doc.revision() == entry_rev {
            from
        } else {
            through_change(&entry_text, st.doc.bytes(), from)
        };
        st.view.push_jump(from);
    }
    set_insert_state(st, cmd, &opened, linewise_change, &entry_text, entry_cursor);
    update_curswant(st, cmd, entry_mode, kept_col);
    effects
}

/// What a line the command opens carries, read from the buffer before the command's edit.
struct Opened {
    /// The smartindent state the opened line arms.
    si: indent::SiState,
    /// Whether the open continues a comment leader, which counts as an untouched autoindent (nvim sets
    /// `did_ai`).
    leader: bool,
    /// The end of a three-piece comment the opened line may close.
    end_comment: Option<u8>,
    /// The indent `^ CTRL-D` deletes, for the next `<CR>`.
    deleted_indent: usize,
}

impl Opened {
    fn of(st: &EditorState, cmd: &Command) -> Opened {
        let deleted_indent = if matches!(cmd, Command::InsertDeleteIndentRestore) {
            let b = st.doc.bytes();
            let ls = line_start(b, st.view.cursor);
            // The `^` is deleted first, by the `DeleteBack` before this command; the indent is the line's.
            indent::indent_cols(&b[ls..line_end(b, ls)], st.view.indent.tabs()).0
        } else {
            0
        };
        Opened {
            si: opened_si_state(st, cmd),
            leader: planner::open_comment_line(st, cmd).is_some_and(|o| o.1),
            end_comment: planner::open_end_comment(st, cmd),
            deleted_indent,
        }
    }
}

/// `'lisp'` under `'autoindent'`: the line an open or a linewise change leaves takes the Lisp indent
/// (nvim's `open_line` and `op_change` run `fixthisline(get_lisp_indent)`, #1163).
fn lisp_reindent(
    st: &mut EditorState,
    cmd: &Command,
    linewise_change: bool,
    opened_leader: bool,
    entry_rev: crate::Revision,
) -> Vec<Effect> {
    if st.view.format.lisp
        && st.view.indent.autoindent
        && st.view.mode == Mode::Insert
        && !opened_leader
        && st.view.caret == CaretGravity::OnChar
        && (linewise_change
            || (matches!(
                cmd,
                Command::OpenBelow | Command::OpenAbove | Command::InsertNewline
            ) && st.doc.revision() != entry_rev))
    {
        let b = st.doc.bytes();
        let row = crate::pos::raw::line_of(b, st.view.cursor);
        let cols = lisp::line_indent(b, row, st.view.indent.tabs(), &st.view.format.lispwords);
        let fix = planner::plan_set_indents(st, row, row, &[Some(cols)], GroupHint::Continue);
        return commit(st, fix);
    }
    Vec::new()
}

/// Opening a line drops the line `U` would restore (nvim's `open_line`). After `o`/`O` the first typed
/// change still saves the new line; after an Insert `<CR>` the rest of the insert saves nothing.
fn drop_opened_line_undo(st: &mut EditorState, cmd: &Command, entry_rev: crate::Revision) {
    match cmd {
        Command::OpenBelow
        | Command::OpenAbove
        | Command::OpenLineIndent {
            kind: OpenKind::Below | OpenKind::Above,
            ..
        } => st.doc.clear_line_undo(false),
        Command::InsertNewline
        | Command::OpenLineIndent {
            kind: OpenKind::Split,
            ..
        } if st.doc.revision() != entry_rev => st.doc.clear_line_undo(true),
        _ => {}
    }
}

/// The Insert-session state a command leaves: the pending autoindent, smartindent, the indent `^ CTRL-D`
/// took, the comment end to close and whether a space was just typed.
fn set_insert_state(
    st: &mut EditorState,
    cmd: &Command,
    opened: &Opened,
    linewise_change: bool,
    entry_text: &[u8],
    entry_cursor: usize,
) {
    // Maintain the auto-indent-pending flag (read by `EnterNormal` for `<Esc>` autoindent cleanup): a
    // tree-suggested open sets it; typing a non-blank clears it; any Insert entry/exit resets it so it
    // never leaks to a later line. Done here (not in the pure planner) because it depends on the Command.
    match cmd {
        Command::OpenLineIndent { level, .. } => st.view.auto_indent_pending = *level > 0,
        Command::OpenBelow | Command::OpenAbove | Command::InsertNewline => {
            st.view.auto_indent_pending =
                st.view.mode == Mode::Insert && (opened.leader || only_indent_before_caret(st));
        }
        _ if linewise_change => st.view.auto_indent_pending = only_indent_before_caret(st),
        // A caret key off the line ends it (nvim's `start_arrow`).
        _ if left_line(entry_text, entry_cursor, st.doc.bytes(), st.view.cursor) => {
            st.view.auto_indent_pending = false;
        }
        // Typing anything keeps the indent (nvim clears `did_ai` on a typed blank too).
        Command::InsertChar(_)
        | Command::InsertTab
        | Command::InsertPaste(_)
        | Command::InsertRegister(_)
        | Command::InsertEval(_)
        | Command::EnterNormal
        | Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::InsertColumnZero
        | Command::AppendLineEnd => st.view.auto_indent_pending = false,
        _ => {}
    }
    st.view.si_state = opened.si;
    match cmd {
        Command::InsertDeleteIndentRestore => st.view.old_indent = opened.deleted_indent,
        Command::InsertNewline if st.view.mode == Mode::Insert => st.view.old_indent = 0,
        _ => {}
    }
    st.view.end_comment = if opened.leader {
        opened.end_comment
    } else {
        None
    };
    if !st.view.auto_indent_pending {
        st.view.end_comment = None;
    }
    st.view.typed_space = match cmd {
        Command::InsertChar(' ') => true,
        Command::InsertChar(_) | Command::DeleteBack | Command::SetIndentCols { .. } => {
            st.view.typed_space
        }
        _ => false,
    };
}

/// `p`, an offset into `old`, in `new`, which differs from it in one contiguous span: unchanged before
/// the span, shifted after it, and the span's start inside it.
pub(super) fn through_change(old: &[u8], new: &[u8], p: usize) -> usize {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    if p <= prefix {
        p
    } else if p >= old.len() - suffix {
        p + new.len() - old.len()
    } else {
        prefix
    }
}

/// Whether `cmd`, which FAILED (nothing moved, changed or switched mode; `beeped` when it beeped), leaves
/// the wanted column alone (#1171). nvim recomputes `w_curswant` lazily, only when a command sets
/// `w_set_curswant`; a failing command keeps the old column unless its handler sets that flag before (or
/// regardless of) the check that fails. These handlers bail out first and so keep it: `h`/`l` (`nv_left`,
/// `nv_right`), `f`/`t`/`;`/`,` (`nv_csearch`), `%` (`nv_percent`), `[(`/`])`/`[{`/`]}`/`[m`/`]M`
/// (`nv_bracket_block`), CTRL-O/CTRL-I/`g;`/`g,` at an end of their list (`nv_pcmark`), `CTRL-A`/`CTRL-X`
/// without a number (`op_addsub`), `J`/`gJ` (`nv_join`), `r` past the line's end (`nv_replace`) and `~` on
/// an empty line (`n_swapchar`); an operator over one of those motions is cancelled with it (`dfz`).
/// Everything else that fails resets the column to the cursor's, as nvim's handlers do: `/`/`?`/`n`/`N`/
/// `*`/`#` (`normal_search`), word, sentence, paragraph and section motions, text objects (`nv_object`),
/// mark jumps (`nv_mark_move_to`), undo and redo, and an operator that ran but failed (`"/dd`).
fn keeps_curswant_on_failure(st: &EditorState, cmd: &Command, beeped: bool) -> bool {
    let keeps = |m: &Motion| {
        matches!(
            m,
            // `j`/`k` (`nv_down`/`nv_up`) and `{count}dd` past the end (`nv_lineop`) fail in `cursor_down`/
            // `cursor_up` before anything sets the column (#1176).
            Motion::Up
                | Motion::Down
                | Motion::Line
                | Motion::Left
                | Motion::Right
                | Motion::WrapLeft
                | Motion::WrapRight
                | Motion::FindChar { .. }
                | Motion::MatchBracket
                | Motion::GotoPercent
                | Motion::UnmatchedParenBack
                | Motion::UnmatchedParenFwd
                | Motion::UnmatchedBraceBack
                | Motion::UnmatchedBraceFwd
                | Motion::MethodStartFwd
                | Motion::MethodStartBack
                | Motion::MethodEndFwd
                | Motion::MethodEndBack
        )
    };
    match cmd {
        Command::Move(_, m)
        | Command::Delete(_, m)
        | Command::Change(_, m)
        | Command::Yank(_, m) => beeped && keeps(m),
        Command::OpForced { motion, .. }
        | Command::CaseMotion { motion, .. }
        | Command::ShiftMotion { motion, .. }
        | Command::Reindent { motion, .. }
        | Command::Format { motion, .. }
        | Command::Comment { motion, .. } => beeped && keeps(motion),
        Command::GotoOlderJump
        | Command::GotoNewerJump
        | Command::GotoOlderChange
        | Command::GotoNewerChange
        // `{count}>>` on the last line (`nv_lineop`'s `cursor_down`).
        | Command::ShiftRight(_)
        | Command::ShiftLeft(_) => beeped,
        // These always edit when they succeed.
        Command::IncrementNumber(_)
        | Command::JoinLines(_)
        | Command::JoinLinesNoSpace(_)
        | Command::ReplaceChar(..) => true,
        Command::ToggleCase(_) => {
            let b = st.doc.bytes();
            line_start(b, st.view.cursor) == line_end(b, st.view.cursor)
        }
        _ => false,
    }
}

/// Whether the cursor left its line: `line_start(after, cur) != line_start(before, entry)`. On an unchanged
/// buffer that is a `\n` between the two positions, which costs the distance moved, not the line (#1231).
fn left_line(before: &[u8], entry: usize, after: &[u8], cur: usize) -> bool {
    // `before` is held for the whole command, so an equal data pointer is the same, unchanged buffer.
    if std::ptr::eq(before.as_ptr(), after.as_ptr()) && before.len() == after.len() {
        let (lo, hi) = (
            entry.min(cur).min(after.len()),
            entry.max(cur).min(after.len()),
        );
        after[lo..hi].contains(&b'\n')
    } else {
        line_start(after, cur) != line_start(before, entry)
    }
}

/// Whether the caret sits after nothing but a non-empty indent on its line (an autoindent's leftover).
fn only_indent_before_caret(st: &EditorState) -> bool {
    let b = st.doc.bytes();
    let cur = st.view.cursor;
    let ls = line_start(b, cur);
    cur > ls && b[ls..cur].iter().all(|&c| c == b' ' || c == b'\t')
}

/// The `'smartindent'` rules a line opened by `cmd` arms (#617): computed from the line it opens from,
/// before the edit. Every other command disarms them (nvim clears them on the first typed key).
fn opened_si_state(st: &EditorState, cmd: &Command) -> indent::SiState {
    let si = st.view.indent.smartindent && st.view.caret == CaretGravity::OnChar;
    let b = st.doc.bytes();
    let cur = st.view.cursor;
    let ls = || line_start(b, cur); // only an open needs the line (#1231)
    match cmd {
        Command::OpenBelow | Command::OpenAbove if si => {
            let below = matches!(cmd, Command::OpenBelow);
            let words = &st.view.format.cinwords;
            st.view
                .indent
                .open_indent(b, ls(), line_end(b, cur), below, words)
                .1
        }
        Command::InsertNewline if si && st.view.mode == Mode::Insert => {
            st.view
                .indent
                .open_indent(b, ls(), cur, true, &st.view.format.cinwords)
                .1
        }
        _ => indent::SiState::default(),
    }
}

/// Whether `cmd` deletes or changes over a motion that always fills `"1` (nvim's `use_reg_one`: `%`, `(`,
/// `)`, `{`, `}`, `` ` ``, `/`, `?`, `n`, `N`, `*`, `#`), even within one line (#749).
fn uses_reg_one(cmd: &Command) -> bool {
    use crate::command::{MarkOp, SearchOp};
    let search_op = |op: &SearchOp| matches!(op, SearchOp::Delete | SearchOp::Change);
    match cmd {
        Command::Delete(_, m) | Command::Change(_, m) => matches!(
            m,
            Motion::MatchBracket
                | Motion::ParagraphFwd
                | Motion::ParagraphBack
                | Motion::SentenceFwd
                | Motion::SentenceBack
        ),
        Command::OpToMark { op, .. } => matches!(op, MarkOp::Delete | MarkOp::Change),
        Command::Search { op, .. } | Command::SearchWordUnder { op, .. } => search_op(op),
        _ => false,
    }
}

/// A yank, delete or change naming a register it cannot write (`"/dd`, `":yy`, `"%cw` — the read-only
/// registers, #1022) fails as nvim's `op_delete`/`op_yank` do: nothing is captured or deleted, the cursor
/// goes to the start of the text the operator covered, a selection ends, and a change still enters Insert
/// there. `None` when the plan writes no register or may write the one named.
fn refuse_unwritable_register(st: &EditorState, cmd: &Command, p: &Plan) -> Option<Plan> {
    let (start, linewise, op) = match p.set_register.as_ref()? {
        RegWrite::Yank(reg, (s, _)) => (*s, reg.is_linewise(), planner::RefusedOp::Yank),
        RegWrite::Edit(reg) | RegWrite::EditRingOne(reg) => (
            match &p.action {
                Action::Txn { edits, .. } => edits.edits().first().map_or(p.cursor, |e| e.pos.0),
                _ => p.cursor,
            },
            reg.is_linewise(),
            if p.mode == Mode::Insert {
                planner::RefusedOp::Change
            } else {
                planner::RefusedOp::Delete
            },
        ),
        RegWrite::Replaced(_)
        | RegWrite::KillAppend(_)
        | RegWrite::KillPrepend(_)
        | RegWrite::Write(_) => return None,
    };
    if RegisterStore::is_writable(st.view.pending_register) {
        return None;
    }
    let mode = if p.mode == Mode::Insert {
        Mode::Insert
    } else {
        Mode::Normal
    };
    // A linewise range starts on its first line (#1174); the edit's start can be the newline before it.
    let start = linewise
        .then(|| planner::refused_linewise_cursor(st, cmd, op))
        .flatten()
        .unwrap_or(start);
    Some(Plan {
        action: Action::Nop,
        cursor: start.min(st.doc.bytes().len()),
        mode,
        is_edit: false,
        effects: vec![Effect::Beep, Effect::RegisterRefused],
        set_register: None,
        set_anchor: None,
        set_mark: None,
    })
}

/// Maintain Vim's `Insstart` column ([`View::insert_col`]) after a command: set at Insert entry; inside
/// Insert a caret jump arms a restart and the next edit moves it to where that edit began (Vim's
/// `stop_arrow`). `CTRL-G U` steps stay inside the run.
fn track_insert_start(
    st: &mut EditorState,
    cmd: &Command,
    entry_mode: Mode,
    entry_cursor: usize,
    entry_rev: crate::Revision,
    entry_col: &dyn Fn() -> usize,
) {
    if !is_text_entry(st.view.mode) {
        st.view.insert_jumped = false;
        st.view.insstart = None;
        st.view.insstart_line = None;
        return;
    }
    if !is_text_entry(entry_mode) {
        let b = st.doc.bytes();
        st.view.insert_col = col_of(b, line_start(b, st.view.cursor), st.view.cursor);
        st.view.insstart = Some(st.view.cursor);
        let ls = line_start(b, st.view.cursor);
        st.view.insstart_line = Some((
            crate::pos::raw::line_of(b, st.view.cursor),
            range::line_cells(b, ls, st.view.indent.tabs()),
        ));
        st.view.insert_jumped = false;
    } else if st.doc.revision() != entry_rev {
        if std::mem::take(&mut st.view.insert_jumped) {
            st.view.insert_col = entry_col();
        }
        // A delete back past the start (`<BS>`, `CTRL-W`, `CTRL-U` with 'backspace' "start") lowers it.
        let cur = st.view.cursor;
        st.view.insstart = Some(st.view.insstart.map_or(cur, |s| s.min(cur)));
    } else if st.view.cursor != entry_cursor && !matches!(cmd, Command::InsertCaretStep { .. }) {
        st.view.insert_jumped = true;
    }
}

/// Maintain [`EditorState::curswant`] after a command (Vim's curswant rule): a vertical move KEEPS the
/// wanted column (ride it through short lines); `$`/`<End>`/`A` (append) set it to [`MAXCOL`] so subsequent
/// `j`/`k` — and an Insert caret — stay at the line's end; a failed command whose nvim handler never sets
/// the column keeps it (`kept_col`, [`keeps_curswant_on_failure`]); every other command recomputes it from
/// the cursor's new char column.
///
/// Insert-mode append column (Vim `i_CTRL-O` at EOL): while `curswant == MAXCOL` in Insert, the caret rests
/// past the line's last char (the append position): a one-shot `CTRL-O` command that leaves the cursor at
/// the line's end PRESERVES that append intent (e.g. `dd`, so `A<C-o>ddX` appends `X` at the end of the line
/// `dd` left behind, and `i<C-o>$X` appends at end), and a cursor resting ON the last char steps past it
/// (nvim's `restart_edit` rule in `edit()`); one landing mid-line (`` A<C-o>`a ``) stays there. Only a
/// command run FROM Insert keeps the intent: `$cb` or `$gi` (Normal to Insert) put the caret where the
/// command did (#702). Called from [`apply_command`], the single plan+commit driver, so `cmd` is always in
/// scope.
fn update_curswant(st: &mut EditorState, cmd: &Command, entry_mode: Mode, kept_col: bool) {
    let insert = matches!(st.view.mode, Mode::Insert);
    let from_insert = insert && matches!(entry_mode, Mode::Insert);
    // Whether the cursor rests on the line's last char or past it.
    let at_line_end = |st: &EditorState| {
        let b = st.doc.bytes();
        let cur = st.view.cursor; // told by the bytes, not the line (#1231)
        motion::at_nul(b, cur) || motion::at_nul(b, motion::next_grapheme(b, cur))
    };
    match cmd {
        // A failed command whose nvim handler never sets `w_set_curswant` (`keeps_curswant_on_failure`).
        _ if kept_col => {}
        Command::Move(_, Motion::Up)
        | Command::Move(_, Motion::Down)
        // `gg`/`G`/`{count}%` move to the wanted column without changing it (`'nostartofline'`).
        | Command::Move(_, Motion::GotoLine | Motion::LastLine | Motion::GotoPercent)
        | Command::MoveUp
        | Command::MoveDown
        // An undo break moves nothing: `i_<Down><Down>` (a break before each move) keeps the column.
        | Command::BreakUndo
        // Starting (or switching) Visual moves nothing: `$v` keeps the end-of-line column, so `$V>` shifts
        // with the cursor at the line's end (nvim's `n_start_visual_mode` leaves `w_curswant` alone, #1176).
        | Command::EnterVisual { .. }
        // A `"x` prefix (or the register a failed `p` spends) moves nothing either (nvim 0.12.6:
        // `/a<CR>:1t$<CR>"add` keeps the search's column).
        | Command::SetRegister(_) => {} // keep the sticky column
        // `$`/`<End>` and `A` (append) want the line's end.
        Command::Move(_, Motion::LineEnd) | Command::MoveLineEnd | Command::AppendLineEnd => {
            st.view.curswant = Want::col(MAXCOL)
        }
        // `{count}|` wants the column it names, not the start of the char it lands on (nvim's `nv_pipe`):
        // `5|` onto a tab or an `<ff>` that covers column 5 keeps 5 for a following `j`/`k`.
        Command::Move(n, Motion::Column) => {
            st.view.curswant = Want::col((*n).max(1) as usize - 1);
        }
        // Commands that establish a definite column: horizontal moves, the Insert-native keys, and every
        // insert-ENTRY except `A` (which is the append/MAXCOL case above) reset the wanted column — they
        // never preserve a stale append intent from an earlier `$`/`A`.
        Command::Move(_, _)
        | Command::MoveLeft
        | Command::MoveRight
        | Command::MoveLineStart
        | Command::InsertChar(_)
        | Command::InsertNewline
        | Command::DeleteBack
        | Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::InsertColumnZero
        | Command::OpenBelow
        | Command::OpenAbove
        | Command::EnterReplace
        | Command::EnterVirtualReplace
        // A `/`/`?` search is a definite motion/edit: it establishes the match column (and, for `c?`,
        // the deletion point where Insert begins). It must never preserve a stale `$`/`A` append intent
        // — `$c?pat` inserts at the match, not at the line end.
        | Command::Search { .. } => st.view.curswant = st.wanted_col(),
        // Everything else (edits like `dd`/`x`, mode changes): a one-shot `CTRL-O` command run while
        // `curswant == MAXCOL` that leaves the cursor at the line's end PRESERVES MAXCOL; otherwise recompute
        // the column.
        _ => {
            if !(from_insert && st.view.curswant == MAXCOL && at_line_end(st)) {
                st.view.curswant = st.wanted_col();
            }
        }
    }
    // In Insert, a MAXCOL wanted column parks a caret on the last char at the append position (end of line).
    if insert && st.view.curswant == MAXCOL && at_line_end(st) {
        let b = st.doc.bytes();
        st.view.cursor = line_end(b, st.view.cursor);
    }
}
