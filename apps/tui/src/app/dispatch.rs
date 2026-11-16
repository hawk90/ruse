//! Frontend command/ex dispatch: run a `Command` (recording it + performing its `Effect`s), execute a
//! parsed `:` line, and the save/label helpers they call. These are thin callers over `Workspace`,
//! `persist::`, and the status/quit sinks — no event-loop or terminal state.

mod files;
mod labels;
mod open;
mod quit;
mod save;

pub(crate) use files::{BufferFile, Files, ReadOnly, E45};
pub(crate) use labels::{
    buffer_list_line, focused_buffer_label, invalid_command_msg, match_count_message,
    regex_error_msg, substituted_message,
};
pub(crate) use open::{
    apply_recovery, attach_file, buffer_for_path, mark_large, open_file_into_buffer, open_file_with,
};
pub(crate) use quit::{exit_status, may_quit, quit_window, set_exit_status};
pub(crate) use save::{disk_image, save};

use quit::no_write_since_last_change;

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{
    BytePos, Command, DocumentId, Effect, ExCmd, ExOutcome, Register, SubRange, Workspace,
};

use crate::app::messages::Status;
use crate::highlight;

/// The per-buffer syntax-highlighter registry (F-007 / F-015), keyed by [`DocumentId`]. Shared by the
/// session loop and the LSP coordinator (both open buffers that need a highlighter).
pub(crate) type Highlighters = HashMap<DocumentId, highlight::CachedHighlight>;

/// Whether `key` is `CTRL-<c>`. Shared frontend key predicate. A C0 punctuation chord also matches the key
/// a legacy terminal delivers for it: crossterm decodes `CTRL-\ CTRL-] CTRL-^ CTRL-_` (0x1c..0x1f) as
/// `CTRL-4`..`CTRL-7` and `CTRL-@` (NUL) as `CTRL-Space` (#603, #1069).
pub(crate) fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && (key.code == KeyCode::Char(c)
            || crate::keys::legacy_ctrl_alias(c).is_some_and(|a| key.code == KeyCode::Char(a)))
}

/// `gq` under `'formatprg'` and `=` under `'equalprg'` (#617, #618): instead of formatting or reindenting,
/// filter the operator's lines through that shell command, as nvim's `op_colon` runs `:{range}!{prg}`.
/// After `gq` the cursor goes to the first non-blank of the last filtered line (nvim's `']`), after `=` to
/// that of the first. `gw` never uses `'formatprg'`. A search or mark target, and a blockwise selection,
/// keep the internal operator. Returns whether the command was handled here.
pub(crate) fn run_prg_operator(
    cmd: &Command,
    ws: &mut Workspace,
    buffers: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
) -> bool {
    use ruse_core::VisualOp;
    let view = ws.focused().view;
    let set = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let (fp_value, ep_value) = (set(view.formatprg()), set(view.equalprg()));
    let fp = || fp_value.clone();
    let ep = || ep_value.clone();
    let visual_lines = |ws: &Workspace| {
        let pane = ws.focused();
        let b = pane.doc.bytes();
        let (BytePos(s), BytePos(e)) = pane.view.selection_span(b)?;
        Some((
            ruse_core::pos::line_of(b, ruse_core::BytePos(s)),
            ruse_core::pos::line_of(b, ruse_core::BytePos(e.saturating_sub(1).max(s))),
        ))
    };
    let (prg, range, to_last, visual) = match cmd {
        Command::Format {
            count,
            motion,
            keep_cursor: false,
        } => match fp() {
            Some(p) => (p, ws.reindent_range(*motion, *count), true, false),
            None => return false,
        },
        Command::Reindent { count, motion } => match ep() {
            Some(p) => (p, ws.reindent_range(*motion, *count), false, false),
            None => return false,
        },
        Command::VisualOp(VisualOp::Format) => match fp() {
            Some(p) => (p, visual_lines(ws), true, true),
            None => return false,
        },
        Command::VisualOp(VisualOp::Reindent) => match ep() {
            Some(p) => (p, visual_lines(ws), false, true),
            None => return false,
        },
        _ => return false,
    };
    let Some((first, last)) = range else {
        return false;
    };
    if visual {
        // The selection ends first, as the `:'<,'>!` line nvim runs does.
        let files = Files::new();
        let mut quit = false;
        run_cmd(
            Command::EnterNormal,
            ws,
            &files,
            recorded,
            status,
            &mut quit,
        );
    }
    let range = SubRange::Lines(first + 1, last + 1);
    let Some(input) = ws.range_text(range) else {
        return true;
    };
    // A range that reaches the last line goes out as the file ends it, as `:{range}!` does (RFC-0024 §3).
    let input = if crate::app::file_cmds::reaches_last_line(ws, range) {
        let fmt = crate::app::file_cmds::buffer_format(buffers, ws.focused_buffer());
        buffers.write.lines_out(&input, fmt).to_vec()
    } else {
        input
    };
    match crate::shell::filter(&prg, &input) {
        Ok(out) => {
            let ex = ExCmd::FilterReplace {
                first: first + 1,
                last: last + 1,
                bytes: out.raw.clone(),
            };
            run_ex(ex, ws, recorded);
            if to_last {
                let n = out.raw.split(|&c| c == b'\n').count()
                    - usize::from(out.raw.last() == Some(&b'\n'));
                let b = ws.focused().doc.bytes();
                let line = (first + n.max(1) - 1).min(ruse_core::pos::count_newlines(b));
                let ls = ruse_core::pos::nth_line_start(b, line).0;
                let at = ls
                    + b[ls..]
                        .iter()
                        .take_while(|&&c| c == b' ' || c == b'\t')
                        .count();
                place_cursor(ws, recorded, at);
            }
            if let Some(note) = out.failure() {
                status.echo(note);
            }
        }
        Err(e) => status.error(e.to_string()),
    }
    true
}

