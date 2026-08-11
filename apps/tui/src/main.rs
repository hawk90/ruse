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

mod caps;
mod highlight;
mod input;
mod log;
mod persist;
mod recover;
mod screen;
mod viewport;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::ClearType;
use crossterm::{cursor, queue, terminal};

use input::{parse_ex, Ex, Feed, InputEngine};
use ruse_core::{Command, Effect, Mode, SelectKind, SplitDir, Trace, Workspace};

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

/// Build the capability ledger (F-010): safe-fallback defaults, then a low-confidence env seed,
/// then — on a real Unix tty — a DA1-fenced active probe that upgrades what the terminal confirms.
/// Every step is non-fatal: a probe that fails or is skipped leaves the honest env/default belief.
fn detect_capabilities() -> caps::ledger::Ledger {
    let mut ledger = caps::ledger::Ledger::with_defaults();
    caps::seed_env(
        &mut ledger,
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("COLORTERM").unwrap_or_default(),
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
    );
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        if io::stdin().is_terminal() {
            let _ = live_probe(&mut ledger); // best-effort; env/default belief stands on failure
        }
    }
    // User overrides win over everything the probe found (architecture §6.3).
    caps::apply_overrides(
        &mut ledger,
        &std::env::var("RUSE_NO_KITTY").unwrap_or_default(),
        &std::env::var("RUSE_NO_MOUSE").unwrap_or_default(),
        &std::env::var("RUSE_NO_PASTE").unwrap_or_default(),
    );
    ledger
}

/// Emit the probe batch and drain the terminal's replies until the DA1 fence (F-010 acceptance #1).
/// The `poll` deadline is a LIVENESS net for a terminal that never answers DA1 — NOT a
/// per-capability timeout; the fence, not the clock, decides support (see `caps::probe`).
#[cfg(unix)]
fn live_probe(ledger: &mut caps::ledger::Ledger) -> io::Result<()> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let mut out = io::stdout();
    out.write_all(&caps::probe::query_batch())?;
    out.flush()?;

    let fd = io::stdin().as_raw_fd();
    let mut parser = caps::probe::ProbeParser::new();
    let mut buf = [0u8; 512];
    // Up to ~20 × 50 ms only if the terminal keeps sending nothing; a real terminal answers in the
    // first poll and the fence breaks the loop immediately.
    for _ in 0..20 {
        // SAFETY: `pfd` is a valid, initialised `pollfd`; `poll` reads/writes only that one struct.
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready <= 0 {
            break; // timeout or error — stop; whatever replied so far stands, defaults for the rest
        }
        let n = io::stdin().read(&mut buf)?;
        if n == 0 {
            break;
        }
        parser.feed(&buf[..n], ledger);
        if parser.is_fenced() {
            break; // the DA1 fence replied — the ledger is final
        }
    }
    Ok(())
}

/// Restores the terminal on drop, even on panic. Owns the capability ledger so the exact set of
/// modes pushed on enter is the exact set reset on exit (F-010 acceptance #3 — no shell corruption).
struct TermGuard {
    ledger: caps::ledger::Ledger,
}
impl TermGuard {
    /// Whether the terminal confirmed DEC synchronized output (mode 2026) at startup — read once
    /// and held (INV-RENDER-PROFILE: the profile is pinned, never re-probed on frame noise). The
    /// render diff fences a repaint in `?2026h`/`l` when this is true so the frame lands atomically.
    fn sync_output(&self) -> bool {
        self.ledger
            .enabled(caps::ledger::Capability::SynchronizedOutput)
    }

