//! The app-layer LSP coordinator (F-014, `CAP-LSP-COORD`). Owns the app-side LSP state and drives the
//! cross-platform [`crate::lsp::LspClient`], exposing a small method surface the session event loop calls at
//! the right points — so the loop is no longer sprinkled with LSP logic. `lsp/` stays the pure client
//! (protocol/codec/model, no app types); THIS is the wiring that touches `Workspace`/`Files`/highlighters.
//!
//! Determinism boundary (F-022): the coordinator issues NO `Command`s — its edits go through
//! `Workspace::apply_edits(_, TransactionOrigin::Lsp)`, external and un-replayed, exactly as before.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ruse_core::{DocumentId, Mode, Revision, TransactionOrigin, Workspace};
use serde_json::Value;

use crate::app::dispatch::{is_ctrl, open_file_into_buffer, Files, Highlighters};
use crate::input::Ex;
use crate::lsp::{self, Diag, LspClient};

/// What a pending LSP request was for, so its response (correlated by id) is dispatched correctly.
#[derive(Clone, Copy)]
enum LspKind {
    Hover,
    Goto,
    Format,
    Rename,
    /// A completion request; carries the buffer revision at request time, so a response for a stale
    /// revision (the buffer moved on since) is discarded rather than applied (F-014 live-filter).
    Completion(Revision),
    /// A references request; the response's locations are stashed for the loop to open its picker.
    References,
    /// A code-action request; the response's actions are stashed for the loop to open its picker.
    CodeAction,
    /// A `completionItem/resolve` for the pum item at this index, at the request-time revision (F-014).
    Resolve(Revision, usize),
    /// A `workspace/executeCommand` (a command-only code action) — its result is ignored; the effect
    /// returns as a server `workspace/applyEdit` (F-014).
    ExecuteCommand,
}

/// A stashed server `workspace/applyEdit`: `(serverKey, reply-id, per-file edits)`.
type ServerEdit = (
    String,
    Value,
    Vec<(String, Vec<lsp::protocol::LspTextEdit>)>,
);

/// The open completion popup menu (pum): the parsed candidates, the highlighted index, and the typed
/// identifier prefix length the accepted item replaces (F-014 #5).
struct CompletionMenu {
    items: Vec<lsp::protocol::CompletionItem>,
    selected: usize,
    prefix_len: usize,
}

/// Owns the app-side LSP state and drives one [`LspClient`] per language server.
pub(crate) struct LspCoordinator {
    root_uri: String,
    cwd: PathBuf,
    /// One client per server (keyed by server command); the buffers opened into them; the servers already
    /// tried to spawn (so a missing binary is not retried each frame); the normalized diagnostics.
    lsp: HashMap<String, LspClient>,
    lsp_docs: HashMap<DocumentId, (String, i64, Revision)>, // id → (uri, version, rev)
    lsp_tried: HashSet<String>,
    diagnostics: HashMap<DocumentId, Vec<Diag>>,
    /// Pending requests keyed by (server command, request id).
    pending: HashMap<(String, i64), LspKind>,
    /// The transient hover panel (shown in the overlay slot; cleared on the next key).
    hover_panel: Option<Vec<String>>,
    /// The open completion pum (persists across frames until dismissed).
    completion: Option<CompletionMenu>,
    /// The `<C-x>` awaiting `<C-o>` omni-trigger prefix (Vim/Native insert only).
    pending_omni: bool,
    /// The id of the LATEST completion request — the ONLY id whose response is applied. Any other (stale,
    /// out-of-order) completion response is discarded. Cleared to `None` whenever the pum closes, so a
    /// response arriving after a close is dropped too (F-014 live-filter, request-id invalidation).
    completion_req: Option<i64>,
    /// A keystroke (word char / backspace) edited the buffer while the pum was open → re-request completion
    /// once the edit is synced (set in `on_completion_key`, consumed in `sync_and_poll`).
    refilter: bool,
    /// The id of the latest `completionItem/resolve` request — the only one whose response is applied.
    resolve_req: Option<i64>,
    /// The pum index whose item to (lazily) resolve — set when the pum opens / selection moves, sent in
    /// `sync_and_poll` (gated on the item having `data` and being unresolved).
    pending_resolve: Option<usize>,
    // Deferred results — set while polling (inside the frame's `spans` borrow) and drained AFTER render.
    goto_jump: Option<(String, u32, u32)>,
    pending_edits: Vec<(usize, usize, String)>,
    pending_rename: Vec<(String, Vec<lsp::protocol::LspTextEdit>)>,
    pending_refs: Option<Vec<(String, u32, u32)>>,
    pending_actions: Option<Vec<lsp::protocol::CodeAction>>,
    /// Server `workspace/applyEdit` requests to apply + reply after render.
    pending_server_edits: Vec<ServerEdit>,
}

