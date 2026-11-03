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
    Command, DocumentId, Effect, Mode, SplitDir, SubRange, Trace, TransactionOrigin, Workspace,
};

use crate::highlight;
use crate::input::{BufTarget, Ex, Feed, InputEngine};
use crate::persist;
use crate::ui::prompts::{prompt_recovery, Confirm, RecoveryChoice};

/// The per-buffer syntax-highlighter registry (F-007 / F-015), keyed by [`DocumentId`]. Shared by the
/// session loop and the LSP coordinator (both open buffers that need a highlighter).
pub(crate) type Highlighters = HashMap<DocumentId, highlight::CachedHighlight>;

/// What [`open_file_into_buffer`] did: the status line to show, and the buffer now focused on `file`
/// (`None` when nothing was opened — a read error, or the user kept a recovery journal and backed out).
pub(crate) struct Opened {
    pub(crate) status: String,
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
    let p = PathBuf::from(file);
    if let Some(id) = buffer_for_path(files, &p) {
        ws.focus_buffer(id);
        return Opened {
            status: format!("\"{file}\" (already open)"),
            id: Some(id),
        };
    }
    let raw = match persist::read_file(&p) {
        Ok(raw) => raw,
        Err(e) => {
            return Opened {
                status: format!("E484: can't open {file}: {e}"),
                id: None,
            }
        }
    };
    let f = persist::encoding::FileFormat::detect(&raw);
    let disk = f.to_buffer(&raw);
    let (journaled, journal_error) = match persist::journal::replay_checked(Some(&p)) {
        Ok(r) => (r, None),
        Err(e) => (None, Some(persist::journal::unreadable_note(Some(&p), &e))),
    };
    let recovered = match persist::assess_recovery(&disk, journaled.as_deref()) {
        persist::Recovery::None => None,
        persist::Recovery::Available(rec) => {
            let note = persist::journal::is_stale(&p).then_some(persist::journal::STALE_NOTE);
            match prompt_recovery(&mut io::stdout(), "q: cancel the open and keep them", note) {
                Ok(RecoveryChoice::Recover) => Some(rec),
                Ok(RecoveryChoice::Discard) => {
                    persist::journal::clear(Some(&p)); // discarded on the user's say-so
                    None
                }
                Ok(RecoveryChoice::Quit) | Err(_) => {
                    return Opened {
                        status: format!(
                            "\"{file}\" not opened: recovery journal kept ({})",
                            persist::journal::journal_path(Some(&p)).display()
                        ),
                        id: None,
                    }
                }
            }
        }
    };
    let id = ws.add_buffer(disk, Some(file.to_string()));
    ws.focus_buffer(id);
    let status = match recovered {
        Some(rec) => {
            // Apply the recovered text as an edit over the disk version, so the buffer is modified (a
            // `:q` refuses) and `u` returns to what is on disk. Non-UTF-8 bytes cannot travel through
            // a text edit; load them as the buffer instead, as the startup path does.
            match String::from_utf8(rec) {
                Ok(text) => {
                    let len = ws.focused().doc.bytes().len();
                    ws.apply_edits(&[(0, len, text)], TransactionOrigin::UserInput);
                }
                Err(e) => ws.reload_focused(e.into_bytes()),
            }
            ws.place_focused_cursor(0);
            format!("\"{file}\" recovered unsaved changes (:w to save)")
        }
        None => match journal_error {
            Some(note) => format!("\"{file}\" {note}"),
            None => format!("\"{file}\" {} bytes", raw.len()),
        },
    };
    if let Some(h) = p
        .extension()
        .and_then(|e| e.to_str())
        .and_then(highlight::CachedHighlight::for_ext)
    {
        highlighters.insert(id, h);
    }
    files.insert(id, BufferFile { path: p, fmt: f });
    Opened {
        status,
        id: Some(id),
    }
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
}

/// The per-buffer file registry: which buffers have a file on disk (a scratch `:enew` buffer has none, so
/// `:w` on it declines). Replaces the old single `path`/`fmt`/`file_buf` triple (F-007 multi-buffer).
pub(crate) type Files = HashMap<DocumentId, BufferFile>;

/// Record a command and apply it to the focused window, performing any effects.
pub(crate) fn run_cmd(
    cmd: Command,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    if let Some(err) = put_error(&cmd, ws).or_else(|| mark_error(&cmd, ws)) {
        // Nothing runs; a `"x` the command would have read is spent, as in Vim.
        *status = err;
        ws.apply(&Command::SetRegister(None));
        return;
    }
    recorded.push(cmd.clone());
    for eff in ws.apply(&cmd) {
        apply_effect(eff, ws, files, status, quit);
    }
}

/// `E20` for a jump to, or an operator over, an unset named mark (`'a`, `` `a ``, `d'a`): nothing runs.
fn mark_error(cmd: &Command, ws: &Workspace) -> Option<String> {
    let name = match cmd {
        Command::GotoNamedMark(c) | Command::GotoNamedMarkLine(c) => *c,
        Command::OpToMark { name, .. } => *name,
        _ => return None,
    };
    (name.is_ascii_lowercase() && ws.focused().view.named_mark(name).is_none())
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
    if matches!(name, Some('_' | '+' | '*' | '=')) || !ws.register_bytes(name).is_empty() {
        return None;
    }
    Some(match name {
        Some(':') => "E30: No previous command line".into(),
        Some('/') => "E35: No previous regular expression".into(),
        Some('.') => "E29: No inserted text yet".into(),
        Some('%') => "E32: No file name".into(),
        Some(c) => format!("E353: Nothing in register {c}"),
        None => "E353: Nothing in register \"".into(),
    })
}

