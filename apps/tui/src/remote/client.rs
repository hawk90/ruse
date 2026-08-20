//! The client side of the client↔agent split (F-017 slice 1): spawn the agent as a subprocess, handshake
//! (exchange version + negotiate capabilities, degrading), and issue blocking request→response calls. The
//! launch [`Command`] is the transport seam — `ruse agent` over a local pipe today, `ssh host ruse agent`
//! later — so the client is transport-agnostic.

use std::io::{self, BufReader};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

use super::protocol::{negotiate, read_message, request, write_message, PROTOCOL_VERSION};

/// A connected Workspace Agent: the child process + framed stdio + the negotiated handshake result.
pub struct AgentClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    protocol_version: u32,
    capabilities: Vec<String>,
}

impl AgentClient {
    /// Spawn `command` (e.g. `ruse agent`, later `ssh host ruse agent`) and handshake. `want` is the set of
    /// capabilities the client needs; the negotiated set (`capabilities()`) is their intersection with the
    /// agent's — a wanted-but-missing capability DEGRADES (dropped), it does not fail the connection.
    pub fn spawn(mut command: Command, want: &[&str]) -> io::Result<AgentClient> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("agent stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("agent stdout unavailable"))?;
        let mut c = AgentClient {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            protocol_version: 0,
            capabilities: Vec::new(),
        };
        let result = c.call("initialize", json!({ "protocolVersion": PROTOCOL_VERSION }))?;
        c.protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let offered: Vec<String> = result
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        c.capabilities = negotiate(want, &offered);
        Ok(c)
    }

    /// The agent's protocol version (from the handshake).
    pub fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    /// The negotiated capability set (what BOTH sides support).
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// Whether a capability survived negotiation.
    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }

    /// A blocking request→response round-trip: returns the `result`, or an `Err` for an error reply / EOF.
    pub fn call(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.stdin, &request(id, method, params))?;
        loop {
            match read_message(&mut self.stdout)? {
                Some(Value::Null) => continue, // skipped malformed frame
                Some(msg) => {
                    if msg.get("id").and_then(Value::as_i64) != Some(id) {
                        continue; // not our reply (slice 1 is strictly request/response, so this is rare)
                    }
                    if let Some(err) = msg.get("error") {
                        let m = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("agent error");
                        return Err(io::Error::other(m.to_string()));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
                }
                None => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "agent closed"));
                }
            }
        }
    }

    /// `fs.readFile` — read a file on the agent (returns its text). Requires the `fs.readFile` capability.
    pub fn read_file(&mut self, path: &str) -> io::Result<String> {
        let r = self.call("fs/readFile", json!({ "path": path }))?;
        Ok(r.get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// `fs.writeFile` — write text to a file on the agent (create/truncate); returns the byte count.
    pub fn write_file(&mut self, path: &str, content: &str) -> io::Result<u64> {
        let r = self.call("fs/writeFile", json!({ "path": path, "content": content }))?;
        Ok(r.get("bytesWritten").and_then(Value::as_u64).unwrap_or(0))
    }

    /// `fs.stat` — metadata for a path on the agent. A missing path is `FileStat { exists: false, .. }`, not
    /// an error (probe before read/write without catching).
    pub fn stat(&mut self, path: &str) -> io::Result<FileStat> {
        let r = self.call("fs/stat", json!({ "path": path }))?;
        Ok(FileStat {
            exists: r.get("exists").and_then(Value::as_bool).unwrap_or(false),
            is_dir: r.get("isDir").and_then(Value::as_bool).unwrap_or(false),
            is_file: r.get("isFile").and_then(Value::as_bool).unwrap_or(false),
            len: r.get("len").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    /// `fs.list` — the immediate entries of a directory on the agent (non-recursive).
    pub fn list(&mut self, path: &str) -> io::Result<Vec<DirEntry>> {
        let r = self.call("fs/list", json!({ "path": path }))?;
        let entries = r
            .get("entries")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|e| DirEntry {
                        name: e
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        is_dir: e.get("isDir").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(entries)
    }
}

/// Metadata for a path on the agent (`fs.stat`). `exists: false` means the path was absent, not an IO error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileStat {
    pub exists: bool,
    pub is_dir: bool,
    pub is_file: bool,
    pub len: u64,
}

/// One immediate entry of a directory on the agent (`fs.list`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

impl Drop for AgentClient {
    fn drop(&mut self) {
        // Best-effort graceful shutdown, then ensure the child is gone.
        let _ = write_message(&mut self.stdin, &request(0, "shutdown", Value::Null));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
