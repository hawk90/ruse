//! Synchronous shell-out helpers for the `:r !{cmd}` read, the `:{range}!{cmd}` filter, the `:w !{cmd}` pipe
//! and the `:!{cmd}` run. These ex commands BLOCK in Vim too, so the editor thread waits for the shell (spawn
//! `sh -c "cmd"`, optionally feed stdin, capture output). LOCAL process only: no network, no long-lived task.
//!
//! Like Vim's default `shellredir` (`>%s 2>&1`), stdout and stderr share one pipe, so a failing command's
//! message reaches the buffer or status line instead of vanishing (#861).
//!
//! The wait can be cancelled (#855, scheduler.md §8): while the shell runs, the editor thread polls the
//! user-interrupt checkpoint ([`ruse_core::interrupt::breakcheck`], CTRL-C in the TUI). On an interrupt the
//! shell's whole process group (it runs in its own) gets `SIGTERM`, then `SIGKILL` if it is still there
//! after 2 s, as in nvim (#1748), and is reaped ([`crate::reap::SHELL`]). Unlike nvim, a group member
//! that outlives the shell (one that ignores `SIGTERM`) is SIGKILLed when the grace runs out, without the
//! cancel waiting for it ([`crate::reap::end_group`], #1787). The helper returns
//! [`ShellError::Interrupted`], so a cancelled filter or read leaves the buffer as it was. The captured
//! output is bounded too: past [`MAX_OUTPUT`] the command is killed and its output kept up to the cap,
//! reported as truncated; a filter, whose output replaces lines, refuses a truncated result instead.
//!
//! Unix-only for now (`sh -c`). On other platforms the helpers return an error so the caller can surface
//! it on the status line rather than silently doing nothing (Windows `cmd /C` is a documented follow-up).

use std::fmt;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::process::{Command, Stdio};

/// The most output a shell command may hand back to `:!`, `:r !` or `:w !`: past it the command is killed
/// and what it wrote up to the cap is kept, marked [`ShellOutput::truncated`].
pub const MAX_OUTPUT: usize = 4 << 20;

/// A filter may write this many times its input (when that is more than [`MAX_OUTPUT`]): its output
/// replaces the lines, so a large buffer through `sort` or `xxd` must still fit.
const FILTER_GROWTH: usize = 8;

/// Why a shell-out failed (D-041: a typed error, never `Result<_, String>`). Its [`fmt::Display`] renders the
/// status-line message the ex commands show (Vim's `E485` for a spawn failure).
#[derive(Debug)]
pub enum ShellError {
    /// The shell (`sh`) process could not be spawned.
    Spawn(std::io::Error),
    /// Waiting for the shell to finish / capturing its output failed.
    Wait(std::io::Error),
    /// The user interrupted the command (CTRL-C); it was killed and nothing was changed.
    Interrupted,
    /// A filter wrote more than its output limit (bytes); it was killed and the lines were left alone.
    TooLarge(usize),
    /// Shell commands are not supported on this (non-unix) platform yet.
    Unsupported,
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::Spawn(e) => write!(f, "E485: cannot run shell: {e}"),
            ShellError::Wait(e) => write!(f, "E485: shell wait failed: {e}"),
            ShellError::Interrupted => f.write_str("Interrupted"),
            ShellError::TooLarge(limit) => write!(
                f,
                "E485: filter output over {} MiB, buffer unchanged",
                limit >> 20
            ),
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
    /// The same output as it came, byte for byte: what a filter puts back into the buffer.
    pub raw: Vec<u8>,
    /// The exit code; `None` when the shell was killed by a signal.
    pub code: Option<i32>,
    /// The command wrote more than its output limit: it was killed, and `text` / `raw` hold what it wrote
    /// up to the limit.
    pub truncated: bool,
}

