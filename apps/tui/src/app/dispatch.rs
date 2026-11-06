//! Frontend command/ex dispatch: run a `Command` (recording it + performing its `Effect`s), execute a
//! parsed `:` line, and the save/label helpers they call. These are thin callers over `Workspace`,
//! `persist::`, and the status/quit sinks — no event-loop or terminal state.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{
    BytePos, Command, DocumentId, Effect, ExCmd, ExOutcome, Mode, Register, Revision, SubRange,
    Workspace,
};

use crate::app::messages::{Msg, Status};
use crate::highlight;
use crate::persist;

/// The per-buffer syntax-highlighter registry (F-007 / F-015), keyed by [`DocumentId`]. Shared by the
/// session loop and the LSP coordinator (both open buffers that need a highlighter).
pub(crate) type Highlighters = HashMap<DocumentId, highlight::CachedHighlight>;

/// What [`open_file_into_buffer`] did: the status line to show, and the buffer now focused on `file`
/// (`None` when nothing was opened — a read error, or the user kept a recovery journal and backed out).
pub(crate) struct Opened {
    pub(crate) status: crate::app::messages::Msg,
    pub(crate) id: Option<DocumentId>,
}

/// The buffer already holding `path`, matched on the canonical path so `f.txt`, `./f.txt` and an
/// absolute spelling name one file (#848). A path that no longer resolves compares as written.
pub(crate) fn buffer_for_path(files: &Files, path: &Path) -> Option<DocumentId> {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canon(path);
    files
        .iter()
        .filter(|(_, bf)| canon(&bf.path) == target)
        .map(|(id, _)| *id)
        .min_by_key(|id| id.0)
}

/// Open `file` and focus it, register its file identity + highlighter, and return a status line. Shared
/// by the file picker / `:e` (session) and goto/rename/references (LSP coordinator). `E484` on a read
/// error.
///
/// A file that is already open is NOT read again: its existing buffer is focused, as in Neovim (#848),
/// so two Documents never shadow one file and overwrite each other on `:w`. A file with a crash journal
/// that differs from disk gets the same recovery prompt as the startup file (#859); keeping the journal
/// (`q`) cancels the open, so the journal is never cleared by a later `:w` the user did not choose.
pub(crate) fn open_file_into_buffer(
    file: &str,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
) -> Opened {
    open_file_with(file, ws, files, highlighters, |fmt| fmt)
}

/// [`open_file_into_buffer`], with `format` adjusting the detected file format (`:e ++ff=dos`). A file
/// that does not exist yet opens as an empty buffer that `:w` creates (`"new.txt" [New]`, as in nvim).
pub(crate) fn open_file_with(
    file: &str,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
    format: impl FnOnce(persist::encoding::FileFormat) -> persist::encoding::FileFormat,
) -> Opened {
    let p = PathBuf::from(file);
    if let Some(id) = buffer_for_path(files, &p) {
        ws.focus_buffer(id);
        return Opened {
            status: Msg::echo(format!("\"{file}\" (already open)")),
            id: Some(id),
        };
    }
    let (raw, new) = match persist::read_file(&p) {
        Ok(raw) => (raw, false),
        // A dangling symlink is not a new file: writing it would create the link's target.
        Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(&p).is_err() => {
            (Vec::new(), true)
        }
        Err(e) => {
            crate::log::open_failed("edit", &p, &e);
            return Opened {
                status: Msg::error(format!("E484: can't open {file}: {e}")),
                id: None,
            };
        }
    };
    let f = format(persist::encoding::FileFormat::detect(&raw));
    let disk = f.to_buffer(&raw);
    // The same prompt as at startup (RFC-0023 §7); `q` cancels this open and keeps the journal.
    let offer =
        persist::journal::current().map_or(persist::Recovery::None, |j| j.offer(Some(&p), &disk));
    let resolved =
        crate::ui::recovery::resolve(&mut io::stdout(), offer, file, Some(&p), &disk, f, "cancel");
    let (recovered, read_only, note) = match resolved {
        Ok(crate::ui::recovery::Resolved::Open {
            recovered,
            read_only,
            note,
        }) => (recovered, read_only, note),
        Ok(crate::ui::recovery::Resolved::Quit { journal }) => {
            return Opened {
                status: Msg::info(format!(
                    "\"{file}\" not opened: recovery journal kept ({})",
                    journal.display()
                )),
                id: None,
            }
        }
        Err(e) => {
            return Opened {
                status: Msg::error(format!(
                    "\"{file}\" not opened: the recovery prompt failed: {e}"
                )),
                id: None,
            }
        }
    };
    let id = ws.add_buffer(disk, Some(file.to_string()));
    ws.focus_buffer(id);
    if let Some(rec) = recovered {
        // A buffer opened later is not part of a trace (it starts from the startup buffer).
        apply_recovery(ws, rec, &mut Vec::new());
        ws.place_focused_cursor(BytePos(0));
    }
    // A file message: shown, not kept (nvim's `:e` message is not in `:messages`).
    let status = Msg::echo(match note {
        Some(note) => format!("\"{file}\" {note}"),
        None if new => format!("\"{file}\" [New]"),
        None => format!("\"{file}\" {} bytes", raw.len()),
    });
    persist::disk::saw(&p, &raw);
    attach_file(files, highlighters, id, p, f);
    if let Some(bf) = files.get_mut(&id) {
        bf.read_only = read_only;
    }
    Opened {
        status,
        id: Some(id),
    }
}

/// Put a recovered text over the focused buffer's disk text (#863, #1194) so the buffer is modified (`:q`
/// refuses with E37) and `u` goes back to the disk text: the commands [`persist::recovery_commands`] gives
/// (one text edit, or a filter or read of the bytes plus a final-newline edit), run through [`run_ex`] into
/// `recorded` as one undo step. Two of them are an `ex_begin`/`ex_end` group, so a saved trace replays them
/// as one step too. The group is labelled `macro` (keys that were not typed: one undo step); the trace has
/// no kind of its own for a recovery, and adding one would change its format.
pub(crate) fn apply_recovery(ws: &mut Workspace, rec: Vec<u8>, recorded: &mut Vec<Command>) {
    let cmds = persist::recovery_commands(ws.focused().doc.bytes(), &rec);
    let group = cmds.len() > 1;
    let opened = group && !ws.focused().doc.in_undo_block();
    if opened {
        ws.focused_doc_mut().begin_undo_block();
    }
    if group {
        note_ex(ExCmd::Begin(ruse_core::GroupKind::Macro), recorded);
    }
    for ex in cmds {
        run_ex(ex, ws, recorded);
    }
    if group {
        note_ex(ExCmd::End, recorded);
    }
    if opened {
        ws.focused_doc_mut().end_undo_block();
    }
    debug_assert_eq!(ws.focused().doc.bytes(), &rec[..]);
}

/// Give buffer `id` its file on disk: record `path` and `fmt` in the file registry and attach the
/// highlighter `path`'s extension selects, dropping any the buffer had (#894). Every way a buffer gets a
/// file goes through here: the startup file, `:e`/the file picker/LSP jumps, and `:w {file}`/`:saveas`.
pub(crate) fn attach_file(
    files: &mut Files,
    highlighters: &mut Highlighters,
    id: DocumentId,
    path: PathBuf,
    fmt: persist::encoding::FileFormat,
) {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(highlight::CachedHighlight::for_ext)
    {
        Some(h) => {
            highlighters.insert(id, h);
        }
        None => {
            highlighters.remove(&id);
        }
    }
    files.insert(
        id,
        BufferFile {
            path,
            fmt,
            read_only: false,
            reloads: 0,
        },
    );
}

