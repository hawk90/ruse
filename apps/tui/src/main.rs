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
mod recover;
mod viewport;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
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
    let initial = path
        .as_ref()
        .and_then(|p| fs::read(p).ok())
        .unwrap_or_default();
    match run(path, initial) {
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

fn run(path: Option<PathBuf>, initial: Vec<u8>) -> io::Result<()> {
    let mut st = EditorState::new(initial.clone());
    let mut recorded: Vec<Command> = Vec::new();
    let mut cmd_line: Option<(char, String)> = None; // ':' ex-line or '/' search-line + typed text
    let mut status = String::from("ruse — :q to quit");

    // Syntax highlighting (Rust only for v0) lives in the frontend; core stays dep-free.
    let mut highlighter = match path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
    {
        Some("rs") => highlight::CachedHighlight::rust(),
        _ => None,
    };

    let _guard = TermGuard::enter()?;
    let mut out = io::stdout();
    let mut engine = InputEngine::new();
    let mut quit = false;
    let mut top: usize = 0; // first visible buffer row (frontend view state; core stays view-free)

    while !quit {
        // Keep the crash-recovery snapshot current so a core panic can rescue unsaved work (§6/§8).
        recover::update(path.as_ref(), st.bytes(), st.is_modified());
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
            cmd_line.as_ref().map(|(p, t)| (*p, t.as_str())),
            &status,
            spans,
            top,
        )?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if let Some((_, line)) = cmd_line.as_mut() {
            match key.code {
                KeyCode::Esc => cmd_line = None,
                KeyCode::Backspace => {
                    line.pop();
                }
                KeyCode::Char(c) => line.push(c),
                KeyCode::Enter => {
                    let (prefix, text) = cmd_line.take().unwrap_or((':', String::new()));
                    if prefix == ':' {
                        run_ex(
                            &parse_ex(&text),
                            &mut st,
                            &path,
                            &initial,
                            &recorded,
                            &mut status,
                            &mut quit,
                        );
                    } else if let Feed::Cmd(cmd) = engine.submit_search(text) {
                        // `submit_search` folds the pattern into any operator/count that preceded `/`
                        // (`d/pat`, `2/pat`) and records it for `n`/`N`; an empty pattern yields Ignored.
                        run_cmd(cmd, &mut st, &path, &mut recorded, &mut status, &mut quit);
                    }
                }
                _ => {}
            }
            continue;
        }
        match engine.feed(key, st.mode()) {
            Feed::OpenExLine => cmd_line = Some((':', String::new())),
            Feed::OpenSearch => cmd_line = Some(('/', String::new())),
            Feed::Pending | Feed::Ignored => {}
            Feed::Cmd(cmd) => run_cmd(cmd, &mut st, &path, &mut recorded, &mut status, &mut quit),
            // `.` (dot-repeat) replays the last change; record and apply each concrete command so the
            // trace (F-022) captures the resolved edit, not the `.` keypress.
            Feed::Replay(cmds) => {
                for cmd in cmds {
                    run_cmd(cmd, &mut st, &path, &mut recorded, &mut status, &mut quit);
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
    recorded: &mut Vec<Command>,
    status: &mut String,
    quit: &mut bool,
) {
    recorded.push(cmd.clone());
    for eff in apply_command(st, &cmd) {
        apply_effect(eff, st, path, status, quit);
    }
}

fn run_ex(
    ex: &Ex,
    st: &mut EditorState,
    path: &Option<PathBuf>,
    initial: &[u8],
    recorded: &[Command],
    status: &mut String,
    quit: &mut bool,
) {
    match ex {
        Ex::Save => save(st, path, status),
        Ex::Quit => *quit = true,
        Ex::SaveQuit => {
            save(st, path, status);
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
    status: &mut String,
    quit: &mut bool,
) {
    match eff {
        Effect::Save => save(st, path, status),
        Effect::Quit => *quit = true,
        Effect::Status(s) => *status = s,
    }
}

fn save(st: &mut EditorState, path: &Option<PathBuf>, status: &mut String) {
    let Some(p) = path else {
        *status = "no file name (open with `ruse <file>`)".into();
        return;
    };
    match fs::write(p, st.bytes()) {
        Ok(()) => {
            st.doc.mark_saved();
            tracing::info!(event = "save", path = %p.display(), bytes = st.bytes().len());
            *status = format!("\"{}\" written", p.display());
        }
        Err(e) => {
            // Expected external failure (§7): surface it in the status bar, log once, keep the buffer.
            tracing::warn!(event = "save.failed", path = %p.display(), error = %e);
            *status = format!("write failed: {e}");
        }
    }
}

fn render(
    out: &mut io::Stdout,
    st: &EditorState,
    path: Option<&PathBuf>,
    cmd_line: Option<(char, &str)>,
    status: &str,
    spans: &[highlight::Span],
    top: usize,
) -> io::Result<()> {
    use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let text_rows = rows.saturating_sub(1);
    queue!(
        out,
        cursor::Hide,
        Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

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
    // Draw the `text_rows` buffer lines starting at `top`. `line` tracks the buffer row so we can skip
    // everything above the viewport; `screen_row` is where it lands. Long lines are truncated at `cols`
    // (no wrap) so screen-row math stays exact — horizontal scroll is deferred (see render doc, v0).
    // The Visual selection, painted in reverse video: one contiguous range for charwise/linewise, or a
    // per-row set of ranges for a blockwise (`CTRL-V`) rectangle.
    let sel = st.selection_span();
    let block = st.block_spans();
    let mut line: usize = 0;
    let mut col: u16 = 0;
    let mut cur = Color::Reset;
    let mut reversed = false;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            line += 1;
            if line >= top + text_rows as usize {
                break; // past the bottom of the viewport
            }
            if line >= top {
                col = 0;
                queue!(out, cursor::MoveTo(0, (line - top) as u16))?;
            }
            continue;
        }
        if line < top || col >= cols {
            continue; // above the viewport, or past the right edge (truncate)
        }
        let selected = sel.is_some_and(|(s, e)| i >= s && i < e)
            || block
                .as_deref()
                .is_some_and(|rows| rows.iter().any(|&(s, e)| i >= s && i < e));
        if selected != reversed {
            queue!(
                out,
                SetAttribute(if selected {
                    Attribute::Reverse
                } else {
                    Attribute::NoReverse
                })
            )?;
            reversed = selected;
        }
        let c = byte_color.get(i).copied().unwrap_or(Color::Reset);
        if c != cur {
            queue!(out, SetForegroundColor(c))?;
            cur = c;
        }
        queue!(out, Print(ch))?;
        col += 1;
    }
    if reversed {
        queue!(out, SetAttribute(Attribute::NoReverse))?;
    }
    queue!(out, ResetColor)?;

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
    let bar: String = bar.chars().take(cols as usize).collect();
    queue!(out, cursor::MoveTo(0, rows.saturating_sub(1)), Print(bar))?;

    if let Some((_, text)) = cmd_line {
        queue!(
            out,
            cursor::MoveTo((text.len() + 1) as u16, rows.saturating_sub(1)),
            cursor::Show
        )?;
    } else {
        // Place the terminal cursor relative to the viewport top; clamp col to the truncation width.
        let (row, col) = row_col(st.bytes(), st.cursor());
        let screen_row = row.saturating_sub(top) as u16;
        let screen_col = (col as u16).min(cols.saturating_sub(1));
        queue!(out, cursor::MoveTo(screen_col, screen_row), cursor::Show)?;
    }
    out.flush()
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