/// Execute `:[range]normal[!] {keys}` (`:help :normal`). The key payload is resolved to key events and fed
/// through the SAME input→`Command` pipeline the macro-replay path uses (`engine.feed` → [`run_cmd`]) — this
/// is "replay this key string once (per line)", not a second key interpreter. With no range it runs ONCE at
/// the current cursor (column preserved); with a range it runs once per line, cursor at column 0 of each line.
///
/// Line iteration matches Vim exactly (verified against nvim v0.12.4): the loop runs over the ORIGINAL
/// `[line1, line2]`, and each step targets `min(lnum, last_line)` of the CURRENT buffer — so `:%normal dd`
/// deletes every line, and `:%normal o-` appends one line per original line (both reproduced in tests). After
/// each line an implicit `<Esc>` terminates any open Insert/pending command, forcing Normal (Vim's rule).
///
/// `bang` (`:normal!`, ignore user mappings) is accepted but inert: this editor has no user remaps yet.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_normal(
    engine: &mut InputEngine,
    ws: &mut Workspace,
    range: Option<SubRange>,
    keys: &str,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    let events = crate::keys::parse_normal_keys(keys);
    match range {
        // No range: run once at the cursor, column preserved (Vim `:normal {keys}`).
        None => run_normal_line(engine, ws, &events, files, recorded, status, quit),
        Some(range) => {
            let (line1, line2) = resolve_normal_range(ws, range);
            for lnum in line1..=line2 {
                // Place the cursor at column 0 of the target line — clamped to the CURRENT last line, so a
                // range that outlives shrinking edits (`:%normal dd`) keeps operating on the surviving lines.
                let snapshot = ws.focused().doc.text_arc();
                let last = line_count(&snapshot).max(1);
                let target = lnum.min(last);
                let off = ruse_core::pos::nth_line_start(&snapshot, target - 1);
                ws.place_focused_cursor(off);
                run_normal_line(engine, ws, &events, files, recorded, status, quit);
            }
        }
    }
}

/// Execute `:g/pat/normal[!] {keys}` (`:help :g`) as a frontend two-pass. PASS 1 marks every matching (or,
/// for `:v` / `:g!`, non-matching) line via [`Workspace::global_marks`] against the untouched buffer; PASS 2
/// replays `{keys}` as Normal-mode input on each marked line through the SAME per-line runner `:normal` uses
/// ([`run_normal_line`] → `engine.feed` → [`run_cmd`], cursor at column 0, implicit `<Esc>` after).
///
/// The marks are STABLE the way Vim's are (verified against nvim v0.12.4): as a line's `{keys}` grow or
/// shrink the buffer, every not-yet-processed mark shifts by that line's net line-count delta — so lines a
/// `normal` INSERTS are never re-matched (`:g/x/normal o-` drops one line under EACH match, not a cascade),
/// and later marks still target their original text after a DELETE (`:g/pat/normal dd` equals `:g/pat/d`).
///
/// `bang` (`:g/pat/normal!`) is accepted but inert — this editor has no user remaps yet. Writes a Vim-style
/// status: "Pattern not found" when the pattern marks nothing, a regex error, else "N lines changed".
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_global_normal(
    engine: &mut InputEngine,
    ws: &mut Workspace,
    range: SubRange,
    pattern: &str,
    negate: bool,
    keys: &str,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    // PASS 1: the 1-based line numbers to act on (ascending), computed against the untouched buffer.
    let mut marks = match ws.global_marks(range, pattern, negate) {
        Ok(m) => m,
        Err(e) => {
            *status = regex_error_msg(&e);
            return;
        }
    };
    if marks.is_empty() {
        // Vim only echoes this for `:g` (no `E486`).
        *status = format!("Pattern not found: {pattern}");
        return;
    }
    let acted = marks.len();
    let events = crate::keys::parse_normal_keys(keys);
    // PASS 2: replay the keys on each marked line, shifting the remaining marks by each line's net delta.
    for i in 0..marks.len() {
        let lnum = marks[i];
        // Place the cursor at column 0 of the (shift-adjusted) target line, clamped to the CURRENT last
        // line so a mark that outlives shrinking edits still lands on a live line.
        let snapshot = ws.focused().doc.text_arc();
        let before = line_count(&snapshot).max(1);
        let target = lnum.min(before);
        let off = ruse_core::pos::nth_line_start(&snapshot, target - 1);
        ws.place_focused_cursor(off);
        drop(snapshot);
        run_normal_line(engine, ws, &events, files, recorded, status, quit);
        // Shift every not-yet-processed mark by this line's net line-count change (Vim's stable marks):
        // inserted lines push later marks down; deleted lines pull them up — so nothing is re-processed.
        let after = line_count(&ws.focused().doc.text_arc()).max(1);
        let delta = after as isize - before as isize;
        if delta != 0 {
            for m in marks.iter_mut().skip(i + 1) {
                *m = (*m as isize + delta).max(1) as usize;
            }
        }
    }
    *status = format!("{acted} lines changed");
}

/// Resolve a `:normal` [`SubRange`] to an INCLUSIVE 1-based `(line1, line2)`. `%` = the whole file, `.`/none
/// = the cursor line, `N,M` = the numbers clamped to the buffer. The end is clamped to the current line
/// count; the START is not, so an out-of-range start (`line1 > line2`) simply runs zero iterations.
fn resolve_normal_range(ws: &Workspace, range: SubRange) -> (usize, usize) {
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

/// Run one `:normal` pass over `events` at the current cursor, then apply the implicit `<Esc>` (Vim's rule:
/// an unterminated Insert is auto-closed and any pending command is aborted, leaving Normal mode). Each key
/// is fed with the LIVE mode (re-read after every command, so `i`→Insert routes the next keys as text).
fn run_normal_line(
    engine: &mut InputEngine,
    ws: &mut Workspace,
    events: &[KeyEvent],
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    for &ev in events {
        let mode = ws.focused().view.mode();
        let feed = engine.feed(ev, mode);
        drive_feed(feed, ws, files, recorded, status, quit);
    }
    // Implicit `<Esc>` — terminate insert / abort any pending command through the engine (so dot-repeat and
    // insert sessions close exactly as a typed Esc would).
    let mode = ws.focused().view.mode();
    let feed = engine.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), mode);
    drive_feed(feed, ws, files, recorded, status, quit);
    // Belt-and-suspenders: guarantee Normal even if the engine leaves a non-Insert non-Normal mode (Visual)
    // on Esc — the per-line invariant `:normal` relies on for the next iteration's column-0 placement.
    if ws.focused().view.mode() != Mode::Normal {
        run_cmd(Command::EnterNormal, ws, files, recorded, status, quit);
    }
}

