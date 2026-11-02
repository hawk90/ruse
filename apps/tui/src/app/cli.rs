//! CLI entry point: parse argv, then dispatch to the interactive session or the headless `--replay`.
//!
//! Split out of `main.rs` so the entire frontend lives in the `ruse-tui` library — the `ruse` binary is
//! a one-line shim over [`run`]. All IO lives here; the core stays pure, so `ruse --replay <trace> <file>`
//! reproduces an edit session deterministically without a terminal.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ruse_core::Trace;

use crate::remote::agent;
use crate::remote::client::AgentClient;
use crate::remote::transport;

use crate::app::session::run as session_run;

/// `ruse --help` text. Paths are the only arguments the interactive editor takes today.
const USAGE: &str = "\
usage: ruse [--] [file]
       ruse --replay <trace> <file>
       ruse agent
       ruse ssh [host]
       ruse --help | --version

Open <file> in the editor, or an unnamed buffer with no file.
Use `--` before a file name that starts with '-'.

Environment:
  RUSE_PROFILE=vim|emacs|native   key profile (default vim)
";

/// What the command line asks for (#896).
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Version,
    /// A usage error: the message, printed with a pointer to `--help`, exit status 2.
    Usage(String),
    Replay(Vec<String>),
    Agent,
    Ssh(Option<String>),
    /// The interactive editor on one file, or on an unnamed buffer.
    Edit(Option<PathBuf>),
}

/// Parse argv (without the program name). A leading `-` word is an option; after `--` every word is a
/// path. Subcommands are recognised only as the first word, so `ruse -- agent` opens a file named `agent`.
fn parse(args: &[OsString]) -> Invocation {
    let first = args.first().and_then(|a| a.to_str());
    let rest_strings = || -> Option<Vec<String>> {
        args[1..]
            .iter()
            .map(|a| a.to_str().map(str::to_owned))
            .collect()
    };
    match first {
        Some("--help" | "-h") => return Invocation::Help,
        Some("--version" | "-V") => return Invocation::Version,
        Some("--replay") => {
            return match rest_strings() {
                Some(rest) => Invocation::Replay(rest),
                None => Invocation::Usage("--replay arguments must be valid UTF-8".into()),
            }
        }
        Some("agent") => return Invocation::Agent,
        Some("ssh") => {
            return match rest_strings().as_deref() {
                Some([]) => Invocation::Ssh(None),
                Some([host]) => Invocation::Ssh(Some(host.clone())),
                _ => Invocation::Usage("usage: ruse ssh [host]".into()),
            }
        }
        _ => {}
    }
    let mut files: Vec<&OsString> = Vec::new();
    let mut options_done = false;
    for arg in args {
        if !options_done && arg == "--" {
            options_done = true;
        } else if !options_done && arg.as_encoded_bytes().first() == Some(&b'-') {
            return Invocation::Usage(format!("unknown option: {}", arg.to_string_lossy()));
        } else {
            files.push(arg);
        }
    }
    match files.as_slice() {
        [] => Invocation::Edit(None),
        [one] => Invocation::Edit(Some(PathBuf::from(one))),
        // nvim opens every file into its argument list. ruse has no argument list yet, and opening only
        // the first would drop the rest without a word, so refuse until it does.
        _ => Invocation::Usage(format!(
            "{} files given; ruse opens one file (use :e for more)",
            files.len()
        )),
    }
}

