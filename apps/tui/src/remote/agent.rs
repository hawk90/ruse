//! The headless Workspace Agent serve loop (F-017 slice 1). Reads framed requests, dispatches to a service,
//! writes a framed response — over any `BufRead`/`Write` (a local pipe today; an SSH channel later). Slice 1
//! offers ONE service, `fs.readFile`, to prove the "execution is remote, UI is local" split end-to-end.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use super::error::AgentError;
use super::protocol::{read_message, response, write_message, PROTOCOL_VERSION};

/// The services this agent offers. The handshake announces these; the client negotiates them down to what it
/// needs (missing ones degrade, never fail). Grows as fs/watch/search/git/lsp/debug/pty land in later slices.
pub const CAPABILITIES: &[&str] = &["fs.readFile"];

/// Serve the client↔agent protocol until EOF (or a `shutdown`). Each request gets exactly one response.
pub fn serve<R: BufRead, W: Write>(mut r: R, mut w: W) -> io::Result<()> {
    while let Some(msg) = read_message(&mut r)? {
        if msg.is_null() {
            continue; // a malformed frame — skip rather than desync
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if method == "shutdown" {
            write_message(&mut w, &response(id, Ok(Value::Null)))?;
            break;
        }
        let reply = dispatch(method, &params);
        write_message(&mut w, &response(id, reply))?;
    }
    Ok(())
}

/// Route one request to its service. `initialize` is the handshake (version + capabilities); other methods
/// are services. An unknown method is an error reply, not a crash.
fn dispatch(method: &str, params: &Value) -> Result<Value, AgentError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": CAPABILITIES,
        })),
        "fs/readFile" => read_file(params),
        other => Err(AgentError::UnknownMethod(other.to_string())),
    }
}

/// `fs.readFile` — read a file ON THE AGENT and return its text content. Slice 1 returns lossy UTF-8 (binary
/// transfer / encoding fidelity is a later concern). Proves the client reads a file it never touches directly.
fn read_file(params: &Value) -> Result<Value, AgentError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or(AgentError::MissingParam {
            method: "fs/readFile",
            field: "path",
        })?;
    match std::fs::read(path) {
        Ok(bytes) => Ok(json!({ "content": String::from_utf8_lossy(&bytes) })),
        Err(e) => Err(AgentError::Service {
            method: "fs/readFile",
            detail: format!("{path}: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{request, write_message};
    use super::*;
    use std::io::Cursor;

    /// Drive `serve` over in-memory buffers (no subprocess): an initialize handshake + an `fs/readFile` of a
    /// temp file both round-trip, and an unknown method is an error reply — the protocol contract, mock-tested.
    #[test]
    fn serve_handshakes_reads_a_file_and_errors_unknown() {
        let dir = std::env::temp_dir();
        let path = dir.join("ruse_agent_serve_test.txt");
        std::fs::write(&path, b"hello agent").unwrap();

        let mut input = Vec::new();
        write_message(&mut input, &request(1, "initialize", json!({}))).unwrap();
        write_message(
            &mut input,
            &request(2, "fs/readFile", json!({ "path": path.to_str().unwrap() })),
        )
        .unwrap();
        write_message(&mut input, &request(3, "nope", json!({}))).unwrap();

        let mut out = Vec::new();
        serve(Cursor::new(input), &mut out).unwrap();
        let _ = std::fs::remove_file(&path);

        let mut r = Cursor::new(out);
        let init = read_message(&mut r).unwrap().unwrap();
        assert_eq!(init["result"]["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(init["result"]["capabilities"], json!(["fs.readFile"]));
        let file = read_message(&mut r).unwrap().unwrap();
        assert_eq!(file["result"]["content"], json!("hello agent"));
        let err = read_message(&mut r).unwrap().unwrap();
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown method"));
    }
}
