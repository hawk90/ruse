//! `ruse` — a terminal-based modal text editor. A thin crossterm frontend over the editor spine in
//! `ruse-core`: keys → semantic commands → plan/commit → the core returns Effects (Save/Quit) that this
//! binary performs. All IO lives here; the core stays pure, so `ruse --replay <trace> <file>` reproduces an
//! edit session deterministically without a terminal.

// D-041: diagnostics go through `tracing`, never the terminal. The only sanctioned stdout/stderr is the
// headless CLI (`--replay`/startup), which carries a scoped `allow` on each such function. A non-test
// `.unwrap()` is an unjustified panic (use `.expect("<why>")` or a `Result`); tests exempt via clippy.toml.
#![deny(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::disallowed_methods
)]

mod app;
mod caps;
mod health;
mod highlight;
mod input;
mod line_index;
mod log;
mod persist;
mod recover;
mod screen;
mod terminal;
mod ui;
mod viewport;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::ClearType;
use crossterm::{cursor, queue, terminal as ct_terminal};

use app::dispatch::{run_cmd, run_ex};
use input::{parse_ex, Ex, Feed, InputEngine};
use ruse_core::{CaretGravity, Command, Mode, SplitDir, Trace, Workspace};
use terminal::guard::TermGuard;
use ui::layout::window_rects;
use ui::render::{render, search_pattern};

// Headless CLI: stderr is the correct channel here (no TUI, no tracing sink yet). D-041 scoped allow.
#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    log::init();
    recover::install_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--replay") {
        return replay(&args[1..]);
    }
    let path = args.first().map(PathBuf::from);
    // Raw on-disk bytes (BOM/CRLF intact); run() detects the format and normalises for the buffer.
    let raw = path
        .as_ref()
        .and_then(|p| fs::read(p).ok())
        .unwrap_or_default();
    match run(path, raw) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ruse: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Headless: replay a trace onto a file and print the resulting document to stdout. Proves the determinism
/// contract end-to-end (`ruse --replay t.trace file.rs`).
#[allow(clippy::print_stderr)] // headless CLI: stderr is the correct channel (D-041).
fn replay(args: &[String]) -> ExitCode {
    let (Some(tp), Some(fp)) = (args.first(), args.get(1)) else {
        eprintln!("usage: ruse --replay <trace> <file>");
        return ExitCode::FAILURE;
    };
    let (text, bytes) = match (fs::read_to_string(tp), fs::read(fp)) {
        (Ok(t), Ok(b)) => (t, b),
        _ => {
            eprintln!("ruse: cannot read trace or file");
            return ExitCode::FAILURE;
        }
    };
    let trace = match Trace::from_text(&text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ruse: bad trace: {e:?}");
            return ExitCode::FAILURE;
        }
    };
    match trace.replay(&bytes) {
        Ok(st) => {
            let _ = io::stdout().write_all(st.bytes());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ruse: replay failed: {e:?}");
            ExitCode::FAILURE
        }
    }
}

/// Rows of context kept above and below the cursor when scrolling (Vim's `scrolloff`).
const SCROLLOFF: usize = 3;

/// Append a recovery-journal frame every Nth modified command — a coarse hard-kill safety net (a
/// panic captures the exact latest state separately). Full incremental journaling is post-MVP.
const JOURNAL_THROTTLE: u32 = 8;

/// Ask the user whether to load recovered unsaved changes (F-008 #3). Renders a minimal full-screen
/// prompt and blocks for one key: `y`/`r` = recover, anything else = discard and open the disk file.
/// The original file is never touched here — the choice only decides the initial BUFFER (#4).
fn prompt_recovery(out: &mut io::Stdout) -> io::Result<bool> {
    queue!(
        out,
        ct_terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
        Print("ruse: unsaved changes were recovered from a previous session."),
        cursor::MoveTo(0, 1),
        Print("Press 'r' to RECOVER them, or any other key to open the on-disk file."),
    )?;
    out.flush()?;
    loop {
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Release {
                continue; // act on the press, not its release (kitty/Windows send both)
            }
            return Ok(matches!(
                k.code,
                KeyCode::Char('r') | KeyCode::Char('y') | KeyCode::Char('R')
            ));
        }
    }
}

