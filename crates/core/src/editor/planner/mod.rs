// `cindent` and `lisp` are imported for `lines.rs`, which names them as `super::cindent` / `super::lisp`.
use super::{
    cindent, cluster_cells, lisp, range, selection_range, want_col, Action, OpSpan, RegWrite,
};
use crate::motion::{self, line_end, line_start, next_grapheme, prev_grapheme};
use crate::{
    BytePos, CaretGravity, Command, Edit, EditList, EditorState, Effect, GroupHint, MarkOp, Mode,
    Motion, Plan, Register, SelectKind,
};

mod block;
mod comment;
mod edit;
mod emacs;
mod insert;
mod lines;
mod marks;
pub(super) use marks::mark_char_target;
mod operator;
mod put;
mod recase;
mod search;
mod visual;

use block::{block_insert_point, block_op};
use lines::{
    fmt_opts, plan_comment, plan_format_lines, plan_join, plan_reindent, plan_shift,
    plan_shift_times,
};
pub(in crate::editor) use lines::{plan_set_indents, refused_linewise_cursor, RefusedOp};
use recase::{recase, recase_bytes, recase_emacs, recase_span};
pub(crate) use search::{span_lines, word_search};

pub(crate) use comment::open_comment_line_in;
use comment::plan_open_comment;
pub(in crate::editor) use comment::{open_comment_line, open_end_comment};

use put::{paste, plan_paste_selection};

use emacs::plan_emacs;

/// The most bytes one counted repeat of text may produce (`{count}p`, `{count}gr`): 64 MiB is far beyond
/// any interactive intent, and clamping the count keeps a hostile one (a digit run that saturates) from
/// requesting a multi-gigabyte allocation (#683).
const MAX_PUT_BYTES: usize = 1 << 26;

/// The shared `cc`-style linewise-change body over a whole-line span `[ls, content_end)` where
/// `content_end` is the CONTENT end (the trailing newline EXCLUDED). Vim keeps the first line's leading
/// indent, deletes the rest of the content, KEEPS the trailing newline (so exactly one empty indented line
/// remains — the separator to the following line is never eaten), captures the whole lines linewise
/// (indent + trailing newline included), and enters Insert at the end of the kept indent. Shared by
/// `cc`/`S`, the paragraph objects, the linewise inner block ([`plan_change`]), AND visual-linewise change
/// (`V…c` — [`Command::ChangeSelection`] over a linewise selection), so all four agree with nvim.
fn plan_linewise_change(
    b: &[u8],
    ls: usize,
    content_end: usize,
    keep_indent: bool,
    hint: GroupHint,
) -> Plan {
    // Without `'autoindent'` the indent goes too (nvim `op_change`).
    let indent_end = if keep_indent {
        motion::first_non_blank(b, ls).min(content_end)
    } else {
        ls
    };
    // Register span: whole lines including the terminating newline where one is present.
    let reg_end = if content_end < b.len() && b[content_end] == b'\n' {
        content_end + 1
    } else {
        content_end
    };
    let reg = captured(b, ls, reg_end, true);
    if indent_end >= content_end {
        // Nothing after the indent to delete (empty/blank line): keep the buffer, but still capture the
        // register linewise and drop into Insert at the indent end.
        Plan {
            set_register: Some(RegWrite::Edit(reg)),
            ..nop(indent_end, Mode::Insert)
        }
    } else {
        edit_yank(
            one(Edit::delete(BytePos(indent_end), content_end - indent_end)),
            indent_end,
            Mode::Insert,
            hint,
            reg,
        )
    }
}

