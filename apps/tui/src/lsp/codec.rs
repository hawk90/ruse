//! LSP JSON-RPC framing (F-014): messages are `Content-Length: N\r\n\r\n` followed by an `N`-byte JSON body.
//! [`spawn_reader`] parses frames off the server's stdout on a dedicated thread (mirroring `pty::spawn_reader`)
//! and forwards each parsed message over an `mpsc` channel; [`write_message`] frames an outgoing message.

use std::io::{self, BufRead, Write};
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

/// Read one frame: parse `Content-Length` from the header block, then that many body bytes. Returns `Ok(None)`
/// at EOF. A header block with no `Content-Length` (malformed) yields `Value::Null` so the reader can skip it.
fn read_frame<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
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
        return Ok(Some(Value::Null)); // malformed header block — skip
    };
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
}