/// Record a command and apply it to the focused window, performing any effects.
/// [`run_cmd`] for a Normal or Visual command, typed or repeated: its line-count report follows
/// (`3 fewer lines`, `3 lines yanked`, #1276) unless it failed or gave a message of its own.
pub(crate) fn run_cmd_reported(
    cmd: Command,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
    report: i64,
) {
    let pending = crate::app::report::NormalReport::before(&cmd, ws);
    let (serial, beeped_before) = (status.serial(), ws.take_beep());
    run_cmd(cmd, ws, files, recorded, status, quit);
    let beeped = ws.take_beep();
    if beeped_before || beeped {
        ws.beep();
    }
    if let Some(p) = pending.filter(|_| !beeped && status.serial() == serial) {
        p.give(ws, status, report);
    }
}

pub(crate) fn run_cmd(
    cmd: Command,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
) {
    if let Some(err) = put_error(&cmd, ws) {
        // nvim checks 'modifiable' before the register (#619).
        let err = if ws.focused().doc.is_modifiable() {
            err
        } else {
            ruse_core::E21.to_string()
        };
        // Nothing runs; a `"x` the command would have read is spent, as in Vim. Recorded, so a replay does
        // not hand that register to the next command.
        status.error(err);
        recorded.push(Command::SetRegister(None));
        ws.apply(&Command::SetRegister(None));
        return;
    }
    if let Some(err) = mark_error(&cmd, ws) {
        // The core fails the jump itself (a beep, nothing moved or recorded on the jumplist); it runs so the
        // wanted column resets as nvim's `nv_mark_move_to` does (#1171), and a `"x` is spent.
        status.error(err);
        recorded.push(cmd.clone());
        ws.apply(&cmd);
        return;
    }
    let at = recorded.len();
    recorded.push(cmd.clone());
    let effects = ws.apply(&cmd);
    note_clipboard_read(ws, recorded, at);
    for eff in effects {
        apply_effect(eff, ws, files, status, quit);
    }
}

/// A clipboard pull the command at `recorded[at]` made (`"+p`, `:put +`, `CTRL-R +`) is recorded ahead of
/// it as the value read (RFC-0020 R6): the OS clipboard is external input, so a replay puts the same text.
fn note_clipboard_read(ws: &mut Workspace, recorded: &mut Vec<Command>, at: usize) {
    if let Some(value) = ws.take_clipboard_read() {
        let record = Command::Ex(ExCmd::Register {
            name: Some('+'),
            value,
        });
        recorded.insert(at.min(recorded.len()), record);
    }
}

/// Write register `name` (`None` = unnamed) through the trace (RFC-0020 R6): the session writes a value no
/// recorded command produced, so it is recorded as an `ex_register` and applied by the same core path a
/// replay runs. Nothing is recorded when the register already holds exactly `value`.
pub(crate) fn write_register(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    name: Option<char>,
    value: Register,
) {
    if *ws.registers().get(name) != value {
        run_ex(ExCmd::Register { name, value }, ws, recorded);
    }
}

