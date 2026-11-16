//! The commit half of the plan / commit pipeline (RFC-0012): [`commit`] applies a `Plan`, and
//! [`apply_command`] plans and commits one command, with the change marks, registers, jumps, Insert state
//! and `curswant` bookkeeping around them. Moved out of `editor/mod.rs` (#1379); a child module, so it
//! reads the private `EditorState` and `View` fields as before.
//!
//! The helpers around the core live in children (#1504): [`placement`] (the `[`/`]` span, the change start
//! and the undo landing), [`classify`] (command predicates), [`insert_state`] (Insert-session bookkeeping)
//! and [`curswant`] (the sticky wanted column).

use super::*;

mod classify;
mod curswant;
mod insert_state;
mod placement;

use classify::{adds_text, is_jump, uses_reg_one};
use curswant::{keeps_curswant_on_failure, update_curswant};
use insert_state::{drop_opened_line_undo, set_insert_state, track_insert_start, Opened};
use placement::{change_start, edit_bracket, undo_step, undo_target};

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

/// Convenience: plan then commit one command, then maintain the sticky desired column (curswant).
pub fn apply_command(st: &mut EditorState, cmd: &Command) -> Vec<Effect> {
    // A command that adds text to the emptied buffer adds it to its one line (RFC-0024 §5).
    if st.makes_the_emptied_line() && adds_text(st, cmd) {
        return st.on_the_emptied_line(|st| apply_command_here(st, cmd));
    }
    apply_command_here(st, cmd)
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