    fn enter() -> io::Result<TermGuard> {
        terminal::enable_raw_mode()?;
        queue!(io::stdout(), terminal::EnterAlternateScreen)?;
        io::stdout().flush()?;
        // Probe AFTER raw mode (so replies arrive as raw bytes) and inside the alt screen (so query
        // echoes, if any, never touch the parent scrollback).
        let ledger = detect_capabilities();
        let mut out = io::stdout();
        out.write_all(&caps::sequences::enter(&ledger))?;
        out.flush()?;
        Ok(TermGuard { ledger })
    }
}
impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(&caps::sequences::exit(&self.ledger)); // pop/reset before leaving
        let _ = queue!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
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
        terminal::Clear(ClearType::All),
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
    let mut highlighter = match path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
    {
        Some("rs") => highlight::CachedHighlight::rust(),
        _ => None,
    };

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
    let mut recorded: Vec<Command> = Vec::new();
    let mut journal_ticks: u32 = 0; // throttle: append the recovery journal every Nth modified frame

    let mut engine = InputEngine::new();
    let mut quit = false;
    // The previous frame's cell grid — the render diff emits only what changes against it (F-006).
    // Starts empty so the first frame paints in full.
    let mut prev_frame = screen::Screen::new(0, 0);
    let sync_output = guard.sync_output(); // pinned once from the F-010 ledger (INV-RENDER-PROFILE)
    let mut pending_window = false; // a `C-w` window-command prefix awaits its second key (F-007)
    let mut confirm: Option<Confirm> = None; // a `:s///c` interactive confirm loop, when active (F-009)
    let mut search_hl: Option<String> = None; // the hlsearch pattern (last `/`-search), until `:noh`
    let mut palette: Option<Palette> = None; // the command palette overlay, when open (F-004)

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
        // During a `:s///c` confirm, follow the current match so the viewport scrolls it into view.
        if let Some(c) = &confirm {
            if let Some(s) = c.subs.get(c.idx) {
                ws.place_focused_cursor(s.start);
                status = confirm_prompt(c);
            }
        }
        // Per-window viewport pass: scroll each pane so ITS cursor stays visible in ITS rectangle
        // (F-007 acceptance #1 — independent scroll). Geometry is shared with render below.
        let (cols, term_rows) = terminal::size().unwrap_or((80, 24));
        let text_rows = term_rows.saturating_sub(1);
        let rects = window_rects(cols, text_rows, ws.window_count(), ws.split_dir());
        for (i, rect) in rects.iter().enumerate() {
            let (cursor_row, cur_top) = {
                let p = ws.pane(i);
                (row_col(p.doc.bytes(), p.view.cursor()).0, p.view.top())
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
            vis_start = vis_start.min(nth_line_start(&snapshot, p.view.top()));
            vis_end = vis_end.max(nth_line_start(
                &snapshot,
                p.view.top() + rect.h as usize + 1,
            ));
        }
        let visible = if vis_start <= vis_end {
            vis_start..vis_end
        } else {
            0..snapshot.len()
        };
        let spans: &[highlight::Span] = highlighter
            .as_mut()
            .map(|h| h.spans(revision, &snapshot, visible))
            .unwrap_or(&[]);
        // The focused pane's extra reverse-video highlights: a `:s///c` confirm match, else the
        // incsearch pattern being typed in `/`…`?`, else the last search (hlsearch) — F-009 #1.
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
                .map(|p| search_highlights(&p, &snapshot))
                .unwrap_or_default()
        };
        // The palette (F-004 #2), when open, owns the command line (its query, prefixed `>`) and paints
        // its context-filtered matches with each command's static binding above the status line.
        let palette_rows: Vec<(String, bool)> = palette
            .as_ref()
            .map(|p| {
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
            })
            .unwrap_or_default();
        let cmd_line: Option<(char, &str)> = match &palette {
            Some(p) => Some(('>', p.query.as_str())),
            None => engine.cmdline().map(|(pfx, t, _)| (pfx, t)),
        };
        render(
            &mut out,
            &ws,
            path.as_ref(),
            cmd_line,
            &status,
            spans,
            &rects,
            &mut prev_frame,
            sync_output,
            &focus_hl,
            &palette_rows,
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
        // The command palette owns the keystream while open (F-004 #2): type to filter, Up/Down to
        // select, Enter to dispatch the selected command by its stable id, Esc to close.
        if palette.is_some() {
            palette_key(
                &mut palette,
                key,
                &mut ws,
                &path,
                fmt,
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
                    ex => run_ex(
                        &ex,
                        &mut ws,
                        &path,
                        fmt,
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

/// Record a command and apply it to the focused window, performing any effects.
fn run_cmd(
    cmd: Command,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    recorded.push(cmd.clone());
    for eff in ws.apply(&cmd) {
        apply_effect(eff, ws, path, fmt, status, quit);
    }
}

// The ex-line dispatcher legitimately needs the full editor context (buffer, file identity+format,
// the trace baseline+recording, and the status/quit sinks); grouping them into a struct would only
// move the wiring, not reduce it.
#[allow(clippy::too_many_arguments)]
fn run_ex(
    ex: &Ex,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    initial: &[u8],
    recorded: &[Command],
    status: &mut String,
    quit: &mut bool,
    confirm: &mut Option<Confirm>,
) {
    match ex {
        Ex::Save => save(ws, path, fmt, status),
        Ex::Quit => *quit = true,
        Ex::SaveQuit => {
            save(ws, path, fmt, status);
            *quit = true;
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
            if spec.confirm {
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
        // `:[range]g/pat/cmd` (F-009 #4): two-pass mark-then-execute over the focused window.
        Ex::Global(spec) => {
            *status = match ws.global(spec.range, &spec.pattern, spec.negate, &spec.cmd) {
                Ok(0) => format!("E486: pattern not found: {}", spec.pattern),
                Ok(n) => format!("{n} lines changed"),
                Err(e) => regex_error_msg(&e),
            };
        }
        // `:noh` is handled in the run loop (it clears the frontend's search highlight); never reaches here.
        Ex::NoHighlight => {}
        // `:lmap`/`:lunmap` are handled in the run loop (they mutate engine-owned Lang-Arg state — the
        // `engine` this fn does not borrow); never reach here.
        Ex::Lmap { .. } | Ex::Lunmap { .. } => {}
        Ex::Unknown(s) => *status = format!("unknown command: {s}"),
    }
}

/// The state of an in-progress `:s///c` confirm loop (F-009 #2): the pending substitutions, the index
/// of the one being confirmed, and the subset accepted so far. The buffer is NOT edited until the loop
/// ends (all confirmed, or `a`/`l`/`q`), so the absolute offsets stay valid throughout; the accepted
/// subset is then applied as one undo group.
struct Confirm {
    subs: Vec<ruse_core::Substitution>,
    idx: usize,
    accepted: Vec<ruse_core::Substitution>,
}

impl Confirm {
    fn new(subs: Vec<ruse_core::Substitution>) -> Confirm {
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
                run_cmd(cmd, ws, path, fmt, recorded, status, quit);
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

/// Human-readable status for a regex compile error (F-009).
fn regex_error_msg(e: &ruse_core::RegexError) -> String {
    match e {
        ruse_core::RegexError::Unsupported(m) => format!("E: unsupported pattern: {m}"),
        ruse_core::RegexError::Syntax(m) => format!("E: bad pattern: {m}"),
    }
}

fn apply_effect(
    eff: Effect,
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    status: &mut String,
    quit: &mut bool,
) {
    match eff {
        Effect::Save => save(ws, path, fmt, status),
        Effect::Quit => *quit = true,
        Effect::Status(s) => *status = s,
    }
}

fn save(
    ws: &mut Workspace,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    status: &mut String,
) {
    let Some(p) = path else {
        *status = "no file name (open with `ruse <file>`)".into();
        return;
    };
    // Restore the original encoding/line-endings (F-008 #2), then write durably (fsync + rename, #1).
    let bytes = fmt.to_disk(ws.focused().doc.bytes());
    match persist::atomic::save(p, &bytes) {
        Ok(()) => {
            ws.focused_doc_mut().mark_saved();
            persist::journal::clear(path.as_deref()); // saved bytes are durable — no work to recover
            tracing::info!(event = "save", path = %p.display(), bytes = bytes.len());
            *status = format!("\"{}\" written", p.display());
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %p.display(), error = %e);
            *status = format!("write failed: {e}");
        }
    }
}

/// One indent level's width in display columns — matches the editor's `editor.tab_width` default.
const TAB_WIDTH: u16 = 4;

/// A window's on-screen sub-rectangle in cells: origin `(x, y)` and size `w × h` (F-007 layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

/// Tile `count` windows into the text area (`cols` × `text_rows`) as equal bands/columns separated by
/// a one-cell divider (F-007 MVP flat layout — no recursive tree). `Horizontal` stacks panes top to
/// bottom, `Vertical` places them side by side. The `count-1` dividers are subtracted first, then the
/// remaining cells split evenly with any remainder handed to the earliest panes (so the area is fully
/// used). Always returns `count.max(1)` rects.
fn window_rects(cols: u16, text_rows: u16, count: usize, split: SplitDir) -> Vec<Rect> {
    let n = count.max(1) as u16;
    let seps = n.saturating_sub(1);
    let mut rects = Vec::with_capacity(n as usize);
    match split {
        SplitDir::Horizontal => {
            let avail = text_rows.saturating_sub(seps);
            let (base, extra) = (avail / n, avail % n);
            let mut y = 0u16;
            for i in 0..n {
                let h = base + u16::from(i < extra);
                rects.push(Rect {
                    x: 0,
                    y,
                    w: cols,
                    h,
                });
                y = y.saturating_add(h + 1); // + the divider row
            }
        }
        SplitDir::Vertical => {
            let avail = cols.saturating_sub(seps);
            let (base, extra) = (avail / n, avail % n);
            let mut x = 0u16;
            for i in 0..n {
                let w = base + u16::from(i < extra);
                rects.push(Rect {
                    x,
                    y: 0,
                    w,
                    h: text_rows,
                });
                x = x.saturating_add(w + 1); // + the divider column
            }
        }
    }
    rects
}

/// Paint one buffer view into its `rect`: `rect.h` lines from `top`, one GRAPHEME CLUSTER per cell at
/// its true display width (F-006 #4), clipped to the rectangle. Tabs expand to the next stop measured
/// from the pane's left edge; a wide glyph that would straddle the right edge is dropped and the rest
/// of that line is skipped. `sel`/`block` paint the Visual selection in reverse video; `byte_color`
/// carries the syntax colour per byte (empty ⇒ default).
#[allow(clippy::too_many_arguments)] // painting one pane legitimately needs the full cell context
fn paint_pane(
    cur: &mut screen::Screen,
    rect: Rect,
    bytes: &[u8],
    byte_color: &[crossterm::style::Color],
    top: usize,
    sel: Option<(usize, usize)>,
    block: Option<&[(usize, usize)]>,
    hl: &[(usize, usize)],
) {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let (x0, x1) = (rect.x, rect.x + rect.w);
    let bottom = top + rect.h as usize;
    let text = std::str::from_utf8(bytes).unwrap_or("<binary>");
    let mut line: usize = 0;
    let mut scol: u16 = x0;
    for (i, g) in text.grapheme_indices(true) {
        if g == "\n" {
            line += 1;
            scol = x0;
            if line >= bottom {
                break; // past the bottom of this pane
            }
            continue;
        }
        if line < top || scol >= x1 {
            continue; // above the pane, or past its right edge (truncate)
        }
        let srow = rect.y + (line - top) as u16;
        let selected = sel.is_some_and(|(s, e)| i >= s && i < e)
            || block.is_some_and(|rows| rows.iter().any(|&(s, e)| i >= s && i < e))
            || hl.iter().any(|&(s, e)| i >= s && i < e);
        let fg = byte_color.get(i).copied().unwrap_or(Color::Reset);
        if g == "\t" {
            let stop = TAB_WIDTH - ((scol - x0) % TAB_WIDTH); // stops measured from the pane's left
            for _ in 0..stop {
                if scol >= x1 {
                    break;
                }
                scol = cur.put(srow, scol, " ", fg, selected);
            }
        } else if scol + screen::cluster_width(g) > x1 {
            scol = x1; // a wide glyph would cross the edge — drop it and skip the rest of the line
        } else {
            scol = cur.put(srow, scol, g, fg, selected);
        }
    }
}

/// Draw the one-cell dividers between adjacent panes (F-007): a `─` band for a horizontal split, a
/// `│` column for a vertical split. Only interior gaps are drawn (never past the text area).
fn draw_separators(
    cur: &mut screen::Screen,
    rects: &[Rect],
    split: SplitDir,
    cols: u16,
    text_rows: u16,
) {
    use crossterm::style::Color;
    for pair in rects.windows(2) {
        match split {
            SplitDir::Horizontal => {
                let y = pair[0].y + pair[0].h;
                if y < text_rows {
                    for x in 0..cols {
                        cur.put(y, x, "─", Color::Reset, false);
                    }
                }
            }
            SplitDir::Vertical => {
                let x = pair[0].x + pair[0].w;
                if x < cols {
                    for y in 0..text_rows {
                        cur.put(y, x, "│", Color::Reset, false);
                    }
                }
            }
        }
    }
}

/// The pattern a search command carries (turns on hlsearch for it), else `None` (F-009 #1).
fn search_pattern(cmd: &Command) -> Option<String> {
    match cmd {
        Command::Search { pattern, .. } => Some(pattern.clone()),
        Command::SearchNext(p) | Command::SearchPrev(p) => Some(p.clone()),
        _ => None,
    }
}

/// All matches of `pattern` in `bytes` as byte spans, for the incsearch/hlsearch reverse-video
/// highlight (F-009 #1). Uses the default search options (case-sensitive magic — the search default);
/// an unrepresentable/malformed pattern or a non-UTF-8 buffer highlights nothing (never an error path).
fn search_highlights(pattern: &str, bytes: &[u8]) -> Vec<(usize, usize)> {
    let Ok(hay) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let Ok(re) = ruse_core::Regex::compile(pattern, ruse_core::RegexOptions::default()) else {
        return Vec::new();
    };
    re.find_all(hay)
        .into_iter()
        .map(|m| (m.start, m.end))
        .collect()
}

#[allow(clippy::too_many_arguments)] // the frame render legitimately needs the full view context
fn render(
    out: &mut io::Stdout,
    ws: &Workspace,
    path: Option<&PathBuf>,
    cmd_line: Option<(char, &str)>,
    status: &str,
    spans: &[highlight::Span],
    rects: &[Rect],
    prev: &mut screen::Screen,
    sync: bool,
    focus_hl: &[(usize, usize)],
    palette_rows: &[(String, bool)],
) -> io::Result<()> {
    use crossterm::style::Color;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text_rows = rows.saturating_sub(1);
    // Paint the whole frame into a fresh cell grid; the diff against `prev` emits only what changed.
    let mut cur = screen::Screen::new(cols, rows);

    // Flatten the focused buffer's highlight spans into a per-byte colour (spans index into it). Panes
    // showing the SAME buffer reuse it; a pane on a different buffer (post-MVP) renders uncoloured.
    let focus = ws.focused();
    let focus_doc = focus.view.doc();
    let fbytes = focus.doc.bytes();
    let mut byte_color = vec![Color::Reset; fbytes.len()];
    for s in spans {
        for slot in byte_color
            .iter_mut()
            .take(s.end.min(fbytes.len()))
            .skip(s.start)
        {
            *slot = s.color;
        }
    }

    // Paint every window into its sub-rectangle; the focused view owns the terminal cursor below.
    for (i, &rect) in rects.iter().enumerate().take(ws.window_count()) {
        let p = ws.pane(i);
        let pbytes = p.doc.bytes();
        let color: &[Color] = if p.view.doc() == focus_doc {
            &byte_color
        } else {
            &[]
        };
        let sel = p.view.selection_span(pbytes);
        let block = p.view.block_spans(pbytes);
        // The focused pane also paints the `:s///c` confirm match / incsearch+hlsearch matches (F-009
        // #1) in reverse video, on top of any live Visual selection.
        let hl: &[(usize, usize)] = if i == ws.focus() { focus_hl } else { &[] };
        paint_pane(
            &mut cur,
            rect,
            pbytes,
            color,
            p.view.top(),
            sel,
            block.as_deref(),
            hl,
        );
    }
    draw_separators(&mut cur, rects, ws.split_dir(), cols, text_rows);

    // The command palette (F-004): paint its match rows just above the status line, newest selection
    // in reverse video. Rows beyond the available height are dropped (the list scrolls with selection
    // in a fuller build; MVP shows the top window).
    if !palette_rows.is_empty() {
        let shown = palette_rows.len().min(text_rows.saturating_sub(1) as usize);
        let top = text_rows.saturating_sub(shown as u16); // first palette row
        for (i, (label, selected)) in palette_rows.iter().take(shown).enumerate() {
            let row = top + i as u16;
            // Clear the row then paint the label (reverse for the selected match).
            for x in 0..cols {
                cur.put(row, x, " ", Color::Reset, *selected);
            }
            cur.put_str(row, 1, label, Color::Reset, *selected);
        }
    }

    let bar = match cmd_line {
        Some((prefix, text)) => format!("{prefix}{text}"),
        None => {
            let mode = match focus.view.mode() {
                Mode::Normal => "NORMAL",
                Mode::Insert => "INSERT",
                Mode::Replace => "REPLACE",
                Mode::VirtualReplace => "V-REPLACE",
                Mode::Visual {
                    kind: SelectKind::Charwise,
                } => "VISUAL",
                Mode::Visual {
                    kind: SelectKind::Linewise,
                } => "V-LINE",
                Mode::Visual {
                    kind: SelectKind::Blockwise,
                } => "V-BLOCK",
                Mode::Select {
                    kind: SelectKind::Charwise,
                } => "SELECT",
                Mode::Select {
                    kind: SelectKind::Linewise,
                } => "S-LINE",
                Mode::Select {
                    kind: SelectKind::Blockwise,
                } => "S-BLOCK",
            };
            let name = path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[No Name]".into());
            let dirty = if focus.doc.is_modified() { " [+]" } else { "" };
            // Show the window position only once split, so the single-window status line is unchanged.
            let win = if ws.window_count() > 1 {
                format!("  [win {}/{}]", ws.focus() + 1, ws.window_count())
            } else {
                String::new()
            };
            format!("{mode}  {name}{dirty}{win}  {status}")
        }
    };
    // Paint the status / command line into the last row (put_str truncates at the right edge).
    cur.put_str(rows.saturating_sub(1), 0, &bar, Color::Reset, false);

    // Diff the finished grid against the previous frame and emit ONLY the changed runs (F-006 #1),
    // wrapped in synchronized output when supported so a big repaint lands atomically (#3).
    flush_diff(out, &cur, prev, sync)?;
    *prev = cur;

    if let Some((_, text)) = cmd_line {
        let ccol = (text.chars().count() as u16 + 1).min(cols.saturating_sub(1));
        queue!(
            out,
            cursor::MoveTo(ccol, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else {
        // The focused view owns the cursor; its DISPLAY column is grapheme-cluster / width based
        // (F-006 #4), offset into the focused window's rectangle (F-007).
        let frect = rects.get(ws.focus()).copied().unwrap_or(Rect {
            x: 0,
            y: 0,
            w: cols,
            h: text_rows,
        });
        let (row, col) = cursor_cell(focus.doc.bytes(), focus.view.cursor(), focus.view.top());
        let screen_row = (frect.y + row).min(rows.saturating_sub(1));
        let screen_col = (frect.x + col).min(cols.saturating_sub(1));
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    out.flush()
}

/// Emit only the cells that changed between `cur` and `prev` (F-006 #1). Each changed run is one
/// `MoveTo` then its cells, printed with lazy SGR (colour/reverse) changes; a continuation cell is
/// skipped (the wide glyph to its left already advanced over it). When `sync` is set the whole batch
/// is fenced in DEC synchronized-output (`?2026h`/`l`) so the terminal shows the frame atomically (#3).
fn flush_diff(
    out: &mut impl Write,
    cur: &screen::Screen,
    prev: &screen::Screen,
    sync: bool,
) -> io::Result<()> {
    use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};

    let runs = cur.diff(prev);
    if runs.is_empty() {
        return Ok(()); // nothing changed — no full-screen redraw
    }
    queue!(out, cursor::Hide)?;
    if sync {
        out.write_all(b"\x1b[?2026h")?;
    }
    let mut fg = Color::Reset;
    let mut reversed = false;
    queue!(out, ResetColor, SetAttribute(Attribute::NoReverse))?;
    for (row, start, cells) in runs {
        queue!(out, cursor::MoveTo(start, row))?;
        for cell in &cells {
            let text: &str = match &cell.content {
                screen::Content::Continuation => continue, // covered by the wide glyph on the left
                screen::Content::Blank => " ",
                screen::Content::Cluster(s) => s,
            };
            if cell.reverse != reversed {
                let a = if cell.reverse {
                    Attribute::Reverse
                } else {
                    Attribute::NoReverse
                };
                queue!(out, SetAttribute(a))?;
                reversed = cell.reverse;
            }
            if cell.fg != fg {
                queue!(out, SetForegroundColor(cell.fg))?;
                fg = cell.fg;
            }
            queue!(out, Print(text))?;
        }
    }
    queue!(out, SetAttribute(Attribute::NoReverse), ResetColor)?;
    if sync {
        out.write_all(b"\x1b[?2026l")?;
    }
    Ok(())
}

/// The cursor's on-screen `(row, col)`: `row` relative to the viewport `top`, `col` in DISPLAY cells
/// (wide glyphs count 2, combining marks 0, tabs to the next stop) — grapheme-correct, not a char
/// count (F-006 #4).
fn cursor_cell(bytes: &[u8], pos: usize, top: usize) -> (u16, u16) {
    use unicode_segmentation::UnicodeSegmentation;
    let pos = pos.min(bytes.len());
    let row = bytes[..pos].iter().filter(|&&c| c == b'\n').count();
    let line_start = bytes[..pos]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let line = std::str::from_utf8(&bytes[line_start..pos]).unwrap_or("");
    let mut col: u16 = 0;
    for g in line.graphemes(true) {
        if g == "\t" {
            col += TAB_WIDTH - (col % TAB_WIDTH);
        } else {
            col += screen::cluster_width(g);
        }
    }
    (row.saturating_sub(top) as u16, col)
}

/// (row, col) of a byte offset — row = newlines before it, col = char count since the line start.
/// Byte offset where 0-indexed `line` starts (after that many newlines), or `bytes.len()` if beyond.
fn nth_line_start(bytes: &[u8], line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    bytes.len()
}

fn row_col(bytes: &[u8], pos: usize) -> (usize, usize) {
    let pos = pos.min(bytes.len());
    let row = bytes[..pos].iter().filter(|&&c| c == b'\n').count();
    let ls = bytes[..pos]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let col = std::str::from_utf8(&bytes[ls..pos])
        .map(|s| s.chars().count())
        .unwrap_or(0);
    (row, col)
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crossterm::style::Color;

    /// F-009 #1: the hlsearch/incsearch highlight computes every match span of the pattern (Vim regex).
    #[test]
    fn search_highlights_all_matches() {
        assert_eq!(search_highlights("a", b"a b a"), vec![(0, 1), (4, 5)]);
        // Magic quantifier, not literal.
        assert_eq!(search_highlights("a\\+", b"aa b aaa"), vec![(0, 2), (5, 8)]);
        // An unrepresentable/invalid pattern highlights nothing (never errors).
        assert!(search_highlights("\\1", b"abc").is_empty());
    }

    /// F-009 #1: a search command carries its pattern for hlsearch; other commands do not.
    #[test]
    fn search_pattern_extracts_only_from_search_commands() {
        assert_eq!(
            search_pattern(&ruse_core::Command::SearchNext("foo".into())),
            Some("foo".to_string())
        );
        assert_eq!(search_pattern(&ruse_core::Command::MoveLeft), None);
    }

    /// F-006 #1: an unchanged frame emits ZERO bytes — no full-screen redraw.
    #[test]
    fn an_unchanged_frame_emits_nothing() {
        let mut a = screen::Screen::new(20, 3);
        a.put_str(0, 0, "hello world", Color::Reset, false);
        let mut b = screen::Screen::new(20, 3);
        b.put_str(0, 0, "hello world", Color::Reset, false);
        let mut buf: Vec<u8> = Vec::new();
        flush_diff(&mut buf, &b, &a, false).unwrap();
        assert!(buf.is_empty(), "identical frames must produce no output");
    }

    /// F-006 #1: a one-cell change emits a small, bounded batch containing just the new glyph — not
    /// the whole screen. (A full redraw of 60 cells would be far larger.)
    #[test]
    fn a_one_cell_change_emits_only_that_cell() {
        let mut a = screen::Screen::new(20, 3);
        a.put_str(0, 0, "hello world", Color::Reset, false);
        let mut b = screen::Screen::new(20, 3);
        b.put_str(0, 0, "hello wOrld", Color::Reset, false); // one char differs
        let mut buf: Vec<u8> = Vec::new();
        flush_diff(&mut buf, &b, &a, false).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains('O'), "the changed glyph is emitted");
        assert!(!s.contains("hello"), "the unchanged run is NOT re-emitted");
    }

    /// F-007: a single window fills the whole text area (the single-pane path is unchanged).
    #[test]
    fn one_window_fills_the_area() {
        let r = window_rects(80, 24, 1, SplitDir::Horizontal);
        assert_eq!(
            r,
            vec![Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24
            }]
        );
    }

    /// F-007: `:split` tiles two equal horizontal bands with a one-row divider between them; the rows
    /// partition the area exactly (band + divider + band = text_rows).
    #[test]
    fn horizontal_split_tiles_equal_bands_with_a_divider() {
        let r = window_rects(80, 25, 2, SplitDir::Horizontal);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 12
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 0,
                y: 13,
                w: 80,
                h: 12
            }
        ); // y = 12 (band) + 1 (divider)
        assert_eq!(r[0].h + 1 + r[1].h, 25, "bands + divider fill the height");
    }

    /// F-007: `:vsplit` tiles two columns side by side with a one-column divider; remainder cells go
    /// to the earliest pane so the width is fully used.
    #[test]
    fn vertical_split_tiles_columns_with_a_divider() {
        let r = window_rects(80, 24, 2, SplitDir::Vertical);
        assert_eq!(r.len(), 2);
        // (80 - 1 divider) / 2 = 39 rem 1 → first pane gets the extra column.
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 24
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 41,
                y: 0,
                w: 39,
                h: 24
            }
        ); // x = 40 (col) + 1 (divider)
        assert_eq!(r[0].w + 1 + r[1].w, 80, "columns + divider fill the width");
    }

    /// F-006 #3: with sync support the batch is fenced in DEC synchronized output (?2026h/l).
    #[test]
    fn sync_output_fences_the_batch_when_supported() {
        let a = screen::Screen::new(10, 1);
        let mut b = screen::Screen::new(10, 1);
        b.put(0, 0, "Z", Color::Reset, false);
        let mut on: Vec<u8> = Vec::new();
        flush_diff(&mut on, &b, &a, true).unwrap();
        assert!(
            on.windows(8).any(|w| w == b"\x1b[?2026h"),
            "begins synchronized output"
        );
        assert!(
            on.windows(8).any(|w| w == b"\x1b[?2026l"),
            "ends synchronized output"
        );
        let mut off: Vec<u8> = Vec::new();
        flush_diff(&mut off, &b, &a, false).unwrap();
        assert!(
            !off.windows(8).any(|w| w == b"\x1b[?2026h"),
            "no fence when unsupported"
        );
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
}
