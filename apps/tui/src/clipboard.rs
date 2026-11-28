//! The REAL OS-clipboard provider behind `"+`/`"*` (`:help quoteplus`) — the impure counterpart of the pure
//! [`ruse_core::Clipboard`] seam. It shells out to whatever platform clipboard tool is on `PATH`, so ruse
//! adds NO clipboard dependency and DEGRADES GRACEFULLY: if no tool is found `get` returns `None` (`"+p`
//! pastes nothing rather than crashing) and `set` is a silent no-op.
//!
//! Tool selection, resolved once at startup ([`SystemClipboard::detect`]):
//! - macOS: `pbcopy` / `pbpaste`.
//! - Linux/BSD: chosen by the SESSION, not just by what is installed (#874): `wl-copy` / `wl-paste` only
//!   when `WAYLAND_DISPLAY` is set, X11 `xclip` (else `xsel`) only when `DISPLAY` is set, and nothing on a
//!   bare TTY or an SSH login without forwarding — so an installed-but-unusable `wl-copy` never shadows
//!   `xclip`.
//! - Windows: `clip` to set, PowerShell `Get-Clipboard` to read.
//!
//! A read is BOUNDED in time ([`READ_TIMEOUT`]) and size ([`MAX_READ`]): a clipboard tool that hangs (a
//! dead X server, a selection owner that never answers) or floods is ended and the read yields `None`, so
//! `"+p` never freezes the UI. A write is bounded too ([`WRITE_TIMEOUT`]): a tool that stops reading its
//! stdin or never exits is ended and the copy dropped (#1735). Tools run in their own process group and
//! are ended through `crate::reap` (`SIGTERM`, then `SIGKILL`); on unix the pipes are driven with `poll`
//! on the calling thread, so no thread outlives a call. A read whose bytes are not UTF-8 also yields
//! `None` rather than a lossily altered copy.
//!
//! For v0 `*` and `+` are the SAME clipboard (the core routes both to one slot). That is exactly right on
//! macOS/Windows; on X11 `*` is really the PRIMARY selection — an accepted, documented v0 divergence.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ruse_core::Clipboard;

/// How long a clipboard read may take before the tool is killed and the read yields `None`.
pub const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a clipboard write may take (taking the text and exiting) before the tool is ended and the copy
/// dropped (#1735). Longer than a read: the text may be large, and `wl-copy` / `xclip` fork first.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// The most a clipboard read may return: past it the tool is ended and the read yields `None`, so a
/// runaway tool cannot fill memory (#1735, `RA-SEC-019`).
pub const MAX_READ: usize = 64 << 20;

/// How a clipboard tool that ran out of time is ended (#1774): `SIGTERM`, then `SIGKILL` after 100 ms. The
/// grace is short because the user is already waiting on `"+p`.
#[cfg(unix)]
const TOOL_END: &crate::reap::Escalation = &[
    crate::reap::Step {
        signal: libc::SIGTERM,
        grace: Duration::from_millis(100),
    },
    crate::reap::Step {
        signal: libc::SIGKILL,
        grace: crate::reap::KILL_GRACE,
    },
];