impl ShellOutput {
    /// Vim's `shell returned {n}` message for a failed command, or the note that the output was cut off at
    /// [`MAX_OUTPUT`]; `None` when it exited 0.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        if self.truncated {
            return Some(format!(
                "output truncated at {} MiB, shell killed",
                MAX_OUTPUT >> 20
            ));
        }
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
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, is interrupted, or on a non-unix
/// platform.
pub fn capture(cmd: &str) -> Result<ShellOutput, ShellError> {
    run(cmd, None, MAX_OUTPUT)
}

/// Run `cmd` through `sh -c`, feeding `input` on stdin, and CAPTURE its output for display (`:w !{cmd}`):
/// as with [`capture`], output past [`MAX_OUTPUT`] is cut off and marked truncated.
///
/// # Errors
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, is interrupted, or on a non-unix
/// platform.
pub fn pipe(cmd: &str, input: &[u8]) -> Result<ShellOutput, ShellError> {
    run(cmd, Some(input), MAX_OUTPUT)
}

/// Run `cmd` through `sh -c`, feeding `input` on stdin and CAPTURING its output (`:{range}!{cmd}` filter).
/// As in Vim the returned text REPLACES the piped lines whatever the exit status (#861).
///
/// # Errors
/// Returns [`ShellError`] when the shell cannot be spawned/awaited, is interrupted, writes more than
/// [`MAX_OUTPUT`] and [`FILTER_GROWTH`] times its input ([`ShellError::TooLarge`]: replacing the lines
/// with a cut-off output would lose text), or on a non-unix platform.
pub fn filter(cmd: &str, input: &[u8]) -> Result<ShellOutput, ShellError> {
    let limit = MAX_OUTPUT.max(input.len().saturating_mul(FILTER_GROWTH));
    let out = run(cmd, Some(input), limit)?;
    if out.truncated {
        return Err(ShellError::TooLarge(limit));
    }
    Ok(out)
}