/// Whether `key` is `CTRL-<c>`. Shared frontend key predicate. A C0 punctuation chord also matches the key
/// a legacy terminal delivers for it: crossterm decodes `CTRL-\ CTRL-] CTRL-^ CTRL-_` (0x1c..0x1f) as
/// `CTRL-4`..`CTRL-7` and `CTRL-@` (NUL) as `CTRL-Space` (#603, #1069).
pub(crate) fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && (key.code == KeyCode::Char(c)
            || crate::keys::legacy_ctrl_alias(c).is_some_and(|a| key.code == KeyCode::Char(a)))
}

/// One file buffer's on-disk identity: its path and the detected encoding/line-ending to restore on save
/// (F-008). Held frontend-side (core is IO-free), keyed by [`DocumentId`] in a [`Files`] registry.
pub(crate) struct BufferFile {
    pub(crate) path: PathBuf,
    pub(crate) fmt: persist::encoding::FileFormat,
    /// Opened read-only from the recovery prompt while another session edits the file (Vim's
    /// `'readonly'`): a write to it without `!` is `E45`.
    pub(crate) read_only: bool,
    /// How many times `:e`/`:e!` re-read the file into the buffer: a re-read is new text from disk and a
    /// fresh undo history, which a trace cannot record (#690, [`crate::app::trace_check`]).
    pub(crate) reloads: u32,
}

/// Neovim's message for a write to a `'readonly'` buffer without `!`.
pub(crate) const E45: &str = "E45: 'readonly' option is set (add ! to override)";

/// Neovim's warning on the first change to a `'readonly'` buffer.
pub(crate) const W10: &str = "W10: Warning: Changing a readonly file";

/// The `'readonly'` state that is not a file buffer's own [`BufferFile::read_only`] (#1211): the flag of a
/// buffer without a file, which buffers gave W10 since `'readonly'` was last set (nvim's `b_did_warn`),
/// and `ruse -R` (nvim's readonly mode, which makes every file the session opens read-only).
#[derive(Default, Debug)]
pub(crate) struct ReadOnly {
    /// `ruse -R`: every file buffer starts read-only, until a `:set noro`.
    pub(crate) mode: bool,
    /// The file buffers the readonly mode already made read-only, so a `:setlocal noro` sticks.
    seen: std::collections::HashSet<DocumentId>,
    nofile: std::collections::HashSet<DocumentId>,
    warned: std::collections::HashSet<DocumentId>,
}

impl ReadOnly {
    /// Whether buffer `id` is `'readonly'`.
    pub(crate) fn get(&self, files: &Files, id: DocumentId) -> bool {
        files
            .get(&id)
            .map_or_else(|| self.nofile.contains(&id), |bf| bf.read_only)
    }

    /// Set or reset `'readonly'` on buffer `id`. Setting it lets the next change warn again (nvim).
    pub(crate) fn set(&mut self, files: &mut Files, id: DocumentId, on: bool) {
        match files.get_mut(&id) {
            Some(bf) => bf.read_only = on,
            None if on => {
                self.nofile.insert(id);
            }
            None => {
                self.nofile.remove(&id);
            }
        }
        if on {
            self.warned.remove(&id);
        }
    }

    /// In readonly mode, make every file buffer not seen yet read-only.
    pub(crate) fn follow(&mut self, files: &mut Files) {
        if !self.mode {
            return;
        }
        for (id, bf) in files.iter_mut() {
            if self.seen.insert(*id) {
                bf.read_only = true;
            }
        }
    }

    /// What a key may warn about: the focused buffer and its revision when it is read-only, unmodified,
    /// and has not warned yet, else `None`.
    pub(crate) fn watch(&self, ws: &Workspace, files: &Files) -> Option<(DocumentId, Revision)> {
        let id = ws.focused_buffer();
        let doc = ws.focused().doc;
        (self.get(files, id) && !doc.is_modified() && !self.warned.contains(&id))
            .then(|| (id, doc.revision()))
    }

    /// After a key: W10 when the watched buffer changed, or Insert or Replace mode began on it (nvim warns
    /// as Insert mode starts, before a character is typed).
    pub(crate) fn warn(
        &mut self,
        watched: Option<(DocumentId, Revision)>,
        ws: &Workspace,
        status: &mut Status,
    ) {
        let Some((id, rev)) = watched else { return };
        let changed = ws.document(id).is_some_and(|d| d.revision() != rev);
        let inserting = ws.focused_buffer() == id
            && matches!(
                ws.focused().view.mode(),
                Mode::Insert | Mode::Replace | Mode::VirtualReplace
            );
        if changed || inserting {
            self.warned.insert(id);
            status.warn(W10);
        }
    }
}

/// The per-buffer file registry: which buffers have a file on disk (a scratch `:enew` buffer has none, so
/// `:w` on it declines). Replaces the old single `path`/`fmt`/`file_buf` triple (F-007 multi-buffer).
pub(crate) type Files = HashMap<DocumentId, BufferFile>;