/// Record the registers `write` changed (RFC-0020 R6): for a session write whose effect on the slots is the
/// store's own rule (a macro's raw keys, `set_register_raw`), each slot that now holds another value is
/// written again through the trace, so a replay sets the same slots.
pub(crate) fn record_register_writes(
    ws: &mut Workspace,
    recorded: &mut Vec<Command>,
    write: impl FnOnce(&mut Workspace),
) {
    let before = ws.registers().filled();
    write(ws);
    for (name, value) in ws.registers().filled() {
        if !before.iter().any(|(n, v)| *n == name && *v == value) {
            run_ex(ExCmd::Register { name, value }, ws, recorded);
        }
    }
}

/// Record a resolved Ex edit and run it on the focused window (RFC-0020, D-061). Every Ex edit goes through
/// here, so none can reach the document without reaching the trace.
pub(crate) fn run_ex(ex: ExCmd, ws: &mut Workspace, recorded: &mut Vec<Command>) -> ExOutcome {
    let out = ws.apply_ex(&ex);
    let at = recorded.len();
    recorded.push(Command::Ex(ex));
    note_clipboard_read(ws, recorded, at);
    out
}

/// Place the focused cursor at `at` for an Ex command (`:normal`'s per-line column 0, a `:g` landing line),
/// recorded so replay puts it in the same place.
pub(crate) fn place_cursor(ws: &mut Workspace, recorded: &mut Vec<Command>, at: usize) {
    run_ex(ExCmd::Place(BytePos(at)), ws, recorded);
}

/// Record a trace marker that changes nothing live by itself (`ex_begin`/`ex_end`, an undo sync the caller
/// performs).
pub(crate) fn note_ex(ex: ExCmd, recorded: &mut Vec<Command>) {
    recorded.push(Command::Ex(ex));
}

/// Apply a counted [`crate::input::Repeat`] one command at a time, as [`run_cmd`] runs each (#683). CTRL-C
/// ([`ruse_core::interrupt`]) stops it between repetitions; the tail (the `<Esc>` that leaves Insert) runs
/// anyway, so an interrupted `999999999ix<Esc>` still ends in Normal mode, as in nvim.
pub(crate) fn run_repeat(
    r: crate::input::Repeat,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut Status,
    quit: &mut bool,
    report: i64,
) {
    for cmd in r.head {
        run_cmd_reported(cmd, ws, files, recorded, status, quit, report);
    }
    let literal = literal_text(&r.unit);
    let mut done: u32 = 0;
    while done < r.times {
        if *quit || ruse_core::interrupt::breakcheck() {
            break;
        }
        match &literal {
            // After one repetition run as typed, the rest of a plain-text unit go in as growing literal
            // splices (#1123): each edit copies the buffer, so one edit per character made
            // `1000000ix<Esc>` quadratic. Doubling the chunk keeps the total copy linear in the result up
            // to the cap, which bounds the memory of one chunk and the time between interrupt checks.
            Some(text) if done > 0 && splice_is_typing(ws) => {
                let per = (REPEAT_CHUNK_BYTES / text.len()).max(1);
                let reps = (done as usize).min(per).min((r.times - done) as usize);
                run_cmd(
                    Command::InsertPaste(text.repeat(reps)),
                    ws,
                    files,
                    recorded,
                    status,
                    quit,
                );
                done += u32::try_from(reps).unwrap_or(u32::MAX);
            }
            _ => {
                for cmd in &r.unit {
                    run_cmd_reported(cmd.clone(), ws, files, recorded, status, quit, report);
                }
                done += 1;
            }
        }
    }
    for cmd in r.tail {
        run_cmd_reported(cmd, ws, files, recorded, status, quit, report);
    }
}

/// The largest literal splice one step of a batched count-insert makes ([`run_repeat`]). Every step still
/// copies the whole buffer, so past the cap the cost is quadratic again, by a factor of the cap smaller.
/// A larger cap is faster for big counts, but lets `999999999ix` grow one long line so fast that by the
/// time CTRL-C stops it each later key is slow on that line (`pty_interrupt.rs`): 64 KiB gives a 1 MB
/// line 300 ms in (1 MiB gave 4 MB), and a million-char insert in about 30 steps.
const REPEAT_CHUNK_BYTES: usize = 64 << 10;

