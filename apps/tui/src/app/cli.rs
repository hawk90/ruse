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

use crate::remote::agent;
use crate::remote::client::AgentClient;
use crate::remote::transport;

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
        // `ruse ssh [host]`: connect to a Workspace Agent and print the negotiated version + capabilities.
        // With a host → SSH stdio (`ssh host ruse agent`, slice 2a; requires `ruse` on the remote PATH until
        // agent bootstrap lands). No host → the local pipe proof (`ruse agent` subprocess, slice 1).
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

/// `ruse ssh [host]` (F-017): connect to a Workspace Agent, complete the version/capability handshake, and
/// print the negotiated result. With a `host` this uses the SSH stdio transport (`ssh host ruse agent`, slice
/// 2a); with none it falls back to the local pipe proof (`<self> agent`, slice 1). Agent bootstrap/install is
/// a later slice, so a remote host must already have `ruse` on its `PATH`.
#[allow(clippy::print_stdout, clippy::print_stderr)] // headless CLI output (D-041).
fn ssh_connect(host: Option<&str>) -> ExitCode {
    let cmd = match host {
        Some(h) => transport::ssh_command(h, transport::DEFAULT_REMOTE_AGENT_CMD),
        None => match std::env::current_exe() {
            Ok(exe) => transport::local_command(exe),
            Err(e) => {
                eprintln!("ruse ssh: cannot locate the agent binary: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    match AgentClient::spawn(cmd, &["fs.readFile"]) {
        Ok(client) => {
            let h = host.unwrap_or("<local>");
            println!(
                "ruse ssh {h}: connected to Workspace Agent (protocol v{}, capabilities: {:?})",
                client.protocol_version(),
                client.capabilities(),
            );
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
