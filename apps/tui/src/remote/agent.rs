//! The headless Workspace Agent serve loop (F-017 slice 1). Reads framed requests, dispatches to a service,
//! writes a framed response — over any `BufRead`/`Write` (a local pipe today; an SSH channel later). It offers
//! the filesystem service set (`fs.readFile`/`writeFile`/`stat`/`list`) — enough to prove "execution is remote,
//! UI is local" and to back remote-fs editing in a later slice. watch/search/git/pty/lsp/debug come later.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use super::error::AgentError;
use super::protocol::{read_message, response, write_message, PROTOCOL_VERSION};

/// The services this agent offers. The handshake announces these; the client negotiates them down to what it
/// needs (missing ones degrade, never fail). Grows as watch/search/git/lsp/debug/pty land in later slices.
pub const CAPABILITIES: &[&str] = &["fs.readFile", "fs.writeFile", "fs.stat", "fs.list"];

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
        "fs/writeFile" => write_file(params),
        "fs/stat" => stat(params),
        "fs/list" => list(params),
        other => Err(AgentError::UnknownMethod(other.to_string())),
    }
}

/// The required `path` string param, or a typed `MissingParam` naming the method (shared by the fs services).
fn path_param<'a>(method: &'static str, params: &'a Value) -> Result<&'a str, AgentError> {
    params
        .get("path")
        .and_then(Value::as_str)
        .ok_or(AgentError::MissingParam {
            method,
            field: "path",
        })
}

/// `fs.readFile` — read a file ON THE AGENT and return its text content. Slice 1 returns lossy UTF-8 (binary
/// transfer / encoding fidelity is a later concern). Proves the client reads a file it never touches directly.
fn read_file(params: &Value) -> Result<Value, AgentError> {
    let path = path_param("fs/readFile", params)?;
    match std::fs::read(path) {
        Ok(bytes) => Ok(json!({ "content": String::from_utf8_lossy(&bytes) })),
        Err(e) => Err(AgentError::Service {
            method: "fs/readFile",
            detail: format!("{path}: {e}"),
        }),
    }
}

/// `fs.writeFile` — write text content to a file ON THE AGENT (create/truncate), returning the byte count.
/// The `content` string is written as UTF-8 (matching `fs.readFile`'s lossy read; binary is a later concern).
fn write_file(params: &Value) -> Result<Value, AgentError> {
    let path = path_param("fs/writeFile", params)?;
    let content =
        params
            .get("content")
            .and_then(Value::as_str)
            .ok_or(AgentError::MissingParam {
                method: "fs/writeFile",
                field: "content",
            })?;
    match std::fs::write(path, content.as_bytes()) {
        Ok(()) => Ok(json!({ "bytesWritten": content.len() })),
        Err(e) => Err(AgentError::Service {
            method: "fs/writeFile",
            detail: format!("{path}: {e}"),
        }),
    }
}

/// `fs.stat` — metadata for a path ON THE AGENT. A missing path is NOT an error (`{ exists: false }`); other
/// IO failures (e.g. permission) are. Lets the client probe before read/write without catching an error.
fn stat(params: &Value) -> Result<Value, AgentError> {
    let path = path_param("fs/stat", params)?;
    match std::fs::metadata(path) {
        Ok(m) => Ok(json!({
            "exists": true,
            "isDir": m.is_dir(),
            "isFile": m.is_file(),
            "len": m.len(),
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({ "exists": false })),
        Err(e) => Err(AgentError::Service {
            method: "fs/stat",
            detail: format!("{path}: {e}"),
        }),
    }
}

/// `fs.list` — the immediate entries of a directory ON THE AGENT (non-recursive), each `{ name, isDir }`.
/// Entries whose type can't be read are skipped rather than failing the whole listing.
fn list(params: &Value) -> Result<Value, AgentError> {
    let path = path_param("fs/list", params)?;
    let read_dir = std::fs::read_dir(path).map_err(|e| AgentError::Service {
        method: "fs/list",
        detail: format!("{path}: {e}"),
    })?;
    let mut entries: Vec<Value> = Vec::new();
    for entry in read_dir.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "isDir": is_dir,
        }));
    }
    Ok(json!({ "entries": entries }))
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
        assert_eq!(init["result"]["capabilities"], json!(CAPABILITIES));
        let file = read_message(&mut r).unwrap().unwrap();
        assert_eq!(file["result"]["content"], json!("hello agent"));
        let err = read_message(&mut r).unwrap().unwrap();
        assert!(err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown method"));
    }

    /// The fs write/stat/list services round-trip over `serve`: write a file, stat it (exists + len), stat a
    /// missing sibling (`exists: false`, NOT an error), and list the dir (the written file appears).
    #[test]
    fn serve_writes_stats_and_lists() {
        let dir = std::env::temp_dir().join(format!("ruse_agent_fs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("w.txt");
        let fp = file.to_str().unwrap();

        let mut input = Vec::new();
        write_message(
            &mut input,
            &request(1, "fs/writeFile", json!({ "path": fp, "content": "abcde" })),
        )
        .unwrap();
        write_message(&mut input, &request(2, "fs/stat", json!({ "path": fp }))).unwrap();
        write_message(
            &mut input,
            &request(
                3,
                "fs/stat",
                json!({ "path": dir.join("nope").to_str().unwrap() }),
            ),
        )
        .unwrap();
        write_message(
            &mut input,
            &request(4, "fs/list", json!({ "path": dir.to_str().unwrap() })),
        )
        .unwrap();

        let mut out = Vec::new();
        serve(Cursor::new(input), &mut out).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let mut r = Cursor::new(out);
        assert_eq!(
            read_message(&mut r).unwrap().unwrap()["result"]["bytesWritten"],
            json!(5)
        );
        let st = read_message(&mut r).unwrap().unwrap();
        assert_eq!(st["result"]["exists"], json!(true));
        assert_eq!(st["result"]["isFile"], json!(true));
        assert_eq!(st["result"]["len"], json!(5));
        let missing = read_message(&mut r).unwrap().unwrap();
        assert_eq!(
            missing["result"]["exists"],
            json!(false),
            "a missing path is not an error"
        );
        let listed = read_message(&mut r).unwrap().unwrap();
        let names: Vec<&str> = listed["result"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(
            names.contains(&"w.txt"),
            "the written file appears in the listing"
        );
    }
}
