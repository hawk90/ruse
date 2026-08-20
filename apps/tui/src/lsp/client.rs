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

    /// A MOCK JSON-RPC contract test (no process): for every request kind, build the exact `{id, result}`
    /// envelope a server sends, run it through `classify` (→ `Response`), then the matching parser, and
    /// assert the normalized output. This pins the protocol round-trip fast + RA-version-independently —
    /// the `#[ignore]` live smoke is the complement (real wire, occasionally), not the everyday guard.
    #[test]
    fn classify_and_parse_every_response_shape() {
        use crate::lsp::protocol::{
            parse_code_actions, parse_completion, parse_definition, parse_hover, parse_locations,
            parse_text_edits, parse_workspace_edit,
        };
        let init_id = 1;
        // Classify `{id, result}` as a (non-init) Response and hand back the inner result.
        let result_of = |id: i64, result: Value| -> Value {
            let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
            match classify(&msg, init_id) {
                Incoming::Response(got, r) => {
                    assert_eq!(got, id, "response id correlates");
                    r
                }
                _ => panic!("expected a Response for id {id}"),
            }
        };
        let loc = |uri: &str| json!({"uri": uri, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}});
        let te = json!({"range": {"start": {"line":0,"character":0}, "end": {"line":0,"character":1}}, "newText": "X"});

        // hover
        assert!(parse_hover(&result_of(2, json!({"contents": "T"}))).is_some());
        // definition (single Location)
        assert_eq!(
            parse_definition(&result_of(3, loc("file:///d.rs"))),
            Some(("file:///d.rs".into(), 0, 0))
        );
        // references (Location[])
        assert_eq!(
            parse_locations(&result_of(
                4,
                json!([loc("file:///a.rs"), loc("file:///b.rs")])
            ))
            .len(),
            2
        );
        // completion (CompletionList)
        assert_eq!(
            parse_completion(&result_of(5, json!({"items": [{"label": "foo"}]}))).len(),
            1
        );
        // formatting (TextEdit[])
        assert_eq!(
            parse_text_edits(&result_of(6, json!([te.clone()]))).len(),
            1
        );
        // rename (WorkspaceEdit, changes form)
        assert_eq!(
            parse_workspace_edit(&result_of(
                7,
                json!({"changes": {"file:///a.rs": [te.clone()]}})
            ))
            .len(),
            1
        );
        // code actions (edit-bearing CodeAction[])
        assert_eq!(
            parse_code_actions(&result_of(
                8,
                json!([{"title": "Fix", "edit": {"changes": {"file:///a.rs": [te]}}}])
            ))
            .len(),
            1
        );
    }

    // End-to-end against a REAL rust-analyzer (not run in CI). Verifies the whole pipeline: spawn → handshake
    // → didOpen → publishDiagnostics (parsed), a hover, a formatting, AND a rename request-response.
    // Run: `cargo test -p ruse-tui --lib -- --ignored live_diagnostics_hover_format_and_rename`. Requires
    // `rustup component add rust-analyzer`.
    #[test]
    #[ignore = "spawns a real rust-analyzer; run with --ignored"]
    fn live_lsp_pipeline() {
        use crate::lsp::protocol::{
            code_action_params, completion_params, formatting_params, parse_completion,
            parse_hover, parse_locations, parse_text_edits, parse_workspace_edit, position_params,
            references_params, rename_params,
        };
        use serde_json::Value;
        use std::process::Command as PCommand;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("ruse_lsp_smoke_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname=\"t\"\nversion=\"0.0.0\"\nedition=\"2021\"\n\n[[bin]]\nname=\"t\"\npath=\"src/main.rs\"\n",
        )
        .unwrap();
        let main_rs = dir.join("src/main.rs");
        // Valid syntax but a TYPE error (diagnostic) AND mis-formatted (rustfmt will emit edits).
        let src = "fn main() {\nlet x:i32=\"nope\";\nlet _=x;\n}\n";
        std::fs::write(&main_rs, src).unwrap();

        let root_uri = format!("file://{}", dir.display());
        let file_uri = format!("file://{}", main_rs.display());
        let mut client =
            LspClient::spawn(PCommand::new("rust-analyzer"), &root_uri).expect("spawn RA");
        client.did_open(&file_uri, "rust", 1, src);

        let (
            mut got_diag,
            mut got_hover,
            mut got_fmt,
            mut got_rename,
            mut got_completion,
            mut got_refs,
            mut got_actions,
        ) = (false, false, false, false, false, false, false);
        let mut hover_id: Option<i64> = None;
        let mut fmt_id: Option<i64> = None;
        let mut rename_id: Option<i64> = None;
        let mut completion_id: Option<i64> = None;
        let mut refs_id: Option<i64> = None;
        let mut action_id: Option<i64> = None;
        let mut last_hover = Instant::now();
        let mut last_fmt = Instant::now();
        let mut last_rename = Instant::now();
        let mut last_completion = Instant::now();
        let mut last_refs = Instant::now();
        let mut last_action = Instant::now();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(120) {
            let polled = client.poll();
            for p in &polled.diagnostics {
                if p.uri == file_uri && !p.diagnostics.is_empty() {
                    got_diag = true;
                }
            }
            for (id, result) in &polled.responses {
                if Some(*id) == hover_id && parse_hover(result).is_some() {
                    got_hover = true;
                }
                if Some(*id) == fmt_id && !parse_text_edits(result).is_empty() {
                    got_fmt = true;
                }
                // Renaming `x` → `y` rewrites its binding (line 1) and its use (line 2): ≥2 edits, one file.
                if Some(*id) == rename_id {
                    let we = parse_workspace_edit(result);
                    if we.iter().map(|(_, e)| e.len()).sum::<usize>() >= 2 {
                        got_rename = true;
                    }
                }
                if Some(*id) == completion_id && !parse_completion(result).is_empty() {
                    got_completion = true;
                }
                if Some(*id) == refs_id && !parse_locations(result).is_empty() {
                    got_refs = true;
                }
                // Code actions: RA's edit-bearing action availability at a spot is version-dependent, so
                // assert the request/response WIRE round-trips (the id comes back), not non-empty content.
                if Some(*id) == action_id {
                    got_actions = true;
                }
            }
            // Once diagnostics flow the server is ready; (re)send hover on `x` (line 1 col 4), a format, and a
            // rename of `x` at the same position.
            if got_diag && !got_hover && last_hover.elapsed() > Duration::from_secs(2) {
                hover_id =
                    Some(client.request("textDocument/hover", position_params(&file_uri, 1, 4)));
                last_hover = Instant::now();
            }
            if got_diag && !got_fmt && last_fmt.elapsed() > Duration::from_secs(2) {
                fmt_id = Some(client.request(
                    "textDocument/formatting",
                    formatting_params(&file_uri, 4, true),
                ));
                last_fmt = Instant::now();
            }
            if got_diag && !got_rename && last_rename.elapsed() > Duration::from_secs(2) {
                rename_id = Some(
                    client.request("textDocument/rename", rename_params(&file_uri, 1, 4, "y")),
                );
                last_rename = Instant::now();
            }
            // Completion at the start of the `let _=x;` expression (line 2 col 6) → many candidates.
            if got_diag && !got_completion && last_completion.elapsed() > Duration::from_secs(2) {
                completion_id = Some(client.request(
                    "textDocument/completion",
                    completion_params(&file_uri, 2, 6),
                ));
                last_completion = Instant::now();
            }
            // References to `x` (line 1 col 4): its binding + use → ≥1 location.
            if got_diag && !got_refs && last_refs.elapsed() > Duration::from_secs(2) {
                refs_id = Some(client.request(
                    "textDocument/references",
                    references_params(&file_uri, 1, 4, true),
                ));
                last_refs = Instant::now();
            }
            // Code action at the type-error line (empty diagnostics context — we assert the wire, not content).
            if got_diag && !got_actions && last_action.elapsed() > Duration::from_secs(2) {
                action_id = Some(client.request(
                    "textDocument/codeAction",
                    code_action_params(&file_uri, 1, 4, Value::Array(Vec::new())),
                ));
                last_action = Instant::now();
            }
            if got_diag
                && got_hover
                && got_fmt
                && got_rename
                && got_completion
                && got_refs
                && got_actions
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(got_diag, "no diagnostics arrived from rust-analyzer");
        assert!(got_hover, "no hover response arrived from rust-analyzer");
        assert!(got_fmt, "no formatting edits arrived from rust-analyzer");
        assert!(
            got_rename,
            "no rename WorkspaceEdit arrived from rust-analyzer"
        );
        assert!(
            got_completion,
            "no completion items arrived from rust-analyzer"
        );
        assert!(got_refs, "no references arrived from rust-analyzer");
        assert!(
            got_actions,
            "no codeAction response arrived from rust-analyzer"
        );
    }
}