fn run(path: Option<PathBuf>, raw: Vec<u8>) -> io::Result<()> {
    // F-008: detect the original encoding/line-ending once, edit in clean LF, restore it on save.
    let fmt = persist::encoding::FileFormat::detect(&raw);
    let disk = fmt.to_buffer(&raw);
    // A crash may have left an append-only journal of unsaved work; offer it, never auto-apply it.
    let recovered = persist::journal::replay(path.as_deref());
    let recovery = persist::assess_recovery(&disk, recovered.as_deref());

    // Syntax highlighting (Rust only for v0) lives in the frontend; core stays dep-free.
    let mut highlighter = path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .and_then(highlight::CachedHighlight::for_ext);
    // hlsearch/incsearch match spans, cached on (revision, viewport, pattern) like the syntax highlighter
    // — no full-buffer regex per frame (F-009 #1).
    let mut cached_search = highlight::CachedSearch::default();
    // Revision-cached line index: O(log n) row/line lookups instead of an O(buffer) newline scan per
    // pane per frame (D-042 win D).
    let mut line_idx = line_index::LineIndex::default();

    let guard = TermGuard::enter()?;
    let mut out = io::stdout();

    // Open-time crash recovery (F-008 #3/#4): the user picks; the original file is never touched.
    let (initial, mut status) = match recovery {
        persist::Recovery::Available(rec) if prompt_recovery(&mut out)? => (
            rec,
            String::from("ruse — recovered unsaved changes (:w to save)"),
        ),
        persist::Recovery::Available(_) => {
            persist::journal::clear(path.as_deref()); // discarded on the user's say-so
            (
                disk,
                String::from("ruse — recovery discarded; opened disk version"),
            )
        }
        persist::Recovery::None => (disk, String::from("ruse — :q to quit")),
    };

    // F-007: the frontend now drives a Workspace (buffers + views + windows), not a single
    // EditorState. With one Window this is byte-identical to the pre-Workspace path; `:split`/
    // `:vsplit` open more Windows onto the same buffer with independent cursors and scroll.
    let mut ws = Workspace::new(initial.clone());
    // The initial buffer owns the session file (`path`/`fmt`): name it, and remember its id so `:w` on any
    // OTHER buffer (`:enew` scratch) declines rather than clobbering the file (F-007 multi-buffer).
    let file_buf = ws.focused_buffer();
    if let Some(p) = path.as_ref() {
        ws.set_focused_buffer_name(p.display().to_string());
    }
    let mut recorded: Vec<Command> = Vec::new();
    let mut journal_ticks: u32 = 0; // throttle: append the recovery journal every Nth modified frame

    // Profile selection (F-012 / F-013): no config loader exists yet, so `RUSE_PROFILE` picks the profile
    // (the same env-override seam as the terminal caps). `emacs` = Emacs, `native` = Native, else Vim.
    let profile_name = match std::env::var("RUSE_PROFILE").as_deref() {
        Ok("emacs") => "emacs",
        Ok("native") => "native",
        _ => "vim",
    };
    let emacs_profile = profile_name == "emacs";
    let mut engine = match profile_name {
        "emacs" => InputEngine::emacs(),
        "native" => InputEngine::native(),
        _ => InputEngine::new(),
    };
    // Caret gravity follows the profile (D-050 / RFC-0015): the Emacs profile rests point BETWEEN chars
    // (line/buffer end = after the last char), so its edits are not Vim-clamped. Vim keeps `OnChar`, and
    // Native reuses the Vim text grammar (NAT-1) so it keeps `OnChar` too.
    if emacs_profile {
        ws.set_caret_gravity(CaretGravity::BetweenChar);
    }
    let mut quit = false;
    // The previous frame's cell grid — the render diff emits only what changes against it (F-006).
    // Starts empty so the first frame paints in full.
    let mut prev_frame = screen::Screen::new(0, 0);
    let sync_output = guard.sync_output(); // pinned once from the F-010 ledger (INV-RENDER-PROFILE)
    let mut pending_window = false; // a `C-w` window-command prefix awaits its second key (F-007)
    let mut confirm: Option<Confirm> = None; // a `:s///c` interactive confirm loop, when active (F-009)
    let mut search_hl: Option<String> = None; // the hlsearch pattern (last `/`-search), until `:noh`
    let mut palette: Option<Palette> = None; // the command palette overlay, when open (F-004)
    let mut line_picker: Option<LinePicker> = None; // the buffer-line fuzzy picker overlay (F-013 NAT-3)

    while !quit {
        // The FOCUSED buffer is the file on disk (splits share it; MVP is single-file). Snapshot it
        // once for the panic-rescue mirror, the recovery journal, and the highlight parse.
        let (revision, modified, snapshot) = {
            let f = ws.focused();
            (
                f.doc.revision(),
                f.doc.is_modified(),
                f.doc.bytes().to_vec(),
            )
        };
        // Refresh the line index (rebuilds only on a revision change) so the per-frame row/viewport
        // lookups below are O(log n), not an O(buffer) newline scan. MVP splits share this one buffer.
        line_idx.refresh(revision, &snapshot);
        // Panic-rescue mirror + recovery journal are the FILE buffer's, keyed to `path` (F-008). Skip them
        // while a scratch/other buffer is focused so its bytes never land in the file's recovery (F-007).
        let on_file = ws.focused_buffer() == file_buf;
        if on_file {
            // Keep the in-memory snapshot current so a core panic can rescue unsaved work (§6/§8).
            recover::update(path.as_ref(), &snapshot, modified);
            // And throttle an append-only journal frame so a hard kill (not just a panic) loses at most
            // a few edits. Cleared on a durable save. Full journal design is post-MVP (C-PERSIST).
            if modified {
                journal_ticks += 1;
                if journal_ticks.is_multiple_of(JOURNAL_THROTTLE) {
                    let _ = persist::journal::append(path.as_deref(), &snapshot);
                }
            }
        }
        // During a `:s///c` confirm, follow the current match so the viewport scrolls it into view.
        if let Some(c) = &confirm {
            if let Some(s) = c.subs.get(c.idx) {
                ws.place_focused_cursor(s.start);
                status = confirm_prompt(c);
            }
        }
        // Per-window viewport pass: scroll each pane so ITS cursor stays visible in ITS rectangle
        // (F-007 acceptance #1 — independent scroll). Geometry is shared with render below.
        let (cols, term_rows) = ct_terminal::size().unwrap_or((80, 24));
        let text_rows = term_rows.saturating_sub(1);
        let rects = window_rects(cols, text_rows, ws.window_count(), ws.split_dir());
        for (i, rect) in rects.iter().enumerate() {
            let (cursor_row, cur_top) = {
                let p = ws.pane(i);
                (line_idx.line_of(p.view.cursor()), p.view.top())
            };
            let new_top = viewport::scroll_top(cursor_row, rect.h as usize, SCROLLOFF, cur_top);
            ws.set_top(i, new_top);
        }
        // Highlight only the VISIBLE byte range of the focused buffer (F-015 #3): the union of the
        // viewports of every pane showing it. The tree is reparsed incrementally on edit (keyed on
        // revision); the viewport-bounded query keeps the per-keystroke cost O(viewport), not O(buffer).
        let focus_doc = ws.focused().view.doc();
        let (mut vis_start, mut vis_end) = (usize::MAX, 0usize);
        for (i, rect) in rects.iter().enumerate() {
            let p = ws.pane(i);
            if p.view.doc() != focus_doc {
                continue;
            }
            vis_start = vis_start.min(line_idx.nth_line_start(p.view.top()));
            vis_end = vis_end.max(line_idx.nth_line_start(p.view.top() + rect.h as usize + 1));
        }
        let visible = if vis_start <= vis_end {
            vis_start..vis_end
        } else {
            0..snapshot.len()
        };
        // Syntax highlighting is the FILE buffer's grammar (chosen from its extension); don't paint it over
        // a scratch/other buffer's bytes (F-007). Its language dispatch per buffer is a follow-up slice.
        let spans: &[highlight::Span] = highlighter
            .as_mut()
            .filter(|_| on_file)
            .map(|h| h.spans(revision, &snapshot, visible.clone()))
            .unwrap_or(&[]);
        // The focused pane's extra reverse-video highlights: a `:s///c` confirm match, else the
        // incsearch pattern being typed in `/`…`?`, else the last search (hlsearch) — F-009 #1. The
        // search matches are viewport-cached (CachedSearch), not a per-frame full-buffer regex.
        let focus_hl: Vec<(usize, usize)> = if let Some(c) = &confirm {
            c.subs
                .get(c.idx)
                .map(|s| vec![(s.start, s.end)])
                .unwrap_or_default()
        } else {
            let active = match engine.cmdline() {
                Some(('/', buf, _)) | Some(('?', buf, _)) if !buf.is_empty() => {
                    Some(buf.to_string())
                }
                _ => search_hl.clone(),
            };
            active
                .map(|p| {
                    cached_search
                        .spans(revision, &snapshot, visible.clone(), &p)
                        .to_vec()
                })
                .unwrap_or_default()
        };
        // The palette (F-004 #2), when open, owns the command line (its query, prefixed `>`) and paints
        // its context-filtered matches with each command's static binding above the status line.
        // Overlay match rows painted above the status line — at most one overlay is open at a time, so the
        // command palette (F-004) and the line picker (F-013 NAT-3) share the same paint slot.
        let overlay_rows: Vec<(String, bool)> = if let Some(p) = palette.as_ref() {
            p.matches
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let binding = engine
                        .binding_label(&s.command)
                        .unwrap_or_else(|| "—".into());
                    (
                        format!("{:<28} {:>5}   {:?}", s.title, binding, s.category),
                        i == p.selected,
                    )
                })
                .collect()
        } else if let Some(p) = line_picker.as_ref() {
            p.rows()
        } else {
            Vec::new()
        };
        // The Native leader (which-key) hint owns the command line while armed (F-013 NAT-2), shown with a
        // Space prefix — below the overlay, above the ordinary `:`/`/` line (none can co-occur).
        let leader_hint = engine.leader_hint();
        let cmd_line: Option<(char, &str)> = match (&palette, &line_picker, &leader_hint) {
            (Some(p), _, _) => Some(('>', p.query.as_str())), // command palette prompt
            (None, Some(p), _) => Some(('#', p.query.as_str())), // line-picker prompt (# = line jump)
            (None, None, Some(h)) => Some((' ', h.as_str())),
            (None, None, None) => engine.cmdline().map(|(pfx, t, _)| (pfx, t)),
        };
        render(
            &mut out,
            &ws,
            cmd_line,
            &status,
            spans,
            &rects,
            &mut prev_frame,
            sync_output,
            &focus_hl,
            &overlay_rows,
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        // A `:s///c` confirm loop owns the keystream while active: y/n/a/l/q per match (F-009 #2).
        if confirm.is_some() {
            confirm_key(&mut confirm, key, &mut ws, &mut status);
            continue;
        }
        // The line picker owns the keystream while open (F-013 NAT-3): type to filter, Up/Down to select,
        // Enter to jump the cursor to the line, Esc to close. Checked before the palette; only one is open.
        if line_picker.is_some() {
            line_picker_key(&mut line_picker, key, &mut ws);
            continue;
        }
        // The command palette owns the keystream while open (F-004 #2): type to filter, Up/Down to
        // select, Enter to dispatch the selected command by its stable id, Esc to close.
        if palette.is_some() {
            palette_key(
                &mut palette,
                key,
                &mut ws,
                &path,
                fmt,
                file_buf,
                &mut recorded,
                &mut status,
                &mut quit,
            );
            continue;
        }
        // F-007 window layer: a `C-w` prefix (Normal mode, no command-line, not Insert where `C-w`
        // deletes a word) arms the next key as a window command. A thin frontend intercept for MVP;
        // F-003's keymap router will absorb it into a proper layer.
        if pending_window {
            pending_window = false;
            dispatch_window(key, &mut ws, &mut quit);
            continue;
        }
        let normal = matches!(ws.focused().view.mode(), Mode::Normal) && engine.cmdline().is_none();
        if normal && is_ctrl(key, 'w') {
            pending_window = true;
            continue;
        }
        // `C-p` opens the command palette (F-004 #2), context-filtered to the focused view.
        if normal && is_ctrl(key, 'p') {
            palette = Some(Palette::open(&focused_context(&ws)));
            continue;
        }
        // `C-l` opens the buffer-line fuzzy picker (F-013 NAT-3). Normal-only, so the Emacs profile's
        // non-modal C-l (recenter) is unaffected; C-l is unbound in the Vim/Native Normal grammar.
        if normal && is_ctrl(key, 'l') {
            line_picker = Some(LinePicker::open(ws.focused().doc.bytes()));
            continue;
        }
        // Every other key — command-line included — goes through the engine (F-026): the command-line
        // namespace owns its buffer, so the frontend no longer special-cases `:`/`/` typing.
        match engine.feed(key, ws.focused().view.mode()) {
            // A finished `:`-line (F-026): parse + run it. `submit_search` already folded a `/`-line
            // into `Feed::Cmd` inside the engine, so the frontend only sees the ex case here.
            Feed::ExecuteEx(text) => {
                match parse_ex(&text) {
                    Ex::NoHighlight => search_hl = None, // `:noh` clears the search highlight (F-009 #1)
                    // Lang-Arg map maintenance (F-027): the map is engine state, so it is applied here
                    // where `engine` is in scope, not in `run_ex` (which owns only workspace/file state).
                    Ex::Lmap { lhs, rhs } => engine.set_lang_mapping(lhs, rhs),
                    Ex::Lunmap { lhs } => engine.clear_lang_mapping(lhs),
                    // `:checkhealth` (F-030): gather the frontend snapshot HERE (guard/profile/highlighter
                    // are in scope, not in `run_ex`) and render the report's one-line summary into status.
                    Ex::CheckHealth => {
                        let inputs = health::HealthInputs {
                            profile: profile_name,
                            caret: if emacs_profile {
                                "between-char"
                            } else {
                                "on-char"
                            },
                            sgr_mouse: guard.sgr_mouse(),
                            sync_output: guard.sync_output(),
                            bracketed_paste: guard.bracketed_paste(),
                            file_ext: path
                                .as_ref()
                                .and_then(|p| p.extension())
                                .map(|e| e.to_string_lossy().into_owned()),
                            grammar_ok: highlighter.is_some(),
                            buffers: ws.window_count(),
                            trace_commands: recorded.len(),
                        };
                        status = health::summary_line(&health::report(&inputs));
                    }
                    ex => run_ex(
                        &ex,
                        &mut ws,
                        &path,
                        fmt,
                        file_buf,
                        &initial,
                        &recorded,
                        &mut status,
                        &mut quit,
                        &mut confirm,
                    ),
                }
            }
            Feed::Pending | Feed::Ignored => {}
            Feed::Cmd(cmd) => {
                // A completed search turns on hlsearch for that pattern (F-009 #1).
                if let Some(p) = search_pattern(&cmd) {
                    search_hl = Some(p);
                }
                run_cmd(
                    cmd,
                    &mut ws,
                    &path,
                    fmt,
                    file_buf,
                    &mut recorded,
                    &mut status,
                    &mut quit,
                );
            }
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    run_cmd(
                        cmd,
                        &mut ws,
                        &path,
                        fmt,
                        file_buf,
                        &mut recorded,
                        &mut status,
                        &mut quit,
                    );
                }
            }
        }
    }
    Ok(())
}

