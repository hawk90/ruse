//! Frontend command/ex dispatch: run a `Command` (recording it + performing its `Effect`s), execute a
//! parsed `:` line, and the save/label helpers they call. These are thin callers over `Workspace`,
//! `persist::`, and the status/quit sinks — no event-loop or terminal state.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, DocumentId, Effect, Mode, SplitDir, SubRange, Trace, Workspace};

use crate::highlight;
use crate::input::{BufTarget, Ex, Feed, InputEngine};
use crate::persist;
use crate::ui::prompts::Confirm;

/// The per-buffer syntax-highlighter registry (F-007 / F-015), keyed by [`DocumentId`]. Shared by the
/// session loop and the LSP coordinator (both open buffers that need a highlighter).
pub(crate) type Highlighters = HashMap<DocumentId, highlight::CachedHighlight>;

/// Open `file` into a NEW buffer, focus it, register its file identity + highlighter, and return a status
/// line. Shared by the file picker / `:e` (session) and goto/rename/references (LSP coordinator). `E484`
/// on a read error.
pub(crate) fn open_file_into_buffer(
    file: &str,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
) -> String {
    let p = PathBuf::from(file);
    match fs::read(&p) {
        Ok(raw) => {
            let f = persist::encoding::FileFormat::detect(&raw);
            let clean = f.to_buffer(&raw);
            let id = ws.add_buffer(clean, Some(file.to_string()));
            ws.focus_buffer(id);
            if let Some(h) = p
                .extension()
                .and_then(|e| e.to_str())
                .and_then(highlight::CachedHighlight::for_ext)
            {
                highlighters.insert(id, h);
            }
            files.insert(id, BufferFile { path: p, fmt: f });
            format!("\"{file}\" {} bytes", raw.len())
        }
        Err(e) => format!("E484: can't open {file}: {e}"),
    }
}

/// Whether `key` is `CTRL-<c>`. Shared frontend key predicate.
pub(crate) fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
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
    recorded.push(cmd.clone());
    for eff in ws.apply(&cmd) {
        apply_effect(eff, ws, files, status, quit);
    }
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
/// status: an E486 when the pattern marks nothing, a regex error, else "N lines changed".
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
        *status = format!("E486: pattern not found: {pattern}");
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
        | Feed::Pending
        | Feed::Ignored => {}
    }
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
        Ex::Save => save(ws, files, status, fixeol),
        // `:q` refuses when the focused buffer has unsaved changes (Vim E37); `:q!` discards them.
        Ex::Quit => {
            if ws.focused().doc.is_modified() {
                *status = "E37: No write since last change (add ! to override)".into();
            } else {
                *quit = true;
            }
        }
        Ex::QuitForce => *quit = true,
        Ex::SaveQuit => {
            // `:wq`/`:x` on a buffer with no file errors (save declines) and does NOT quit — matching Vim.
            let had_file = files.contains_key(&ws.focused_buffer());
            save(ws, files, status, fixeol);
            if had_file {
                *quit = true;
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
                *status = "cannot close last window (:q to quit)".into();
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
                    Ok(0) => format!("E486: pattern not found: {}", spec.pattern),
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
        // `:registers` is handled in the run loop (it opens the frontend-owned register-viewer picker).
        Ex::Registers => {}
        // `:digraphs` is handled in the run loop (it opens the frontend-owned digraph-listing picker).
        Ex::Digraphs => {}
        // `:marks` is handled in the run loop (it opens the frontend-owned marks-viewer picker).
        Ex::Marks => {}
        // `:jumps` / `:changes` are handled in the run loop (frontend-owned position-viewer picker).
        Ex::Jumps | Ex::Changes => {}
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
        // `:terminal` (F-011) / `:fmt` / `:rename` / `:references` (F-014) are handled in `session::run`
        // (they need the terminals / lsp maps); they never reach here, but the match stays exhaustive.
        Ex::Terminal
        | Ex::Format
        | Ex::Rename(_)
        | Ex::References
        | Ex::CodeAction
        | Ex::Diagnostics => {}
        // `:[range]normal` is handled in the run loop (it drives the input `engine`, which this fn does not
        // borrow, to replay the keys through the same pipeline); never reaches here.
        Ex::Normal { .. } => {}
        Ex::Unknown(s) => *status = format!("unknown command: {s}"),
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
        Effect::Save => save(ws, files, status, false),
        Effect::Quit => *quit = true,
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
    buf.iter().filter(|&&b| b == b'\n').count() + usize::from(buf.last() != Some(&b'\n'))
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

pub(crate) fn save(ws: &mut Workspace, files: &Files, status: &mut String, fixeol: bool) {
    // Multi-buffer honesty (F-007): only a buffer WITH a file writes. A scratch buffer (`:enew`) has no
    // registry entry, so `:w` declines rather than clobbering another buffer's file.
    let Some(bf) = files.get(&ws.focused_buffer()) else {
        *status = "E32: No file name".into();
        return;
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
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %bf.path.display(), error = %e);
            *status = format!("write failed: {e}");
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use ruse_core::Command;

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
        // `:j` joins the current line + next on a single space; cursor rests on the join seam.
        let (bytes, cur) = run_ex_oracle("foo\nbar\nbaz\n", 0, "j");
        assert_eq!(bytes, "foo bar\nbaz\n");
        assert_eq!(cur, 3);
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
        ws.filter_lines(range, out.as_bytes());

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
