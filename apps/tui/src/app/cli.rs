//! CLI entry point: parse argv, then dispatch to the interactive session or the headless `--replay`.
//!
//! Split out of `main.rs` so the entire frontend lives in the `ruse-tui` library — the `ruse` binary is
//! a one-line shim over [`run`]. All IO lives here; the core stays pure, so `ruse --replay <trace> <file>`
//! reproduces an edit session deterministically without a terminal.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ruse_core::Trace;

use crate::app::session::run as session_run;

/// Program entry: initialise logging + the crash-recovery hook, then run the editor (or `--replay`).
// Headless CLI: stderr is the correct channel here (no TUI, no tracing sink yet). D-041 scoped allow.
#[allow(clippy::print_stderr)]
pub fn run() -> ExitCode {
    crate::log::init();
    crate::recover::install_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--replay") {
        return replay(&args[1..]);
    }
    let path = args.first().map(PathBuf::from);
    // Raw on-disk bytes (BOM/CRLF intact); session run() detects the format and normalises for the buffer.
    let raw = path
        .as_ref()
        .and_then(|p| fs::read(p).ok())
        .unwrap_or_default();
    match session_run(path, raw) {
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
