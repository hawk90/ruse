//! Synchronous shell-out helpers for the `:r !{cmd}` read, the `:{range}!{cmd}` filter, and the `:!{cmd}`
//! run. These ex commands BLOCK in Vim too, so a blocking `std::process::Command` pipe (spawn `sh -c "cmd"`,
//! optionally feed stdin, capture stdout) is the honest, low-risk shape — the same pattern the OS-clipboard
//! provider ([`crate::clipboard`]) already uses. LOCAL process only: no network, no long-lived task.
//!
//! Unix-only for now (`sh -c`). On other platforms both helpers return an error string so the caller can
//! surface it on the status line rather than silently doing nothing (Windows `cmd /C` is a documented
//! follow-up).

use std::fmt;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::process::{Command, Stdio};

/// Why a shell-out failed (D-041: a typed error, never `Result<_, String>`). Its [`fmt::Display`] renders the
/// status-line message the ex commands show (Vim's `E485` for a spawn failure).
#[derive(Debug)]
pub enum ShellError {
    /// The shell (`sh`) process could not be spawned.
    Spawn(std::io::Error),
    /// Waiting for the shell to finish / capturing its output failed.
    Wait(std::io::Error),
    /// Shell commands are not supported on this (non-unix) platform yet.
    Unsupported,
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::Spawn(e) => write!(f, "E485: cannot run shell: {e}"),
            ShellError::Wait(e) => write!(f, "E485: shell wait failed: {e}"),
            ShellError::Unsupported => {
                write!(f, "shell commands are not supported on this platform")
            }
        }
    }
}

impl std::error::Error for ShellError {}

/// Run `cmd` through `sh -c` and CAPTURE its stdout (`:!{cmd}` and `:r !{cmd}`). No stdin is fed. Returns the
/// captured stdout on success. stderr is inherited (it appears wherever the process's stderr goes), matching
/// how these commands are informational.
///
/// # Errors
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, or on a non-unix platform.
pub fn capture(cmd: &str) -> Result<String, ShellError> {
    run(cmd, None)
}

/// Run `cmd` through `sh -c`, feeding `input` on stdin and CAPTURING stdout (`:{range}!{cmd}` filter). The
/// returned stdout REPLACES the piped lines.
///
/// # Errors
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, or on a non-unix platform.
pub fn filter(cmd: &str, input: &str) -> Result<String, ShellError> {
    run(cmd, Some(input))
}

/// The shared unix `sh -c` runner: spawn the shell, optionally write `input` to stdin (dropping it to send
/// EOF), wait, and return captured stdout. A non-zero exit is still returned as `Ok(stdout)` — Vim inserts /
/// filters whatever the command produced regardless of its status — unless the shell could not be spawned.
#[cfg(unix)]
fn run(cmd: &str, input: Option<&str>) -> Result<String, ShellError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .spawn()
        .map_err(ShellError::Spawn)?;
    if let Some(text) = input {
        if let Some(mut stdin) = child.stdin.take() {
            // A broken pipe (the command exited before reading all input, e.g. `head`) is not an error.
            let _ = stdin.write_all(text.as_bytes());
            // Dropping `stdin` here closes it, sending EOF so the command can finish.
        }
    }
    let out = child.wait_with_output().map_err(ShellError::Wait)?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(unix))]
fn run(_cmd: &str, _input: Option<&str>) -> Result<String, ShellError> {
    Err(ShellError::Unsupported)
}

#[cfg(all(test, unix))]
#[allow(clippy::print_stderr)] // test-only skip diagnostics when an expected POSIX tool is absent
mod tests {
    use super::*;

    /// Whether `program` resolves on PATH — skip a test gracefully when the tool is missing.
    fn has(program: &str) -> bool {
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {program}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn capture_reads_command_stdout() {
        let out = capture("printf 'a\\nb\\n'").expect("printf runs");
        assert_eq!(out, "a\nb\n");
    }

    #[test]
    fn filter_pipes_stdin_through_the_command() {
        if !has("sort") {
            eprintln!("skipping: `sort` not on PATH");
            return;
        }
        let out = filter("sort", "gamma\nalpha\nbeta\n").expect("sort runs");
        assert_eq!(out, "alpha\nbeta\ngamma\n");
    }

    #[test]
    fn filter_transforms_with_tr() {
        if !has("tr") {
            eprintln!("skipping: `tr` not on PATH");
            return;
        }
        let out = filter("tr a-z A-Z", "one\ntwo\n").expect("tr runs");
        assert_eq!(out, "ONE\nTWO\n");
    }

    #[test]
    fn a_failing_command_is_not_an_error_and_yields_its_stdout() {
        // `false` exits non-zero with no stdout; Vim would filter to nothing (delete the lines), so we return
        // Ok("") rather than Err — the caller decides what an empty result means.
        let out = filter("cat; false", "keep\n").expect("cat still emitted stdout");
        assert_eq!(out, "keep\n");
    }
}