impl LspCoordinator {
    pub(crate) fn new(cwd: PathBuf) -> LspCoordinator {
        LspCoordinator {
            root_uri: lsp::path_to_uri(&cwd),
            cwd,
            lsp: HashMap::new(),
            lsp_docs: HashMap::new(),
            lsp_tried: HashSet::new(),
            diagnostics: HashMap::new(),
            pending: HashMap::new(),
            hover_panel: None,
            completion: None,
            pending_omni: false,
            completion_req: None,
            refilter: false,
            resolve_req: None,
            pending_resolve: None,
            goto_jump: None,
            pending_edits: Vec::new(),
            pending_rename: Vec::new(),
            pending_refs: None,
            pending_actions: None,
            pending_server_edits: Vec::new(),
        }
    }

    /// The `(server key, uri)` for the FOCUSED buffer, if it is backed by a spawned+opened language server.
    fn focused_server(&self, ws: &Workspace, files: &Files) -> Option<(&'static str, String)> {
        let bid = ws.focused_buffer();
        let key = files
            .get(&bid)?
            .path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(lsp::server_for_ext)
            .map(|(k, _, _)| k)?;
        let uri = self.lsp_docs.get(&bid).map(|(u, _, _)| u.clone())?;
        Some((key, uri))
    }

    /// Send a request to `key`'s client and record its pending kind; returns the request id (or `None` if
    /// the client is gone).
    fn send(&mut self, key: &str, method: &str, params: Value, kind: LspKind) -> Option<i64> {
        let client = self.lsp.get_mut(key)?;
        let rid = client.request(method, params);
        self.pending.insert((key.to_string(), rid), kind);
        Some(rid)
    }

    /// Send a `textDocument/completion` at the cursor and mark it the latest completion request (its id
    /// gates which response is applied). Shared by the `<C-x><C-o>` trigger and the live-filter re-request.
    fn request_completion(
        &mut self,
        ws: &Workspace,
        files: &Files,
        snapshot: &[u8],
        revision: Revision,
    ) {
        if let Some((key_s, uri)) = self.focused_server(ws, files) {
            let cur = ws.focused().view.cursor().min(snapshot.len());
            let (line, ch) = lsp::model::byte_to_lsp_pos(snapshot, cur);
            self.completion_req = self.send(
                key_s,
                "textDocument/completion",
                lsp::protocol::completion_params(&uri, line, ch),
                LspKind::Completion(revision),
            );
        }
    }

    /// Apply a completion response — the pure core of the live-filter contract, unit-tested without a client.
    /// DISCARDS the response unless it is the LATEST request (`rid == completion_req`) AND the buffer has not
    /// moved since (`req_rev == current_rev`). Recomputes the typed identifier prefix from the CURRENT cursor,
    /// filters items by it, and refreshes the pum — preserving the selected item by label when it survives.
    /// An empty prefix or no surviving items closes the pum.
    fn ingest_completion(
        &mut self,
        rid: i64,
        req_rev: Revision,
        result: &Value,
        cur: usize,
        snapshot: &[u8],
        current_rev: Revision,
    ) {
        if Some(rid) != self.completion_req || req_rev != current_rev {
            return; // stale: a newer request is in flight, or the buffer moved since this one
        }
        let prefix_len = identifier_prefix_len(snapshot, cur);
        if prefix_len == 0 {
            self.completion = None; // empty prefix → close rather than list the whole world
            self.completion_req = None;
            return;
        }
        let prefix =
            String::from_utf8_lossy(&snapshot[cur.saturating_sub(prefix_len)..cur]).to_lowercase();
        let items: Vec<_> = lsp::protocol::parse_completion(result)
            .into_iter()
            .filter(|it| it.label.to_lowercase().starts_with(&prefix))
            .collect();
        if items.is_empty() {
            self.completion = None;
            self.completion_req = None;
            return;
        }
        // Preserve the current selection by label when it survives the refilter; else reset to the top.
        let keep = self
            .completion
            .as_ref()
            .and_then(|m| m.items.get(m.selected))
            .map(|it| it.label.clone());
        let selected = keep
            .and_then(|lbl| items.iter().position(|it| it.label == lbl))
            .unwrap_or(0);
        self.completion = Some(CompletionMenu {
            items,
            selected,
            prefix_len,
        });
        self.pending_resolve = Some(selected); // lazily resolve the shown selection
    }