/// `gq` under `'formatprg'` and `=` under `'equalprg'` (#617, #618): instead of formatting or reindenting,
/// filter the operator's lines through that shell command, as nvim's `op_colon` runs `:{range}!{prg}`.
/// After `gq` the cursor goes to the first non-blank of the last filtered line (nvim's `']`), after `=` to
/// that of the first. `gw` never uses `'formatprg'`. A search or mark target, and a blockwise selection,
/// keep the internal operator. Returns whether the command was handled here.
pub(crate) fn run_prg_operator(
    cmd: &Command,
    ws: &mut Workspace,
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

/// The unsaved-changes check of every quit that would end the editor (`:q`, `C-w q`, `:wq`/`:x`, `ZZ`/`ZQ`
/// on the last window, `:qa`): refuse with E37 when the focused buffer is modified, or E162 when another
/// (hidden) one is. `abandon_focused` (`:q!`, `ZQ`) skips only the focused buffer's check; a modified
/// hidden buffer still refuses, as in nvim. Returns whether to quit.
pub(crate) fn may_quit(ws: &Workspace, status: &mut Status, abandon_focused: bool) -> bool {
    let focused = ws.focused_buffer();
    if !abandon_focused && ws.focused().doc.is_modified() {
        status.error("E37: No write since last change (add ! to override)");
        false
    } else if let Some(b) = ws
        .buffers()
        .into_iter()
        .find(|b| b.modified && b.id != focused)
    {
        status.error(format!(
            "E162: No write since last change for buffer \"{}\"",
            b.name
        ));
        false
    } else {
        true
    }
}

/// A short label for the focused buffer (name + `[+]` when modified) — the status after `:b`/`:bn`/`:bp`.
pub(crate) fn focused_buffer_label(ws: &Workspace) -> String {
    let id = ws.focused_buffer();
    let name = ws.buffer_name(id).unwrap_or("[No Name]");
    let dirty = if ws.focused().doc.is_modified() {
        " [+]"
    } else {
        ""
    };
    format!("buffer {}: {name}{dirty}", id.0)
}

/// The one-line `:ls` buffer list: `1%a name  2  # other  3 + scratch …` (`%`=current, `#`=alternate,
/// `+`=modified). Vim renders a multi-line list; the single status line shows a compact form for MVP.
pub(crate) fn buffer_list_line(ws: &Workspace) -> String {
    ws.buffers()
        .iter()
        .map(|b| {
            let cur = if b.current { "%" } else { "" };
            let alt = if b.alt { "#" } else { "" };
            let modified = if b.modified { "+" } else { "" };
            format!("{}{cur}{alt} {}{modified}", b.id.0, b.name)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// The `:s///n` report-only status line. Wording + per-noun pluralization verified vs nvim v0.12.4:
/// `6 matches on 3 lines`, `2 matches on 1 line`, `1 match on 1 line` (each noun pluralizes on its own
/// count). The no-match case is reported separately (E486) by the caller, never here.
pub(crate) fn match_count_message(matches: usize, lines: usize) -> String {
    let m = if matches == 1 { "match" } else { "matches" };
    let l = if lines == 1 { "line" } else { "lines" };
    format!("{matches} {m} on {lines} {l}")
}

/// The status line after a substitute replaced text: `N substitutions on M lines` (one wording for `:s`,
/// `&`, `g&`, `:&&` and the `:s///c` loop), when there were more substitutions than `report`; else nothing.
pub(crate) fn substituted_message(out: &ruse_core::SubOutcome, report: i64) -> Msg {
    crate::app::report::substituted(out.replacements, out.lines, report)
        .map_or_else(|| Msg::echo(""), Msg::info)
}

/// Status for a regex compile error (F-009): the pattern reader's own `E…:` message when it has one (nvim's
/// number and wording, #1087), else a plain note. When nvim gives a second error after the first (the NFA
/// engine's `E870` after a bad `\{…}`), that one leads, since it is the last error nvim reports.
pub(crate) fn regex_error_msg(e: &ruse_core::RegexError) -> String {
    match e {
        ruse_core::RegexError::Unsupported(m) => format!("E: unsupported pattern: {m}"),
        ruse_core::RegexError::Syntax(m) if m.starts_with("E554:") => {
            format!("E870: (NFA regexp) Error while reading repetition limits: {m}")
        }
        ruse_core::RegexError::Syntax(m) if has_error_number(m) => m.clone(),
        ruse_core::RegexError::Syntax(m) => format!("E: bad pattern: {m}"),
    }
}

/// The status for an `:s` whose pattern cannot be used: nvim follows the pattern's own error with `E476`
/// (`do_sub`), and so does `:g` (`ex_global`).
pub(crate) fn invalid_command_msg(e: &ruse_core::RegexError) -> String {
    format!("E476: Invalid command: {}", regex_error_msg(e))
}

/// Whether `msg` opens with a Vim error number (`E54: …`).
fn has_error_number(msg: &str) -> bool {
    let digits = msg
        .strip_prefix('E')
        .map_or(0, |r| r.bytes().take_while(u8::is_ascii_digit).count());
    digits > 0 && msg[1 + digits..].starts_with(':')
}

/// The process exit status a `:cquit` asked for; 0 otherwise. Read by the CLI once the session returns.
static EXIT_STATUS: AtomicU8 = AtomicU8::new(0);

/// The exit status the session should end with (`:cq [N]` sets it; a normal quit leaves it 0).
pub(crate) fn exit_status() -> u8 {
    EXIT_STATUS.load(Ordering::Relaxed)
}

/// Record the exit status `:cq [N]` asked for.
pub(crate) fn set_exit_status(code: u8) {
    EXIT_STATUS.store(code, Ordering::Relaxed);
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

/// `:q` (Vim `:quit`): with other windows open, close the focused one — its buffer stays loaded and
/// hidden even when modified (#736; nvim's 'hidden' is on by default). On the last window, end the editor
/// through [`may_quit`]. `force` (`:q!`, `ZQ`) lets the FOCUSED buffer's changes go; a modified hidden
/// buffer still refuses, as in nvim, so `:q!` can never silently drop work the user cannot see.
pub(crate) fn quit_window(ws: &mut Workspace, status: &mut Status, quit: &mut bool, force: bool) {
    if ws.close_focused() {
        return;
    }
    *quit = may_quit(ws, status, force);
}

pub(crate) fn apply_effect(
    eff: Effect,
    ws: &mut Workspace,
    files: &Files,
    status: &mut Status,
    quit: &mut bool,
) {
    match eff {
        // A command-driven write (e.g. `ZZ`) always byte-preserves; `:set fixeol` is honored only on the
        // `:w`/`:wq` ex path (which threads the session flag), matching where the opt-in is documented.
        Effect::Save => {
            save(ws, files, status, false);
        }
        // The palette's Quit, Emacs `C-x C-c` and Native `q` end the whole editor, so any modified buffer
        // blocks them, as Vim's `:qa` does (E162).
        Effect::Quit => match ws.buffers().into_iter().find(|b| b.modified) {
            Some(b) => {
                status.error(format!(
                    "E162: No write since last change for buffer \"{}\"",
                    b.name
                ));
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

/// The write-report suffix flagging a missing final newline: `" [noeol]"` when `buf` is non-empty and does
/// not end in `\n`, else `""`. ruse preserves EOL presence exactly (no silent fixeol), so this makes the
/// missing terminator visible on `:w` (as Vim does) rather than a silent surprise.
fn noeol_marker(buf: &[u8]) -> &'static str {
    if buf.is_empty() || buf.last() == Some(&b'\n') {
        ""
    } else {
        " [noeol]"
    }
}

/// Whether `save` should append a final `\n` to the buffer before writing: only when `add` (the buffer's
/// `'endofline'` or `:set fixeol`; a file opened without a final newline has neither by default), and only
/// when the buffer is non-empty and lacks one. An empty buffer is left empty (Vim writes no newline into one).
fn needs_final_newline(buf: &[u8], add: bool) -> bool {
    add && !buf.is_empty() && buf.last() != Some(&b'\n')
}

/// The bytes a write of `buffer` puts on disk for a file read as `fmt`: the final newline a buffer with
/// `'endofline'` (or `:set fixeol`) always gets (#1107), then the file's BOM and line endings (F-008 #2).
/// `ruse --replay` prints the same image of the replayed buffer, so it equals what `:w` wrote (#690).
pub(crate) fn disk_image(
    fmt: persist::encoding::FileFormat,
    buffer: &[u8],
    fixeol: bool,
) -> Vec<u8> {
    fmt.to_disk(&disk_source(fmt, buffer, fixeol))
}

/// The buffer text a write encodes: `buffer`, plus the final newline it gets on a write (#1107).
fn disk_source(fmt: persist::encoding::FileFormat, buffer: &[u8], fixeol: bool) -> Vec<u8> {
    let mut source = buffer.to_vec();
    if needs_final_newline(&source, fixeol || fmt.final_newline) {
        source.push(b'\n');
    }
    source
}

/// Returns whether the focused buffer reached disk.
pub(crate) fn save(ws: &mut Workspace, files: &Files, status: &mut Status, fixeol: bool) -> bool {
    // Multi-buffer honesty (F-007): only a buffer WITH a file writes. A scratch buffer (`:enew`) has no
    // registry entry, so `:w` declines rather than clobbering another buffer's file.
    let Some(bf) = files.get(&ws.focused_buffer()) else {
        status.error("E32: No file name");
        return false;
    };
    if bf.read_only {
        status.error(E45);
        return false;
    }
    // `:set fixeol` (opt-in) ADDS a trailing `\n` when the buffer lacks one. Append it to the SOURCE bytes
    // BEFORE `to_disk` so the encoding step (BOM/CRLF re-application) treats the newline like any other —
    // e.g. a CRLF file gets `\r\n`. Default (byte-preserve) leaves the bytes untouched.
    // Restore the original encoding/line-endings (F-008 #2), then write durably (fsync + rename, #1).
    // A buffer with `'endofline'` (a new file, or one read with a final newline) always gets it (#1107).
    let source = disk_source(bf.fmt, ws.focused().doc.bytes(), fixeol);
    let bytes = bf.fmt.to_disk(&source);
    // A command-driven write is never forced: a read-only file is `E505`, as nvim's `ZZ` is, and a file
    // changed on disk since ruse read or saved it is not overwritten (RFC-0023 §8).
    if persist::disk::conflicts(&bf.path) {
        status.warn(persist::disk::CHANGED_SINCE_READ);
        return false;
    }
    match persist::atomic::write(&bf.path, &bytes, false) {
        Ok(method) => {
            persist::disk::saw(&bf.path, &bytes);
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(Some(bf.path.as_path())); // saved bytes are durable — nothing to recover
            crate::log::save(&bf.path, bytes.len());
            // Vim-style write report: `"file" 42L, 1024B written` (L = buffer lines, B = bytes on disk).
            // ruse preserves final-newline presence exactly by default (no silent fixeol); when the file was
            // written WITHOUT a trailing newline, surface `[noeol]` (as Vim does) so the missing EOL is never
            // a silent surprise. The check is on the SOURCE bytes actually written (to_disk only re-applies
            // BOM/CRLF, not EOL), so a successful `:set fixeol` write ends in `\n` and shows no marker.
            // A filesystem without directory fsync is saved, but the report says what was not proven.
            status.echo(format!(
                "\"{}\"{} {}L, {}B written{}",
                bf.path.display(),
                noeol_marker(&source),
                line_count(&source),
                bytes.len(),
                method.note()
            ));
            true
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            // Not saved: the buffer stays modified and keeps its journal (an fsync that failed after the
            // rename is `E667`, never "written", #936).
            crate::log::save_failed(&bf.path, &e);
            status.error(e.to_string());
            false
        }
    }
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
mod dispatch_tests {
    use super::*;
    use crate::input::{Ex, Feed, InputEngine};
    use ruse_core::{Command, SplitDir};

    /// #1069: a C0 punctuation chord matches in both encodings — the kitty protocol's `CTRL-]` and the
    /// `CTRL-5` crossterm decodes from a legacy terminal's 0x1d — and a letter chord only as itself.
    #[test]
    fn is_ctrl_accepts_the_legacy_encoding_of_punctuation_chords() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for (c, legacy) in [('\\', '4'), (']', '5'), ('^', '6'), ('_', '7'), ('@', ' ')] {
            assert!(is_ctrl(ctrl(c), c), "CTRL-{c}");
            assert!(is_ctrl(ctrl(legacy), c), "CTRL-{c} sent as CTRL-{legacy}");
            assert!(!is_ctrl(
                KeyEvent::new(KeyCode::Char(legacy), KeyModifiers::NONE),
                c
            ));
        }
        assert!(is_ctrl(ctrl('w'), 'w'));
        assert!(!is_ctrl(ctrl('5'), 'w'));
        assert!(!is_ctrl(
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
            ']'
        ));
    }

    /// #894: attaching a file records its identity and the highlighter its extension selects; a later file
    /// with no grammar drops the old highlighter instead of keeping the previous language's.
    #[test]
    fn attach_file_sets_identity_and_swaps_the_highlighter() {
        let (mut files, mut hl): (Files, Highlighters) = (HashMap::new(), HashMap::new());
        let id = DocumentId(1);
        let fmt = persist::encoding::FileFormat::detect(b"");
        attach_file(&mut files, &mut hl, id, PathBuf::from("a.rs"), fmt);
        assert_eq!(files[&id].path, PathBuf::from("a.rs"));
        assert!(hl.contains_key(&id), "a .rs file is highlighted");
        attach_file(&mut files, &mut hl, id, PathBuf::from("notes.xyz"), fmt);
        assert_eq!(files[&id].path, PathBuf::from("notes.xyz"));
        assert!(!hl.contains_key(&id), "no grammar, no highlighter");
    }

    /// `:q` refuses (E37) once the focused buffer is modified; `:q!` quits regardless; a clean `:q` quits.
    #[test]
    fn quit_guards_unsaved_changes_and_bang_overrides() {
        let run = |line: &str, ws: &mut Workspace| ex_run(ws, line);

        // Clean buffer → `:q` quits.
        let mut ws = Workspace::new(b"hello\n".to_vec());
        let (_, quit) = run("q", &mut ws);
        assert!(quit, "clean buffer quits on :q");

        // Modify the buffer → `:q` refuses with E37, `:q!` quits.
        let mut ws = Workspace::new(b"hello\n".to_vec());
        ws.apply(&Command::EnterInsert);
        ws.apply(&Command::InsertChar('x'));
        assert!(ws.focused().doc.is_modified(), "edit dirtied the buffer");
        let (status, quit) = run("q", &mut ws);
        assert!(!quit, ":q does not quit a dirty buffer");
        assert!(status.starts_with("E37"), "E37 surfaced: {status}");
        let (_, quit) = run("q!", &mut ws);
        assert!(quit, ":q! discards changes and quits");
    }

    /// Run one ex line against `ws` through the session's Ex dispatch, returning `(status, quit)`.
    fn ex_run(ws: &mut Workspace, line: &str) -> (String, bool) {
        let s = crate::app::session::Session::ex_on(ws, line);
        (s.status.to_string(), s.quit)
    }

    /// A workspace with a.txt focused and a MODIFIED b.txt loaded but hidden (`:split`, `:e b.txt`, `x`,
    /// `:close` in nvim).
    fn ws_with_hidden_modified() -> Workspace {
        let mut ws = Workspace::new(b"a\n".to_vec());
        ws.set_focused_buffer_name("a.txt");
        let b = ws.add_buffer(b"b\n".to_vec(), Some("b.txt".into()));
        ws.split(SplitDir::Horizontal);
        ws.focus_buffer(b);
        ws.apply(&Command::DeleteUnder(1));
        assert!(ws.close_focused());
        assert!(!ws.focused().doc.is_modified());
        ws
    }

    /// #611, verified against nvim v0.12.6: `:q` / `:q!` in a split close only the focused window, even
    /// when its buffer is modified (that buffer stays loaded, hidden); `:close`/`:hide` refuse the last
    /// window with E444.
    #[test]
    fn quit_in_a_split_closes_the_window_only() {
        for line in ["q", "quit", "q!"] {
            let mut ws = Workspace::new(b"a\n".to_vec());
            ws.split(SplitDir::Horizontal);
            ws.apply(&Command::DeleteUnder(1));
            let (status, quit) = ex_run(&mut ws, line);
            assert!(!quit, ":{line} in a split must not end the editor");
            assert_eq!(ws.window_count(), 1, ":{line} closes the focused window");
            assert!(ws.focused().doc.is_modified(), "the edit survives");
            assert_eq!(status, "");
        }
        for line in ["close", "close!", "hide"] {
            let mut ws = Workspace::new(b"a\n".to_vec());
            let (status, quit) = ex_run(&mut ws, line);
            assert!(!quit);
            assert_eq!(status, "E444: Cannot close last window", ":{line}");
        }
    }

    /// #611: on the last window, every quit that does not say "all" refuses while a HIDDEN buffer is
    /// modified — `:q`, `:q!`, `:wq`, `:x` (nvim reports E37 for each; ruse names the buffer, E162).
    /// `:qa` refuses too; only `:qa!` abandons it.
    #[test]
    fn last_window_quits_refuse_over_a_modified_hidden_buffer() {
        for line in ["q", "q!", "x", "qa", "qall", "quitall"] {
            let mut ws = ws_with_hidden_modified();
            let (status, quit) = ex_run(&mut ws, line);
            assert!(!quit, ":{line} must not drop b.txt's change");
            assert_eq!(
                status, "E162: No write since last change for buffer \"b.txt\"",
                ":{line}"
            );
        }
        let mut ws = ws_with_hidden_modified();
        assert!(ex_run(&mut ws, "qa!").1, ":qa! abandons every change");
    }

    /// `:qa` names the FOCUSED buffer first (E37) when it is the modified one.
    #[test]
    fn quit_all_refuses_a_modified_focused_buffer_with_e37() {
        let mut ws = Workspace::new(b"a\n".to_vec());
        ws.apply(&Command::DeleteUnder(1));
        let (status, quit) = ex_run(&mut ws, "qa");
        assert!(!quit);
        assert!(status.starts_with("E37"), "{status}");
        assert!(ex_run(&mut ws, "quitall!").1);
    }

    /// `:x` (and `ZZ`) writes only when modified: a clean unnamed buffer quits without a write attempt,
    /// while a modified one with no file name fails to write and stays open (#842).
    #[test]
    fn exit_write_writes_only_a_modified_buffer() {
        let mut ws = Workspace::new(b"a\n".to_vec());
        assert!(ex_run(&mut ws, "x").1, "clean: quits, nothing to write");
        let mut ws = Workspace::new(b"a\n".to_vec());
        ws.apply(&Command::DeleteUnder(1));
        let (_, quit) = ex_run(&mut ws, "x");
        assert!(!quit, "a failed write keeps the editor open");
        let (_, quit) = ex_run(&mut ws, "wq");
        assert!(!quit, ":wq likewise");
    }

    /// `:cq [N]` quits at once, even over unsaved changes, and records the exit status (default 1).
    #[test]
    fn cquit_quits_with_a_status() {
        let mut ws = Workspace::new(b"a\n".to_vec());
        ws.apply(&Command::DeleteUnder(1));
        assert!(ex_run(&mut ws, "cq 3").1);
        assert_eq!(exit_status(), 3);
        assert!(ex_run(&mut ws, "cquit").1);
        assert_eq!(exit_status(), 1);
        EXIT_STATUS.store(0, Ordering::Relaxed);
    }

    /// `:t`/`:m` to an absolute line past the end is E16 too (#801), and the cursor rests on the last line
    /// moved or copied, at the wanted column (nvim v0.12.6 `ll:2,3t0`: line 2, column 2).
    #[test]
    fn move_and_copy_refuse_absolute_lines_past_the_end_and_keep_the_column() {
        let mut ws = Workspace::new(b"abcd\n  defgh\nghi\n  jkl\n".to_vec());
        ws.place_focused_cursor(BytePos(2));
        ws.apply(&Command::MoveRight); // a horizontal move sets the wanted column
        ws.apply(&Command::MoveLeft);
        assert_eq!(ex_run(&mut ws, "t99").0, "E16: Invalid range");
        assert_eq!(ex_run(&mut ws, "m99").0, "E16: Invalid range");
        ex_run(&mut ws, "2,3t0");
        assert_eq!(
            ws.focused().doc.bytes(),
            b"  defgh\nghi\nabcd\n  defgh\nghi\n  jkl\n"
        );
        assert_eq!(ws.focused().view.cursor().0, 8 + 2);
        // `:d` lands on the first non-blank of the line that took the range's place.
        let mut ws = Workspace::new(b"abc\n  defgh\nghi\n  jkl\n".to_vec());
        ex_run(&mut ws, "2,3d");
        assert_eq!(ws.focused().doc.bytes(), b"abc\n  jkl\n");
        assert_eq!(ws.focused().view.cursor().0, 4 + 2);
        ex_run(&mut ws, "2d");
        assert_eq!(ws.focused().doc.bytes(), b"abc\n");
        assert_eq!(
            ws.focused().view.cursor().0,
            0,
            "through the end: the new last line"
        );
    }

    /// #601: `:m`/`:t` destinations relative to the cursor line, verified against nvim v0.12.6. Before the
    /// fix `+1` parsed as the ABSOLUTE line 1, so `:m+1` on line 2 was a silent no-op.
    #[test]
    fn move_and_copy_take_relative_addresses() {
        let run = |cursor_line: usize, line: &str| {
            let mut ws = Workspace::new(b"1\n2\n3\n4\n".to_vec());
            ws.place_focused_cursor(BytePos(cursor_line * 2));
            let (status, _) = ex_run(&mut ws, line);
            (
                String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap(),
                status,
            )
        };
        assert_eq!(run(1, "m+1").0, "1\n3\n2\n4\n");
        assert_eq!(run(1, "m +1").0, "1\n3\n2\n4\n");
        assert_eq!(run(1, "m+").0, "1\n3\n2\n4\n");
        assert_eq!(run(2, "m-2").0, "1\n3\n2\n4\n");
        assert_eq!(run(1, "m.+2").0, "1\n3\n4\n2\n");
        assert_eq!(run(0, "m$-1").0, "2\n3\n1\n4\n");
        assert_eq!(run(0, "m1+2").0, "2\n3\n1\n4\n");
        assert_eq!(run(1, "t+1").0, "1\n2\n3\n2\n4\n");
        assert_eq!(run(1, "t-1").0, "1\n2\n2\n3\n4\n");
        assert_eq!(run(3, "t-3").0, "1\n4\n2\n3\n4\n");
        // Off the buffer: E16, nothing moves.
        assert_eq!(
            run(1, "m+5"),
            ("1\n2\n3\n4\n".into(), "E16: Invalid range".into())
        );
        assert_eq!(
            run(1, "t-3"),
            ("1\n2\n3\n4\n".into(), "E16: Invalid range".into())
        );
    }

    /// The `:w` line count matches Vim across the edge cases: empty buffer = 1 line, a trailing newline
    /// does NOT add a phantom empty line, and a missing final newline still counts its line.
    #[test]
    fn line_count_matches_vim() {
        assert_eq!(line_count(b""), 1, "empty buffer is one line");
        assert_eq!(line_count(b"a"), 1, "single line, no newline");
        assert_eq!(line_count(b"a\n"), 1, "trailing newline is not a new line");
        assert_eq!(line_count(b"a\nb"), 2, "missing final newline still counts");
        assert_eq!(line_count(b"a\nb\n"), 2, "two lines with trailing newline");
        assert_eq!(line_count(b"\n"), 1, "a lone newline is one line");
        assert_eq!(line_count(b"\n\n"), 2, "two blank lines");
    }

    #[test]
    fn noeol_marker_flags_a_missing_final_newline() {
        assert_eq!(noeol_marker(b"a\n"), "", "trailing newline → no marker");
        assert_eq!(noeol_marker(b""), "", "empty buffer → no marker");
        assert_eq!(
            noeol_marker(b"a"),
            " [noeol]",
            "no trailing newline → marker"
        );
        assert_eq!(
            noeol_marker(b"a\nb"),
            " [noeol]",
            "missing final newline → marker"
        );
    }

    /// Drive the `:normal` executor directly on a known buffer and return `(bytes, cursor)`. This is the
    /// hard-oracle harness the task calls for: `parity_compare` cannot drive ex commands (it feeds raw keys),
    /// so every expectation below is the buffer nvim v0.12.4 produces for the SAME `:normal` line, confirmed
    /// by hand (see the change evidence). `cursor` starts at `cursor0` (byte offset) before the run.
    fn run_normal_oracle(
        buf: &str,
        cursor0: usize,
        range: Option<SubRange>,
        keys: &str,
    ) -> (String, usize) {
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        ws.place_focused_cursor(BytePos(cursor0));
        let range = match range {
            None => String::new(),
            Some(SubRange::WholeFile) => "%".into(),
            Some(SubRange::CurrentLine) => ".".into(),
            Some(SubRange::Lines(a, b)) => format!("{a},{b}"),
        };
        ex_run(&mut ws, &format!("{range}normal {keys}"));
        let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
        (bytes, ws.focused().view.cursor().0)
    }

    #[test]
    fn normal_no_range_runs_once_at_cursor() {
        // nvim: `:normal dw` with the cursor on the 3rd char of "hello world foo" → "heworld foo".
        let (bytes, _) = run_normal_oracle("hello world foo\n", 2, None, "dw");
        assert_eq!(bytes, "heworld foo\n");
        // nvim: `:normal 3x` deletes three chars from the cursor.
        let (bytes, _) = run_normal_oracle("abcdef\n", 0, None, "3x");
        assert_eq!(bytes, "def\n");
    }

    #[test]
    fn normal_implicit_esc_terminates_insert() {
        // nvim: `:normal Ihi` (NO trailing <Esc>) inserts "hi" at the start and auto-terminates insert.
        let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi");
        assert_eq!(bytes, "hiabc\n");
        // An EXPLICIT Escape (the raw byte `c_CTRL-V <Esc>` types) is equivalent (the implicit one is a
        // no-op once already in Normal); `<Esc>` spelled out is five keys (#815).
        let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi\u{1b}");
        assert_eq!(bytes, "hiabc\n");
        let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi<Esc>");
        assert_eq!(bytes, "hi<Esc>abc\n");
    }

    #[test]
    fn normal_whole_file_range_runs_per_line_at_column_zero() {
        // nvim: `:%normal A;` appends ";" to every line (A goes to each line's end).
        let (bytes, _) = run_normal_oracle("a\nbb\nccc\n", 0, Some(SubRange::WholeFile), "A;");
        assert_eq!(bytes, "a;\nbb;\nccc;\n");
        // nvim: `:2,3normal 0x` deletes the first char of lines 2 and 3 (cursor placed at column 0 each).
        let (bytes, _) = run_normal_oracle("abc\ndef\nghi\n", 0, Some(SubRange::Lines(2, 3)), "0x");
        assert_eq!(bytes, "abc\nef\nhi\n");
    }

    #[test]
    fn normal_range_tracks_line_shifts_like_nvim() {
        // nvim: `:%normal dd` over 3 lines deletes them ALL (the fixed range end + current-line clamp).
        let (bytes, _) = run_normal_oracle("a\nb\nc\n", 0, Some(SubRange::WholeFile), "dd");
        assert_eq!(bytes, "");
        // nvim: `:1,2normal dd` on 4 lines leaves "b\nd\n" (2nd iter lands on the shifted-up 2nd line).
        let (bytes, _) = run_normal_oracle("a\nb\nc\nd\n", 0, Some(SubRange::Lines(1, 2)), "dd");
        assert_eq!(bytes, "b\nd\n");
        // nvim: `:%normal o-` appends one "-" line per ORIGINAL line — both cluster after "a" because the
        // fixed line numbers 1,2 both fall in the a-block as it grows.
        let (bytes, _) = run_normal_oracle("a\nb\n", 0, Some(SubRange::WholeFile), "o-");
        assert_eq!(bytes, "a\n-\n-\nb\n");
    }

    #[test]
    fn normal_range_places_cursor_at_column_zero_not_first_nonblank() {
        // nvim: `:2normal x` on "   xyz" deletes the leading space at column 0 (NOT first non-blank).
        let (bytes, _) = run_normal_oracle("a\n   xyz\nb\n", 0, Some(SubRange::Lines(2, 2)), "x");
        assert_eq!(bytes, "a\n  xyz\nb\n");
    }

    /// Drive `:g/pat/normal {keys}` directly on a known buffer and return the resulting bytes. Like
    /// [`run_normal_oracle`], every expectation is the buffer nvim v0.12.4 produces for the SAME `:g`
    /// line, captured headlessly by hand (`parity_compare` cannot drive ex commands — it feeds raw keys).
    fn run_global_normal_oracle(buf: &str, pattern: &str, negate: bool, keys: &str) -> String {
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        let verb = if negate { "v" } else { "g" };
        ex_run(&mut ws, &format!("{verb}/{pattern}/normal {keys}"));
        String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap()
    }

    #[test]
    fn global_normal_appends_to_matching_lines() {
        // nvim: `:g/foo/normal A;` appends ";" to every line containing "foo".
        let out = run_global_normal_oracle("afoo\nbar\ncfoo\n", "foo", false, "A;");
        assert_eq!(out, "afoo;\nbar\ncfoo;\n");
    }

    #[test]
    fn global_normal_v_and_bang_act_on_non_matching_lines() {
        // nvim: `:v/foo/normal A;` (and the equivalent `:g!/foo/normal A;`) append to the NON-matching line.
        let out = run_global_normal_oracle("afoo\nbar\ncfoo\n", "foo", true, "A;");
        assert_eq!(out, "afoo\nbar;\ncfoo\n");
    }

    #[test]
    fn global_normal_deletes_first_char_of_matching_lines() {
        // nvim: `:g/^#/normal 0x` deletes the first char of every line starting with "#".
        let out = run_global_normal_oracle("#one\ntwo\n#three\n", "^#", false, "0x");
        assert_eq!(out, "one\ntwo\nthree\n");
    }

    #[test]
    fn global_normal_stable_marks_when_line_count_grows() {
        // nvim: `:g/x/normal o-` inserts one "-" line UNDER each match — the stable two-pass shifts the
        // remaining marks past the inserted line, so it is not re-matched (each match gets exactly one dash).
        let out = run_global_normal_oracle("ax\nbx\ncy\n", "x", false, "o-");
        assert_eq!(out, "ax\n-\nbx\n-\ncy\n");
        // Every line matches → a dash under each, still no cascade.
        let out = run_global_normal_oracle("ax\nbx\n", "x", false, "o-");
        assert_eq!(out, "ax\n-\nbx\n-\n");
    }

    #[test]
    fn global_normal_dd_equals_global_delete() {
        // nvim: `:g/foo/normal dd` deletes every matching line — identical to `:g/foo/d`.
        let out = run_global_normal_oracle("afoo\nbar\ncfoo\n", "foo", false, "dd");
        assert_eq!(out, "bar\n");
        // Interleaved matches: the mark shift keeps later deletes on their ORIGINAL text (= `:g/a/d`).
        let out = run_global_normal_oracle("a1\nb\na2\nc\n", "a", false, "dd");
        assert_eq!(out, "b\nc\n");
    }

    #[test]
    fn global_normal_no_match_is_a_noop() {
        // No line matches → the buffer is untouched (nvim reports "Pattern not found").
        let out = run_global_normal_oracle("a\nb\nc\n", "zzz", false, "A;");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn needs_final_newline_only_when_fixeol_and_missing() {
        // Default (byte-preserve): never append, whatever the buffer looks like.
        assert!(
            !needs_final_newline(b"a", false),
            "fixeol off → never append"
        );
        assert!(!needs_final_newline(b"a\n", false));
        // `:set fixeol`: append only for a non-empty buffer that lacks a trailing newline.
        assert!(
            needs_final_newline(b"a", true),
            "fixeol on + missing newline → append"
        );
        assert!(
            needs_final_newline(b"a\nb", true),
            "fixeol on + missing final newline → append"
        );
        assert!(
            !needs_final_newline(b"a\n", true),
            "already ends in newline → no-op (idempotent)"
        );
        assert!(
            !needs_final_newline(b"", true),
            "empty buffer stays empty (Vim writes no newline into one)"
        );
    }

    /// A workspace whose focused buffer is the (new) file `name` in a fresh temp dir, edited once.
    fn ws_on_temp_file(name: &str, text: &[u8]) -> (Workspace, Files, PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ruse-dispatch-save-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        let mut ws = Workspace::new(text.to_vec());
        ws.apply(&Command::EnterInsert);
        ws.apply(&Command::InsertChar('x'));
        ws.apply(&Command::EnterNormal);
        let (mut files, mut hl): (Files, Highlighters) = (HashMap::new(), HashMap::new());
        let fmt = persist::encoding::FileFormat::detect(text);
        attach_file(&mut files, &mut hl, ws.focused_buffer(), path.clone(), fmt);
        (ws, files, path)
    }

    /// #936: when the directory fsync fails after the rename, `:w` does not report "written" and the buffer
    /// stays modified (it was "written" and clean before).
    #[cfg(unix)]
    #[test]
    fn save_with_a_failed_directory_fsync_is_not_saved() {
        let (mut ws, files, path) = ws_on_temp_file("f.txt", b"a\n");
        let mut status = crate::app::messages::Status::default();
        persist::atomic::inject(Some(persist::atomic::Step::SyncDir));
        let ok = save(&mut ws, &files, &mut status, false);
        persist::atomic::inject(None);
        assert!(!ok);
        assert!(status.starts_with("E667: Fsync failed"), "{status}");
        assert!(!status.ends_with("B written"), "{status}");
        assert!(ws.focused().doc.is_modified(), "not saved: still modified");
        assert_eq!(std::fs::read(&path).unwrap(), b"xa\n");
        // A later save that succeeds clears it.
        assert!(save(&mut ws, &files, &mut status, false));
        assert!(status.ends_with("written"), "{status}");
        assert!(!ws.focused().doc.is_modified());
    }

    /// A filesystem that does not support a directory fsync (EINVAL) saves: the buffer is clean and the
    /// journal cleared, and the report says the rename's durability is not proven instead of failing.
    #[cfg(unix)]
    #[test]
    fn save_where_directory_fsync_is_unsupported_is_saved_with_a_note() {
        let (mut ws, files, path) = ws_on_temp_file("f.txt", b"a\n");
        let mut status = crate::app::messages::Status::default();
        persist::atomic::inject_errno(persist::atomic::Step::SyncDir, libc::EINVAL);
        let ok = save(&mut ws, &files, &mut status, false);
        persist::atomic::inject(None);
        assert!(ok, "{status}");
        assert_eq!(
            status,
            format!(
                "\"{}\" 1L, 3B written (directory sync not supported here; rename durability not proven)",
                path.display()
            )
        );
        assert!(!ws.focused().doc.is_modified());
        assert_eq!(std::fs::read(&path).unwrap(), b"xa\n");
    }

    /// #872: `:set fixeol` adds the newline to the written file only, the buffer stays as typed and is clean,
    /// as in nvim v0.12.6 (`'eol'` stays off, `&modified` 0; a reload then reads the newline). ruse's buffer
    /// bytes play nvim's lines + `'eol'`, so a later `:set nofixeol` write omits the newline again, as nvim's.
    #[test]
    fn fixeol_writes_the_newline_without_changing_the_buffer() {
        let (mut ws, files, path) = ws_on_temp_file("n.txt", b"a");
        let mut status = crate::app::messages::Status::default();
        assert!(save(&mut ws, &files, &mut status, true));
        assert_eq!(status, format!("\"{}\" 1L, 3B written", path.display()));
        assert_eq!(std::fs::read(&path).unwrap(), b"xa\n");
        assert_eq!(ws.focused().doc.bytes(), b"xa", "the buffer is not edited");
        assert!(!ws.focused().doc.is_modified(), "and is clean");
        ws.apply(&Command::EnterInsert);
        ws.apply(&Command::InsertChar('y'));
        ws.apply(&Command::EnterNormal);
        assert!(save(&mut ws, &files, &mut status, false));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"yxa",
            "nofixeol: no newline"
        );
        assert!(status.contains("[noeol]"), "{status}");
    }

    /// Drive the ex executor END TO END for `:[range]j[oin][!]`: parse the raw line with `parse_ex` and run
    /// it through the session's Ex dispatch exactly as the run loop does. `parity_compare` cannot drive ex commands, so each
    /// expectation is the buffer nvim v0.12.4 produces for the SAME line (`nvim -u NONE`, confirmed by hand).
    fn run_ex_oracle(buf: &str, cursor0: usize, line: &str) -> (String, usize) {
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        ws.place_focused_cursor(BytePos(cursor0));
        ex_run(&mut ws, line);
        let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
        (bytes, ws.focused().view.cursor().0)
    }

    #[test]
    fn ex_join_drives_end_to_end_like_nvim() {
        // `:j` joins the current line + next on a single space; unlike `J` the cursor rests on the joined
        // line's first non-blank (nvim v0.12.6), not on the join seam.
        let (bytes, cur) = run_ex_oracle("foo\nbar\nbaz\n", 0, "j");
        assert_eq!(bytes, "foo bar\nbaz\n");
        assert_eq!(cur, 0);
        let (bytes, cur) = run_ex_oracle("x\n  foo\nbar\n", 3, "j");
        assert_eq!(bytes, "x\n  foo bar\n");
        assert_eq!(cur, 4);
        // `:j!` raw-concatenates (like `gJ`).
        let (bytes, _) = run_ex_oracle("foo\nbar\nbaz\n", 0, "j!");
        assert_eq!(bytes, "foobar\nbaz\n");
        // `:2,4j` collapses the three-line range into one.
        let (bytes, _) = run_ex_oracle("a\nb\nc\nd\ne\n", 0, "2,4j");
        assert_eq!(bytes, "a\nb c d\ne\n");
        // No space before a leading `)`.
        let (bytes, _) = run_ex_oracle("foo\n)bar\n", 0, "join");
        assert_eq!(bytes, "foo)bar\n");
        // A single-line range joins that line with the NEXT (Vim: a join needs two lines).
        let (bytes, _) = run_ex_oracle("a\nb\nc\nd\n", 0, "2j");
        assert_eq!(bytes, "a\nb c\nd\n");
    }

    #[test]
    fn match_count_message_matches_nvim_pluralization() {
        // Verified vs nvim v0.12.4 headless (`:%s/foo//gn` and friends).
        assert_eq!(match_count_message(6, 3), "6 matches on 3 lines");
        assert_eq!(match_count_message(2, 1), "2 matches on 1 line");
        assert_eq!(match_count_message(1, 1), "1 match on 1 line");
    }

    /// F-009: `:s///n` drives the executor END TO END — it must echo nvim's tally, and must NOT edit the
    /// buffer, move the cursor, dirty the file, or add an undo entry. The counts are what nvim v0.12.4
    /// reports for the identical `:%s/foo//gn` (`nvim -u NONE`, confirmed by hand).
    #[test]
    fn ex_substitute_count_only_reports_without_mutating() {
        let src = "foo bar foo\nfoo baz\nno match here\nfoo foo foo\n";
        let mut ws = Workspace::new(src.as_bytes().to_vec());
        ws.place_focused_cursor(BytePos(5)); // must not move
        let s = crate::app::session::Session::ex_on(&mut ws, "%s/foo//gn");
        assert_eq!(s.status, "6 matches on 3 lines", "nvim wording");
        assert!(
            s.confirm.is_none(),
            "`n` overrides `c` — no confirm loop opens"
        );
        assert_eq!(ws.focused().doc.bytes(), src.as_bytes(), "buffer unchanged");
        assert_eq!(ws.focused().view.cursor().0, 5, "cursor did not move");
        assert!(!ws.focused().doc.is_modified(), "buffer not dirtied");
        // No undo entry was created: an undo is a genuine no-op.
        ws.apply(&Command::Undo);
        assert_eq!(
            ws.focused().doc.bytes(),
            src.as_bytes(),
            "no undo entry from :s///n"
        );

        // No-match reports E486 (nvim), still without editing.
        let (status, _) = ex_run(&mut ws, "%s/zzz//gn");
        assert_eq!(status, "E486: Pattern not found: zzz");
        assert_eq!(
            ws.focused().doc.bytes(),
            src.as_bytes(),
            "no edit on a no-match count"
        );
    }

    /// Whether `program` resolves on PATH — skip a shell-dependent filter test when the tool is absent.
    #[cfg(unix)]
    fn has_program(program: &str) -> bool {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {program}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Drive the Normal-mode `!` filter operator END TO END: feed the operator keys (`op_keys`, e.g. `!G` /
    /// `!!` / `3!!`) into the input engine, take the resulting `Feed::FilterMotion`, resolve its line range
    /// and seed the `:{range}!` cmdline (the SAME glue `session::run` performs), type `cmd` + `<CR>` to get
    /// the finished ex line, then apply it through the SAME `:{range}!{cmd}` executor the run loop uses
    /// (`range_text` → `shell::filter` → `filter_lines`). Returns `(bytes, cursor)`. Every expectation below
    /// is the buffer nvim v0.12.4 produces for the identical keystrokes (verified headlessly by hand).
    #[cfg(unix)]
    fn run_filter_oracle(buf: &str, cursor0: usize, op_keys: &str, cmd: &str) -> (String, usize) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ruse_core::Mode;

        let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let mut engine = InputEngine::new();
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        ws.place_focused_cursor(BytePos(cursor0));

        // 1. Operator keys → the engine hands back the linewise motion + folded count.
        let mut feed = Feed::Ignored;
        for c in op_keys.chars() {
            feed = engine.feed(key(c), Mode::Normal);
        }
        let Feed::FilterMotion { count, motion } = feed else {
            panic!("`{op_keys}` should arm the filter operator, got {feed:?}");
        };

        // 2. Frontend glue (mirrors `session::run`): number the motion's lines, open the seeded cmdline.
        let (first, last) = ws
            .reindent_range(motion, count)
            .expect("filter motion spans at least one line");
        engine.open_filter_cmdline(&format!("{},{}!", first + 1, last + 1));

        // 3. Type the shell command, then <CR> to finish the ex line.
        for c in cmd.chars() {
            engine.feed(key(c), Mode::Normal);
        }
        let submitted = engine.feed(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Mode::Normal,
        );
        let Feed::ExecuteEx(line) = submitted else {
            panic!("submitting the filter cmdline should yield ExecuteEx, got {submitted:?}");
        };

        // 4. Run it through the SAME executor the run loop uses for `:{range}!{cmd}`.
        let Ex::Filter { range, cmd } = crate::input::parse_ex(&line) else {
            panic!("`{line}` should parse as Ex::Filter");
        };
        let input = ws.range_text(range).expect("range has text");
        let out = crate::shell::filter(&cmd, &input).expect("shell filter runs");
        ws.filter_lines(range, out.text.as_bytes());

        let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
        (bytes, ws.focused().view.cursor().0)
    }

    #[test]
    #[cfg(unix)]
    #[allow(clippy::print_stderr)] // test-only skip diagnostic when a POSIX tool is absent
    fn filter_operator_bang_motion_drives_end_to_end_like_nvim() {
        if !has_program("sort") {
            eprintln!("skipping: `sort` not on PATH");
            return;
        }
        // nvim: `!Gsort<CR>` from line 1 sorts the whole buffer; cursor lands on line 1 (byte 0).
        let (bytes, cur) = run_filter_oracle("3\n1\n2\n", 0, "!G", "sort");
        assert_eq!(bytes, "1\n2\n3\n");
        assert_eq!(
            cur, 0,
            "cursor lands on the first line of the filtered region"
        );

        // nvim: `!2jsort<CR>` on 5 lines filters lines 1-3 (`!2j` = current + 2 down), leaving 4,5 alone.
        let (bytes, cur) = run_filter_oracle("5\n4\n3\n2\n1\n", 0, "!2j", "sort");
        assert_eq!(bytes, "3\n4\n5\n2\n1\n");
        assert_eq!(cur, 0);

        // nvim: `!ipsort<CR>` filters only the inner paragraph (the blank line + "keep" survive).
        let (bytes, _) = run_filter_oracle("z\ny\nx\n\nkeep\n", 0, "!ip", "sort");
        assert_eq!(bytes, "x\ny\nz\n\nkeep\n");
    }

    #[test]
    #[cfg(unix)]
    #[allow(clippy::print_stderr)] // test-only skip diagnostic when a POSIX tool is absent
    fn filter_operator_double_bang_drives_end_to_end_like_nvim() {
        if !has_program("tr") || !has_program("cat") {
            eprintln!("skipping: `tr`/`cat` not on PATH");
            return;
        }
        // nvim: on line 2 (byte 4), `!!tr a-z A-Z<CR>` uppercases ONLY the current line; cursor stays line 2.
        let (bytes, cur) = run_filter_oracle("aaa\nbbb\nccc\n", 4, "!!", "tr a-z A-Z");
        assert_eq!(bytes, "aaa\nBBB\nccc\n");
        assert_eq!(cur, 4, "cursor on the first non-blank of the filtered line");

        // nvim: `3!!cat<CR>` on 4 lines is an identity filter over lines 1-3 (buffer unchanged); cursor line 1.
        let (bytes, cur) = run_filter_oracle("d\nc\nb\na\n", 0, "3!!", "cat");
        assert_eq!(bytes, "d\nc\nb\na\n");
        assert_eq!(cur, 0);

        // A count-CHANGING filter: `!!tr " " "\n"<CR>` splits "one two" into two lines (1 in, 2 out).
        let (bytes, cur) = run_filter_oracle("one two\n", 0, "!!", "tr \" \" \"\\n\"");
        assert_eq!(bytes, "one\ntwo\n");
        assert_eq!(cur, 0);
    }

    /// #1194: recover `rec` over the disk text `disk` as the session does, and check it is the disk text plus
    /// one change: modified, one `u` back to the clean disk text, and a saved trace that replays from the
    /// disk text to `rec` with the same single undo step. The commands recorded, for the caller to inspect.
    fn recovers_as_one_change(disk: &[u8], rec: &[u8]) -> Vec<Command> {
        let mut ws = Workspace::new(disk.to_vec());
        ws.focused_doc_mut().mark_saved();
        let mut recorded = Vec::new();
        apply_recovery(&mut ws, rec.to_vec(), &mut recorded);
        assert_eq!(ws.focused().doc.bytes(), rec);
        assert!(ws.focused().doc.is_modified(), "`:q` gives E37");
        ws.focused_doc_mut().undo();
        assert_eq!(
            ws.focused().doc.bytes(),
            disk,
            "one `u` reaches the disk text"
        );
        assert!(!ws.focused().doc.is_modified());
        let text = ruse_core::Trace::record(disk, recorded.clone()).to_text();
        let mut st = ruse_core::Trace::from_text(&text)
            .unwrap()
            .replay(disk)
            .unwrap();
        assert_eq!(st.doc.bytes(), rec, "the trace replays the recovery");
        st.doc.undo();
        assert_eq!(st.doc.bytes(), disk, "and replays it as one undo step");
        recorded
    }

    /// #1194's two remaining cases: an empty disk text (no line to filter: a read above line 1, then the
    /// final newline), and a non-UTF-8 change whose final newline differs (a filter, then a one-character
    /// edit). Both are one `ex_begin macro` group.
    #[test]
    fn a_recovery_the_line_filter_cannot_carry_is_still_one_change() {
        for (disk, rec) in [
            (&b""[..], &b"\xff\n"[..]),
            (b"", b"a\n\xff\n\n"),
            (b"a\n", b"\xff"),
            (b"a\nb\n", b"a\nb\xff"),
            (b"a", b"a\xff\n"),
            (b"caf\xe9", b"caf\xe9 \xff\n"),
        ] {
            let cmds = recovers_as_one_change(disk, rec);
            assert_eq!(cmds.len(), 4, "{disk:?} -> {rec:?}: {cmds:?}");
            assert_eq!(
                cmds[0],
                Command::Ex(ExCmd::Begin(ruse_core::GroupKind::Macro))
            );
            assert_eq!(cmds[3], Command::Ex(ExCmd::End));
        }
        // A UTF-8 change stays one edit, and a read or filter that leaves the right final newline is one
        // command, with no group.
        assert_eq!(recovers_as_one_change(b"", b"new\n").len(), 1);
        assert_eq!(recovers_as_one_change(b"", b"\xff").len(), 1);
        assert_eq!(
            recovers_as_one_change(b"caf\xe9\n", b"caf\xe9 au lait\n").len(),
            1
        );
    }

    mod recovery_fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(crate::test_cases(256))]

            /// #1194: every recovery, whatever the bytes, is the disk text plus one undoable change that a
            /// saved trace replays.
            #[test]
            fn every_recovery_is_the_disk_text_plus_one_change(
                disk in proptest::collection::vec(prop_oneof![Just(b'a'), Just(b'\n'), Just(0xC3u8), Just(0xA9u8), Just(0xFFu8), Just(0x80u8)], 0..16),
                rec in proptest::collection::vec(prop_oneof![Just(b'b'), Just(b'a'), Just(b'\n'), Just(0xC3u8), Just(0xA9u8), Just(0xFFu8)], 0..16),
            ) {
                prop_assume!(disk != rec);
                recovers_as_one_change(&disk, &rec);
            }
        }
    }
}
