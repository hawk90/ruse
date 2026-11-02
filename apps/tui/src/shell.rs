//! Synchronous shell-out helpers for the `:r !{cmd}` read, the `:{range}!{cmd}` filter, and the `:!{cmd}`
//! run. These ex commands BLOCK in Vim too, so a blocking `std::process::Command` pipe (spawn `sh -c "cmd"`,
//! optionally feed stdin, capture output) is the honest, low-risk shape — the same pattern the OS-clipboard
//! provider ([`crate::clipboard`]) already uses. LOCAL process only: no network, no long-lived task.
//!
//! Like Vim's default `shellredir` (`>%s 2>&1`), stdout and stderr share one pipe, so a failing command's
//! message reaches the buffer or status line instead of vanishing (#861).
//!
//! Unix-only for now (`sh -c`). On other platforms both helpers return an error string so the caller can
//! surface it on the status line rather than silently doing nothing (Windows `cmd /C` is a documented
//! follow-up).

use std::fmt;
#[cfg(unix)]
use std::io::{Read, Write};
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

/// What a finished shell command produced: its stdout and stderr on one stream, and how it exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    /// stdout and stderr interleaved in the order the command wrote them (Vim's `shellredir` is
    /// `>%s 2>&1`), lossily decoded as UTF-8.
    pub text: String,
    /// The exit code; `None` when the shell was killed by a signal.
    pub code: Option<i32>,
}

impl ShellOutput {
    /// Vim's `shell returned {n}` message for a failed command; `None` when it exited 0.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        match self.code {
            Some(0) => None,
            Some(n) => Some(format!("shell returned {n}")),
            None => Some("shell killed by a signal".to_string()),
        }
    }
}

/// Run `cmd` through `sh -c` and CAPTURE its output (`:!{cmd}` and `:r !{cmd}`). stdin is `/dev/null`.
///
/// # Errors
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, or on a non-unix platform.
pub fn capture(cmd: &str) -> Result<ShellOutput, ShellError> {
    run(cmd, None)
}

/// Run `cmd` through `sh -c`, feeding `input` on stdin and CAPTURING its output (`:{range}!{cmd}` filter).
/// As in Vim the returned text REPLACES the piped lines whatever the exit status (#861).
///
/// # Errors
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, or on a non-unix platform.
pub fn filter(cmd: &str, input: &str) -> Result<ShellOutput, ShellError> {
    run(cmd, Some(input))
}

/// The shared unix `sh -c` runner: spawn the shell with stdout and stderr on ONE pipe, optionally feed
/// `input` to stdin from a writer thread (dropping it to send EOF) while the output is drained, then wait. A
/// non-zero exit is still `Ok` — Vim inserts / filters whatever the command produced and only reports
/// `shell returned {n}` — unless the shell could not be spawned. stderr must not be inherited: it would land
/// on the editor's own terminal, under the TUI.
#[cfg(unix)]
fn run(cmd: &str, input: Option<&str>) -> Result<ShellOutput, ShellError> {
    let (mut reader, writer) = std::io::pipe().map_err(ShellError::Spawn)?;
    let writer_err = writer.try_clone().map_err(ShellError::Spawn)?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(writer)
        .stderr(writer_err);
    let spawned = command.spawn();
    // `command` still owns the parent's copies of the write end; drop them so the read below sees EOF once
    // the shell (and anything it started) has closed its output.
    drop(command);
    let mut child = spawned.map_err(ShellError::Spawn)?;
    // Feed stdin from a thread while this thread drains stdout: writing all of it first deadlocks once the
    // input outgrows the pipe buffers, because a command like `cat` blocks writing output nobody reads (#854).
    let feeder = match (input, child.stdin.take()) {
        (Some(text), Some(mut stdin)) => {
            let bytes = text.as_bytes().to_vec();
            Some(std::thread::spawn(move || {
                // A broken pipe (the command exited before reading all input, e.g. `head`) is not an error.
                let _ = stdin.write_all(&bytes);
                // Dropping `stdin` closes it, sending EOF so the command can finish.
            }))
        }
        _ => None,
    };
    let mut out = Vec::new();
    let read = reader.read_to_end(&mut out);
    if let Some(handle) = feeder {
        // The writer returns once its input is written or the pipe is closed; it holds no result.
        let _ = handle.join();
    }
    let status = child.wait().map_err(ShellError::Wait)?;
    read.map_err(ShellError::Wait)?;
    Ok(ShellOutput {
        text: String::from_utf8_lossy(&out).into_owned(),
        code: status.code(),
    })
}

#[cfg(not(unix))]
fn run(_cmd: &str, _input: Option<&str>) -> Result<ShellOutput, ShellError> {
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
        assert_eq!(out.text, "a\nb\n");
        assert_eq!(out.failure(), None);
    }

    #[test]
    fn filter_pipes_stdin_through_the_command() {
        if !has("sort") {
            eprintln!("skipping: `sort` not on PATH");
            return;
        }
        let out = filter("sort", "gamma\nalpha\nbeta\n").expect("sort runs");
        assert_eq!(out.text, "alpha\nbeta\ngamma\n");
    }

    #[test]
    fn filter_transforms_with_tr() {
        if !has("tr") {
            eprintln!("skipping: `tr` not on PATH");
            return;
        }
        let out = filter("tr a-z A-Z", "one\ntwo\n").expect("tr runs");
        assert_eq!(out.text, "ONE\nTWO\n");
    }

    /// #854: input larger than the pipe buffers must not deadlock against a command that streams output.
    #[test]
    fn filter_streams_input_larger_than_the_pipe_buffers() {
        if !has("cat") {
            eprintln!("skipping: `cat` not on PATH");
            return;
        }
        let input = "0123456789abcdef\n".repeat(64 * 1024); // 1 MiB
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = input.clone();
        std::thread::spawn(move || {
            let _ = tx.send(filter("cat", &sent).map_err(|e| e.to_string()));
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("`cat` filter of 1 MiB deadlocked")
            .expect("cat runs");
        assert_eq!(out.text.len(), input.len());
        assert!(out.text == input);
    }

    #[test]
    fn a_failing_command_is_not_an_error_and_yields_its_output() {
        // nvim v0.12.6 `:2!cat; exit 1` keeps what `cat` wrote and reports `shell returned 1`; the caller
        // decides what to show.
        let out = filter("cat; exit 1", "keep\n").expect("cat still emitted stdout");
        assert_eq!(out.text, "keep\n");
        assert_eq!(out.failure().as_deref(), Some("shell returned 1"));
    }

    /// #861: stderr is captured with stdout, in write order, as Vim's `2>&1` shellredir does. nvim v0.12.6
    /// `:%!echo out; echo err >&2; exit 3` leaves `out\nerr\n` and says `shell returned 3`.
    #[test]
    fn stderr_is_captured_in_order_with_stdout() {
        let out = filter("echo out; echo err >&2; exit 3", "x\n").expect("sh runs");
        assert_eq!(out.text, "out\nerr\n");
        assert_eq!(out.code, Some(3));
        let out = capture("nonexistent_cmd_zz_861").expect("sh runs");
        assert!(out.text.contains("nonexistent_cmd_zz_861"), "{out:?}");
        assert_eq!(out.failure().as_deref(), Some("shell returned 127"));
    }
}