/// Whether a literal splice does what typing the characters one by one does: only in plain Insert mode
/// (Replace overwrites instead of inserting) and with `'textwidth'` off (typed blanks can wrap the line).
fn splice_is_typing(ws: &Workspace) -> bool {
    let view = &ws.focused().view;
    view.mode() == ruse_core::Mode::Insert
        && view
            .editing_options()
            .iter()
            .all(|o| !matches!(o, ruse_core::EditorOption::TextWidth(n) if *n > 0))
}

/// The text a repeat unit inserts when it is nothing but typed characters (`ix`, `ihello`, an Emacs
/// `C-u 8 x`), so its repetitions can be spliced as one literal paste. `None` for any other command: a
/// newline (autoindent), `<Tab>` (`'expandtab'`), `<BS>`, a line-opening `o`, a register.
fn literal_text(unit: &[Command]) -> Option<String> {
    let mut text = String::new();
    for cmd in unit {
        match cmd {
            Command::InsertChar(c) => text.push(*c),
            _ => return None,
        }
    }
    (!text.is_empty()).then_some(text)
}

/// `E20` for a jump to, or an operator over, an unset named mark (`'a`, `` `a ``, `d'a`): nothing runs.
fn mark_error(cmd: &Command, ws: &Workspace) -> Option<String> {
    let view = &ws.focused().view;
    // `` `[ ``/`'[`/`` `] ``/`']` before any change or yank (nvim v0.12.6, #1087).
    let unset = match cmd {
        Command::GotoChangeMarkStart | Command::GotoChangeMarkStartLine => {
            Some(view.change_mark_start().is_none())
        }
        Command::GotoChangeMarkEnd | Command::GotoChangeMarkEndLine => {
            Some(view.change_mark_end().is_none())
        }
        // `''`/`` `` `` with an empty jumplist (nvim v0.12.6, #686).
        Command::GotoContextMark | Command::GotoContextMarkLine => {
            Some(view.context_mark().is_none())
        }
        _ => None,
    };
    if let Some(unset) = unset {
        return unset.then(|| "E20: Mark not set".to_string());
    }
    let name = match cmd {
        Command::GotoNamedMark(c) | Command::GotoNamedMarkLine(c) => *c,
        Command::OpToMark { name, .. } => *name,
        _ => return None,
    };
    ((name.is_ascii_alphanumeric() || matches!(name, '^' | '<' | '>'))
        && ws.focused().view.named_mark(name).is_none())
    .then(|| "E20: Mark not set".to_string())
}

/// The error a Normal-mode put from an EMPTY register fails with (`:help E353`), or `None` when the put
/// can run. The read-only registers report what is missing instead (nvim v0.12.6). The black hole, the
/// clipboard and the expression register never fail here.
fn put_error(cmd: &Command, ws: &mut Workspace) -> Option<String> {
    if !matches!(
        cmd,
        Command::Paste { .. } | Command::PasteIndent { .. } | Command::PasteBlockInner { .. }
    ) {
        return None;
    }
    let name = ws.focused().view.pending_register();
    // A register written with nothing (`qaq`) puts nothing without an error (#1091); the read-only
    // registers are synced from frontend state, so only their text says whether they hold anything.
    let reg = ws.registers().get(name);
    let holds = match name {
        Some(':' | '/' | '.' | '%' | '#') => !reg.is_empty(),
        _ => reg.is_set(),
    };
    // nvim's `do_put` refuses a characterwise one-line put whose result would pass `INT_MAX` bytes
    // (`yiw999999999p`, v0.12.6) at once, rather than building it (#898).
    let count = match cmd {
        Command::Paste { count, .. }
        | Command::PasteIndent { count, .. }
        | Command::PasteBlockInner { count, .. } => *count,
        _ => 1,
    };
    let len = reg.text().len();
    if reg.kind() == ruse_core::RegKind::Charwise
        && !reg.text().contains(&b'\n')
        && len > 0
        && u64::from(count) * len as u64 > i32::MAX as u64
    {
        return Some("E1240: Resulting text too long".into());
    }
    if matches!(name, Some('_' | '+' | '*' | '=')) || holds {
        return None;
    }
    Some(match name {
        Some(':') => "E30: No previous command line".into(),
        Some('/') => "E35: No previous regular expression".into(),
        Some('.') => "E29: No inserted text yet".into(),
        Some('%') => "E32: No file name".into(),
        Some('#') => "E23: No alternate file".into(),
        Some(c) => format!("E353: Nothing in register {c}"),
        None => "E353: Nothing in register \"".into(),
    })
}