    /// Keep the language server in sync with the FOCUSED file buffer (spawn once per server, didOpen once
    /// per buffer, didChange on a revision change), then poll every client: apply the focused buffer's
    /// diagnostics and dispatch request responses. Missing servers / non-code buffers are silent no-ops.
    pub(crate) fn sync_and_poll(
        &mut self,
        ws: &Workspace,
        files: &Files,
        revision: Revision,
        snapshot: &[u8],
        status: &mut String,
    ) {
        let id = ws.focused_buffer();
        let focused_uri = if let Some(bf) = files.get(&id) {
            bf.path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(lsp::server_for_ext)
                .and_then(|(key, cmd, lang)| {
                    // Spawn the server once (track attempts so a missing binary is not retried each frame).
                    if !self.lsp.contains_key(key) && self.lsp_tried.insert(key.to_string()) {
                        if let Some(c) = LspClient::spawn(cmd, &self.root_uri) {
                            self.lsp.insert(key.to_string(), c);
                        }
                    }
                    let client = self.lsp.get_mut(key)?;
                    let text = String::from_utf8_lossy(snapshot);
                    match self.lsp_docs.get_mut(&id) {
                        None => {
                            let uri = std::fs::canonicalize(&bf.path)
                                .map(|p| lsp::path_to_uri(&p))
                                .unwrap_or_else(|_| lsp::path_to_uri(&bf.path));
                            client.did_open(&uri, lang, 1, &text);
                            self.lsp_docs.insert(id, (uri.clone(), 1, revision));
                            Some(uri)
                        }
                        Some((uri, version, last_rev)) => {
                            if *last_rev != revision {
                                *version += 1;
                                *last_rev = revision;
                                client.did_change(uri, *version, &text);
                            }
                            Some(uri.clone())
                        }
                    }
                })
        } else {
            None
        };
        // Poll every client: apply diagnostics for the focused buffer (matched by uri), and dispatch any
        // request responses by their pending (serverKey, id).
        let mut completion_responses: Vec<(i64, Revision, Value)> = Vec::new();
        let mut resolve_responses: Vec<(i64, Revision, usize, Value)> = Vec::new();
        for (key, client) in self.lsp.iter_mut() {
            let polled = client.poll();
            for params in polled.diagnostics {
                if Some(&params.uri) == focused_uri.as_ref() {
                    self.diagnostics
                        .insert(id, lsp::protocol::to_diags(snapshot, &params));
                }
            }
            for (rid, result) in polled.responses {
                match self.pending.remove(&(key.clone(), rid)) {
                    Some(LspKind::Hover) => self.hover_panel = lsp::protocol::parse_hover(&result),
                    // Defer the jump until AFTER render — it may open a buffer (mutating `highlighters`),
                    // which the frame's live `spans` borrow forbids here.
                    Some(LspKind::Goto) => {
                        self.goto_jump = lsp::protocol::parse_definition(&result)
                    }
                    // Convert TextEdits (UTF-16 ranges) to byte ranges; applied after render.
                    Some(LspKind::Format) => {
                        self.pending_edits = lsp::protocol::parse_text_edits(&result)
                            .into_iter()
                            .map(|((sl, sc), (el, ec), text)| {
                                (
                                    lsp::model::lsp_pos_to_byte(snapshot, sl, sc),
                                    lsp::model::lsp_pos_to_byte(snapshot, el, ec),
                                    text,
                                )
                            })
                            .collect();
                    }
                    // A WorkspaceEdit spans files; keep the raw per-file UTF-16 edits and convert each
                    // against its own bytes when applied after render (offsets are per-document).
                    Some(LspKind::Rename) => {
                        self.pending_rename = lsp::protocol::parse_workspace_edit(&result);
                    }
                    // Completion: stash for after the loop — applying it calls `&mut self` (ingest_completion),
                    // which cannot run while `self.lsp` is borrowed by `iter_mut()`.
                    Some(LspKind::Completion(req_rev)) => {
                        completion_responses.push((rid, req_rev, result));
                    }
                    // Resolve: stash for after the loop (ingest_resolve is `&mut self`).
                    Some(LspKind::Resolve(req_rev, idx)) => {
                        resolve_responses.push((rid, req_rev, idx, result));
                    }
                    // executeCommand result is ignored — the effect returns via `workspace/applyEdit`.
                    Some(LspKind::ExecuteCommand) => {}
                    // References: stash the locations; the loop opens its picker after render.
                    Some(LspKind::References) => {
                        let locs = lsp::protocol::parse_locations(&result);
                        if locs.is_empty() {
                            *status = "no references".to_string();
                        } else {
                            *status = format!("{} reference(s)", locs.len());
                            self.pending_refs = Some(locs);
                        }
                    }
                    // Code actions: stash the edit-bearing actions; the loop opens its picker after render.
                    Some(LspKind::CodeAction) => {
                        let actions = lsp::protocol::parse_code_actions(&result);
                        if actions.is_empty() {
                            *status = "no code actions".to_string();
                        } else {
                            *status = format!("{} code action(s)", actions.len());
                            self.pending_actions = Some(actions);
                        }
                    }
                    None => {}
                }
            }
            // Server `workspace/applyEdit` requests: stash (key, reply-id, parsed edit) to apply + reply
            // after render (applying needs `&mut ws`). `self.pending_server_edits` is a disjoint field.
            for (id, params) in polled.apply_edits {
                let edit = params
                    .get("edit")
                    .map(lsp::protocol::parse_workspace_edit)
                    .unwrap_or_default();
                self.pending_server_edits.push((key.clone(), id, edit));
            }
        }
        // Apply completion responses now the `self.lsp` borrow is released (ingest_completion is `&mut self`).
        let cur = ws.focused().view.cursor().min(snapshot.len());
        for (rid, req_rev, result) in completion_responses {
            self.ingest_completion(rid, req_rev, &result, cur, snapshot, revision);
        }
        // Live-filter: a word char / backspace edited the buffer while the pum was open — re-request now the
        // edit is synced (the didChange above bumped the revision). One request per keystroke; stale
        // responses are dropped by the completion_req id + revision guard in ingest_completion.
        if self.refilter {
            self.refilter = false;
            if self.completion.is_some() {
                self.request_completion(ws, files, snapshot, revision);
            }
        }
        // Merge resolve responses (detail/documentation/additionalTextEdits) into the pum item.
        for (rid, req_rev, idx, result) in resolve_responses {
            self.ingest_resolve(rid, req_rev, idx, &result, revision);
        }
        // Lazily resolve the selected item (fills detail/docs + auto-import), gated on it carrying `data`
        // and being unresolved. One request per selection; stale responses dropped by resolve_req + revision.
        if let Some(idx) = self.pending_resolve.take() {
            let raw = self
                .completion
                .as_ref()
                .and_then(|m| m.items.get(idx))
                .and_then(|it| {
                    (!it.resolved && lsp::protocol::has_resolve_data(it)).then(|| it.raw.clone())
                });
            if let Some(raw) = raw {
                if let Some((key_s, _uri)) = self.focused_server(ws, files) {
                    self.resolve_req = self.send(
                        key_s,
                        "completionItem/resolve",
                        raw,
                        LspKind::Resolve(revision, idx),
                    );
                }
            }
        }
    }

