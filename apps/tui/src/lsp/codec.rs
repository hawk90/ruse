//! LSP JSON-RPC framing (F-014): messages are `Content-Length: N\r\n\r\n` followed by an `N`-byte JSON body.
//! [`spawn_reader`] parses frames off the server's stdout on a dedicated thread (mirroring `pty::spawn_reader`)
//! and forwards each parsed message over an `mpsc` channel; [`write_message`] frames an outgoing message.

use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use serde_json::Value;

/// Frame and write one JSON-RPC message.
pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// The largest LSP message body the client will buffer. LSP has no protocol-level cap and no ruse contract
/// pins one (the 4 MiB `CONTRACT-REMOTE` budget is for the client↔agent wire, not language servers, whose
/// legitimate replies — big completion lists, whole-file diagnostics — can run to several MiB), so this is a
/// generous memory-safety bound: it stops a server's `Content-Length` from requesting an arbitrary allocation.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The longest header line accepted (`Content-Length: <digits>\r\n` needs ~40); bounds a newline-less stream.
const MAX_HEADER_LINE: u64 = 1024;

/// Read one frame: parse `Content-Length` from the header block, then that many body bytes. Returns `Ok(None)`
/// at EOF. A header block with no `Content-Length` (malformed) yields `Value::Null` so the reader can skip it.
/// A body over [`MAX_FRAME_BYTES`] is DRAINED (streamed to a sink, never allocated) and also yields
/// `Value::Null`, so one oversized reply is dropped without desyncing the stream. A header line over 1 KiB is
/// an `InvalidData` error (the reader thread ends).
fn read_frame<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
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
                format!("LSP header line exceeds {MAX_HEADER_LINE} bytes"),
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
        return Ok(Some(Value::Null)); // malformed header block — skip
    };
    if len > MAX_FRAME_BYTES {
        tracing::warn!(
            len,
            limit = MAX_FRAME_BYTES,
            "LSP message over the size limit; dropped"
        );
        let want = len as u64;
        if io::copy(&mut r.by_ref().take(want), &mut io::sink())? < want {
            return Ok(None); // EOF inside the oversized body
        }
        return Ok(Some(Value::Null));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

/// Read frames off `r` on a dedicated thread, forwarding each parsed message to `tx`. Ends on EOF (server
/// exit), an I/O error, or once the receiver is dropped.
pub fn spawn_reader<R: BufRead + Send + 'static>(mut r: R, tx: Sender<Value>) -> JoinHandle<()> {
    thread::spawn(move || {
        // Ends when `read_frame` yields `Ok(None)` (EOF) or `Err` — both fail the `while let` pattern.
        while let Ok(Some(v)) = read_frame(&mut r) {
            if !v.is_null() && tx.send(v).is_err() {
                break; // receiver dropped
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn write_then_read_round_trips() {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"x":42}});
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        // The header is present and the body follows a blank line.
        assert!(buf.starts_with(b"Content-Length: "));
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), msg);
        assert!(read_frame(&mut cur).unwrap().is_none()); // EOF after the one frame
    }

    #[test]
    fn reads_two_back_to_back_frames() {
        let a = json!({"a":1});
        let b = json!({"b":2});
        let mut buf = Vec::new();
        write_message(&mut buf, &a).unwrap();
        write_message(&mut buf, &b).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), a);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), b);
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    /// Regression: a server-declared `Content-Length` over the limit is never allocated. The oversized body
    /// is drained and skipped (`Null`), and the NEXT frame still parses — the stream stays in sync. A
    /// `usize::MAX` length with no body behind it simply reaches EOF.
    #[test]
    fn oversized_frame_is_skipped_without_allocating() {
        let big = MAX_FRAME_BYTES + 1;
        let mut buf = format!("Content-Length: {big}\r\n\r\n").into_bytes();
        buf.resize(buf.len() + big, b' ');
        let after = json!({"after": true});
        write_message(&mut buf, &after).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap(), Some(Value::Null));
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), after);

        let mut cur = Cursor::new(format!("Content-Length: {}\r\n\r\n", usize::MAX).into_bytes());
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    /// A newline-less header stream is capped (an error), not buffered without bound.
    #[test]
    fn overlong_header_line_is_rejected() {
        let mut cur = Cursor::new(vec![b'a'; 64 * 1024]);
        assert_eq!(
            read_frame(&mut cur).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