/// Whether `key` is `CTRL-<c>`.
fn is_ctrl(key: crossterm::event::KeyEvent, c: char) -> bool {
    key.modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == KeyCode::Char(c)
}

/// Dispatch the key after a `C-w` prefix (F-007 MVP window commands): `w`/`C-w` focus next, `s` split
/// horizontally, `v` split vertically, `c` close focused, `q` close (or quit on the last window). An
/// unrecognised key is ignored (Vim beeps). The full `C-w` family is post-MVP.
fn dispatch_window(key: crossterm::event::KeyEvent, ws: &mut Workspace, quit: &mut bool) {
    match key.code {
        KeyCode::Char('w') => ws.focus_next(),
        KeyCode::Char('s') => {
            ws.split(SplitDir::Horizontal);
        }
        KeyCode::Char('v') => {
            ws.split(SplitDir::Vertical);
        }
        KeyCode::Char('c') => {
            ws.close_focused();
        }
        // `C-w q`: close the focused window, or quit if it was the last one.
        KeyCode::Char('q') => *quit = !ws.close_focused(),
        _ => {}
    }
}

/// The state of an in-progress `:s///c` confirm loop (F-009 #2): the pending substitutions, the index
/// of the one being confirmed, and the subset accepted so far. The buffer is NOT edited until the loop
/// ends (all confirmed, or `a`/`l`/`q`), so the absolute offsets stay valid throughout; the accepted
/// subset is then applied as one undo group.
pub(crate) struct Confirm {
    subs: Vec<ruse_core::Substitution>,
    idx: usize,
    accepted: Vec<ruse_core::Substitution>,
}