/// Apply one engine outcome during a `:normal` run. A finished `Command` (or a dot-repeat `Replay` list) is
/// applied via [`run_cmd`], exactly as the event loop does; `Pending`/`Ignored` are absorbed. A nested ex
/// line (`:normal :…<CR>`) is NOT executed in this MVP — a rare form, deliberately left inert.
fn drive_feed(
    feed: Feed,
    ws: &mut Workspace,
    files: &Files,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    match feed {
        Feed::Cmd(cmd) => run_cmd(cmd, ws, files, recorded, status, quit),
        Feed::Replay(cmds) => {
            for cmd in cmds {
                run_cmd(cmd, ws, files, recorded, status, quit);
            }
        }
        // Nested ex from `:normal` is out of scope for this slice; Pending/Ignored are absorbed. A cmdline
        // `c_CTRL-R` word-insert has no interactive command line in this replay context, so it is inert too;
        // the `!{motion}` filter operator likewise needs the interactive `:{range}!` cmdline, so it is inert.
        Feed::ExecuteEx(_)
        | Feed::CmdlineInsertUnder { .. }
        | Feed::FilterMotion { .. }
        | Feed::FilterSpan(_)
        | Feed::CountCmdline { .. }
        | Feed::Pending
        | Feed::Ignored => {}
    }
}

/// The unsaved-changes check of every quit that would end the editor (`:q`, `C-w q`, `:wq`/`:x`, `ZZ`/`ZQ`
/// on the last window, `:qa`): refuse with E37 when the focused buffer is modified, or E162 when another
/// (hidden) one is. `abandon_focused` (`:q!`, `ZQ`) skips only the focused buffer's check; a modified
/// hidden buffer still refuses, as in nvim. Returns whether to quit.
pub(crate) fn may_quit(ws: &Workspace, status: &mut String, abandon_focused: bool) -> bool {
    let focused = ws.focused_buffer();
    if !abandon_focused && ws.focused().doc.is_modified() {
        *status = "E37: No write since last change (add ! to override)".into();
        false
    } else if let Some(b) = ws
        .buffers()
        .into_iter()
        .find(|b| b.modified && b.id != focused)
    {
        *status = format!("E162: No write since last change for buffer \"{}\"", b.name);
        false
    } else {
        true
    }
}

/// An ex line whose leading range [`resolve_ex_range`] turned into plain line numbers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExRange {
    /// The line to parse: its range, if any, is now `N` or `N,M` (1-based), or still `%`.
    Line(String),
    /// A range and no command (`:5`, `:$`, `:.+2`): go to that line, clamped to the buffer.
    Goto(usize),
}

/// Why [`resolve_ex_range`] refused a range.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ExRangeError {
    /// An address above the last line or below 0 (`E16`).
    InvalidRange,
    /// `'<`/`'>` before any Visual selection ended (`E20`).
    MarkNotSet,
}

impl std::fmt::Display for ExRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExRangeError::InvalidRange => "E16: Invalid range",
            ExRangeError::MarkNotSet => "E20: Mark not set",
        })
    }
}

/// Resolve the leading range of an ex line (`:help :range`) against the focused buffer, so the numeric
/// range parser sees `N` or `N,M`. An address is a line number, `.` (the cursor's line), `$` (the last
/// line) or `'<`/`'>` (the last Visual selection, `:help v_:`), followed by any `+N`/`-N` offsets; a bare
/// offset counts from the cursor's line. Addresses are separated by `,`, or by `;`, which makes the one
/// before it the current line for the next (`:2;+1`). A missing address after a separator is the cursor's
/// line. Of more than two addresses the last two count.
///
/// As in Vim, an address above the last line or below 0 is `E16: Invalid range` for every range-taking
/// command (#1079), while a range with no command just goes to the line, clamped. A line without a range
/// comes back unchanged. Other marks (`'a`) are left for the parser.
pub(crate) fn resolve_ex_range(line: &str, ws: &Workspace) -> Result<ExRange, ExRangeError> {
    let rest = line.trim_start_matches([' ', ':']);
    let pane = ws.focused();
    let b = pane.doc.bytes();
    let nlines = (b.split(|&c| c == b'\n').count() - usize::from(b.ends_with(b"\n"))).max(1) as i64;
    let mut cur_line = (ruse_core::pos::line_of(b, pane.view.cursor()) as i64 + 1).min(nlines);
    let visual = pane.view.last_visual_bounds().map(|(s, e, _)| {
        (
            ruse_core::pos::line_of(b, s) as i64 + 1,
            ruse_core::pos::line_of(b, e) as i64 + 1,
        )
    });
    let s = rest.as_bytes();
    let digits = |i: &mut usize| -> Option<i64> {
        let start = *i;
        while s.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        (start < *i).then(|| rest[start..*i].parse().unwrap_or(i64::MAX))
    };
    let mut addrs: Vec<i64> = Vec::new();
    let mut i = 0;
    loop {
        while s.get(i) == Some(&b' ') {
            i += 1;
        }
        let start = i;
        let base = match s.get(i) {
            Some(b'0'..=b'9') => digits(&mut i),
            Some(b'.') => {
                i += 1;
                Some(cur_line)
            }
            Some(b'$') => {
                i += 1;
                Some(nlines)
            }
            Some(b'\'') if matches!(s.get(i + 1), Some(b'<' | b'>')) => {
                let Some((first, last)) = visual else {
                    return Err(ExRangeError::MarkNotSet);
                };
                i += 2;
                Some(if s[i - 1] == b'<' { first } else { last })
            }
            Some(b'\'') => return Ok(ExRange::Line(rest.to_string())),
            _ => None,
        };
        let mut v = base.unwrap_or(cur_line);
        let mut have = base.is_some();
        while let Some(&sign @ (b'+' | b'-')) = s.get(i) {
            i += 1;
            let n = digits(&mut i).unwrap_or(1);
            v = if sign == b'+' {
                v.saturating_add(n)
            } else {
                v.saturating_sub(n)
            };
            have = true;
        }
        if !have && addrs.is_empty() && i == start {
            break; // no range at all
        }
        addrs.push(v);
        while s.get(i) == Some(&b' ') {
            i += 1;
        }
        match s.get(i) {
            Some(b',') => i += 1,
            Some(b';') => {
                i += 1;
                cur_line = v.clamp(1, nlines);
            }
            _ => break,
        }
        // A separator with nothing after it (`:5,d`) closes the range on the cursor's line.
        if !matches!(
            s.get(i),
            Some(b'0'..=b'9' | b'.' | b'$' | b'\'' | b'+' | b'-' | b' ')
        ) {
            addrs.push(cur_line);
            break;
        }
    }
    let Some(&last) = addrs.last() else {
        return Ok(ExRange::Line(rest.to_string()));
    };
    let tail = &rest[i..];
    if tail.trim().is_empty() {
        // Going to a line clamps past the end (`:99`), but not below the start (`:-5` on line 3 is E16).
        if last < 0 {
            return Err(ExRangeError::InvalidRange);
        }
        return Ok(ExRange::Goto(last.clamp(1, nlines) as usize));
    }
    let used = &addrs[addrs.len().saturating_sub(2)..];
    if used.iter().any(|&a| a < 0 || a > nlines) {
        return Err(ExRangeError::InvalidRange);
    }
    let range = match used {
        [a, b] => format!("{a},{b}"),
        _ => last.to_string(),
    };
    Ok(ExRange::Line(format!("{range}{tail}")))
}

