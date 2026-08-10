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
use ruse_core::{apply_command, Command, EditorState, Effect, Mode, SelectKind, Trace};

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

    let mut st = EditorState::new(initial.clone());
    let mut recorded: Vec<Command> = Vec::new();
    let mut journal_ticks: u32 = 0; // throttle: append the recovery journal every Nth modified frame

    let mut engine = InputEngine::new();
    let mut quit = false;
    let mut top: usize = 0; // first visible buffer row (frontend view state; core stays view-free)
                            // The previous frame's cell grid — the render diff emits only what changes against it (F-006).
                            // Starts empty so the first frame paints in full.
    let mut prev_frame = screen::Screen::new(0, 0);
    let sync_output = guard.sync_output(); // pinned once from the F-010 ledger (INV-RENDER-PROFILE)

    while !quit {
        // Keep the in-memory snapshot current so a core panic can rescue unsaved work (§6/§8).
        recover::update(path.as_ref(), st.bytes(), st.is_modified());
        // And throttle an append-only journal frame so a hard kill (not just a panic) loses at most
        // a few edits. Cleared on a durable save. Full journal design is post-MVP (C-PERSIST).
        if st.is_modified() {
            journal_ticks += 1;
            if journal_ticks.is_multiple_of(JOURNAL_THROTTLE) {
                let _ = persist::journal::append(path.as_deref(), st.bytes());
            }
        }
        // Scroll so the cursor stays on screen with a scrolloff margin (view state, not core state).
        let (_, term_rows) = terminal::size().unwrap_or((80, 24));
        let text_rows = term_rows.saturating_sub(1) as usize;
        let (cursor_row, _) = row_col(st.bytes(), st.cursor());
        top = viewport::scroll_top(cursor_row, text_rows, SCROLLOFF, top);
        // Recompute highlight spans only when the buffer changed (keyed on revision, D-042 win A):
        // cursor motion, mode changes and scrolling reuse the cached parse.
        let spans: &[highlight::Span] = highlighter
            .as_mut()
            .map(|h| h.spans(st.doc.revision(), st.bytes()))
            .unwrap_or(&[]);
        render(
            &mut out,
            &st,
            path.as_ref(),
            engine.cmdline().map(|(p, t, _)| (p, t)),
            &status,
            spans,
            top,
            &mut prev_frame,
            sync_output,
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        // Every key — command-line included — goes through the engine now (F-026): the command-line
        // namespace owns its buffer, so the frontend no longer special-cases `:`/`/` typing.
        match engine.feed(key, st.mode()) {
            // A finished `:`-line (F-026): parse + run it. `submit_search` already folded a `/`-line
            // into `Feed::Cmd` inside the engine, so the frontend only sees the ex case here.
            Feed::ExecuteEx(text) => run_ex(
                &parse_ex(&text),
                &mut st,
                &path,
                fmt,
                &initial,
                &recorded,
                &mut status,
                &mut quit,
            ),
            Feed::Pending | Feed::Ignored => {}
            Feed::Cmd(cmd) => run_cmd(
                cmd,
                &mut st,
                &path,
                fmt,
                &mut recorded,
                &mut status,
                &mut quit,
            ),
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    run_cmd(
                        cmd,
                        &mut st,
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

/// Record a command and apply it, performing any effects.
fn run_cmd(
    cmd: Command,
    st: &mut EditorState,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    recorded.push(cmd.clone());
    for eff in apply_command(st, &cmd) {
        apply_effect(eff, st, path, fmt, status, quit);
    }
}

// The ex-line dispatcher legitimately needs the full editor context (buffer, file identity+format,
// the trace baseline+recording, and the status/quit sinks); grouping them into a struct would only
// move the wiring, not reduce it.
#[allow(clippy::too_many_arguments)]
fn run_ex(
    ex: &Ex,
    st: &mut EditorState,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    initial: &[u8],
    recorded: &[Command],
    status: &mut String,
    quit: &mut bool,
) {
    match ex {
        Ex::Save => save(st, path, fmt, status),
        Ex::Quit => *quit = true,
        Ex::SaveQuit => {
            save(st, path, fmt, status);
            *quit = true;
        }
        Ex::SaveTrace(p) => {
            let trace = Trace::record(initial, recorded.to_vec());
            match fs::write(p, trace.to_text()) {
                Ok(()) => *status = format!("trace saved: {p} ({} commands)", recorded.len()),
                Err(e) => *status = format!("trace save failed: {e}"),
            }
        }
        Ex::Unknown(s) => *status = format!("unknown command: {s}"),
    }
}

fn apply_effect(
    eff: Effect,
    st: &mut EditorState,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    status: &mut String,
    quit: &mut bool,
) {
    match eff {
        Effect::Save => save(st, path, fmt, status),
        Effect::Quit => *quit = true,
        Effect::Status(s) => *status = s,
    }
}

fn save(
    st: &mut EditorState,
    path: &Option<PathBuf>,
    fmt: persist::encoding::FileFormat,
    status: &mut String,
) {
    let Some(p) = path else {
        *status = "no file name (open with `ruse <file>`)".into();
        return;
    };
    // Restore the original encoding/line-endings (F-008 #2), then write durably (fsync + rename, #1).
    let bytes = fmt.to_disk(st.bytes());
    match persist::atomic::save(p, &bytes) {
        Ok(()) => {
            st.doc.mark_saved();
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

#[allow(clippy::too_many_arguments)] // the frame render legitimately needs the full view context
fn render(
    out: &mut io::Stdout,
    st: &EditorState,
    path: Option<&PathBuf>,
    cmd_line: Option<(char, &str)>,
    status: &str,
    spans: &[highlight::Span],
    top: usize,
    prev: &mut screen::Screen,
    sync: bool,
) -> io::Result<()> {
    use crossterm::style::Color;
    use unicode_segmentation::UnicodeSegmentation;

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text_rows = rows.saturating_sub(1) as usize;
    // Paint the whole frame into a fresh cell grid; the diff against `prev` emits only what changed.
    let mut cur = screen::Screen::new(cols, rows);

    let bytes = st.bytes();
    // Flatten the highlight spans into a per-byte color, then walk the text applying it.
    let mut byte_color = vec![Color::Reset; bytes.len()];
    for s in spans {
        for slot in byte_color
            .iter_mut()
            .take(s.end.min(bytes.len()))
            .skip(s.start)
        {
            *slot = s.color;
        }
    }
    let text = st.as_str().unwrap_or("<binary>");
    // Paint `text_rows` buffer lines from `top` into the grid, one GRAPHEME CLUSTER per cell (F-006
    // #4: a ZWJ emoji / base+combining is one user-perceived char at its true display width). Long
    // lines truncate at `cols` (no wrap; horizontal scroll deferred — render doc v0). Visual selection
    // paints in reverse video (a charwise/linewise span, or a blockwise rectangle).
    let sel = st.selection_span();
    let block = st.block_spans();
    let mut line: usize = 0;
    let mut col: u16 = 0;
    for (i, g) in text.grapheme_indices(true) {
        if g == "\n" {
            line += 1;
            col = 0;
            if line >= top + text_rows {
                break; // past the bottom of the viewport
            }
            continue;
        }
        if line < top || col >= cols {
            continue; // above the viewport, or past the right edge (truncate)
        }
        let srow = (line - top) as u16;
        let selected = sel.is_some_and(|(s, e)| i >= s && i < e)
            || block
                .as_deref()
                .is_some_and(|rows| rows.iter().any(|&(s, e)| i >= s && i < e));
        let fg = byte_color.get(i).copied().unwrap_or(Color::Reset);
        if g == "\t" {
            let stop = TAB_WIDTH - (col % TAB_WIDTH); // expand the tab to blanks up to the next stop
            for _ in 0..stop {
                if col >= cols {
                    break;
                }
                col = cur.put(srow, col, " ", fg, selected);
            }
        } else {
            col = cur.put(srow, col, g, fg, selected);
        }
    }

    let bar = match cmd_line {
        Some((prefix, text)) => format!("{prefix}{text}"),
        None => {
            let mode = match st.mode() {
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
            let dirty = if st.is_modified() { " [+]" } else { "" };
            format!("{mode}  {name}{dirty}  {status}")
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
        // The cursor's DISPLAY column is grapheme-cluster / width based (F-006 #4), never a char count.
        let (screen_row, screen_col) = cursor_cell(st.bytes(), st.cursor(), top);
        queue!(
            out,
            cursor::MoveTo(screen_col.min(cols.saturating_sub(1)), screen_row),
            cursor::Show
        )?;
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