/// Resolve a `:normal` [`SubRange`] to an INCLUSIVE 1-based `(line1, line2)`. `%` = the whole file, `.`/none
/// = the cursor line, `N,M` = the numbers clamped to the buffer. The end is clamped to the current line
/// count; the START is not, so an out-of-range start (`line1 > line2`) simply runs zero iterations.
pub(crate) fn resolve_normal_range(ws: &Workspace, range: SubRange) -> (usize, usize) {
    let snapshot = ws.focused().doc.text_arc();
    let total = line_count(&snapshot).max(1);
    match range {
        SubRange::WholeFile => (1, total),
        SubRange::CurrentLine => {
            let line = ruse_core::pos::line_of(&snapshot, ws.focused().view.cursor()) + 1;
            (line, line)
        }
        SubRange::Lines(a, b) => (a.max(1), b.min(total)),
    }
}

/// Whether a `:m`/`:t` destination lands inside the focused buffer. A relative one (`:m+5`, `:m$-9`) can
/// fall off either end, which Vim refuses with E16 rather than clamping.
pub(crate) fn dest_in_range(ws: &Workspace, dest: ruse_core::LineAddr) -> bool {
    let doc = ws.focused().doc;
    let cursor_line = doc.line_of(ws.focused().view.cursor());
    let nlines = doc.line_starts().len() - usize::from(doc.bytes().ends_with(b"\n"));
    // An absolute line past the end is E16 too (`:t99`, #801), not the end of the buffer.
    let absolute_ok = !matches!(dest, ruse_core::LineAddr::Line(n) if n > nlines.max(1));
    absolute_ok && dest.insert_index(cursor_line, nlines).is_some()
}

pub(crate) fn apply_effect(
    eff: Effect,
    ws: &mut Workspace,
    files: &Files,
    status: &mut Status,
    quit: &mut bool,
) {
    match eff {
        // A command-driven write (Emacs `C-x C-s`, the palette) runs under the same write options as `:w`.
        Effect::Save => {
            save(ws, files, status);
        }
        // The palette's Quit, Emacs `C-x C-c` and Native `q` end the whole editor, so any modified buffer
        // blocks them, as Vim's `:qa` does (E162).
        Effect::Quit => match ws.buffers().into_iter().find(|b| b.modified) {
            Some(b) => {
                status.error(no_write_since_last_change(&b.name));
            }
            None => *quit = true,
        },
        // The core gives only errors this way (`E662`, `E664`, the changelist).
        Effect::Status(s) => status.error(s),
        // Recorded by the workspace; the session flushes the replayed keys (#1085).
        Effect::Beep | Effect::RegisterRefused => {}
    }
}

/// Buffer line count for the `:w` write report, matching Vim: an empty buffer is 1 line; otherwise it is
/// the newline count, plus one when the last line has no trailing newline (`"a\nb"` and `"a\nb\n"` are both
/// 2 lines).
pub(crate) fn line_count(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 1;
    }
    ruse_core::pos::count_newlines(buf) + usize::from(buf.last() != Some(&b'\n'))
}

/// The `:s` result an [`ExOutcome`] carries.
pub(crate) fn sub_outcome(out: ExOutcome) -> Result<ruse_core::SubOutcome, ruse_core::RegexError> {
    match out {
        ExOutcome::Substituted(o) => Ok(o),
        ExOutcome::Failed(e) => Err(e),
        _ => Ok(ruse_core::SubOutcome {
            replacements: 0,
            lines: 0,
        }),
    }
}

/// The line count an [`ExOutcome`] carries (`0` for any other outcome).
pub(crate) fn lines_outcome(out: &ExOutcome) -> usize {
    match out {
        ExOutcome::Lines(n) => *n,
        _ => 0,
    }
}

/// A resolved `:s` over `range` of the focused window (RFC-0020): the lines it covers, the literal pattern,
/// replacement and flags, and the `:s///c` answers when there are any.
pub(crate) fn substitute_cmd(
    ws: &Workspace,
    range: SubRange,
    pattern: &str,
    replacement: &str,
    flags: ruse_core::SubFlags,
    answers: Option<Vec<bool>>,
) -> ExCmd {
    let (first, last) = ws.resolve_range(range);
    ExCmd::Substitute {
        first,
        last,
        pattern: pattern.to_string(),
        replacement: replacement.to_string(),
        flags,
        answers,
    }
}

#[cfg(test)]
mod open_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod save_tests;
#[cfg(test)]
mod tests;