    /// Merge a `completionItem/resolve` response into the pum item at `idx` — discarded unless it is the
    /// latest resolve request (`resolve_req` id) at the current revision and the pum + index are still valid.
    fn ingest_resolve(
        &mut self,
        rid: i64,
        req_rev: Revision,
        idx: usize,
        result: &Value,
        current_rev: Revision,
    ) {
        if Some(rid) != self.resolve_req || req_rev != current_rev {
            return; // stale: superseded, or the buffer moved since the request
        }
        if let Some(item) = self.completion.as_mut().and_then(|m| m.items.get_mut(idx)) {
            lsp::protocol::apply_resolve(item, result);
        }
    }

    /// Apply the deferred goto jump, format edits, and multi-file rename now render is done (opening a
    /// buffer mutates `highlighters`, which the frame's `spans` borrow forbids during the poll).
    pub(crate) fn apply_pending(
        &mut self,
        ws: &mut Workspace,
        files: &mut Files,
        highlighters: &mut Highlighters,
        snapshot: &[u8],
        status: &mut String,
    ) {
        // Goto: same file → move the cursor; another file → open it, then move.
        if let Some((uri, l, c)) = self.goto_jump.take() {
            let path = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
            let target = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
            let cur_path = files
                .get(&ws.focused_buffer())
                .and_then(|bf| std::fs::canonicalize(&bf.path).ok());
            if cur_path.as_deref() == Some(target.as_path()) {
                ws.place_focused_cursor(lsp::model::lsp_pos_to_byte(snapshot, l, c));
            } else {
                open_file_into_buffer(&target.display().to_string(), ws, files, highlighters);
                let bytes = ws.focused().doc.bytes().to_vec();
                ws.place_focused_cursor(lsp::model::lsp_pos_to_byte(&bytes, l, c));
            }
        }
        // Format edits: one Lsp-origin undo group.
        if !self.pending_edits.is_empty() {
            ws.apply_edits(&self.pending_edits, TransactionOrigin::Lsp);
            self.pending_edits.clear();
            *status = "formatted".to_string();
        }
        // Rename across every affected file (the multi-file WorkspaceEdit apply, shared with code actions).
        if !self.pending_rename.is_empty() {
            let edit = std::mem::take(&mut self.pending_rename);
            let (files_n, edits_n) = apply_workspace_edit(edit, ws, files, highlighters);
            *status = format!("renamed: {edits_n} edit(s) across {files_n} file(s)");
        }
        // Server-initiated `workspace/applyEdit` (from an executeCommand): apply the TEXT edits as one
        // Lsp-origin transaction per file, then reply `{applied}` to the (blocked) server request. Trust:
        // text edits only (resource ops are already dropped by parse_workspace_edit), no process execution.
        for (key, id, edit) in std::mem::take(&mut self.pending_server_edits) {
            let applied = !edit.is_empty();
            if applied {
                apply_workspace_edit(edit, ws, files, highlighters);
                *status = "applied server edit".to_string();
            }
            if let Some(client) = self.lsp.get_mut(&key) {
                client.respond(id, serde_json::json!({ "applied": applied }));
            }
        }
    }