/// One delete / change / yank / recase over a resolved operator span: the tail every operator path shares (a
/// motion, a forced motion, a mark, a search), so their register, mode and cursor rules agree with nvim
/// (#693, #712, #1077). `landing` is where the motion left the cursor (see [`op_landing`]).
/// - Delete: a linewise delete rests on the line that moves up, at the wanted column (`'nostartofline'`,
///   nvim's `old_col`), or on its first non-blank when the exclusive-linewise rule made the span linewise
///   (`sp.adjusted`). A linewise delete reaching an unterminated last line also takes the newline before it.
/// - Change: a linewise span is `cc` over those lines ([`plan_linewise_change`]); a FAILED linewise motion
///   (`cj` on the last line) stays in Normal.
/// - Yank / recase: a linewise span rests on the earlier of the cursor and the landing, keeping its column
///   (`wyy`, `jwyk`, `wgUj`); a charwise one on the span start.
fn apply_op(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: MarkOp,
    sp: OpSpan,
    landing: usize,
    hint: GroupHint,
) -> Plan {
    let OpSpan {
        s,
        e,
        linewise,
        adjusted,
        failed,
    } = sp;
    let mode = st.view.mode;
    let rest = if linewise { cur.min(landing) } else { s };
    // An empty buffer is nvim's "--No lines in buffer--" (a new or empty file, or every line deleted):
    // `yy` there yanks one empty line (nvim v0.12.6 sets `""` and `"0` to a linewise "\n"), while `dd` has
    // no line to delete and fails, leaving the registers alone (#906).
    if linewise && s >= e && b.is_empty() && op == MarkOp::Yank {
        let mut plan = nop(cur, mode);
        plan.set_register = Some(RegWrite::Yank(Register::linewise(Vec::new()), (s, e)));
        return plan;
    }
    match op {
        MarkOp::Delete | MarkOp::Yank | MarkOp::Case(_) if s >= e => nop(cur, mode),
        MarkOp::Delete if !linewise => {
            let reg = captured(b, s, e, false);
            edit_yank(one(Edit::delete(BytePos(s), e - s)), s, mode, hint, reg)
        }
        MarkOp::Delete => {
            let reg = captured(b, s, e, true);
            // The line that ends up under the cursor, as `(its start before the delete, the bytes removed
            // before it)`: the line below the span, or — when the span runs to an unterminated last line,
            // whose preceding newline goes too — the line above it. A span that runs to the end of a
            // newline-terminated buffer leaves only the empty slot after the final newline below it, which
            // is no line (#677): the cursor takes the line above too, at the wanted column (#1167).
            let (del_start, line, shift) = if e == b.len() && s > 0 && b[e - 1] != b'\n' {
                (s - 1, line_start(b, s - 1), 0)
            } else if e == b.len() && s > 0 {
                (s, line_start(b, s - 1), 0)
            } else {
                (s, e, e - s)
            };
            let at = if adjusted {
                motion::first_non_blank(b, line)
            } else {
                want_col(b, line, st.view.curswant.get(), st.view.indent.tabs())
            };
            edit_yank(
                one(Edit::delete(BytePos(del_start), e - del_start)),
                at - shift,
                mode,
                hint,
                reg,
            )
        }
        MarkOp::Change if linewise && failed => nop(cur, Mode::Normal),
        MarkOp::Change if linewise => {
            let content_end = if e > s && b[e - 1] == b'\n' { e - 1 } else { e };
            plan_linewise_change(b, s, content_end, st.view.indent.autoindent, hint)
        }
        MarkOp::Change if s >= e => nop(s, Mode::Insert),
        MarkOp::Change => {
            // The register captures the removed content charwise (a partial-line change like `c$` pastes
            // inline).
            let reg = captured(b, s, e, false);
            edit_yank(
                one(Edit::delete(BytePos(s), e - s)),
                s,
                Mode::Insert,
                hint,
                reg,
            )
        }
        MarkOp::Yank => Plan {
            set_register: Some(RegWrite::Yank(captured(b, s, e, linewise), (s, e))),
            ..nop(rest, mode)
        },
        // Recase the span; an all-non-letter span is just a cursor move (Vim). Invalid UTF-8 bytes are kept.
        MarkOp::Case(case) => {
            let recased = recase_bytes(&b[s..e], case);
            // The resting point after the edit: its offset inside the span shifts with the recased prefix.
            let at = if rest > s && rest <= e {
                s + recase_bytes(&b[s..rest], case).len()
            } else {
                rest
            };
            if recased == b[s..e] {
                nop(rest, mode)
            } else {
                edit(
                    one(Edit::replace(BytePos(s), e - s, recased)),
                    at,
                    mode,
                    hint,
                )
            }
        }
        MarkOp::Shift { .. } | MarkOp::Reindent | MarkOp::Format { .. } | MarkOp::Comment => {
            unreachable!("the line operators take plan_op_over_span")
        }
    }
}

