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
//! A read is BOUNDED ([`READ_TIMEOUT`]): a clipboard tool that hangs (a dead X server, a selection owner
//! that never answers) is killed and the read yields `None`, so `"+p` never freezes the UI. A read whose
//! bytes are not UTF-8 also yields `None` rather than a lossily altered copy.
//!
//! For v0 `*` and `+` are the SAME clipboard (the core routes both to one slot). That is exactly right on
//! macOS/Windows; on X11 `*` is really the PRIMARY selection — an accepted, documented v0 divergence.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ruse_core::Clipboard;

/// How long a clipboard read may take before the tool is killed and the read yields `None`.
pub const READ_TIMEOUT: Duration = Duration::from_secs(1);

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
        let bytes = read_bounded(self.reader?, READ_TIMEOUT)?;
        String::from_utf8(bytes).ok()
    }

    fn set(&self, text: &str) {
        let Some((prog, args)) = self.writer else {
            return; // no clipboard tool → graceful no-op
        };
        let Ok(mut child) = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
            // Drop stdin to send EOF before waiting, so the tool can finish.
        }
        let _ = child.wait();
    }
}

/// Run `tool` and collect its stdout, giving up after `timeout`: the child is killed (and reaped) on
/// expiry and the result is `None`. Stdout is drained on a helper thread so a full pipe cannot deadlock
/// the child, and the caller waits on a channel with a deadline, so even a pipe held open by a forked
/// grandchild cannot block the read past `timeout`. `None` also on a spawn failure or a non-zero exit.
fn read_bounded((prog, args): Tool, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut command = Command::new(prog);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    own_group(&mut command);
    let mut child = command.spawn().ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        kill(&mut child);
        return None;
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let read = stdout.read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(read);
    });
    let Ok(Ok(bytes)) = rx.recv_timeout(timeout) else {
        kill(&mut child);
        return None;
    };
    // Stdout hit EOF; the tool normally exits right after. Wait for its status within what is left of the
    // deadline rather than a blocking `wait`.
    let left = deadline.saturating_duration_since(Instant::now());
    wait_or_end(&mut child, left)?.success().then_some(bytes)
}

/// Start the tool in its own process group, so ending it also ends what it started.
#[cfg(unix)]
fn own_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_command: &mut Command) {}

/// Wait up to `timeout` for `child` to exit; past it, end it ([`TOOL_END`]) and reap it. Its status when
/// it exited in time, `None` otherwise.
#[cfg(unix)]
fn wait_or_end(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        kill(child);
        return None;
    };
    crate::reap::wait_or_end(pid, timeout, TOOL_END)
}

#[cfg(not(unix))]
fn wait_or_end(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
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

/// End and reap `child` now, ignoring errors (it may already have exited).
#[cfg(unix)]
fn kill(child: &mut Child) {
    match libc::pid_t::try_from(child.id()) {
        Ok(pid) => {
            let _ = crate::reap::end(pid, TOOL_END);
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// End and reap `child` now, ignoring errors (it may already have exited).
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
        let got = read_bounded(("sleep", &["5"]), Duration::from_millis(100));
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
        );
        assert_eq!(got, None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn a_prompt_reader_returns_its_bytes() {
        let got = read_bounded(("printf", &["hello"]), READ_TIMEOUT);
        assert_eq!(got.as_deref(), Some(&b"hello"[..]));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_reader_yields_none() {
        assert_eq!(read_bounded(("false", &[]), READ_TIMEOUT), None);
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
}