    /// The references locations resolved this frame (the loop opens its generic picker from these).
    pub(crate) fn take_refs(&mut self) -> Option<Vec<(String, u32, u32)>> {
        self.pending_refs.take()
    }

    /// The code actions resolved this frame (the loop opens its generic picker from these).
    pub(crate) fn take_actions(&mut self) -> Option<Vec<lsp::protocol::CodeAction>> {
        self.pending_actions.take()
    }

    /// Run a selected code action (from the action picker's accept, past the `spans` borrow): apply its
    /// inline `WorkspaceEdit` (multi-file, like rename), THEN — if it carries a `command` — fire
    /// `workspace/executeCommand` for it (its result is ignored; the effect returns as a server
    /// `workspace/applyEdit`, applied in `apply_pending`). Executing only a user-SELECTED action's command
    /// (never auto) is the trust boundary; a namespace allowlist is future hardening.
    pub(crate) fn apply_code_action(
        &mut self,
        action: &lsp::protocol::CodeAction,
        ws: &mut Workspace,
        files: &mut Files,
        highlighters: &mut Highlighters,
        status: &mut String,
    ) {
        if !action.edit.is_empty() {
            let (files_n, edits_n) =
                apply_workspace_edit(action.edit.clone(), ws, files, highlighters);
            *status = format!("applied: {edits_n} edit(s) across {files_n} file(s)");
        }
        if let Some((command, arguments)) = &action.command {
            if let Some((key_s, _uri)) = self.focused_server(ws, files) {
                self.send(
                    key_s,
                    "workspace/executeCommand",
                    serde_json::json!({ "command": command, "arguments": arguments }),
                    LspKind::ExecuteCommand,
                );
                *status = format!("running: {}", action.title);
            }
        }
    }

    /// The working directory, for relativizing picker paths.
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
    pub(crate) fn clear_hover(&mut self) {
        self.hover_panel = None;
    }

    /// Close the completion pum AND clear the latest-request id, so a completion response arriving after the
    /// close is discarded (its id no longer matches `completion_req`).
    fn close_completion(&mut self) {
        self.completion = None;
        self.completion_req = None;
        self.resolve_req = None;
        self.pending_resolve = None;
    }