impl Confirm {
    pub(crate) fn new(subs: Vec<ruse_core::Substitution>) -> Confirm {
        Confirm {
            subs,
            idx: 0,
            accepted: Vec::new(),
        }
    }
}

/// The status-line prompt shown while confirming the current match.
fn confirm_prompt(c: &Confirm) -> String {
    let rep = c
        .subs
        .get(c.idx)
        .map(|s| String::from_utf8_lossy(&s.replacement).into_owned())
        .unwrap_or_default();
    format!(
        "replace with {rep} ({}/{})?  (y)es (n)o (a)ll (l)ast (q)uit",
        c.idx + 1,
        c.subs.len()
    )
}

/// Handle one key of the `:s///c` confirm loop. Applies the accepted subset (one undo group) and clears
/// `confirm` when the loop ends; otherwise advances to the next match.
fn confirm_key(
    confirm: &mut Option<Confirm>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
    status: &mut String,
) {
    let Some(mut c) = confirm.take() else {
        return;
    };
    match key.code {
        KeyCode::Char('y') => {
            if let Some(s) = c.subs.get(c.idx) {
                c.accepted.push(s.clone());
            }
            c.idx += 1;
        }
        KeyCode::Char('n') => c.idx += 1,
        KeyCode::Char('a') => {
            c.accepted.extend(c.subs[c.idx..].iter().cloned());
            c.idx = c.subs.len();
        }
        KeyCode::Char('l') => {
            if let Some(s) = c.subs.get(c.idx) {
                c.accepted.push(s.clone());
            }
            c.idx = c.subs.len();
        }
        KeyCode::Char('q') | KeyCode::Esc => c.idx = c.subs.len(),
        _ => {} // any other key: re-prompt (Vim ignores it)
    }
    if c.idx >= c.subs.len() {
        let out = ws.apply_substitutions(&c.accepted);
        *status = format!("{} substitutions on {} lines", out.replacements, out.lines);
        // `confirm` was taken and stays None — the loop is over.
    } else {
        *confirm = Some(c);
    }
}