// The ex-line dispatcher legitimately needs the full editor context (buffer, file identity+format,
// the trace baseline+recording, and the status/quit sinks); grouping them into a struct would only
// move the wiring, not reduce it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_ex(
    ex: &Ex,
    ws: &mut Workspace,
    files: &Files,
    initial: &[u8],
    recorded: &[Command],
    status: &mut String,
    quit: &mut bool,
    confirm: &mut Option<Confirm>,
    fixeol: bool,
) {
    match ex {
        Ex::Save => {
            save(ws, files, status, fixeol);
        }
        Ex::Quit => quit_window(ws, status, quit, false),
        Ex::QuitForce => quit_window(ws, status, quit, true),
        // `:wq` / `:x` close the window only after a successful save (#842): no file, or a failed write,
        // keeps it open. `:x` writes only a modified buffer.
        Ex::SaveQuit => {
            if save(ws, files, status, fixeol) {
                quit_window(ws, status, quit, false);
            }
        }
        Ex::ExitWrite => {
            if !ws.focused().doc.is_modified() || save(ws, files, status, fixeol) {
                quit_window(ws, status, quit, false);
            }
        }
        Ex::QuitAll { force: true } => *quit = true,
        Ex::QuitAll { force: false } => *quit = may_quit(ws, status, false),
        Ex::CQuit(code) => {
            EXIT_STATUS.store(*code, Ordering::Relaxed);
            *quit = true;
        }
        Ex::Hide => {
            if !ws.close_focused() {
                *status = "E444: Cannot close last window".into();
            }
        }
        // F-007 window commands: split the focused window onto the same buffer, or close it.
        Ex::Split => {
            ws.split(SplitDir::Horizontal);
            *status = "window split".into();
        }
        Ex::VSplit => {
            ws.split(SplitDir::Vertical);
            *status = "window vsplit".into();
        }
        Ex::Close => {
            if !ws.close_focused() {
                *status = "E444: Cannot close last window".into();
            }
        }
        Ex::Only => {
            let closed = ws.only();
            *status = match closed {
                0 => "already one window".into(),
                1 => "1 window closed".into(),
                n => format!("{n} windows closed"),
            };
        }
        Ex::SaveTrace(p) => {
            let trace = Trace::record(initial, recorded.to_vec());
            match fs::write(p, trace.to_text()) {
                Ok(()) => *status = format!("trace saved: {p} ({} commands)", recorded.len()),
                Err(e) => *status = format!("trace save failed: {e}"),
            }
        }
        // `:[range]s/pat/rep/flags` (F-009 #2). The `c` (confirm) flag is the interactive loop (PR-c2);
        // for now it is declined rather than silently applied.
        Ex::Substitute(spec) => {
            let flags = ruse_core::SubFlags {
                global: spec.global,
                ignore_case: spec.ignore_case,
            };
            if spec.count_only {
                // `n`: REPORT ONLY — count the matches and echo the tally, editing nothing (Vim `:s///n`).
                // Overrides `c` (like Vim). `substitute_count` borrows `&`, so no edit / undo / cursor move.
                *status = match ws.substitute_count(spec.range, &spec.pattern, flags) {
                    Ok(out) if out.replacements == 0 => {
                        format!("E486: pattern not found: {}", spec.pattern)
                    }
                    Ok(out) => match_count_message(out.replacements, out.lines),
                    Err(e) => regex_error_msg(&e),
                };
            } else if spec.confirm {
                // `c`: compute the matches and hand control to the interactive confirm loop (F-009 #2).
                match ws.substitute_preview(spec.range, &spec.pattern, &spec.replacement, flags) {
                    Ok(subs) if subs.is_empty() => {
                        *status = format!("E486: pattern not found: {}", spec.pattern);
                    }
                    Ok(subs) => *confirm = Some(Confirm::new(subs)),
                    Err(e) => *status = regex_error_msg(&e),
                }
            } else {
                *status = match ws.substitute(spec.range, &spec.pattern, &spec.replacement, flags) {
                    Ok(out) if out.replacements == 0 && spec.quiet => String::new(),
                    Ok(out) if out.replacements == 0 => {
                        format!("E486: pattern not found: {}", spec.pattern)
                    }
                    Ok(out) => format!("{} substitutions on {} lines", out.replacements, out.lines),
                    Err(e) => regex_error_msg(&e),
                };
            }
        }
        // Bare `:s` / `:s {flags}` / `:&` / `:&&` — repeat the last `:s`. Resolved in the run loop against the
        // last-substitute history (like `&`/`g&`), which `run_ex` cannot see; a no-op here.
        Ex::RepeatSubstitute { .. } => {}
        // `:[range]d` — delete the range's lines (no range = the current line) as one undo group.
        Ex::Delete(range) => {
            let n = ws.delete_lines(*range);
            *status = if n == 1 {
                "1 line deleted".into()
            } else {
                format!("{n} lines deleted")
            };
        }
        // `:[range]y` — yank the range's lines linewise into the unnamed register (like `yy`).
        Ex::Yank(range) => {
            let n = ws.yank_lines(*range);
            *status = if n == 1 {
                "1 line yanked".into()
            } else {
                format!("{n} lines yanked")
            };
        }
        // `:[range]j[oin][!]` — join the range's lines into one, reusing the core `J`/`gJ` join (no range =
        // the current line, which joins with the next). `bang` is the raw `gJ` form.
        Ex::Join { range, bang } => {
            let n = ws.join_lines(*range, *bang);
            *status = if n <= 1 {
                String::new()
            } else {
                format!("{n} lines joined")
            };
        }
        // `:[range]m {addr}` — move the range's lines to after the destination line.
        Ex::Move(_, dest) | Ex::Copy(_, dest) if !dest_in_range(ws, *dest) => {
            *status = "E16: Invalid range".into();
        }
        Ex::Move(range, dest) => {
            *status = match ws.move_lines(*range, *dest) {
                Some(1) => "1 line moved".into(),
                Some(n) => format!("{n} lines moved"),
                None => "E134: Cannot move a range of lines into itself".into(),
            };
        }
        // `:[range]t {addr}` / `:copy` — copy the range's lines to after the destination line.
        Ex::Copy(range, dest) => {
            *status = match ws.copy_lines(*range, *dest) {
                Some(1) => "1 line copied".into(),
                Some(n) => format!("{n} lines copied"),
                None => "E486: invalid copy".into(),
            };
        }
        // `:[range]>` / `:[range]<` — shift the range's lines one indent level per repeated verb char,
        // reusing the core `>>`/`<<` shift (no range = the current line).
        Ex::Shift {
            range,
            left,
            levels,
        } => {
            let n = ws.shift_lines(*range, *left, *levels);
            *status = if n == 1 {
                String::new()
            } else {
                format!("{n} lines {}ed 1 time", if *left { '<' } else { '>' })
            };
        }
        // `:[line]put [reg]` — put a register's text LINEWISE as new whole line(s) after the addressed
        // line (a charwise register is still put as whole lines — the linewise-forcing rule).
        Ex::Put { addr, reg } => {
            let n = ws.put_lines(*addr, *reg);
            *status = if n == 1 {
                "1 more line".into()
            } else if n > 1 {
                format!("{n} more lines")
            } else {
                String::new()
            };
        }
        // `:[range]sort[!] [i][n][r][u] [/pattern/]` — sort the range's lines (whole file with no range).
        Ex::Sort(range, spec) => {
            let removed = ws.sort_lines(*range, spec);
            *status = if removed > 0 {
                format!("sorted, {removed} fewer lines")
            } else {
                "sorted".into()
            };
        }
        // `:set {option}` — set one editor option on the focused view.
        Ex::Set(opt) => {
            ws.set_option(*opt);
            *status = format!("{opt:?}");
        }
        // `:[range]g/pat/cmd` (F-009 #4): two-pass mark-then-execute over the focused window. The `d`/`s`
        // payloads run in core here; a `normal` payload drives the input engine and is handled in the run
        // loop (`run_global_normal`), never reaching here — a `.` no-op keeps the match exhaustive.
        Ex::Global(spec) => match &spec.cmd {
            crate::input::GlobalPayload::Core(cmd) => {
                *status = match ws.global(spec.range, &spec.pattern, spec.negate, cmd) {
                    // Not an error in Vim: `:g` with no match only echoes the message (no `E486`).
                    Ok(0) => format!("Pattern not found: {}", spec.pattern),
                    Ok(n) => format!("{n} lines changed"),
                    Err(e) => regex_error_msg(&e),
                };
            }
            crate::input::GlobalPayload::Normal { .. } => {}
        },
        // `:noh` is handled in the run loop (it clears the frontend's search highlight); never reaches here.
        Ex::NoHighlight => {}
        // `:set (no)hlsearch/incsearch` are handled in the run loop (frontend render flags); never here.
        Ex::SetHlSearch(_) | Ex::SetIncSearch(_) => {}
        // `:set (no)fixeol` is handled in the run loop (a frontend write flag threaded into `save`); never here.
        Ex::SetFixEol(_) => {}
        // `:set (no)tildeop` is handled in the run loop (an input-engine option); never here.
        Ex::SetTildeOp(_) => {}
        // `:set foldmethod/foldlevel/foldenable` are handled in the run loop (frontend fold state); never here.
        Ex::SetFoldMethod(_) | Ex::SetFoldLevel(_) | Ex::SetFoldEnable(_) => {}
        // `:registers` is handled in the run loop (it opens the frontend-owned register-viewer picker).
        Ex::Registers => {}
        // `:digraphs` is handled in the run loop (it opens the frontend-owned digraph-listing picker).
        Ex::Digraphs => {}
        // `:marks` is handled in the run loop (it opens the frontend-owned marks-viewer picker).
        Ex::Marks => {}
        // `:jumps` / `:changes` are handled in the run loop (frontend-owned position-viewer picker).
        Ex::Jumps | Ex::Changes => {}
        // `:ascii`/`:as` is handled in the run loop (it reads the focused buffer + cursor and sets status).
        Ex::Ascii => {}
        // `:lmap`/`:lunmap` are handled in the run loop (they mutate engine-owned Lang-Arg state — the
        // `engine` this fn does not borrow); never reach here.
        Ex::Lmap { .. } | Ex::Lunmap { .. } => {}
        // `:checkhealth` is handled in the run loop (it reads the terminal-cap ledger + profile this fn
        // does not borrow); never reaches here.
        Ex::CheckHealth => {}
        // `:e {file}` / `:e!` are handled in the run loop (they read the file + mutate the buffer and the
        // `files`/highlighter registries this fn only borrows immutably); never reach here.
        Ex::Edit(_) | Ex::EditReload => {}
        // `:r`/`:read`, the `:{range}!` filter, and `:!` are handled in the run loop: they do frontend file
        // IO / shell-out (which `run_ex`, a pure caller over `Workspace`, must not do); never reach here.
        Ex::Read { .. } | Ex::Filter { .. } | Ex::Shell(_) => {}
        // `:bd` is handled in the run loop (it drops the deleted buffer's `files`/highlighter entries,
        // which this fn only borrows immutably); never reaches here.
        Ex::BufferDelete { .. } => {}
        // `:earlier`/`:later` are handled in the run loop (they record + apply undo-time commands into
        // the `recorded` stream this fn only borrows immutably); never reach here.
        Ex::Earlier(_) | Ex::Later(_) => {}
        // F-007 multi-buffer navigation. `:enew` opens a scratch buffer and focuses it; `:ls` lists the
        // buffers; `:bn`/`:bp` cycle; `:b {n}`/`:b#` switch by number / to the alternate.
        Ex::Enew => {
            let id = ws.add_buffer(Vec::new(), None);
            ws.focus_buffer(id);
            *status = format!("[No Name] (buffer {})", id.0);
        }
        Ex::Buffers => *status = buffer_list_line(ws),
        Ex::BufferNext => {
            ws.cycle_buffer(true);
            *status = focused_buffer_label(ws);
        }
        Ex::BufferPrev => {
            ws.cycle_buffer(false);
            *status = focused_buffer_label(ws);
        }
        Ex::Buffer(target) => {
            let id = match target {
                BufTarget::Number(n) => Some(ruse_core::DocumentId(*n)),
                BufTarget::Alternate => ws.alternate(),
            };
            match id {
                Some(id) if ws.focus_buffer(id) => *status = focused_buffer_label(ws),
                Some(id) => *status = format!("E86: buffer {} does not exist", id.0),
                None => *status = "E23: no alternate buffer".into(),
            }
        }
        // `:terminal` (F-011) / `:fmt` / `:rename` / `:references` (F-014) / `:trust` (D-058) are handled in
        // `session::run`
        // (they need the terminals / lsp maps); they never reach here, but the match stays exhaustive.
        Ex::Terminal
        | Ex::Format
        | Ex::Rename(_)
        | Ex::References
        | Ex::CodeAction
        | Ex::Diagnostics
        | Ex::Trust => {}
        // `:[range]normal` is handled in the run loop (it drives the input `engine`, which this fn does not
        // borrow, to replay the keys through the same pipeline); never reaches here.
        Ex::Normal { .. } => {}
        Ex::Unknown(s) => *status = format!("E492: Not an editor command: {s}"),
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

/// Human-readable status for a regex compile error (F-009).
pub(crate) fn regex_error_msg(e: &ruse_core::RegexError) -> String {
    match e {
        ruse_core::RegexError::Unsupported(m) => format!("E: unsupported pattern: {m}"),
        ruse_core::RegexError::Syntax(m) => format!("E: bad pattern: {m}"),
    }
}

/// The process exit status a `:cquit` asked for; 0 otherwise. Read by the CLI once the session returns.
static EXIT_STATUS: AtomicU8 = AtomicU8::new(0);

/// The exit status the session should end with (`:cq [N]` sets it; a normal quit leaves it 0).
pub(crate) fn exit_status() -> u8 {
    EXIT_STATUS.load(Ordering::Relaxed)
}

/// Whether a `:m`/`:t` destination lands inside the focused buffer. A relative one (`:m+5`, `:m$-9`) can
/// fall off either end, which Vim refuses with E16 rather than clamping.
fn dest_in_range(ws: &Workspace, dest: ruse_core::LineAddr) -> bool {
    let b = ws.focused().doc.bytes();
    let cursor = ws.focused().view.cursor().min(b.len());
    let cursor_line = b[..cursor].iter().filter(|&&c| c == b'\n').count();
    let nlines = b.split(|&c| c == b'\n').count() - usize::from(b.ends_with(b"\n"));
    // An absolute line past the end is E16 too (`:t99`, #801), not the end of the buffer.
    let absolute_ok = !matches!(dest, ruse_core::LineAddr::Line(n) if n > nlines.max(1));
    absolute_ok && dest.insert_index(cursor_line, nlines).is_some()
}

/// `:q` (Vim `:quit`): with other windows open, close the focused one — its buffer stays loaded and
/// hidden even when modified (#736; nvim's 'hidden' is on by default). On the last window, end the editor
/// through [`may_quit`]. `force` (`:q!`, `ZQ`) lets the FOCUSED buffer's changes go; a modified hidden
/// buffer still refuses, as in nvim, so `:q!` can never silently drop work the user cannot see.
pub(crate) fn quit_window(ws: &mut Workspace, status: &mut String, quit: &mut bool, force: bool) {
    if ws.close_focused() {
        return;
    }
    *quit = may_quit(ws, status, force);
}

pub(crate) fn apply_effect(
    eff: Effect,
    ws: &mut Workspace,
    files: &Files,
    status: &mut String,
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
                *status = format!("E162: No write since last change for buffer \"{}\"", b.name);
            }
            None => *quit = true,
        },
        Effect::Status(s) => *status = s,
    }
}

