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

mod highlight;
mod input;
mod log;
mod recover;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, queue, terminal};

use input::{parse_ex, Ex, Feed, InputEngine};
use ruse_core::{apply_command, Command, EditorState, Effect, Mode, Trace};

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

/// Restores the terminal on drop, even on panic.
struct TermGuard;
impl TermGuard {
    fn enter() -> io::Result<TermGuard> {
        terminal::enable_raw_mode()?;
        queue!(io::stdout(), terminal::EnterAlternateScreen)?;
        io::stdout().flush()?;
        Ok(TermGuard)
    }
}
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = queue!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        let _ = io::stdout().flush();
        let _ = terminal::disable_raw_mode();
    }
}

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
        Some("rs") => highlight::Highlight::rust(),
        _ => None,
    };

    let _guard = TermGuard::enter()?;
    let mut out = io::stdout();
    let mut engine = InputEngine::new();
    let mut quit = false;

    while !quit {
        // Keep the crash-recovery snapshot current so a core panic can rescue unsaved work (§6/§8).
        recover::update(path.as_ref(), st.bytes(), st.is_modified());
        let spans = highlighter
            .as_mut()
            .map(|h| h.spans(st.bytes()))
            .unwrap_or_default();
        render(
            &mut out,
            &st,
            path.as_ref(),
            cmd_line.as_ref().map(|(p, t)| (*p, t.as_str())),
            &status,
            &spans,
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
                    } else {
                        engine.set_last_search(text.clone());
                        if !text.is_empty() {
                            run_cmd(
                                Command::SearchNext(text),
                                &mut st,
                                &path,
                                &mut recorded,
                                &mut status,
                                &mut quit,
                            );
                        }
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
) -> io::Result<()> {
    use crossterm::style::{Color, ResetColor, SetForegroundColor};

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
    let mut row: u16 = 0;
    let mut cur = Color::Reset;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            row += 1;
            if row >= text_rows {
                break;
            }
            queue!(out, cursor::MoveTo(0, row))?;
            continue;
        }
        let c = byte_color.get(i).copied().unwrap_or(Color::Reset);
        if c != cur {
            queue!(out, SetForegroundColor(c))?;
            cur = c;
        }
        queue!(out, Print(ch))?;
    }
    queue!(out, ResetColor)?;

    let bar = match cmd_line {
        Some((prefix, text)) => format!("{prefix}{text}"),
        None => {
            let mode = match st.mode() {
                Mode::Normal => "NORMAL",
                Mode::Insert => "INSERT",
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
        let (row, col) = row_col(st.bytes(), st.cursor());
        queue!(out, cursor::MoveTo(col as u16, row as u16), cursor::Show)?;
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
