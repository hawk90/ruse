//! The client↔agent wire protocol (F-017 / RFC-0006, slice 1): the same proven `Content-Length: N\r\n\r\n` +
//! JSON-body framing as the LSP codec, plus the version/capability negotiation contract. A message is a
//! request `{ id, method, params }` or a response `{ id, result }` / `{ id, error: { message } }`.

use std::io::{self, BufRead, Read, Write};

use serde_json::{json, Value};

use super::error::AgentError;

/// The wire protocol version. A handshake records the peer's version; incompatible versions DEGRADE (a
/// smaller negotiated capability set), they do not fail the connection (F-017 acceptance #3).
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest frame body either side accepts or sends: `CONTRACT-REMOTE` `budgets.frame_max_bytes`
/// (`spec/contracts/remote-runtime.yaml`, 4 MiB). Bounds the allocation a peer's `Content-Length` can request.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// The longest header line accepted (`Content-Length: <digits>\r\n` needs ~40). Without a cap, a peer that
/// never sends `\n` grows the header buffer without bound — the same exhaustion the body cap prevents.
const MAX_HEADER_LINE: u64 = 1024;

/// An `InvalidData` error for a frame body of `len` bytes over [`MAX_FRAME_BYTES`].
fn frame_too_large(len: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte protocol limit"),
    )
}

/// Frame and write one JSON message (`Content-Length` header + body). Same framing as `lsp/codec.rs`.
/// A body over [`MAX_FRAME_BYTES`] is refused with `InvalidData` and NOTHING is written, so the stream stays
/// in sync and the caller can report the failure (the peer would reject the frame anyway).
pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    write_body(w, &serde_json::to_vec(msg)?)
}

/// Frame and write an already-serialized body (see [`write_message`]; same size check).
pub(crate) fn write_body<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(frame_too_large(body.len()));
    }
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()
}

/// Read one framed message. `Ok(None)` at EOF; a header block with no `Content-Length` yields `Value::Null`
/// so the reader can skip a malformed frame rather than desync. A `Content-Length` over [`MAX_FRAME_BYTES`]
/// (or a header line over 1 KiB) is an `InvalidData` error — checked BEFORE allocating, so a hostile or
/// corrupt peer cannot make the reader allocate an arbitrary amount of memory. The connection is unusable
/// after that error (the body was not consumed).
pub fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.by_ref().take(MAX_HEADER_LINE).read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        if n as u64 == MAX_HEADER_LINE && !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame header line exceeds {MAX_HEADER_LINE} bytes"),
            ));
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
    if len > MAX_FRAME_BYTES {
        return Err(frame_too_large(len));
    }
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

    /// Regression: a peer-declared `Content-Length` over the contract limit is rejected with a clear
    /// `InvalidData` error BEFORE any allocation (previously `vec![0; len]` trusted the peer — a
    /// `usize::MAX` header aborted the process). A body exactly at the limit is still accepted.
    #[test]
    fn oversized_content_length_is_rejected_before_allocating() {
        for len in [MAX_FRAME_BYTES + 1, usize::MAX] {
            let mut r = Cursor::new(format!("Content-Length: {len}\r\n\r\n").into_bytes());
            let err = read_message(&mut r).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains("exceeds"), "{err}");
        }
        // At the limit: a JSON string padded to exactly MAX_FRAME_BYTES round-trips.
        let s = "x".repeat(MAX_FRAME_BYTES - 2); // + 2 quotes = MAX_FRAME_BYTES
        let mut buf = Vec::new();
        write_message(&mut buf, &json!(s)).unwrap();
        let got = read_message(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(got.as_str().map(str::len), Some(MAX_FRAME_BYTES - 2));
    }

    /// A header line with no newline is capped at 1 KiB (an `InvalidData` error), not read without bound.
    #[test]
    fn overlong_header_line_is_rejected() {
        let mut r = Cursor::new(vec![b'a'; 64 * 1024]);
        let err = read_message(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// The sender refuses an oversized body and writes NOTHING, so the stream never carries a frame the peer
    /// must reject (and stays in sync for the next message).
    #[test]
    fn oversized_body_is_not_written() {
        let mut buf = Vec::new();
        let err = write_message(&mut buf, &json!("x".repeat(MAX_FRAME_BYTES))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(buf.is_empty());
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