/// A `(program, args)` clipboard command.
type Tool = (&'static str, &'static [&'static str]);

/// A pair of shell-out clipboard tools: `set_*` writes text on stdin, `get_*` reads text on stdout. `None`
/// means "no tool available on this platform" → graceful degradation.
#[derive(Clone, Debug, Default)]
pub struct SystemClipboard {
    /// `(program, args)` that reads the clipboard to stdout, or `None` when no reader was found.
    reader: Option<Tool>,
    /// `(program, args)` that writes stdin to the clipboard, or `None` when no writer was found.
    writer: Option<Tool>,
}

impl SystemClipboard {
    /// Probe `PATH` for a usable clipboard tool pair for this platform. Returns a provider that no-ops
    /// gracefully when nothing is found (e.g. a headless CI box or a bare TTY).
    #[must_use]
    pub fn detect() -> SystemClipboard {
        if cfg!(target_os = "macos") {
            SystemClipboard {
                reader: has("pbpaste").then_some(("pbpaste", &[])),
                writer: has("pbcopy").then_some(("pbcopy", &[])),
            }
        } else if cfg!(target_os = "windows") {
            SystemClipboard {
                reader: has("powershell")
                    .then_some(("powershell", &["-NoProfile", "-Command", "Get-Clipboard"])),
                writer: has("clip").then_some(("clip", &[])),
            }
        } else {
            let env_set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
            unix_backend(env_set("WAYLAND_DISPLAY"), env_set("DISPLAY"), has)
        }
    }
}

/// The Linux/BSD tool pair for a session: Wayland tools only under a Wayland session, X11 tools only under
/// an X display (XWayland included), nothing otherwise. Reader and writer come from the SAME tool family
/// so a round-trip is consistent. Pure over its inputs so the choice is unit-testable.
fn unix_backend(wayland: bool, x11: bool, has: impl Fn(&str) -> bool) -> SystemClipboard {
    if wayland && has("wl-copy") && has("wl-paste") {
        SystemClipboard {
            reader: Some(("wl-paste", &["--no-newline"])),
            writer: Some(("wl-copy", &[])),
        }
    } else if x11 && has("xclip") {
        SystemClipboard {
            reader: Some(("xclip", &["-selection", "clipboard", "-o"])),
            writer: Some(("xclip", &["-selection", "clipboard"])),
        }
    } else if x11 && has("xsel") {
        SystemClipboard {
            reader: Some(("xsel", &["--clipboard", "--output"])),
            writer: Some(("xsel", &["--clipboard", "--input"])),
        }
    } else {
        SystemClipboard::default()
    }
}

impl Clipboard for SystemClipboard {
    fn get(&self) -> Option<String> {
        let bytes = read_bounded(self.reader?, READ_TIMEOUT, MAX_READ)?;
        String::from_utf8(bytes).ok()
    }

    fn set(&self, bytes: &[u8]) {
        let Some(tool) = self.writer else {
            return; // no clipboard tool → graceful no-op
        };
        // The register's bytes as they are (#1736). A failed or timed-out copy leaves the clipboard as it
        // was; the register still holds the text.
        let _ = write_bounded(tool, bytes, WRITE_TIMEOUT);
    }
}

/// Spawn `tool` in its own process group (so ending it also ends what it started) with the given stdin
/// and stdout; stderr is discarded.
fn spawn((prog, args): Tool, stdin: Stdio, stdout: Stdio) -> Option<Child> {
    let mut command = Command::new(prog);
    command
        .args(args)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().ok()
}

/// Run `tool` and collect its stdout, giving up after `timeout` or past `cap` bytes: the tool is ended
/// and reaped and the result is `None`. `None` also on a spawn failure or a non-zero exit.
///
/// On unix stdout is read on THIS thread with `poll` against the deadline, so no thread is left behind
/// when a forked grandchild holds the pipe open (#1735); ending the tool's process group ends that
/// grandchild too.
#[cfg(unix)]
fn read_bounded(tool: Tool, timeout: Duration, cap: usize) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut child = spawn(tool, Stdio::null(), Stdio::piped())?;
    let pid = pid_of(&child);
    let bytes = child
        .stdout
        .take()
        .and_then(|mut out| pipe_io::read_until(&mut out, deadline, cap));
    let Some(bytes) = bytes else {
        let _ = crate::reap::end(pid, TOOL_END);
        return None;
    };
    // Stdout hit EOF; the tool normally exits right after. Wait for it within what is left of the
    // deadline, ending it if it does not exit.
    let left = deadline.saturating_duration_since(Instant::now());
    crate::reap::wait_or_end(pid, left, TOOL_END)?
        .success()
        .then_some(bytes)
}

/// Run `tool` with `bytes` on its stdin, giving up after `timeout`: a tool that stops reading its stdin
/// or never exits is ended and reaped (#1735). `true` when it took the bytes and exited 0 in time.
#[cfg(unix)]
fn write_bounded(tool: Tool, bytes: &[u8], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Some(mut child) = spawn(tool, Stdio::piped(), Stdio::null()) else {
        return false;
    };
    let pid = pid_of(&child);
    // Dropping stdin after the write sends EOF, so the tool can finish.
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| pipe_io::write_until(&mut stdin, bytes, deadline));
    if !written {
        let _ = crate::reap::end(pid, TOOL_END);
        return false;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    crate::reap::wait_or_end(pid, left, TOOL_END).is_some_and(|s| s.success())
}

/// The pid of a child spawned by [`spawn`], which leads its own process group.
#[cfg(unix)]
fn pid_of(child: &Child) -> libc::pid_t {
    // A pid is a positive `pid_t`; `Child::id` only widened it to `u32`.
    libc::pid_t::try_from(child.id()).unwrap_or(libc::pid_t::MAX)
}

/// Non-blocking pipe I/O against a deadline, on the calling thread (unix).
#[cfg(unix)]
mod pipe_io {
    use std::io::{self, Read, Write};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::time::Instant;

    /// Put `fd` in non-blocking mode.
    fn set_nonblocking(fd: RawFd) -> bool {
        // SAFETY: `fd` is a live pipe end owned by the caller; F_GETFL / F_SETFL only touch its flags.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
        }
    }

    /// Wait until `fd` is ready for `events` or `deadline` passes. `false` on timeout or a poll error.
    fn ready(fd: RawFd, events: libc::c_short, deadline: Instant) -> bool {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
            let mut pfd = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            // SAFETY: `pfd` is one valid pollfd; poll writes only its `revents`.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r > 0 {
                return true; // ready, or POLLHUP / POLLERR: the next read or write says which
            }
            if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return false;
            }
        }
    }

    /// Read `src` to EOF by `deadline`. `None` on timeout, on an error, or past `cap` bytes.
    pub(super) fn read_until<R: Read + AsRawFd>(
        src: &mut R,
        deadline: Instant,
        cap: usize,
    ) -> Option<Vec<u8>> {
        let fd = src.as_raw_fd();
        if !set_nonblocking(fd) {
            return None;
        }
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match src.read(&mut chunk) {
                Ok(0) => return Some(out),
                Ok(n) if out.len() + n > cap => return None,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !ready(fd, libc::POLLIN, deadline) {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Write all of `bytes` to `dst` by `deadline`. `false` on timeout or an error; a broken pipe (the
    /// tool exited before taking everything) is an error too.
    pub(super) fn write_until<W: Write + AsRawFd>(
        dst: &mut W,
        mut bytes: &[u8],
        deadline: Instant,
    ) -> bool {
        let fd = dst.as_raw_fd();
        if !set_nonblocking(fd) {
            return false;
        }
        while !bytes.is_empty() {
            match dst.write(bytes) {
                Ok(0) => return false,
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !ready(fd, libc::POLLOUT, deadline) {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
        true
    }
}

/// Non-unix [`read_bounded`]: stdout is drained on a helper thread (there is no `poll` on a Windows pipe),
/// capped at `cap` bytes, and the caller waits on a channel with a deadline. A thread left reading by a
/// tool's grandchild ends when that grandchild closes the pipe.
#[cfg(not(unix))]
fn read_bounded(tool: Tool, timeout: Duration, cap: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let deadline = Instant::now() + timeout;
    let mut child = spawn(tool, Stdio::null(), Stdio::piped())?;
    let Some(stdout) = child.stdout.take() else {
        kill(&mut child);
        return None;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
        let read = stdout.take(limit).read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(read);
    });
    let bytes = match rx.recv_timeout(timeout) {
        Ok(Ok(bytes)) if bytes.len() <= cap => bytes,
        _ => {
            kill(&mut child);
            return None;
        }
    };
    wait_until(&mut child, deadline)?.success().then_some(bytes)
}

/// Non-unix [`write_bounded`]: the write itself blocks (no `poll` on a Windows pipe); the wait is bounded.
#[cfg(not(unix))]
fn write_bounded(tool: Tool, bytes: &[u8], timeout: Duration) -> bool {
    use std::io::Write;
    let deadline = Instant::now() + timeout;
    let Some(mut child) = spawn(tool, Stdio::piped(), Stdio::null()) else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(bytes);
    }
    wait_until(&mut child, deadline).is_some_and(|s| s.success())
}

/// Wait for `child` until `deadline`; past it, kill and reap it and return `None`.
#[cfg(not(unix))]
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                kill(child);
                return None;
            }
        }
    }
}

