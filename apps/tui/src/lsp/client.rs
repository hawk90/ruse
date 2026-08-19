//! An LSP client: one language-server child process (F-014 slice 1). Spawns the server over stdio pipes,
//! reads framed messages off a dedicated thread (the terminal's async pattern, but cross-platform), drives the
//! `initialize`→`initialized`→`didOpen` handshake, and surfaces `publishDiagnostics`. Notifications sent before
//! the server is ready are queued and flushed on `initialized`.

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;

use serde_json::{json, Value};

use super::codec::{spawn_reader, write_message};
use super::protocol::{
    did_change_params, did_open_params, initialize_params, PublishDiagnosticsParams,
};

pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    next_id: i64,
    init_id: i64,
    ready: bool,
    /// Notifications (didOpen/didChange) issued before `initialized` — flushed once the server is ready.
    pending: Vec<Value>,
}

impl LspClient {
    /// Spawn `command` (already carrying its args) as a language server rooted at `root_uri`, sending the
    /// `initialize` request. Returns `None` if the server binary cannot be launched (e.g. not installed) so a
    /// missing server is a silent no-op, never a hang.
    pub fn spawn(mut command: Command, root_uri: &str) -> Option<LspClient> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::channel();
        let reader = spawn_reader(BufReader::new(stdout), tx);
        let mut client = LspClient {
            child,
            stdin,
            rx,
            reader: Some(reader),
            next_id: 1,
            init_id: 0,
            ready: false,
            pending: Vec::new(),
        };
        client.init_id = client.request("initialize", initialize_params(root_uri));
        Some(client)
    }

    fn send(&mut self, msg: &Value) {
        let _ = write_message(&mut self.stdin, msg);
    }

    /// Send a request, returning its id (correlate the reply via [`Polled::responses`]).
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        id
    }

    /// Send a notification now if ready, else queue it until `initialized`.
    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        if self.ready {
            self.send(&msg);
        } else {
            self.pending.push(msg);
        }
    }

    pub fn did_open(&mut self, uri: &str, language_id: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didOpen",
            did_open_params(uri, language_id, version, text),
        );
    }

    pub fn did_change(&mut self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            did_change_params(uri, version, text),
        );
    }

    /// Drain incoming messages: drive the handshake, answer server-to-client requests (so the server never
    /// blocks), and return diagnostics + any request responses that arrived. Call once per frame.
    pub fn poll(&mut self) -> Polled {
        let mut out = Polled::default();
        while let Ok(msg) = self.rx.try_recv() {
            match classify(&msg, self.init_id) {
                // A server→client REQUEST: reply with a null result so the server proceeds (we advertise no
                // dynamic capabilities / config).
                Incoming::ServerRequest(id) => {
                    self.send(&json!({"jsonrpc":"2.0","id":id,"result":Value::Null}));
                }
                Incoming::Diagnostics(p) => out.diagnostics.push(p),
                Incoming::Response(id, result) => out.responses.push((id, result)),
                Incoming::InitResponse => self.become_ready(),
                Incoming::Other => {}
            }
        }
        out
    }

    fn become_ready(&mut self) {
        if self.ready {
            return;
        }
        self.ready = true;
        self.send(&json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
        for msg in std::mem::take(&mut self.pending) {
            let _ = write_message(&mut self.stdin, &msg);
        }
    }
}

/// What [`LspClient::poll`] collected this tick.
#[derive(Default)]
pub struct Polled {
    pub diagnostics: Vec<PublishDiagnosticsParams>,
    /// `(request id, result)` for each reply to one of our requests (hover, definition, …).
    pub responses: Vec<(i64, Value)>,
}

/// A classified incoming message — the pure core of [`LspClient::poll`], testable without a process.
enum Incoming {
    /// A server→client request; carries the id to reply to.
    ServerRequest(Value),
    Diagnostics(PublishDiagnosticsParams),
    /// A reply to one of our (non-initialize) requests: `(id, result)`.
    Response(i64, Value),
    /// The response to our `initialize` request (handshake complete).
    InitResponse,
    Other,
}

fn classify(msg: &Value, init_id: i64) -> Incoming {
    match msg.get("method").and_then(Value::as_str) {
        Some(_) if msg.get("id").is_some() => {
            Incoming::ServerRequest(msg.get("id").cloned().unwrap_or(Value::Null))
        }
        Some("textDocument/publishDiagnostics") => msg
            .get("params")
            .cloned()
            .and_then(|p| serde_json::from_value::<PublishDiagnosticsParams>(p).ok())
            .map_or(Incoming::Other, Incoming::Diagnostics),
        Some(_) => Incoming::Other, // window/logMessage, $/progress, …
        None => match msg.get("id").and_then(Value::as_i64) {
            // A response to one of our requests. The initialize reply is special (handshake); others carry a
            // result we correlate by id.
            Some(id) if msg.get("result").is_some() => {
                if id == init_id {
                    Incoming::InitResponse
                } else {
                    Incoming::Response(id, msg.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            _ => Incoming::Other,
        },
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort graceful shutdown, then ensure the child is gone and the reader thread joined.
        let id = self.next_id;
        let _ = write_message(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","id":id,"method":"shutdown"}),
        );
        let _ = write_message(&mut self.stdin, &json!({"jsonrpc":"2.0","method":"exit"}));
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_init_response_diagnostics_and_requests() {
        // The initialize response (id matches, has a result) → InitResponse.
        let init = json!({"jsonrpc":"2.0","id":7,"result":{"capabilities":{}}});
        assert!(matches!(classify(&init, 7), Incoming::InitResponse));
        // A different id / no result is not the init response.
        assert!(matches!(classify(&json!({"id":7}), 7), Incoming::Other));
        // A non-init response is correlated by id.
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","id":9,"result":{"x":1}}), 7),
            Incoming::Response(9, _)
        ));
        // A server→client request (method + id) → reply.
        let req =
            json!({"jsonrpc":"2.0","id":"cfg","method":"workspace/configuration","params":{}});
        assert!(matches!(classify(&req, 7), Incoming::ServerRequest(_)));
        // publishDiagnostics → parsed model.
        let diag = json!({
            "jsonrpc":"2.0","method":"textDocument/publishDiagnostics",
            "params":{"uri":"file:///x.rs","diagnostics":[]}
        });
        assert!(matches!(classify(&diag, 7), Incoming::Diagnostics(_)));
        // A plain notification we don't handle → Other.
        let log = json!({"jsonrpc":"2.0","method":"window/logMessage","params":{}});
        assert!(matches!(classify(&log, 7), Incoming::Other));
    }
}