/// Vi's `d` quirk (nvim `op_delete`) on a plain charwise delete: over several lines, starting in the indent
/// and leaving only blanks after its end, it deletes whole lines (`2d$`, `daw` on an empty line). Objects
/// take [`crate::textobj::delete_reshape`], every other motion
/// [`crate::editor::range::delete_promotes_linewise`]. Forced motions and Visual deletes never take it.
fn delete_quirk(b: &[u8], sp: OpSpan, object: bool) -> OpSpan {
    if sp.linewise {
        return sp;
    }
    let (s, e, linewise) = if object {
        crate::textobj::delete_reshape(b, sp.s, sp.e)
    } else {
        match crate::editor::range::delete_promotes_linewise(b, sp.s, sp.e) {
            Some((ls, le)) => (ls, le, true),
            None => (sp.s, sp.e, false),
        }
    };
    OpSpan {
        s,
        e,
        linewise,
        ..sp
    }
}

/// Apply a span operator over `[lo, hi)` — the cursor..mark span of [`Command::OpToMark`], and the case /
/// shift / reindent / format forms of a search ([`SearchOp::span_op`]) and of `gn` (#800). `linewise` widens
/// the span to the whole lines of `lo` and `hi` (the `'` mark form, a line-offset search); otherwise it is
/// charwise-EXCLUSIVE, so for the line operators an end resting at column 0 of the far line does NOT pull that
/// line in (Vim's exclusive-motion rule — the same reduction `ShiftMotion`/`Reindent` apply to a motion).
/// `end_adjusted` says the exclusive-linewise rule moved the span's end (nvim's `oap->end_adjusted`): `gq`
/// then leaves the cursor on the line BELOW the formatted block (#1071). `landing` is where the motion left
/// the cursor ([`apply_op`]).
#[allow(clippy::too_many_arguments)]
fn plan_op_over_span(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: MarkOp,
    lo: usize,
    hi: usize,
    linewise: bool,
    end_adjusted: bool,
    landing: usize,
    hint: GroupHint,
) -> Plan {
    // Shift (`>`), reindent (`=`) and format (`gq`/`gw`) operate on whole LINES regardless of the charwise /
    // linewise form (Vim: they are linewise even over a charwise motion), so dispatch them on the touched line
    // range before the span split the others share. They reuse the SAME planners as over a motion.
    if matches!(
        op,
        MarkOp::Shift { .. } | MarkOp::Reindent | MarkOp::Format { .. } | MarkOp::Comment
    ) {
        let (first, last) = if linewise {
            (
                crate::pos::raw::line_of(b, lo),
                crate::pos::raw::line_of(b, hi),
            )
        } else if lo >= hi {
            return nop(cur, st.view.mode);
        } else {
            (
                crate::pos::raw::line_of(b, lo),
                crate::pos::raw::line_of(b, hi - 1),
            )
        };
        return match op {
            MarkOp::Shift { left } => {
                let lines = (last - first + 1) as u32;
                plan_shift(st, line_start(b, lo), lines, !left, hint)
            }
            MarkOp::Reindent => plan_reindent(st, first, last, hint),
            MarkOp::Format { keep } => {
                let start = crate::pos::raw::nth_line_start(b, first);
                let end = line_end(b, crate::pos::raw::nth_line_start(b, last));
                plan_format_lines(st, cur, start, end, keep, end_adjusted, hint)
            }
            MarkOp::Comment => plan_comment(st, first, last, cur.min(lo), hint),
            _ => unreachable!("guarded by matches! above"),
        };
    }
    let sp = if linewise {
        let le = line_end(b, hi);
        OpSpan {
            s: line_start(b, lo),
            e: if le < b.len() { le + 1 } else { le },
            linewise: true,
            adjusted: end_adjusted,
            failed: false,
        }
    } else {
        OpSpan {
            s: lo,
            e: hi,
            linewise: false,
            adjusted: end_adjusted,
            failed: false,
        }
    };
    apply_op(st, b, cur, op, sp, landing, hint)
}