/// Buffer line count for the `:w` write report, matching Vim: an empty buffer is 1 line; otherwise it is
/// the newline count, plus one when the last line has no trailing newline (`"a\nb"` and `"a\nb\n"` are both
/// 2 lines).
fn line_count(buf: &[u8]) -> usize {
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

/// Whether `save` should append a final `\n` to the buffer before writing: only under `:set fixeol` (OFF by
/// default — byte-preserve stays the honest default), and only when the buffer is non-empty and lacks one.
/// Mirrors Vim's `fixendofline` opt-in; an empty buffer is left empty (Vim writes no newline into one).
fn needs_final_newline(buf: &[u8], fixeol: bool) -> bool {
    fixeol && !buf.is_empty() && buf.last() != Some(&b'\n')
}

/// Returns whether the focused buffer reached disk.
pub(crate) fn save(ws: &mut Workspace, files: &Files, status: &mut String, fixeol: bool) -> bool {
    // Multi-buffer honesty (F-007): only a buffer WITH a file writes. A scratch buffer (`:enew`) has no
    // registry entry, so `:w` declines rather than clobbering another buffer's file.
    let Some(bf) = files.get(&ws.focused_buffer()) else {
        *status = "E32: No file name".into();
        return false;
    };
    // `:set fixeol` (opt-in) ADDS a trailing `\n` when the buffer lacks one. Append it to the SOURCE bytes
    // BEFORE `to_disk` so the encoding step (BOM/CRLF re-application) treats the newline like any other —
    // e.g. a CRLF file gets `\r\n`. Default (byte-preserve) leaves the bytes untouched.
    let doc_bytes = ws.focused().doc.bytes();
    let mut source = doc_bytes.to_vec();
    if needs_final_newline(&source, fixeol) {
        source.push(b'\n');
    }
    // Restore the original encoding/line-endings (F-008 #2), then write durably (fsync + rename, #1).
    let bytes = bf.fmt.to_disk(&source);
    match persist::atomic::save(&bf.path, &bytes) {
        Ok(()) => {
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(Some(bf.path.as_path())); // saved bytes are durable — nothing to recover
            tracing::info!(event = "save", path = %bf.path.display(), bytes = bytes.len());
            // Vim-style write report: `"file" 42L, 1024B written` (L = buffer lines, B = bytes on disk).
            // ruse preserves final-newline presence exactly by default (no silent fixeol); when the file was
            // written WITHOUT a trailing newline, surface `[noeol]` (as Vim does) so the missing EOL is never
            // a silent surprise. The check is on the SOURCE bytes actually written (to_disk only re-applies
            // BOM/CRLF, not EOL), so a successful `:set fixeol` write ends in `\n` and shows no marker.
            *status = format!(
                "\"{}\"{} {}L, {}B written",
                bf.path.display(),
                noeol_marker(&source),
                line_count(&source),
                bytes.len()
            );
            true
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %bf.path.display(), error = %e);
            *status = format!("write failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use ruse_core::Command;

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

    /// `:q` refuses (E37) once the focused buffer is modified; `:q!` quits regardless; a clean `:q` quits.
    #[test]
    fn quit_guards_unsaved_changes_and_bang_overrides() {
        let files = Files::new();
        let run = |ex: &Ex, ws: &mut Workspace| {
            let mut status = String::new();
            let mut quit = false;
            let mut confirm = None;
            run_ex(
                ex,
                ws,
                &files,
                b"",
                &[],
                &mut status,
                &mut quit,
                &mut confirm,
                false,
            );
            (status, quit)
        };

        // Clean buffer → `:q` quits.
        let mut ws = Workspace::new(b"hello\n".to_vec());
        let (_, quit) = run(&Ex::Quit, &mut ws);
        assert!(quit, "clean buffer quits on :q");

        // Modify the buffer → `:q` refuses with E37, `:q!` quits.
        let mut ws = Workspace::new(b"hello\n".to_vec());
        ws.apply(&Command::EnterInsert);
        ws.apply(&Command::InsertChar('x'));
        assert!(ws.focused().doc.is_modified(), "edit dirtied the buffer");
        let (status, quit) = run(&Ex::Quit, &mut ws);
        assert!(!quit, ":q does not quit a dirty buffer");
        assert!(status.starts_with("E37"), "E37 surfaced: {status}");
        let (_, quit) = run(&Ex::QuitForce, &mut ws);
        assert!(quit, ":q! discards changes and quits");
    }

    /// Run one ex line against `ws`, returning `(status, quit)`.
    fn ex_run(ws: &mut Workspace, line: &str) -> (String, bool) {
        let files = Files::new();
        let mut status = String::new();
        let mut quit = false;
        run_ex(
            &crate::input::parse_ex(line),
            ws,
            &files,
            b"",
            &[],
            &mut status,
            &mut quit,
            &mut None,
            false,
        );
        (status, quit)
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

    /// #1079: the leading range resolves `.`, `$`, offsets and `;` to line numbers, and an address past the
    /// last line is E16 for a command but clamps for a bare `:N` (nvim v0.12.6).
    #[test]
    fn ex_ranges_resolve_and_refuse_past_the_end() {
        let resolve = |cursor_line: usize, line: &str| {
            let mut ws = Workspace::new(b"a\nb\nc\nd\n".to_vec());
            ws.place_focused_cursor(cursor_line * 2);
            resolve_ex_range(line, &ws)
        };
        let line = |s: &str| Ok(ExRange::Line(s.to_string()));
        let e16 = Err(ExRangeError::InvalidRange);
        assert_eq!(resolve(0, "2,$d"), line("2,4d"));
        assert_eq!(resolve(1, ".,$d"), line("2,4d"));
        assert_eq!(resolve(1, ".,.+1d"), line("2,3d"));
        assert_eq!(resolve(0, ".+1,$-1d"), line("2,3d"));
        assert_eq!(resolve(0, "2;+1d"), line("2,3d"));
        assert_eq!(resolve(2, "-,+d"), line("2,4d"));
        assert_eq!(
            resolve(0, "1,2,3d"),
            line("2,3d"),
            "the last two addresses count"
        );
        assert_eq!(
            resolve(3, "2,d"),
            line("2,4d"),
            "a missing address is the cursor's line"
        );
        assert_eq!(resolve(0, "%d"), line("%d"));
        assert_eq!(resolve(0, "s/a/b/"), line("s/a/b/"));
        assert_eq!(
            resolve(0, "'a,'bd"),
            line("'a,'bd"),
            "other marks are left for the parser"
        );
        assert_eq!(resolve(0, "0d"), line("0d"));
        for past in [
            "2,6d",
            "5d",
            "2,6y",
            "2,6s/a/b/",
            "2,6>",
            "2,6m0",
            "2,6t0",
            "2,6normal x",
            "5put",
        ] {
            assert_eq!(resolve(0, past), e16, "{past}");
        }
        assert_eq!(resolve(0, "$-5,$d"), e16);
        assert_eq!(
            resolve(2, "-5"),
            e16,
            "a bare range above line 1 is still E16"
        );
        assert_eq!(resolve(0, "9"), Ok(ExRange::Goto(4)), "a bare range clamps");
        assert_eq!(resolve(0, "$"), Ok(ExRange::Goto(4)));
        assert_eq!(resolve(2, "-"), Ok(ExRange::Goto(2)));
        assert_eq!(resolve(0, "0"), Ok(ExRange::Goto(1)));
        assert_eq!(resolve(0, "'<,'>d"), Err(ExRangeError::MarkNotSet));
        // A buffer without a final newline has the same lines; an empty buffer has one.
        let ws = Workspace::new(b"a\nb".to_vec());
        assert_eq!(resolve_ex_range("3d", &ws), e16);
        assert_eq!(resolve_ex_range("2d", &ws), line("2d"));
        let ws = Workspace::new(Vec::new());
        assert_eq!(resolve_ex_range("1d", &ws), line("1d"));
        assert_eq!(resolve_ex_range("2d", &ws), e16);
    }

    /// `:t`/`:m` to an absolute line past the end is E16 too (#801), and the cursor rests on the last line
    /// moved or copied, at the wanted column (nvim v0.12.6 `ll:2,3t0`: line 2, column 2).
    #[test]
    fn move_and_copy_refuse_absolute_lines_past_the_end_and_keep_the_column() {
        let mut ws = Workspace::new(b"abcd\n  defgh\nghi\n  jkl\n".to_vec());
        ws.place_focused_cursor(2);
        ws.apply(&Command::MoveRight); // a horizontal move sets the wanted column
        ws.apply(&Command::MoveLeft);
        assert_eq!(ex_run(&mut ws, "t99").0, "E16: Invalid range");
        assert_eq!(ex_run(&mut ws, "m99").0, "E16: Invalid range");
        ex_run(&mut ws, "2,3t0");
        assert_eq!(
            ws.focused().doc.bytes(),
            b"  defgh\nghi\nabcd\n  defgh\nghi\n  jkl\n"
        );
        assert_eq!(ws.focused().view.cursor(), 8 + 2);
        // `:d` lands on the first non-blank of the line that took the range's place.
        let mut ws = Workspace::new(b"abc\n  defgh\nghi\n  jkl\n".to_vec());
        ex_run(&mut ws, "2,3d");
        assert_eq!(ws.focused().doc.bytes(), b"abc\n  jkl\n");
        assert_eq!(ws.focused().view.cursor(), 4 + 2);
        ex_run(&mut ws, "2d");
        assert_eq!(ws.focused().doc.bytes(), b"abc\n");
        assert_eq!(
            ws.focused().view.cursor(),
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
            ws.place_focused_cursor(cursor_line * 2);
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
        let mut engine = InputEngine::new();
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        ws.place_focused_cursor(cursor0);
        let files = Files::new();
        let mut recorded = Vec::new();
        let mut status = String::new();
        let mut quit = false;
        run_normal(
            &mut engine,
            &mut ws,
            range,
            keys,
            &files,
            &mut recorded,
            &mut status,
            &mut quit,
        );
        let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
        (bytes, ws.focused().view.cursor())
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
        // An EXPLICIT <Esc> is equivalent (the implicit one is a no-op once already in Normal).
        let (bytes, _) = run_normal_oracle("abc\n", 1, None, "Ihi<Esc>");
        assert_eq!(bytes, "hiabc\n");
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
        let mut engine = InputEngine::new();
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        let files = Files::new();
        let mut recorded = Vec::new();
        let mut status = String::new();
        let mut quit = false;
        run_global_normal(
            &mut engine,
            &mut ws,
            SubRange::WholeFile,
            pattern,
            negate,
            keys,
            &files,
            &mut recorded,
            &mut status,
            &mut quit,
        );
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

    /// Drive the ex executor END TO END for `:[range]j[oin][!]`: parse the raw line with `parse_ex` and run
    /// it through `run_ex` exactly as the run loop does. `parity_compare` cannot drive ex commands, so each
    /// expectation is the buffer nvim v0.12.4 produces for the SAME line (`nvim -u NONE`, confirmed by hand).
    fn run_ex_oracle(buf: &str, cursor0: usize, line: &str) -> (String, usize) {
        let mut ws = Workspace::new(buf.as_bytes().to_vec());
        ws.place_focused_cursor(cursor0);
        let files = Files::new();
        let recorded: Vec<Command> = Vec::new();
        let mut status = String::new();
        let mut quit = false;
        let mut confirm = None;
        let ex = crate::input::parse_ex(line);
        run_ex(
            &ex,
            &mut ws,
            &files,
            buf.as_bytes(),
            &recorded,
            &mut status,
            &mut quit,
            &mut confirm,
            false,
        );
        let bytes = String::from_utf8(ws.focused().doc.bytes().to_vec()).unwrap();
        (bytes, ws.focused().view.cursor())
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
        ws.place_focused_cursor(5); // must not move
        let files = Files::new();
        let recorded: Vec<Command> = Vec::new();
        let mut status = String::new();
        let mut quit = false;
        let mut confirm = None;
        let ex = crate::input::parse_ex("%s/foo//gn");
        run_ex(
            &ex,
            &mut ws,
            &files,
            src.as_bytes(),
            &recorded,
            &mut status,
            &mut quit,
            &mut confirm,
            false,
        );
        assert_eq!(status, "6 matches on 3 lines", "nvim wording");
        assert!(
            confirm.is_none(),
            "`n` overrides `c` — no confirm loop opens"
        );
        assert_eq!(ws.focused().doc.bytes(), src.as_bytes(), "buffer unchanged");
        assert_eq!(ws.focused().view.cursor(), 5, "cursor did not move");
        assert!(!ws.focused().doc.is_modified(), "buffer not dirtied");
        // No undo entry was created: an undo is a genuine no-op.
        ws.apply(&Command::Undo);
        assert_eq!(
            ws.focused().doc.bytes(),
            src.as_bytes(),
            "no undo entry from :s///n"
        );

        // No-match reports E486 (nvim), still without editing.
        let mut status = String::new();
        let ex = crate::input::parse_ex("%s/zzz//gn");
        run_ex(
            &ex,
            &mut ws,
            &files,
            src.as_bytes(),
            &recorded,
            &mut status,
            &mut quit,
            &mut confirm,
            false,
        );
        assert_eq!(status, "E486: pattern not found: zzz");
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
        ws.place_focused_cursor(cursor0);

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
        (bytes, ws.focused().view.cursor())
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
}