/// The shared unix `sh -c` runner: spawn the shell in its own process group with stdout and stderr on ONE
/// pipe, optionally feed `input` to stdin from a writer thread (dropping it to send EOF), drain the output
/// from a reader thread, and wait, polling the interrupt checkpoint all along: CTRL-C ends the group
/// (`SIGTERM`, then `SIGKILL`) and returns [`ShellError::Interrupted`]. Output past `limit` bytes ends the
/// group too and comes back cut to
/// `limit`, marked truncated. A non-zero exit is still `Ok` — Vim inserts / filters whatever the command
/// produced and only reports `shell returned {n}` — unless the shell could not be spawned. stderr must not
/// be inherited: it would land on the editor's own terminal, under the TUI.
#[cfg(unix)]
fn run(cmd: &str, input: Option<&[u8]>, limit: usize) -> Result<ShellOutput, ShellError> {
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc::{self, RecvTimeoutError};

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
        .stderr(writer_err)
        // Its own group, so an interrupt also kills what the shell started (`a | b`, `x &`).
        .process_group(0);
    let spawned = command.spawn();
    // `command` still owns the parent's copies of the write end; drop them so the reader sees EOF once
    // the shell (and anything it started) has closed its output.
    drop(command);
    let mut child = spawned.map_err(ShellError::Spawn)?;
    // The shell leads its own group, so its pid names the group too. From here the helpers in
    // `crate::reap` wait on and reap it by pid; `child` is never waited on.
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ShellError::Spawn(std::io::Error::other("pid out of range")));
    };
    // Feed stdin from a thread while the output is drained: writing all of it first deadlocks once the
    // input outgrows the pipe buffers, because a command like `cat` blocks writing output nobody reads (#854).
    let feeder = match (input, child.stdin.take()) {
        (Some(text), Some(mut stdin)) => {
            let bytes = text.to_vec();
            Some(std::thread::spawn(move || {
                // A broken pipe (the command exited before reading all input, e.g. `head`) is not an error.
                let _ = stdin.write_all(&bytes);
                // Dropping `stdin` closes it, sending EOF so the command can finish.
            }))
        }
        _ => None,
    };
    // The reader hands chunks over so this thread stays free to poll for CTRL-C. It ends at EOF, on a read
    // error, or once this side stops listening (after an interrupt or at the cap).
    let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let sent = match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => tx.send(Ok(buf[..n].to_vec())),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => tx.send(Err(e)),
            };
            if sent.is_err() {
                return;
            }
        }
    });
    let mut out = Vec::new();
    let mut truncated = false;
    loop {
        match rx.recv_timeout(ruse_core::interrupt::POLL_INTERVAL) {
            Ok(Ok(chunk)) => {
                let room = limit - out.len();
                if chunk.len() > room {
                    out.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                out.extend_from_slice(&chunk);
            }
            Ok(Err(e)) => {
                stop(pid);
                return Err(ShellError::Wait(e));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if ruse_core::interrupt::breakcheck() {
            stop(pid);
            return Err(ShellError::Interrupted);
        }
    }
    // The output is closed, but the shell may still run (`exec >&-; sleep 9`), so the wait polls too.
    let code = if truncated {
        // Past the cap: end the command; its status is whatever the signal left (`None` if killed).
        stop(pid).and_then(|s| s.code())
    } else {
        let waited = crate::reap::wait(pid, None, ruse_core::interrupt::breakcheck)
            .map_err(ShellError::Wait)?;
        let Some(status) = waited else {
            stop(pid);
            return Err(ShellError::Interrupted);
        };
        status.code()
    };
    if let (Some(handle), false) = (feeder, truncated) {
        // The writer returns once its input is written or the pipe is closed; it holds no result.
        let _ = handle.join();
    }
    Ok(ShellOutput {
        text: String::from_utf8_lossy(&out).into_owned(),
        raw: out,
        code,
        truncated,
    })
}

/// End the shell's process group (the shell leads it) and reap the shell, so no zombie stays behind:
/// `SIGTERM` first, so the command can clean up, then `SIGKILL` after the grace, as nvim does (#1748).
/// Returns as soon as the shell exits; whatever is left of its group is SIGKILLed when the grace runs out
/// (#1787). Returns the shell's exit status, `None` when it was given up on.
#[cfg(unix)]
fn stop(pid: libc::pid_t) -> Option<std::process::ExitStatus> {
    crate::reap::end_group(pid, crate::reap::SHELL)
}

#[cfg(not(unix))]
fn run(_cmd: &str, _input: Option<&[u8]>, _limit: usize) -> Result<ShellOutput, ShellError> {
    Err(ShellError::Unsupported)
}

#[cfg(all(test, unix))]
#[allow(clippy::print_stderr)] // test-only skip diagnostics when an expected POSIX tool is absent
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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

    /// #1796: unlike the PTY's `forkpty`, `std::process::Command` puts SIGPIPE (which the Rust runtime
    /// ignores) back to its default and empties the signal mask before exec, so a `:!` pipeline's producer
    /// is killed by SIGPIPE (status 141) once its reader exits.
    #[test]
    fn a_shell_pipeline_producer_dies_of_sigpipe() {
        let out = capture("(yes; echo \"yes=$?\" >&2) | head -n1 >/dev/null; :").expect("sh runs");
        assert_eq!(out.text.trim(), format!("yes={}", 128 + libc::SIGPIPE));
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
        let out = filter("sort", b"gamma\nalpha\nbeta\n").expect("sort runs");
        assert_eq!(out.text, "alpha\nbeta\ngamma\n");
    }

    /// #875: a filter gets and gives back raw bytes, so a buffer that is not UTF-8 survives `:%!cat`.
    #[test]
    fn filter_passes_bytes_that_are_not_utf8() {
        if !has("cat") {
            eprintln!("skipping: `cat` not on PATH");
            return;
        }
        let out = filter("cat", b"a\xff\n").expect("cat runs");
        assert_eq!(out.raw, b"a\xff\n");
    }

    #[test]
    fn filter_transforms_with_tr() {
        if !has("tr") {
            eprintln!("skipping: `tr` not on PATH");
            return;
        }
        let out = filter("tr a-z A-Z", b"one\ntwo\n").expect("tr runs");
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
            let _ = tx.send(filter("cat", sent.as_bytes()).map_err(|e| e.to_string()));
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
        let out = filter("cat; exit 1", b"keep\n").expect("cat still emitted stdout");
        assert_eq!(out.text, "keep\n");
        assert_eq!(out.failure().as_deref(), Some("shell returned 1"));
    }

    /// #861: stderr is captured with stdout, in write order, as Vim's `2>&1` shellredir does. nvim v0.12.6
    /// `:%!echo out; echo err >&2; exit 3` leaves `out\nerr\n` and says `shell returned 3`.
    #[test]
    fn stderr_is_captured_in_order_with_stdout() {
        let out = filter("echo out; echo err >&2; exit 3", b"x\n").expect("sh runs");
        assert_eq!(out.text, "out\nerr\n");
        assert_eq!(out.code, Some(3));
        let out = capture("nonexistent_cmd_zz_861").expect("sh runs");
        assert!(out.text.contains("nonexistent_cmd_zz_861"), "{out:?}");
        assert_eq!(out.failure().as_deref(), Some("shell returned 127"));
    }

    /// #855: output past the limit kills the command and comes back cut to the limit, marked truncated, so
    /// `:r !yes` neither grows without bound nor runs forever.
    #[test]
    fn output_past_the_limit_is_cut_off_and_the_command_killed() {
        if !has("yes") {
            eprintln!("skipping: `yes` not on PATH");
            return;
        }
        let out = run("yes", None, 1000).expect("yes runs");
        assert!(out.truncated);
        assert_eq!(out.raw.len(), 1000);
        assert!(
            out.failure().is_some_and(|n| n.contains("truncated")),
            "{out:?}"
        );
        let out = capture("yes").expect("yes runs");
        assert!(out.truncated);
        assert_eq!(out.raw.len(), MAX_OUTPUT);
        // A filter would replace the lines with the cut-off text, so it refuses instead.
        let err = filter("yes", b"x\n").expect_err("an endless filter is refused");
        assert!(
            matches!(err, ShellError::TooLarge(n) if n == MAX_OUTPUT),
            "{err:?}"
        );
        assert!(err.to_string().contains("buffer unchanged"), "{err}");
        // Output that fits is whole.
        let out = run("printf abc", None, 3).expect("printf runs");
        assert!(!out.truncated);
        assert_eq!(out.raw, b"abc");
    }

    thread_local! {
        static STARTED: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    }

    /// An interrupt hook that sees CTRL-C 200 ms after the test started, once the shell surely runs.
    fn ctrl_c_after_200ms() -> bool {
        STARTED.with(|s| {
            s.get()
                .is_some_and(|t| t.elapsed() >= Duration::from_millis(200))
        })
    }

    /// Run `f` with [`ctrl_c_after_200ms`] as this thread's interrupt hook; returns what `f` returned and how
    /// long it took, and checks the interrupt was left raised for the caller to take.
    fn interrupted<T>(f: impl FnOnce() -> T) -> (T, Duration) {
        STARTED.with(|s| s.set(Some(Instant::now())));
        ruse_core::interrupt::set_hook(Some(ctrl_c_after_200ms));
        let t0 = Instant::now();
        let got = f();
        let took = t0.elapsed();
        ruse_core::interrupt::set_hook(None);
        assert!(
            ruse_core::interrupt::take(),
            "the interrupt stays raised for the caller"
        );
        (got, took)
    }

    /// Whether process `pid` is gone, waiting up to two seconds for it to be killed and reaped.
    fn gone(pid: libc::pid_t) -> bool {
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            // SAFETY: signal 0 checks for existence only; nothing is delivered.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// #855: CTRL-C (the interrupt checkpoint) ends a running command at once: the shell's whole process
    /// group is killed, a background job included, and the caller gets `Interrupted`, not the output.
    #[test]
    fn an_interrupt_kills_the_running_command_and_its_group() {
        let dir = std::env::temp_dir().join(format!("ruse-shell-855-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let _ = std::fs::remove_file(&pidfile);
        let cmd = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let (got, took) = interrupted(|| filter(&cmd, b"keep\n"));
        assert!(matches!(got, Err(ShellError::Interrupted)), "{got:?}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .expect("the shell wrote its job's pid")
            .trim()
            .parse()
            .unwrap();
        assert!(
            gone(pid),
            "the background `sleep` {pid} outlived the interrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1748: CTRL-C sends SIGTERM first, so a command that traps it can clean up (here: remove its temp
    /// file) and exit within the grace, before any SIGKILL; nvim's `proc_stop` does the same.
    #[test]
    fn an_interrupt_lets_the_command_clean_up_on_sigterm() {
        let dir = std::env::temp_dir().join(format!("ruse-shell-1748-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("lock");
        let cmd = format!(
            "touch '{0}'; trap 'rm -f \"{0}\"; exit 0' TERM; while :; do sleep 0.05; done",
            tmp.display()
        );
        let (got, took) = interrupted(|| capture(&cmd));
        assert!(matches!(got, Err(ShellError::Interrupted)), "{got:?}");
        assert!(
            took < Duration::from_secs(2),
            "the trap ran within the grace, so no SIGKILL wait: {took:?}"
        );
        assert!(!tmp.exists(), "the SIGTERM trap never cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1748: a command that ignores SIGTERM is still ended: SIGKILL follows once the grace runs out.
    #[test]
    fn an_interrupt_kills_a_command_that_ignores_sigterm() {
        let (got, took) = interrupted(|| capture("trap '' TERM; sleep 30"));
        assert!(matches!(got, Err(ShellError::Interrupted)), "{got:?}");
        assert!(
            took >= Duration::from_secs(2) && took < Duration::from_secs(6),
            "SIGKILL comes after the 2 s grace: {took:?}"
        );
    }

    /// #1787: a grandchild in the command's group that ignores SIGTERM does not outlive the cancel: the
    /// shell exits on SIGTERM at once, the cancel returns, and the group is SIGKILLed when the grace runs
    /// out. nvim leaves such a process running; ruse does not.
    #[test]
    fn an_interrupt_kills_a_grandchild_that_ignores_sigterm() {
        let dir = std::env::temp_dir().join(format!("ruse-shell-1787-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let _ = std::fs::remove_file(&pidfile);
        // The subshell ignores SIGTERM and execs `sleep`, which keeps the ignored disposition.
        let cmd = format!(
            "(trap '' TERM; exec sleep 30) & echo $! > '{}'; wait",
            pidfile.display()
        );
        let (got, took) = interrupted(|| capture(&cmd));
        assert!(matches!(got, Err(ShellError::Interrupted)), "{got:?}");
        assert!(
            took < Duration::from_secs(2),
            "the shell exited on SIGTERM, so the cancel does not wait out the grace: {took:?}"
        );
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .expect("the shell wrote its job's pid")
            .trim()
            .parse()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 checks for existence only; nothing is delivered.
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: as above.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if alive {
            // SAFETY: `pid` is the test's own grandchild; do not leak it past a failure.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !alive,
            "the grandchild {pid} that ignores SIGTERM survived the cancel"
        );
    }

    /// #855: the wait after the command closed its output is interruptible too.
    #[test]
    fn an_interrupt_ends_a_command_that_closed_its_output() {
        let (got, took) = interrupted(|| capture("exec >&- 2>&-; sleep 30"));
        assert!(matches!(got, Err(ShellError::Interrupted)), "{got:?}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }
}