/// A Vim-profile motion that fails ([`motion::fails_with`]), bare in Normal or Visual or under an operator: the
/// command does nothing and beeps, so the frontend flushes the keys that were not typed (#1085). The
/// operator is cancelled whole (`dfz`, `cj` on the last line stays in Normal).
fn failed_motion(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    cmd: &Command,
    memo: &std::cell::OnceCell<usize>,
) -> Option<Plan> {
    if st.view.caret != CaretGravity::OnChar
        || !matches!(st.view.mode, Mode::Normal | Mode::Visual { .. })
    {
        return None;
    }
    let (count, m, op) = match *cmd {
        Command::Move(count, m) => (count, m, false),
        Command::Delete(count, m) | Command::Change(count, m) | Command::Yank(count, m) => {
            (count, m, true)
        }
        Command::OpForced { count, motion, .. }
        | Command::CaseMotion { count, motion, .. }
        | Command::ShiftMotion { count, motion, .. }
        | Command::Reindent { count, motion }
        | Command::Format { count, motion, .. }
        | Command::Comment { count, motion } => (count, motion, true),
        _ => return None,
    };
    let visual = st.view.mode.selection().is_some();
    if op && visual {
        return None;
    }
    // A failed word object leaves the cursor where Vim's scan stopped (`daw` on blanks: their end). In
    // Visual the anchor stays, so the selection reaches there (`vaw` on a blank line, #1111).
    if op || visual {
        let anchor = visual.then(|| st.view.anchor.unwrap_or(cur));
        if let Some(at) = crate::textobj::word_object_fails(b, cur, anchor, m, count) {
            let at = motion::off_phantom_at(b, at);
            let (ls, le) = (line_start(b, at), line_end(b, at));
            let at = if at >= le && le > ls {
                prev_grapheme(b, le)
            } else {
                at
            };
            return Some(beep(at, st.view.mode));
        }
    }
    // A block, quote or sentence object with nothing to select fails under an operator (nvim's `nv_object`
    // → `clearopbeep`), so `di(` without a `(` is no change for `.` and stops a macro (#1179).
    if op && crate::textobj::object_fails(b, cur, m, count) {
        return Some(beep(cur, st.view.mode));
    }
    // A bare `{count}e` that runs out still moves as far as it can (handled in the `Move` arm).
    if !op && matches!(m, Motion::WordEnd | Motion::BigWordEnd) {
        return None;
    }
    motion::fails_with(b, cur, m, count, op, visual, memo).then(|| beep(cur, st.view.mode))
}

/// A failed command: no change, the cursor at `cursor`, and a beep ([`Effect::Beep`], #1085).
fn beep(cursor: usize, mode: Mode) -> Plan {
    Plan {
        effects: vec![Effect::Beep],
        ..nop(cursor, mode)
    }
}

