//! CLI entry point: parse argv, then dispatch to the interactive session or the headless `--replay`.
//!
//! Split out of `main.rs` so the entire frontend lives in the `ruse-tui` library — the `ruse` binary is
//! a one-line shim over [`run`]. All IO lives here; the core stays pure, so `ruse --replay <trace> <file>`
//! reproduces an edit session deterministically without a terminal.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use ruse_core::Trace;

use crate::remote::agent;
use crate::remote::client::AgentClient;

use crate::app::session::run as session_run;

/// Program entry: initialise logging + the crash-recovery hook, then run the editor (or `--replay`).
// Headless CLI: stderr is the correct channel here (no TUI, no tracing sink yet). D-041 scoped allow.
#[allow(clippy::print_stderr)]
pub fn run() -> ExitCode {
    crate::log::init();
    crate::recover::install_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // `ruse --replay <trace> <file>`: headless determinism replay (F-022).
        Some("--replay") => return replay(&args[1..]),
        // `ruse agent`: run the headless Workspace Agent over stdio (F-017). Spawned locally by `ruse ssh`
        // today, over SSH later. Blocks serving the client↔agent protocol until EOF.
        Some("agent") => return agent_serve(),
        // `ruse ssh <host>`: F-017 slice 1 is a LOCAL proof of the client↔agent split — spawn `ruse agent`
        // as a subprocess, handshake, and print the negotiated protocol version + capabilities. Real SSH
        // transport + agent bootstrap are later slices.
        Some("ssh") => return ssh_connect(args.get(1).map(String::as_str)),
        _ => {}
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

/// `ruse agent` (F-017): run the headless Workspace Agent, serving the client↔agent protocol over stdio
/// until EOF. No terminal, no UI — pure execution runtime (the piece that runs remotely in later slices).
#[allow(clippy::print_stderr)] // headless: stderr is the correct channel (D-041).
fn agent_serve() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    match agent::serve(stdin.lock(), stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ruse agent: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `ruse ssh <host>` (F-017 slice 1): the LOCAL proof of the client↔agent split — spawn `<self> agent` as a
/// subprocess, complete the version/capability handshake, and print the negotiated result. Real SSH transport
/// + agent bootstrap are later slices; this proves the protocol foundation end-to-end without them.
#[allow(clippy::print_stdout, clippy::print_stderr)] // headless CLI output (D-041).
fn ssh_connect(host: Option<&str>) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ruse ssh: cannot locate the agent binary: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg("agent");
    match AgentClient::spawn(cmd, &["fs.readFile"]) {
        Ok(client) => {
            let h = host.unwrap_or("<local>");
            println!(
                "ruse ssh {h}: connected to Workspace Agent (protocol v{}, capabilities: {:?})",
                client.protocol_version(),
                client.capabilities(),
            );
            if host.is_some() {
                eprintln!(
                    "note: real SSH transport is not wired yet — F-017 slice 1 is a local proof."
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ruse ssh: agent handshake failed: {e}");
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
