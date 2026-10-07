//! How the client LAUNCHES the agent — the transport seam (F-017 slice 2a). Slice 1 ran the agent as a local
//! subprocess (`ruse agent`) over a pipe; this adds the RFC-0006 "SSH stdio first" transport
//! (`ssh <host> ruse agent`). Every transport reduces to a [`Command`] whose child stdio IS the wire, so
//! [`super::client::AgentClient`] stays transport-agnostic — only the launch command differs.
//!
//! **stdout is protocol-only** (RFC-0006): the agent writes nothing but frames to stdout, and SSH banners /
//! motd / the agent's own logs travel on stderr, never stdout — so the frame reader never desyncs.

use std::ffi::OsStr;
use std::io;
use std::process::Command;

/// The remote command that starts the agent's stdio serve loop. Until agent bootstrap (a later slice) installs
/// a version-matched binary under `$HOME` (RFC-0006 / D-030), the agent is invoked as `ruse agent` on the
/// remote's `PATH` — so slice 2a requires `ruse` to already be installed remotely.
pub const DEFAULT_REMOTE_AGENT_CMD: &[&str] = &["ruse", "agent"];

/// A LOCAL subprocess transport: run this binary's own `agent` subcommand over a pipe (the slice-1 proof
/// path, and the fallback when no host is given).
pub fn local_command(exe: impl AsRef<OsStr>) -> Command {
    let mut c = Command::new(exe);
    c.arg("agent");
    c
}

/// An SSH stdio transport: `ssh -o BatchMode=yes -- <host> <remote_cmd…>`. `BatchMode=yes` fails fast on a
/// missing key rather than blocking on an interactive password prompt (a headless client must never hang).
/// Everything after `host` is the remote command; the agent's stdio becomes the wire.
///
/// **Option injection guard:** `host` is user input, so it is placed after a `--` end-of-options marker, and
/// a host that is empty or starts with `-` is rejected outright (`InvalidInput`) — `-oProxyCommand=…` must
/// never reach ssh(1) as an option, whatever its argument parser does with `--`.
pub fn ssh_command(host: &str, remote_cmd: &[&str]) -> io::Result<Command> {
    if host.is_empty() || host.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid ssh host {host:?}: must be non-empty and not start with '-'"),
        ));
    }
    let mut c = Command::new("ssh");
    // Non-interactive: fail fast instead of prompting. (Key/agent auth is delegated to SSH — RFC-0006.)
    c.arg("-o").arg("BatchMode=yes");
    c.arg("--");
    c.arg(host);
    c.args(remote_cmd);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(c: &Command) -> (String, Vec<String>) {
        let prog = c.get_program().to_string_lossy().into_owned();
        let args = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        (prog, args)
    }

    #[test]
    fn local_runs_this_binary_agent_subcommand() {
        let (prog, args) = parts(&local_command("/usr/bin/ruse"));
        assert_eq!(prog, "/usr/bin/ruse");
        assert_eq!(args, ["agent"]);
    }

    #[test]
    fn ssh_builds_batchmode_host_then_remote_command() {
        let (prog, args) = parts(&ssh_command("build-box", DEFAULT_REMOTE_AGENT_CMD).unwrap());
        assert_eq!(prog, "ssh");
        // options BEFORE `--`, then the host, remote command AFTER it — the ssh(1) argument order.
        assert_eq!(
            args,
            ["-o", "BatchMode=yes", "--", "build-box", "ruse", "agent"]
        );
    }

    /// Regression: a host beginning with `-` would be parsed by ssh(1) as an option (e.g.
    /// `-oProxyCommand=sh -c …` → local command execution). It is rejected before any `Command` exists.
    #[test]
    fn ssh_rejects_option_like_or_empty_host() {
        for bad in ["-oProxyCommand=touch /tmp/pwned", "-v", "--", ""] {
            let err = ssh_command(bad, DEFAULT_REMOTE_AGENT_CMD).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "host {bad:?}");
        }
        // A user@host form (and a host merely CONTAINING '-') is still fine.
        assert!(ssh_command("me@build-box", DEFAULT_REMOTE_AGENT_CMD).is_ok());
    }
}