/// Kill and reap `child`, ignoring errors (it may already have exited).
#[cfg(not(unix))]
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether `program` resolves on `PATH` — a cheap `command -v` (POSIX) / `where` (Windows) probe, so we
/// never spawn a clipboard tool that does not exist.
fn has(program: &str) -> bool {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("where");
        c.arg(program);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("command -v {program}"));
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(c: &SystemClipboard) -> Option<&'static str> {
        c.reader.map(|(p, _)| p)
    }

    fn writer(c: &SystemClipboard) -> Option<&'static str> {
        c.writer.map(|(p, _)| p)
    }

    #[test]
    fn installed_wl_copy_does_not_shadow_xclip_without_a_wayland_session() {
        // #874: X11 (or forwarded-X SSH) session with wl-clipboard installed too.
        let c = unix_backend(false, true, |_| true);
        assert_eq!(reader(&c), Some("xclip"));
        assert_eq!(writer(&c), Some("xclip"));
    }

    #[test]
    fn wayland_session_prefers_wl_tools() {
        let c = unix_backend(true, true, |_| true);
        assert_eq!(reader(&c), Some("wl-paste"));
        assert_eq!(writer(&c), Some("wl-copy"));
    }

    #[test]
    fn wayland_session_without_wl_tools_falls_back_to_x11() {
        let c = unix_backend(true, true, |p| p == "xsel");
        assert_eq!(reader(&c), Some("xsel"));
        assert_eq!(writer(&c), Some("xsel"));
    }

    #[test]
    fn no_display_session_yields_no_clipboard() {
        // Bare TTY / SSH without forwarding: installed tools are unusable, so degrade to a no-op.
        let c = unix_backend(false, false, |_| true);
        assert_eq!(reader(&c), None);
        assert_eq!(writer(&c), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_reader_is_killed_at_the_deadline() {
        let start = Instant::now();
        let got = read_bounded(("sleep", &["5"]), Duration::from_millis(100), MAX_READ);
        assert_eq!(got, None);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "read must not block past the timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_reader_holding_stdout_open_after_output_is_bounded() {
        // Writes, then keeps the pipe open: the drain never reaches EOF, so only the deadline ends the read.
        let start = Instant::now();
        let got = read_bounded(
            ("sh", &["-c", "printf x; sleep 5"]),
            Duration::from_millis(100),
            MAX_READ,
        );
        assert_eq!(got, None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn a_prompt_reader_returns_its_bytes() {
        let got = read_bounded(("printf", &["hello"]), READ_TIMEOUT, MAX_READ);
        assert_eq!(got.as_deref(), Some(&b"hello"[..]));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_reader_yields_none() {
        assert_eq!(read_bounded(("false", &[]), READ_TIMEOUT, MAX_READ), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_clipboard_bytes_are_not_lossily_rewritten() {
        let c = SystemClipboard {
            reader: Some(("printf", &["\\377ok"])),
            writer: None,
        };
        assert_eq!(c.get(), None);
    }

    /// #1735: a writer that never reads its stdin cannot block `set`: with more text than the pipe holds,
    /// the write times out and the tool is ended.
    #[cfg(unix)]
    #[test]
    fn a_writer_that_never_reads_stdin_is_bounded() {
        let text = vec![b'x'; 4 << 20];
        let start = Instant::now();
        let ok = write_bounded(("sleep", &["5"]), &text, Duration::from_millis(200));
        assert!(!ok);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }

    /// #1735: a writer that takes the text but never exits is ended at the timeout.
    #[cfg(unix)]
    #[test]
    fn a_writer_that_never_exits_is_bounded() {
        let start = Instant::now();
        let ok = write_bounded(
            ("sh", &["-c", "cat >/dev/null; sleep 5"]),
            b"hello",
            Duration::from_millis(200),
        );
        assert!(!ok);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
    }

    /// A writer that takes the text and exits 0 succeeds, also with more text than the pipe holds.
    #[cfg(unix)]
    #[test]
    fn a_prompt_writer_takes_all_the_text() {
        let text = vec![b'x'; 1 << 20];
        assert!(write_bounded(
            ("sh", &["-c", "cat >/dev/null"]),
            &text,
            WRITE_TIMEOUT
        ));
        assert!(!write_bounded(("false", &[]), b"x", WRITE_TIMEOUT));
    }

    /// #1735 (`RA-SEC-019`): a read past the cap is refused and the tool ended, not buffered without end.
    #[cfg(unix)]
    #[test]
    fn a_read_past_the_cap_yields_none() {
        let start = Instant::now();
        assert_eq!(read_bounded(("yes", &[]), READ_TIMEOUT, 1000), None);
        assert!(
            start.elapsed() < READ_TIMEOUT,
            "the cap, not the clock, ended it"
        );
        let got = read_bounded(("printf", &["abc"]), READ_TIMEOUT, 3);
        assert_eq!(got.as_deref(), Some(&b"abc"[..]));
    }

    /// #1735: a grandchild that holds stdout open past the timeout is ended with the tool's group, and the
    /// read runs on the calling thread, so nothing is left behind.
    #[cfg(unix)]
    #[test]
    fn a_grandchild_holding_stdout_is_ended_with_the_group() {
        let dir = std::env::temp_dir().join(format!("ruse-clip-1735-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; printf x", pidfile.display());
        // A `Tool` holds `'static` args; leaking one test string is fine.
        let script: &'static str = Box::leak(script.into_boxed_str());
        let args: &'static [&'static str] = Box::leak(vec!["-c", script].into_boxed_slice());
        let start = Instant::now();
        let got = read_bounded(("sh", args), Duration::from_millis(200), MAX_READ);
        assert_eq!(got, None);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .expect("the shell wrote its job's pid")
            .trim()
            .parse()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        // SAFETY: signal 0 only probes whether `pid` exists.
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: as above.
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "the grandchild outlived the read"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