    /// The completion pum owns the keystream while open: `<C-n>`/`↓` and `<C-p>`/`↑` move (wrapping),
    /// `<CR>`/`<Tab>` accept (replace the typed prefix, stay in Insert), `<Esc>` closes; any OTHER key
    /// closes the menu and returns `false` so the keypress still types. Returns whether the key was consumed.
    pub(crate) fn on_completion_key(
        &mut self,
        key: KeyEvent,
        ws: &mut Workspace,
        status: &mut String,
    ) -> bool {
        let Some(menu) = self.completion.as_mut() else {
            return false;
        };
        let n = menu.items.len().max(1);
        if is_ctrl(key, 'n') || key.code == KeyCode::Down {
            menu.selected = (menu.selected + 1) % n;
            self.pending_resolve = Some(menu.selected);
            return true;
        }
        if is_ctrl(key, 'p') || key.code == KeyCode::Up {
            menu.selected = (menu.selected + n - 1) % n;
            self.pending_resolve = Some(menu.selected);
            return true;
        }
        if key.code == KeyCode::Esc {
            self.close_completion();
            return true;
        }
        if matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
            let item = menu.items[menu.selected].clone();
            let prefix_len = menu.prefix_len;
            self.close_completion();
            let cursor = ws.focused().view.cursor();
            let start = cursor.saturating_sub(prefix_len);
            // A snippet item's `insert` is a snippet body → expand to plain text + the first-tabstop cursor;
            // a plain item inserts literally (cursor after it).
            let (text, caret) = if item.snippet {
                let e = lsp::snippet::expand(&item.insert);
                (e.text, e.cursor)
            } else {
                (item.insert.clone(), item.insert.len())
            };
            // The resolved `additionalTextEdits` (e.g. an auto-import), converted to byte edits against the
            // CURRENT bytes and combined with the main insert into ONE Lsp-origin transaction. Cross-file
            // edits (uri != focused) are dropped this slice. The caret is shifted by imports landing before it.
            let bytes = ws.focused().doc.bytes().to_vec();
            let mut edits = vec![(start, cursor, text)];
            let mut shift: isize = 0;
            for (uri, ledits) in &item.additional {
                if !uri.is_empty() {
                    continue; // additionalTextEdits target the completed (focused) doc; others deferred
                }
                for &((sl, sc), (el, ec), ref t) in ledits {
                    let s = lsp::model::lsp_pos_to_byte(&bytes, sl, sc);
                    let e = lsp::model::lsp_pos_to_byte(&bytes, el, ec);
                    if e <= start {
                        shift += t.len() as isize - (e as isize - s as isize);
                    }
                    edits.push((s, e, t.clone()));
                }
            }
            ws.apply_edits(&edits, TransactionOrigin::Lsp);
            let caret_pos = (start as isize + caret as isize + shift).max(0) as usize;
            ws.place_focused_cursor(caret_pos);
            *status = format!("completed: {}", item.label);
            return true;
        }
        // Live-filter: a word char or backspace edits the buffer AND keeps the pum open — the edit falls
        // through to be typed, and `refilter` triggers a fresh request (at the new revision) in sync_and_poll.
        let word_char = matches!(key.code, KeyCode::Char(c) if c.is_alphanumeric() || c == '_');
        if word_char || key.code == KeyCode::Backspace {
            self.refilter = true;
            return false; // do NOT consume — let the key edit the buffer
        }
        self.close_completion(); // any other key (space, punctuation, …): dismiss, then type it
        false
    }

    /// `<C-x><C-o>` in Vim/Native Insert opens the omni-completion pum (a frontend two-key prefix). Gated
    /// to non-Emacs insert. Returns whether the key was consumed.
    #[allow(clippy::too_many_arguments)] // the omni trigger needs the full key + workspace + LSP context
    pub(crate) fn on_omni_key(
        &mut self,
        key: KeyEvent,
        ws: &Workspace,
        files: &Files,
        snapshot: &[u8],
        revision: Revision,
        emacs_profile: bool,
        status: &mut String,
    ) -> bool {
        if !(matches!(ws.focused().view.mode(), Mode::Insert) && !emacs_profile) {
            return false;
        }
        if self.pending_omni {
            self.pending_omni = false;
            if is_ctrl(key, 'o') {
                self.request_completion(ws, files, snapshot, revision);
                if self.completion_req.is_some() {
                    *status = "completing…".to_string();
                }
                return true;
            }
            // Not `<C-o>`: the swallowed `<C-x>` is dropped; fall through to type this key.
            return false;
        }
        if is_ctrl(key, 'x') {
            self.pending_omni = true;
            return true;
        }
        false
    }

    /// `K` = hover, `<C-]>` = goto-definition (Normal, when the focused buffer has a live server). Returns
    /// whether the key was consumed (both keys fall through when there is no server).
    pub(crate) fn on_normal_key(
        &mut self,
        key: KeyEvent,
        normal: bool,
        ws: &Workspace,
        files: &Files,
        snapshot: &[u8],
    ) -> bool {
        if !(normal && (matches!(key.code, KeyCode::Char('K')) || is_ctrl(key, ']'))) {
            return false;
        }
        if let Some((key_s, uri)) = self.focused_server(ws, files) {
            let (line, ch) = lsp::model::byte_to_lsp_pos(snapshot, ws.focused().view.cursor());
            let hover = matches!(key.code, KeyCode::Char('K'));
            let method = if hover {
                "textDocument/hover"
            } else {
                "textDocument/definition"
            };
            self.send(
                key_s,
                method,
                lsp::protocol::position_params(&uri, line, ch),
                if hover { LspKind::Hover } else { LspKind::Goto },
            );
            return true;
        }
        false
    }

    /// `:fmt` / `:rename {new}` / `:references` / `:codeaction` (F-014). Returns whether the ex line was an
    /// LSP one (handled).
    pub(crate) fn on_ex(
        &mut self,
        ex: &Ex,
        ws: &Workspace,
        files: &Files,
        snapshot: &[u8],
        status: &mut String,
    ) -> bool {
        if !matches!(
            ex,
            Ex::Format | Ex::Rename(_) | Ex::References | Ex::CodeAction
        ) {
            return false;
        }
        let Some((key_s, uri)) = self.focused_server(ws, files) else {
            *status = "no language server for this buffer".to_string();
            return true;
        };
        match ex {
            Ex::Format => {
                self.send(
                    key_s,
                    "textDocument/formatting",
                    lsp::protocol::formatting_params(&uri, 4, true),
                    LspKind::Format,
                );
                *status = "formatting…".to_string();
            }
            Ex::Rename(new_name) => {
                let (line, ch) = lsp::model::byte_to_lsp_pos(snapshot, ws.focused().view.cursor());
                self.send(
                    key_s,
                    "textDocument/rename",
                    lsp::protocol::rename_params(&uri, line, ch, new_name),
                    LspKind::Rename,
                );
                *status = format!("renaming → {new_name}…");
            }
            Ex::References => {
                let (line, ch) = lsp::model::byte_to_lsp_pos(snapshot, ws.focused().view.cursor());
                self.send(
                    key_s,
                    "textDocument/references",
                    lsp::protocol::references_params(&uri, line, ch, true),
                    LspKind::References,
                );
                *status = "finding references…".to_string();
            }
            Ex::CodeAction => {
                let cursor = ws.focused().view.cursor();
                let (line, ch) = lsp::model::byte_to_lsp_pos(snapshot, cursor);
                // Reconstruct the LSP diagnostics overlapping the cursor so the server offers their
                // quickfixes (alongside cursor-position assists/refactors).
                let diags = self.diagnostics.get(&ws.focused_buffer());
                let ctx: Vec<Value> = diags
                    .into_iter()
                    .flatten()
                    .filter(|d| d.start <= cursor && cursor <= d.end)
                    .map(|d| {
                        let (sl, sc) = lsp::model::byte_to_lsp_pos(snapshot, d.start);
                        let (el, ec) = lsp::model::byte_to_lsp_pos(snapshot, d.end);
                        serde_json::json!({
                            "range": {
                                "start": {"line": sl, "character": sc},
                                "end": {"line": el, "character": ec}
                            },
                            "severity": d.severity.to_lsp(),
                            "message": d.message,
                        })
                    })
                    .collect();
                self.send(
                    key_s,
                    "textDocument/codeAction",
                    lsp::protocol::code_action_params(&uri, line, ch, Value::Array(ctx)),
                    LspKind::CodeAction,
                );
                *status = "finding code actions…".to_string();
            }
            _ => unreachable!("guarded by the matches! above"),
        }
        true
    }

    // --- render inputs (getters the loop folds into its render call / overlay chains / poll-gate) ---

    /// The focused buffer's diagnostics (for the underline + `[E:n W:n]` status count).
    pub(crate) fn diagnostics_for(&self, id: DocumentId) -> &[Diag] {
        self.diagnostics.get(&id).map_or(&[][..], Vec::as_slice)
    }

    /// The completion pum view for the renderer: `(items, selected)`.
    pub(crate) fn completion_view(&self) -> Option<(&[lsp::protocol::CompletionItem], usize)> {
        self.completion
            .as_ref()
            .map(|m| (m.items.as_slice(), m.selected))
    }

    /// The hover panel's overlay rows (shares the overlay slot when no picker is open).
    pub(crate) fn hover_overlay(&self) -> Option<Vec<(String, bool)>> {
        self.hover_panel
            .as_ref()
            .map(|lines| lines.iter().map(|l| (l.clone(), false)).collect())
    }

    /// Whether any language server is live (so the loop polls with a timeout for async diagnostics).
    pub(crate) fn has_live_client(&self) -> bool {
        !self.lsp.is_empty()
    }
}