/// The command-palette overlay state (F-004 #2): the commands AVAILABLE in the current context, the
/// query filtering them, and the selected row. Opened with a dedicated key; Enter dispatches the
/// selected command by its stable id (never a key), Esc closes.
struct Palette {
    /// The typed filter.
    query: String,
    /// Commands available in the opening context (before the query filter).
    available: Vec<ruse_core::CommandSpec>,
    /// The current query's matches (a subset of `available`).
    matches: Vec<ruse_core::CommandSpec>,
    /// Selected row into `matches`.
    selected: usize,
}

impl Palette {
    fn open(ctx: &ruse_core::Context) -> Palette {
        let mut p = Palette {
            query: String::new(),
            available: ruse_core::available(ctx),
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Recompute `matches` from `available` and the query (case-insensitive substring on title or id).
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.matches = self
            .available
            .iter()
            .filter(|s| q.is_empty() || s.title.to_lowercase().contains(&q) || s.id.contains(&q))
            .cloned()
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    fn selected_command(&self) -> Option<ruse_core::Command> {
        self.matches.get(self.selected).map(|s| s.command.clone())
    }
}

/// A buffer-line fuzzy picker — the first picker "special view" overlay (view-window-workspace.md §7
/// VW-OVERLAY / F-013 NAT-3). It lists the focused buffer's lines, filters them by a typed query, and on
/// Enter jumps the focused cursor to the selected line. Structurally it is the same modal overlay as the
/// command [`Palette`] — a query, a filtered match list, a selection, and a transient keymap that owns the
/// keystream while open — but over a different item source (lines) and a different action (jump, not a
/// [`Command`]). The eventual home is one generic picker over VW-OVERLAY's `OverlayStack`; folding the
/// command palette into it is a follow-up. Buffer/file pickers wait on the multi-buffer arena.
struct LinePicker {
    /// The typed filter.
    query: String,
    /// Every line as `(display/search text, byte offset of the line start)`, in file order.
    lines: Vec<(String, usize)>,
    /// Indices into `lines` that match the current query (a subset).
    matches: Vec<usize>,
    /// Selected row into `matches`.
    selected: usize,
}

impl LinePicker {
    /// Open over `bytes`: one entry per line (a trailing final newline does not yield an empty last
    /// entry — there is no line to jump to there), each carrying the byte offset of its start.
    fn open(bytes: &[u8]) -> LinePicker {
        let mut lines = Vec::new();
        let mut start = 0usize;
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                lines.push((
                    String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                    start,
                ));
                start = i + 1;
            }
        }
        if start < bytes.len() {
            lines.push((String::from_utf8_lossy(&bytes[start..]).into_owned(), start));
        }
        let mut p = LinePicker {
            query: String::new(),
            lines,
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Recompute `matches` from the query (case-insensitive substring on the line text — the same match
    /// rule as the command palette; a fuzzy subsequence score is a later refinement).
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, (text, _))| q.is_empty() || text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// The byte offset of the selected line's start, or `None` when nothing matches.
    fn selected_offset(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|&i| self.lines[i].1)
    }

    /// The overlay's match rows (`"<1-based file line>: <text>"`, selected flag), reusing the same
    /// above-the-status-line paint path as the command palette.
    fn rows(&self) -> Vec<(String, bool)> {
        self.matches
            .iter()
            .enumerate()
            .map(|(row, &i)| {
                (
                    format!("{:>5}: {}", i + 1, self.lines[i].0),
                    row == self.selected,
                )
            })
            .collect()
    }
}

/// Handle one key of the line picker (F-013 NAT-3). Enter jumps the focused cursor to the selected line
/// (the frontend's per-frame viewport pass then scrolls it into view); Esc closes; typing filters.
fn line_picker_key(
    picker: &mut Option<LinePicker>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
) {
    let Some(p) = picker.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => *picker = None,
        KeyCode::Enter => {
            let offset = p.selected_offset();
            *picker = None;
            if let Some(offset) = offset {
                ws.place_focused_cursor(offset);
            }
        }
        KeyCode::Up => p.selected = p.selected.saturating_sub(1),
        KeyCode::Down if p.selected + 1 < p.matches.len() => p.selected += 1,
        KeyCode::Backspace => {
            p.query.pop();
            p.refilter();
        }
        KeyCode::Char(c) => {
            p.query.push(c);
            p.refilter();
        }
        _ => {}
    }
}