fn nop(cursor: usize, mode: Mode) -> Plan {
    Plan {
        action: Action::Nop,
        cursor,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

fn edit(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// An `edit` that also captures the removed span into a register (`d`/`c`/`x` fill the unnamed slot).
fn edit_yank(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: Some(RegWrite::Edit(reg)),
        set_anchor: None,
        set_mark: None,
    }
}

fn one(e: Edit) -> EditList {
    EditList::new(vec![e]).expect("single edit is always valid")
}

/// Like [`edit_yank`] but routes the captured span through [`RegWrite::KillAppend`] — an Emacs kill that
/// accumulates onto the current unnamed entry when it follows another kill (kill-ring behaviour).
/// Select's `open/replace-selection` over a charwise or linewise selection, shared by a typed key
/// ([`Command::ReplaceSelection`]) and a paste ([`Command::InsertPaste`] in a selection): delete the
/// selection into the unnamed register, insert `ins` where it began, and enter Insert after it.
fn replace_selection_with(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    ins: Vec<u8>,
) -> Plan {
    let line = matches!(
        st.view.mode,
        Mode::Visual {
            kind: SelectKind::Linewise
        } | Mode::Select {
            kind: SelectKind::Linewise
        }
    );
    let n = ins.len();
    match st.view.anchor {
        Some(anchor) => {
            let (s, e) = selection_range(b, anchor, cur, line);
            // The last line's newline is never part of it (nvim: `gHX` on the last line leaves `"def"`).
            let e = e.min(crate::motion::content_end(b)).max(s);
            if s < e {
                // The removed span fills the unnamed register, as a Visual/Normal delete does —
                // charwise even for whole lines, as nvim stores it (`gHX` leaves `"abc\n"`, `v`).
                let reg = captured(b, s, e, false);
                edit_yank(
                    one(Edit::replace(BytePos(s), e - s, ins)),
                    s + n,
                    Mode::Insert,
                    hint,
                    reg,
                )
            } else {
                edit(
                    one(Edit::insert(BytePos(s), ins)),
                    s + n,
                    Mode::Insert,
                    hint,
                )
            }
        }
        // No anchor (not really in a selection): degrade to a plain insert-and-enter-Insert.
        None => edit(
            one(Edit::insert(BytePos(cur), ins)),
            cur + n,
            Mode::Insert,
            hint,
        ),
    }
}

/// Capture `b[s..e]` as a register value with the given paste geometry.
fn captured(b: &[u8], s: usize, e: usize, linewise: bool) -> Register {
    let bytes = b[s..e].to_vec();
    if linewise {
        Register::linewise(bytes)
    } else {
        Register::charwise(bytes)
    }
}

/// Whether `byte` is horizontal whitespace (a space or tab) — never a newline. The shared predicate for
/// the whitespace-fixup commands (`J`, `just-one-space`, `delete-horizontal-space`, `delete-indentation`).
fn is_hspace(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// The first index `>= from` that is not horizontal whitespace (scan right).
fn hspace_end(b: &[u8], from: usize) -> usize {
    let mut e = from;
    while e < b.len() && is_hspace(b[e]) {
        e += 1;
    }
    e
}

/// The apostrophe-mark (`'{a-z}` / `'.`) target for a mark at `pos`: the first non-blank of that position's
/// LINE (Vim's linewise mark jump, `beginline(BL_WHITE | BL_FIX)`), clamped into range. On an all-blank
/// line that is the last blank, never the newline past it (#728).
fn mark_line_target(b: &[u8], pos: usize) -> usize {
    motion::first_non_blank_fix(b, pos.min(b.len()))
}

/// A caret key that takes an Insert cursor off a line it opened with an untouched autoindent (or comment
/// leader) removes that line's trailing blanks first (nvim's `start_arrow` → `stop_insert` with `did_ai`,
/// #617): `o<Up>` leaves an empty line, not one holding the indent.
fn leave_autoindent(st: &EditorState, cur: usize, p: Plan, hint: GroupHint) -> Plan {
    let b = st.bytes();
    if st.view.mode != Mode::Insert
        || !st.view.auto_indent_pending
        || st.view.caret != CaretGravity::OnChar
        || p.is_edit
        || !matches!(p.action, Action::Nop)
        || p.mode != Mode::Insert
        || line_start(b, p.cursor) == line_start(b, cur)
    {
        return p;
    }
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let mut cut = le;
    while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
        cut -= 1;
    }
    if cut == le || cur != le {
        return p;
    }
    let cursor = if p.cursor >= le {
        p.cursor - (le - cut)
    } else {
        p.cursor
    };
    edit(
        one(Edit::delete(BytePos(cut), le - cut)),
        cursor,
        Mode::Insert,
        hint,
    )
}

pub fn plan(st: &EditorState, cmd: &Command) -> Plan {
    let hint = if st.view.last_was_edit {
        GroupHint::Continue
    } else {
        GroupHint::BreakBefore
    };
    let resolved = display_columns(st, cmd);
    let p = plan_cmd(st, resolved.as_ref().unwrap_or(cmd));
    leave_autoindent(st, st.view.cursor, p, hint)
}

/// `cmd` with a `{count}|` or `{count}gM` motion resolved in DISPLAY columns (nvim's `nv_pipe` / `nv_g_cmd`
/// `gM`: a `<Tab>` spans to its tabstop, a wide char two cells and a byte that is not UTF-8 its four-cell
/// `<ff>`, #1251), or `None` when it has neither. The motion layer counts clusters and knows no tabstop, so
/// the landing is found here and handed back as the `|` count that steps that many clusters onto it: `|`
/// lands on the char covering the column (the last char past the line end), `gM` on the one at `count`
/// percent of the line's cells.
fn display_columns(st: &EditorState, cmd: &Command) -> Option<Command> {
    let (count, m) = match cmd {
        Command::Move(n, m)
        | Command::Delete(n, m)
        | Command::Change(n, m)
        | Command::Yank(n, m) => (*n, *m),
        Command::OpForced { count, motion, .. } | Command::CaseMotion { count, motion, .. } => {
            (*count, *motion)
        }
        _ => return None,
    };
    if !matches!(m, Motion::Column | Motion::MidLine) {
        return None;
    }
    let b = st.bytes();
    let ts = st.view.indent.tabs();
    let ls = line_start(b, st.view.cursor);
    let n = count.max(1) as usize;
    let vcol = match m {
        Motion::Column => n - 1,
        // nvim takes a count over 100 as a bare `gM`.
        _ if n > 100 => range::line_cells(b, ls, ts) / 2,
        _ => range::line_cells(b, ls, ts).saturating_mul(n) / 100,
    };
    // The char covering `vcol`, or `None` past the line's last cell. Only the cells up to it are read, so
    // `5000|` on a huge line costs 5000 cells.
    // The line's end is found by the walk, not scanned for first: a huge line costs the cells crossed.
    let (mut i, mut v, mut last) = (ls, 0, ls);
    let mut hit = None;
    while i < b.len() && b[i] != b'\n' {
        // A cluster never crosses a line break, but `\r\n` would be one: bound a CR to itself.
        let bound = if b[i] == b'\r' { i + 1 } else { b.len() };
        let (j, w) = cluster_cells(b, i, bound, v, ts);
        if vcol < v + w {
            hit = Some(i);
            break;
        }
        last = i;
        i = j;
        v += w;
    }
    // Past the end Normal stops on the last char; Visual may go one past it (nvim's `coladvance`
    // `one_more`): `v10|` on `abc` takes the line break.
    let at = match hit {
        Some(at) => at,
        None if st.view.mode.selection().is_some() => i,
        None => last,
    };
    let mut steps = 0usize;
    let mut i = ls;
    while i < at {
        i = motion::next_boundary(b, i);
        steps += 1;
    }
    let col = u32::try_from(steps + 1).unwrap_or(u32::MAX);
    let mut out = cmd.clone();
    match &mut out {
        Command::Move(n, m)
        | Command::Delete(n, m)
        | Command::Change(n, m)
        | Command::Yank(n, m) => {
            *n = col;
            *m = Motion::Column;
        }
        Command::OpForced { count, motion, .. } | Command::CaseMotion { count, motion, .. } => {
            *count = col;
            *motion = Motion::Column;
        }
        _ => {}
    }
    Some(out)
}

fn plan_cmd(st: &EditorState, cmd: &Command) -> Plan {
    let b = st.bytes();
    let cur = st.view.cursor;
    let hint = if st.view.last_was_edit {
        GroupHint::Continue
    } else {
        GroupHint::BreakBefore
    };
    // The bare motion's target, shared by the failure check and the `Move` arm (#1231).
    let memo = std::cell::OnceCell::new();
    if let Some(p) = failed_motion(st, b, cur, cmd, &memo) {
        return p;
    }
    if let Some(p) = plan_open_comment(st, b, cur, cmd, hint) {
        return p;
    }

    match cmd {
        Command::MoveLeft
        | Command::MoveRight
        | Command::MoveLineStart
        | Command::MoveLineEnd
        | Command::MoveUp
        | Command::MoveDown => plan_cursor_move(st, b, cur, cmd),
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
        | Command::ReplaceType(..)
        | Command::ReplaceBackspace
        | Command::EnterVirtualReplace
        | Command::VirtualReplaceType(..)
        | Command::VirtualReplaceChar(..)
        | Command::InsertChar(..)
        | Command::InsertRegister(..)
        | Command::InsertEval(..)
        | Command::InsertPaste(..)
        | Command::ReplacePaste(..)
        | Command::EmacsPaste(..)
        | Command::InsertDeleteWordBack
        | Command::InsertDeleteToLineStart
        | Command::CompleteWord { .. }
        | Command::InsertIndent
        | Command::InsertDedent
        | Command::InsertDeleteIndent
        | Command::InsertDeleteIndentRestore
        | Command::InsertStartColumnLine { .. }
        | Command::InsertCaretStep { .. }
        | Command::InsertTab
        | Command::InsertNewline
        | Command::OpenLineIndent { .. }
        | Command::InsertCloser { .. } => insert::plan(st, b, cur, hint, cmd),
        Command::DeleteBack
        | Command::DeleteUnder(..)
        | Command::DeleteForward(..)
        | Command::ReplaceChar(..)
        | Command::ToggleCase(..)
        | Command::JoinLines(..)
        | Command::JoinLinesNoSpace(..)
        | Command::IncrementNumber(..)
        | Command::IncrementSelection { .. } => edit::plan(st, b, cur, hint, cmd),
        Command::GotoLastChange
        | Command::GotoLastChangeLine
        | Command::GotoChangeMarkStart
        | Command::GotoChangeMarkEnd
        | Command::GotoChangeMarkStartLine
        | Command::GotoChangeMarkEndLine
        | Command::GotoVisualMarkStart
        | Command::GotoVisualMarkEnd
        | Command::GotoVisualMarkStartLine
        | Command::GotoVisualMarkEndLine
        | Command::GotoContextMark
        | Command::GotoContextMarkLine
        | Command::GotoOlderChange
        | Command::GotoNewerChange
        | Command::GotoOlderJump
        | Command::GotoNewerJump
        | Command::SetNamedMark(..)
        | Command::GotoNamedMark(..)
        | Command::GotoNamedMarkLine(..)
        | Command::OpToMark { .. } => marks::plan(st, b, cur, hint, cmd),
        Command::Move(..)
        | Command::Delete(..)
        | Command::Change(..)
        | Command::Yank(..)
        | Command::CaseMotion { .. }
        | Command::OpForced { .. }
        | Command::ShiftRight(..)
        | Command::ShiftLeft(..)
        | Command::ShiftMotion { .. }
        | Command::Reindent { .. }
        | Command::Format { .. }
        | Command::Comment { .. }
        | Command::SetIndents { .. }
        | Command::SetIndentCols { .. }
        | Command::InsertReindent => operator::plan(st, b, cur, hint, &memo, cmd),
        Command::Paste { .. }
        | Command::PasteBlockInner { .. }
        | Command::PasteIndent { .. }
        | Command::SetRegister(..)
        | Command::SetExprRegister(..) => put::plan(st, b, cur, cmd),
        Command::EnterVisual { .. }
        | Command::EnterSelect { .. }
        | Command::ReselectVisual
        | Command::ReplaceSelection(..)
        | Command::BlockInsert(..)
        | Command::SwapSelectionEnds
        | Command::YankSelection
        | Command::DeleteSelection
        | Command::ChangeSelection
        | Command::VisualOp(..)
        | Command::CaseSelection(..)
        | Command::ReplaceSelectionChar(..)
        | Command::PasteSelection { .. } => visual::plan(st, b, cur, hint, cmd),
        Command::SearchNext(..)
        | Command::SearchPrev(..)
        | Command::Search { .. }
        | Command::SearchObject { .. }
        | Command::SearchWordUnder { .. }
        | Command::GotoDeclaration { .. }
        | Command::ShowKeywordLines { .. }
        | Command::GotoFirstMatch(..) => search::plan(st, b, cur, hint, cmd),
        Command::BreakUndo
        | Command::AsciiInfo
        | Command::Utf8Info
        | Command::FileInfo
        | Command::CursorInfo
        | Command::RepeatSubstituteGlobal
        | Command::RepeatSubstituteLine
        | Command::Ex(_)
        | Command::Save
        | Command::Quit => plan_session(st, cur, cmd),
        Command::Undo
        | Command::Redo
        | Command::UndoOlder
        | Command::UndoNewer
        | Command::LineUndo => plan_undo(st, cur, cmd),
        // Emacs-profile commands: the depth-1 region ops (D-027) and the distinct Emacs commands
        // (D-051), grouped into `plan_emacs` to keep this match readable.
        Command::SetMark
        | Command::ExchangePointMark
        | Command::CopyRegion
        | Command::KillRegion
        | Command::EmacsYank { .. }
        | Command::EmacsKillLine
        | Command::EmacsKillWord { .. }
        | Command::EmacsBackwardKillWord { .. }
        | Command::EmacsTransposeChars
        | Command::EmacsTransposeWords
        | Command::EmacsCaseWord { .. }
        | Command::EmacsHorizontalSpace { .. }
        | Command::EmacsOpenLine
        | Command::EmacsMarkWord
        | Command::EmacsKillWholeLine
        | Command::EmacsCaseRegion { .. }
        | Command::EmacsDeleteIndentation
        | Command::EmacsBufferEdge { .. } => plan_emacs(st, b, cur, hint, cmd),
    }
}

/// The bare cursor motions `h` `l` `0` `$` `k` `j`.
fn plan_cursor_move(st: &EditorState, b: &[u8], cur: usize, cmd: &Command) -> Plan {
    match cmd {
        // `h`/`l` move by whole grapheme cluster so the cursor never lands mid-emoji/ZWJ/combining
        // (F-002 #2). Operator/selection internals still step by char boundary — grapheme-aware
        // deletion is a follow-up; this slice makes the CURSOR stay synced to user-perceived chars.
        Command::MoveLeft => nop(prev_grapheme(b, cur), st.view.mode),
        Command::MoveRight => nop(next_grapheme(b, cur), st.view.mode),
        Command::MoveLineStart => nop(line_start(b, cur), st.view.mode),
        Command::MoveLineEnd => nop(line_end(b, cur), st.view.mode),
        Command::MoveUp | Command::MoveDown => nop(
            motion::vmove(
                b,
                cur,
                1,
                matches!(cmd, Command::MoveDown),
                st.view.curswant.get(),
                st.view.indent.tabs(),
            ),
            st.view.mode,
        ),
        _ => unreachable!("plan_cursor_move plans only the bare cursor motions"),
    }
}

/// Commands the pure core does not edit for: they keep cursor and mode, and at most hand the
/// frontend an effect.
fn plan_session(st: &EditorState, cur: usize, cmd: &Command) -> Plan {
    match cmd {
        // `CTRL-G u`: break the undo group. A pure nop (is_edit = false), so `commit` sets
        // `last_was_edit = false` and the NEXT edit's `GroupHint` becomes `BreakBefore` — a fresh undo
        // group starts here mid-insert-session (Vim `i_CTRL-G_u`).
        // `ga`/`:ascii`, `CTRL-G`, `g CTRL-G` — INFO commands resolved by the frontend (it reads the
        // buffer bytes + cursor and sets the status line).
        // `g&` — resolved in the frontend against its last-substitute state.
        // `&` — resolved in the frontend (current-line, flagless repeat).
        // A resolved Ex edit never reaches the planner: `apply_command` runs it through
        // `EditorState::apply_ex` (RFC-0020). Planned alone it changes nothing.
        Command::BreakUndo
        | Command::AsciiInfo
        | Command::Utf8Info
        | Command::FileInfo
        | Command::CursorInfo
        | Command::RepeatSubstituteGlobal
        | Command::RepeatSubstituteLine
        | Command::Ex(_) => nop(cur, st.view.mode),
        Command::Save => Plan {
            effects: vec![Effect::Save],
            ..nop(cur, st.view.mode)
        },
        Command::Quit => Plan {
            effects: vec![Effect::Quit],
            ..nop(cur, st.view.mode)
        },
        _ => unreachable!("plan_session plans only the frontend-resolved commands"),
    }
}

/// `u`, `CTRL-R`, `g-`, `g+` and `U`.
fn plan_undo(st: &EditorState, cur: usize, cmd: &Command) -> Plan {
    // Undo/redo restore DOCUMENT state, not mode. The engine only reaches them from Normal or from
    // Insert via `i_CTRL-O` (Vim's `u` in Visual is not an undo), so preserving Insert makes
    // `i_CTRL-O u` return to Insert — the one-shot's resume (KL-OBL-5) — while everything else
    // collapses to Normal exactly as before.
    let undo_resume_mode = if st.view.mode == Mode::Insert {
        Mode::Insert
    } else {
        Mode::Normal
    };
    match cmd {
        // Nothing to undo or redo: Vim's `u_doit` beeps ("Already at oldest change"), which stops a macro
        // replay or a `:normal` (#1111).
        Command::Undo if !st.doc.can_undo() => beep(cur, st.view.mode),
        Command::Redo if !st.doc.can_redo() => beep(cur, st.view.mode),
        Command::Undo => Plan {
            action: Action::Undo,
            ..nop(cur, undo_resume_mode)
        },
        Command::Redo => Plan {
            action: Action::Redo,
            ..nop(cur, undo_resume_mode)
        },
        Command::UndoOlder => Plan {
            action: Action::UndoChrono { older: true },
            ..nop(cur, undo_resume_mode)
        },
        // `U` — a change in its own right (`u` undoes it), so it is an edit; nothing to restore is a no-op.
        Command::LineUndo if st.doc.can_line_undo() => Plan {
            action: Action::LineUndo,
            is_edit: true,
            ..nop(cur, Mode::Normal)
        },
        Command::LineUndo => nop(cur, st.view.mode),
        Command::UndoNewer => Plan {
            action: Action::UndoChrono { older: false },
            ..nop(cur, undo_resume_mode)
        },
        _ => unreachable!("plan_undo plans only the undo commands"),
    }
}

use super::indent::indent_cols;