/// Apply a `WorkspaceEdit` (per-file UTF-16 `TextEdit`s) across every affected file, returning
/// `(files, edits)` applied. Each file is focused in turn (reuse an open buffer, else open it), its edits
/// are mapped to byte offsets against ITS OWN bytes, and applied as one `Lsp`-origin undo group; then the
/// original buffer is refocused. Opened files are modified buffers the user saves. Shared by rename and
/// code-action apply. Must run AFTER render (opening a buffer mutates `highlighters`).
fn apply_workspace_edit(
    edit: Vec<(String, Vec<lsp::protocol::LspTextEdit>)>,
    ws: &mut Workspace,
    files: &mut Files,
    highlighters: &mut Highlighters,
) -> (usize, usize) {
    if edit.is_empty() {
        return (0, 0);
    }
    let orig = ws.focused_buffer();
    let (mut files_n, mut edits_n) = (0usize, 0usize);
    for (uri, ledits) in edit {
        let path = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
        let target = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
        let existing = files.iter().find_map(|(id, bf)| {
            (std::fs::canonicalize(&bf.path).ok().as_deref() == Some(target.as_path()))
                .then_some(*id)
        });
        match existing {
            Some(id) => {
                ws.focus_buffer(id);
            }
            None => {
                open_file_into_buffer(&target.display().to_string(), ws, files, highlighters);
            }
        }
        let bytes = ws.focused().doc.bytes().to_vec();
        let byte_edits: Vec<(usize, usize, String)> = ledits
            .into_iter()
            .map(|((sl, sc), (el, ec), text)| {
                (
                    lsp::model::lsp_pos_to_byte(&bytes, sl, sc),
                    lsp::model::lsp_pos_to_byte(&bytes, el, ec),
                    text,
                )
            })
            .collect();
        edits_n += byte_edits.len();
        ws.apply_edits(&byte_edits, TransactionOrigin::Lsp);
        files_n += 1;
    }
    ws.focus_buffer(orig);
    (files_n, edits_n)
}