/// Program entry: initialise logging + the crash-recovery hook, then run the editor (or `--replay`).
// Headless CLI: stderr is the correct channel here (no TUI, no tracing sink yet). D-041 scoped allow.
#[allow(clippy::print_stderr)]
pub fn run() -> ExitCode {
    crate::log::init();
    crate::recover::install_hook();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let path = match parse(&args) {
        // Written, not `print!`ed: a closed stdout (`ruse --help | true`) must not panic.
        Invocation::Help => {
            let _ = io::stdout().write_all(USAGE.as_bytes());
            return ExitCode::SUCCESS;
        }
        Invocation::Version => {
            let _ = writeln!(io::stdout(), "ruse {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Invocation::Usage(msg) => {
            eprintln!("ruse: {msg}\nTry 'ruse --help' for more information.");
            return ExitCode::from(2);
        }
        // `ruse --replay <trace> <file>`: headless determinism replay (F-022).
        Invocation::Replay(rest) => return replay(&rest),
        // `ruse agent`: run the headless Workspace Agent over stdio (F-017). Spawned locally by `ruse ssh`
        // today, over SSH later. Blocks serving the client↔agent protocol until EOF.
        Invocation::Agent => return agent_serve(),
        // `ruse ssh [host]`: connect to a Workspace Agent and print the negotiated version + capabilities.
        // With a host → SSH stdio (`ssh host ruse agent`, slice 2a; requires `ruse` on the remote PATH until
        // agent bootstrap lands). No host → the local pipe proof (`ruse agent` subprocess, slice 1).
        Invocation::Ssh(host) => return ssh_connect(host.as_deref()),
        Invocation::Edit(path) => path,
    };
    // The editor draws on stdout and reads keys from stdin. Without a terminal on both it cannot run, and
    // the raw-mode call would fail with a bare OS error (#896); say which side is missing instead.
    {
        use std::io::IsTerminal;
        for (name, is_tty) in [
            ("input", io::stdin().is_terminal()),
            ("output", io::stdout().is_terminal()),
        ] {
            if !is_tty {
                eprintln!("ruse: standard {name} is not a terminal");
                return ExitCode::FAILURE;
            }
        }
    }
    // Raw on-disk bytes (BOM/CRLF intact); session run() detects the format and normalises for the buffer.
    // Only a missing file is a new buffer. Any other read error must not open blank: `:w` would then replace
    // the unread file.
    let raw = match path.as_deref().map(crate::persist::read_file) {
        None => Vec::new(),
        Some(Ok(bytes)) => bytes,
        Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Some(Err(e)) => {
            let shown = path.as_deref().unwrap_or(Path::new("")).display();
            eprintln!("ruse: {shown}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = session_run(path, raw);
    // From here this thread ends the process; the signal watchdog stands down (#897).
    let _exiting = crate::signals::begin_exit();
    // The terminal guard has already restored the terminal. A fatal signal or an I/O error ended the session
    // with unsaved work possibly unjournaled (#857): write every modified buffer's journal. A normal quit
    // (including `:q!`, which discards on purpose) leaves nothing to rescue.
    let signal = crate::signals::pending();
    if signal.is_some() || result.is_err() {
        crate::recover::rescue_on_exit(if signal.is_some() {
            "signal"
        } else {
            "io_error"
        });
    }
    if let Some(sig) = signal {
        crate::signals::reraise(sig);
    }
    match result {
        // `:cq [N]` ends the session normally but asks for a failing exit status (default 1).
        Ok(()) => ExitCode::from(crate::app::dispatch::exit_status()),
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
        Some(h) => match transport::ssh_command(h, transport::DEFAULT_REMOTE_AGENT_CMD) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ruse ssh: {e}");
                return ExitCode::FAILURE;
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Invocation {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(&args)
    }

    fn usage(inv: Invocation) -> String {
        match inv {
            Invocation::Usage(m) => m,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_opens_an_unnamed_buffer() {
        assert_eq!(p(&[]), Invocation::Edit(None));
    }

    #[test]
    fn one_word_is_a_file() {
        assert_eq!(p(&["a.txt"]), Invocation::Edit(Some("a.txt".into())));
    }

    #[test]
    fn help_and_version() {
        for a in ["--help", "-h"] {
            assert_eq!(p(&[a]), Invocation::Help);
        }
        for a in ["--version", "-V"] {
            assert_eq!(p(&[a]), Invocation::Version);
        }
        assert!(USAGE.starts_with("usage: ruse"));
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        for a in ["-x", "--bogus", "-"] {
            assert_eq!(usage(p(&[a])), format!("unknown option: {a}"));
        }
        // An option after a file is still an option.
        assert_eq!(usage(p(&["a.txt", "-x"])), "unknown option: -x");
        // `--help` is only recognised first, like the subcommands.
        assert_eq!(usage(p(&["a.txt", "--help"])), "unknown option: --help");
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(p(&["--", "-x"]), Invocation::Edit(Some("-x".into())));
        assert_eq!(p(&["--", "--"]), Invocation::Edit(Some("--".into())));
        assert_eq!(p(&["--"]), Invocation::Edit(None));
        assert_eq!(p(&["--", "agent"]), Invocation::Edit(Some("agent".into())));
        assert_eq!(p(&["a.txt", "--"]), Invocation::Edit(Some("a.txt".into())));
    }

    #[test]
    fn more_than_one_file_is_refused() {
        assert!(usage(p(&["a", "b"])).starts_with("2 files given"));
        assert!(usage(p(&["--", "a", "-b", "c"])).starts_with("3 files given"));
    }

    #[test]
    fn subcommands_keep_their_arguments() {
        assert_eq!(
            p(&["--replay", "t", "f"]),
            Invocation::Replay(vec!["t".into(), "f".into()])
        );
        assert_eq!(p(&["agent"]), Invocation::Agent);
        assert_eq!(p(&["ssh"]), Invocation::Ssh(None));
        assert_eq!(p(&["ssh", "h"]), Invocation::Ssh(Some("h".into())));
        assert!(matches!(p(&["ssh", "h", "x"]), Invocation::Usage(_)));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_open() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"caf\xe9.txt".to_vec());
        assert_eq!(
            parse(std::slice::from_ref(&raw)),
            Invocation::Edit(Some(PathBuf::from(raw)))
        );
    }
}
