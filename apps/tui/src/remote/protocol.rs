//! The client↔agent wire protocol (F-017 / RFC-0006, slice 1): the same proven `Content-Length: N\r\n\r\n` +
//! JSON-body framing as the LSP codec, plus the version/capability negotiation contract. A message is a
//! request `{ id, method, params }` or a response `{ id, result }` / `{ id, error: { message } }`.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use super::error::AgentError;

/// The wire protocol version. A handshake records the peer's version; incompatible versions DEGRADE (a
/// smaller negotiated capability set), they do not fail the connection (F-017 acceptance #3).
pub const PROTOCOL_VERSION: u32 = 1;

/// Frame and write one JSON message (`Content-Length` header + body). Same framing as `lsp/codec.rs`.
pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one framed message. `Ok(None)` at EOF; a header block with no `Content-Length` yields `Value::Null`
/// so the reader can skip a malformed frame rather than desync.
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of the header block
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let Some(len) = len else {
        return Ok(Some(Value::Null)); // malformed header — skip
    };
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

/// A request envelope `{ id, method, params }`.
pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "id": id, "method": method, "params": params })
}

/// A response envelope from a service result: `{ id, result }` on `Ok`, `{ id, error: { message } }` on `Err`.
/// The typed [`AgentError`] collapses to its `Display` string on the wire (the peer only sees the message).
pub fn response(id: Value, reply: Result<Value, AgentError>) -> Value {
    match reply {
        Ok(result) => json!({ "id": id, "result": result }),
        Err(e) => json!({ "id": id, "error": { "message": e.to_string() } }),
    }
}

/// Negotiate the effective capability set: the intersection of what the client WANTS and what the agent
/// OFFERS. A capability the client wants but the agent lacks is silently dropped (DEGRADE — a partial set,
/// never a failed connection). Order follows `wanted`.
pub fn negotiate(wanted: &[&str], offered: &[String]) -> Vec<String> {
    wanted
        .iter()
        .filter(|w| offered.iter().any(|o| o == *w))
        .map(|w| (*w).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_round_trips() {
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({ "id": 1, "method": "ping" })).unwrap();
        write_message(&mut buf, &json!({ "id": 2, "result": 7 })).unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(
            read_message(&mut r).unwrap().unwrap()["method"],
            json!("ping")
        );
        assert_eq!(read_message(&mut r).unwrap().unwrap()["result"], json!(7));
        assert!(read_message(&mut r).unwrap().is_none()); // EOF
    }

    #[test]
    fn negotiate_intersects_and_degrades() {
        let offered = vec!["fs.readFile".to_string(), "search".to_string()];
        // The client wants a superset — missing caps are dropped, present ones kept in the client's order.
        assert_eq!(
            negotiate(&["fs.readFile", "git", "search"], &offered),
            vec!["fs.readFile".to_string(), "search".to_string()]
        );
        // No overlap → empty (a connection with zero shared services, not an error).
        assert!(negotiate(&["debug"], &offered).is_empty());
    }
}