/// The command-availability context of the focused view (F-004 #2 C-CONTEXT).
fn focused_context(ws: &Workspace) -> ruse_core::Context {
    let f = ws.focused();
    let bytes = f.doc.bytes();
    let has_selection =
        f.view.selection_span(bytes).is_some() || f.view.block_spans(bytes).is_some();
    ruse_core::Context {
        mode: f.view.mode(),
        has_selection,
    }
}

/// Handle one key of the command palette (F-004 #2). Enter dispatches the selected command by its id
/// (through the normal command path, so it undoes/records like any other); Esc closes.
#[allow(clippy::too_many_arguments)]
fn palette_key(
    palette: &mut Option<Palette>,
    key: crossterm::event::KeyEvent,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    file_buf: ruse_core::DocumentId,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    let Some(p) = palette.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => *palette = None,
        KeyCode::Enter => {
            let cmd = p.selected_command();
            *palette = None;
            if let Some(cmd) = cmd {
                run_cmd(cmd, ws, path, fmt, file_buf, recorded, status, quit);
            }
        }
        KeyCode::Up => p.selected = p.selected.saturating_sub(1),
        KeyCode::Down if p.selected + 1 < p.matches.len() => p.selected += 1,
        KeyCode::Backspace => {
            p.query.pop();
            p.refilter();
        }
        KeyCode::Char(c) => {
            p.query.push(c);
            p.refilter();
        }
        _ => {}
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;
    use ruse_core::{Context, Mode};

    fn normal_ctx() -> Context {
        Context {
            mode: Mode::Normal,
            has_selection: false,
        }
    }

    /// F-004 #2: the palette opens context-filtered and the query narrows it further; Enter yields the
    /// selected command (by id, decoupled from any key).
    #[test]
    fn palette_filters_by_context_then_query() {
        let mut p = Palette::open(&normal_ctx());
        let opened: Vec<_> = p.matches.iter().map(|s| s.id).collect();
        assert!(
            opened.contains(&"editor.undo"),
            "Normal-family command is offered"
        );
        assert!(
            !opened.contains(&"editor.delete_back"),
            "Insert-only command is hidden in Normal"
        );

        for c in "save".chars() {
            p.query.push(c);
            p.refilter();
        }
        assert_eq!(
            p.matches.len(),
            1,
            "query narrows to the single 'Save File' match"
        );
        assert_eq!(p.selected_command(), Some(ruse_core::Command::Save));
    }

    /// F-004: an empty query keeps the full available set; selection clamps to the match count.
    #[test]
    fn palette_empty_query_keeps_all_available() {
        let p = Palette::open(&normal_ctx());
        assert_eq!(p.matches.len(), p.available.len());
        assert!(!p.matches.is_empty());
    }

    /// F-013 NAT-3: the line picker lists every line with its byte offset; a trailing newline yields no
    /// empty final entry; the query narrows by substring and the selection resolves to the line's offset.
    #[test]
    fn line_picker_filters_and_resolves_offsets() {
        let src = b"alpha\nbeta line\ngamma\nbeta again\n";
        let p = LinePicker::open(src);
        assert_eq!(p.lines.len(), 4, "four lines, no empty trailing entry");
        assert_eq!(p.lines[0], ("alpha".to_string(), 0));
        assert_eq!(p.lines[2].1, 16, "gamma starts after the first two lines");

        let mut q = LinePicker::open(src);
        for c in "beta".chars() {
            q.query.push(c);
            q.refilter();
        }
        assert_eq!(q.matches.len(), 2, "two lines contain `beta`");
        // Selection defaults to the first match — the line starting at offset 6 (`beta line`).
        assert_eq!(q.selected_offset(), Some(6));
    }

    /// F-013 NAT-3: an empty query lists all lines; selection moves clamp; a no-match query resolves to
    /// None (nothing to jump to).
    #[test]
    fn line_picker_empty_and_nomatch() {
        let src = b"one\ntwo\nthree";
        let p = LinePicker::open(src);
        assert_eq!(p.matches.len(), 3, "empty query keeps every line");
        assert_eq!(p.selected_offset(), Some(0));

        let mut none = LinePicker::open(src);
        for c in "zzz".chars() {
            none.query.push(c);
            none.refilter();
        }
        assert!(none.matches.is_empty());
        assert_eq!(none.selected_offset(), None, "no match resolves to no jump");
    }
}