/// The byte length of the identifier run (`[A-Za-z0-9_]`) immediately before `cur` in `snapshot` — the
/// prefix a completion filters by and the accepted item replaces.
fn identifier_prefix_len(snapshot: &[u8], cur: usize) -> usize {
    let cur = cur.min(snapshot.len());
    snapshot[..cur]
        .iter()
        .rev()
        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_')
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(label: &str) -> lsp::protocol::CompletionItem {
        lsp::protocol::CompletionItem {
            label: label.to_string(),
            insert: label.to_string(),
            detail: None,
            snippet: false,
            documentation: None,
            additional: Vec::new(),
            raw: serde_json::json!({"label": label}),
            resolved: false,
        }
    }

    fn menu(items: &[&str], selected: usize) -> CompletionMenu {
        CompletionMenu {
            items: items.iter().map(|l| item(l)).collect(),
            selected,
            prefix_len: 1,
        }
    }

    /// The live-filter contract (F-014), tested WITHOUT a server: `ingest_completion` applies a response only
    /// when it is the latest request (id) at the current revision, recomputes the prefix, filters, preserves
    /// the selection when the item survives, and closes on an empty prefix / no matches.
    #[test]
    fn ingest_completion_stale_and_refilter_contract() {
        let bytes = b"let wi"; // cursor at 6 → identifier prefix "wi" (len 2)
        let cur = bytes.len();
        let rev = Revision::ZERO;
        let newer = rev.next();
        let result =
            json!({"items": [{"label": "width"}, {"label": "window"}, {"label": "wibble"}]});

        // STALE by request-id: a response whose id isn't the latest is discarded.
        let mut c = LspCoordinator::new(std::path::PathBuf::from("/"));
        c.completion_req = Some(9);
        c.ingest_completion(1, rev, &result, cur, bytes, rev);
        assert!(c.completion.is_none(), "wrong id → discarded");

        // STALE by revision: the buffer moved on since the request → discarded.
        c.completion_req = Some(1);
        c.ingest_completion(1, rev, &result, cur, bytes, newer);
        assert!(c.completion.is_none(), "req_rev != current → discarded");

        // FRESH: latest id + matching revision → open, filtered to the "wi" prefix (width/window/wibble).
        c.completion_req = Some(1);
        c.ingest_completion(1, rev, &result, cur, bytes, rev);
        let m = c.completion.as_ref().expect("fresh response opens the pum");
        assert_eq!(m.items.len(), 3);
        assert_eq!(m.prefix_len, 2);

        // SELECTION PRESERVED across a refilter when the selected label survives.
        c.completion = Some(menu(&["width", "window", "wibble"], 1)); // "window" selected
        c.completion_req = Some(2);
        c.ingest_completion(2, rev, &result, cur, bytes, rev);
        let m = c.completion.as_ref().unwrap();
        assert_eq!(
            m.items[m.selected].label, "window",
            "selection kept by label"
        );

        // EMPTY PREFIX closes the pum (cursor not after an identifier char).
        c.completion = Some(menu(&["width"], 0));
        c.completion_req = Some(3);
        c.ingest_completion(3, rev, &result, 0, b" ", rev);
        assert!(c.completion.is_none(), "empty prefix → closed");
        assert!(
            c.completion_req.is_none(),
            "closing clears the latest-id guard"
        );
    }

    /// `ingest_resolve` merges only for the latest request at the current revision with a valid index; every
    /// stale / out-of-bounds case is a no-op (F-014 resolve contract).
    #[test]
    fn ingest_resolve_stale_index_and_merge() {
        let rev = Revision::ZERO;
        let resolved = json!({"detail": "struct HashMap"});
        let mut c = LspCoordinator::new(std::path::PathBuf::from("/"));
        c.completion = Some(menu(&["HashMap", "HashSet"], 0));
        let detail0 = |c: &LspCoordinator| c.completion.as_ref().unwrap().items[0].detail.clone();

        c.resolve_req = Some(9); // stale id
        c.ingest_resolve(1, rev, 0, &resolved, rev);
        assert_eq!(detail0(&c), None);

        c.resolve_req = Some(1);
        c.ingest_resolve(1, rev, 0, &resolved, rev.next()); // stale revision
        assert_eq!(detail0(&c), None);

        c.ingest_resolve(1, rev, 99, &resolved, rev); // bad index → no panic, no change
        assert_eq!(detail0(&c), None);

        c.ingest_resolve(1, rev, 0, &resolved, rev); // fresh + valid → merged
        assert_eq!(detail0(&c).as_deref(), Some("struct HashMap"));
        assert!(c.completion.as_ref().unwrap().items[0].resolved);
    }
}
