//! The REAL OS-clipboard provider behind `"+`/`"*` (`:help quoteplus`) — the impure counterpart of the pure
//! [`ruse_core::Clipboard`] seam. It shells out to whatever platform clipboard tool is on `PATH`, so ruse
//! adds NO clipboard dependency and DEGRADES GRACEFULLY: if no tool is found `get` returns `None` (`"+p`
//! pastes nothing rather than crashing) and `set` is a silent no-op.
//!
//! Tool selection, resolved once at startup ([`SystemClipboard::detect`]):
//! - macOS: `pbcopy` / `pbpaste`.
//! - Linux: Wayland `wl-copy` / `wl-paste` first, else X11 `xclip`, else `xsel`.
//! - Windows: `clip` to set, PowerShell `Get-Clipboard` to read.
//!
//! For v0 `*` and `+` are the SAME clipboard (the core routes both to one slot). That is exactly right on
//! macOS/Windows; on X11 `*` is really the PRIMARY selection — an accepted, documented v0 divergence.

use std::io::Write;
use std::process::{Command, Stdio};

use ruse_core::Clipboard;

/// A pair of shell-out clipboard tools: `set_*` writes text on stdin, `get_*` reads text on stdout. `None`
/// means "no tool available on this platform" → graceful degradation.
#[derive(Clone, Debug, Default)]
pub struct SystemClipboard {
    /// `(program, args)` that reads the clipboard to stdout, or `None` when no reader was found.
    reader: Option<(&'static str, &'static [&'static str])>,
    /// `(program, args)` that writes stdin to the clipboard, or `None` when no writer was found.
    writer: Option<(&'static str, &'static [&'static str])>,
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
            // Linux/BSD: prefer Wayland, then X11 (xclip, then xsel). Reader and writer are chosen from the
            // SAME tool family so a round-trip is consistent.
            if has("wl-copy") && has("wl-paste") {
                SystemClipboard {
                    reader: Some(("wl-paste", &["--no-newline"])),
                    writer: Some(("wl-copy", &[])),
                }
            } else if has("xclip") {
                SystemClipboard {
                    reader: Some(("xclip", &["-selection", "clipboard", "-o"])),
                    writer: Some(("xclip", &["-selection", "clipboard"])),
                }
            } else if has("xsel") {
                SystemClipboard {
                    reader: Some(("xsel", &["--clipboard", "--output"])),
                    writer: Some(("xsel", &["--clipboard", "--input"])),
                }
            } else {
                SystemClipboard::default()
            }
        }
    }
}

impl Clipboard for SystemClipboard {
    fn get(&self) -> Option<String> {
        let (prog, args) = self.reader?;
        let out = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
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
