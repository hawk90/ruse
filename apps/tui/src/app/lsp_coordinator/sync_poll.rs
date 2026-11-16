//! The per-frame LSP tick (`LspCoordinator::sync_and_poll`), split by phase: sync the focused buffer to its
//! server, drain every client (diagnostics, request responses, server `workspace/applyEdit`s), then apply
//! the responses that need `&mut self` once the clients borrow is released. The phase order is load-bearing:
//! the sync phase's didChange precedes the poll, and the completion ingest precedes the live-filter
//! re-request, which precedes the resolve merge and the lazy resolve request.

use std::path::Path;

use ruse_core::{BytePos, DocumentId, Revision, Workspace};
use serde_json::Value;

use super::{LspCoordinator, LspKind, UNTRUSTED_NOTICE};
use crate::app::dispatch::Files;
use crate::app::messages::Status;
use crate::lsp::{self, LspClient};

/// The responses drained during the poll whose ingest is `&mut self` (`ingest_completion`,
/// `ingest_resolve`), stashed until the drain is done.
#[derive(Default)]
struct Drained {
    completions: Vec<(i64, Revision, Value)>,
    resolves: Vec<(i64, Revision, usize, Value)>,
}

impl LspCoordinator {
    /// Keep the language server in sync with the FOCUSED file buffer (spawn once per server, didOpen once
    /// per buffer, didChange on a revision change), then poll every client: apply the focused buffer's
    /// diagnostics and dispatch request responses. Missing servers / non-code buffers are silent no-ops.
    pub(crate) fn sync_and_poll(
        &mut self,
        ws: &Workspace,
        files: &Files,
        revision: Revision,
        snapshot: &[u8],
        status: &mut Status,
    ) {
        let id = ws.focused_buffer();
        let focused_uri = self.sync_focused(id, files, revision, snapshot, status);
        // Diagnostics match by DECODED path, not URI string: servers may re-encode a URI differently (hex
        // case, which reserved chars they escape) than we sent it.
        let focused_path = focused_uri.as_deref().map(lsp::uri_to_path);
        let drained = self.poll_clients(id, focused_path.as_deref(), snapshot, status);
        self.apply_drained(ws, files, revision, snapshot, drained);
    }

    /// The sync phase: spawn the focused buffer's server once (trust-gated), then didOpen the buffer once
    /// or didChange it on a revision change. Returns the URI the server knows the buffer by, if any.
    fn sync_focused(
        &mut self,
        id: DocumentId,
        files: &Files,
        revision: Revision,
        snapshot: &[u8],
        status: &mut Status,
    ) -> Option<String> {
        let bf = files.get(&id)?;
        let (key, cmd, lang) = bf
            .path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(lsp::server_for_ext)?;
        // INV-TRUST-1 / D-058: never spawn a server (which runs workspace code) in an untrusted
        // workspace. Say why once, instead of failing silently.
        if !self.trusted {
            if !self.trust_notice_shown {
                self.trust_notice_shown = true;
                status.warn(UNTRUSTED_NOTICE);
            }
            return None;
        }
        // Spawn the server once (track attempts so a missing binary is not retried each frame).
        if !self.lsp.contains_key(key) && self.lsp_tried.insert(key.to_string()) {
            if let Some(c) = LspClient::spawn(cmd, &self.root_uri) {
                self.lsp.insert(key.to_string(), c);
            }
        }
        let client = self.lsp.get_mut(key)?;
        // The server sees the file's text: without `'endofline'` the buffer's final newline is not part
        // of it (nvim's `_buf_get_full_text`, RFC-0024 §3).
        let file_text = match snapshot.strip_suffix(b"\n") {
            Some(body) if !bf.fmt.final_newline => body,
            _ => snapshot,
        };
        let text = String::from_utf8_lossy(file_text);
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
    }

    /// The drain phase: poll every client and, per client in this order, apply its diagnostics for the
    /// focused buffer, dispatch its responses by their pending `(serverKey, id)`, and stash its server
    /// `workspace/applyEdit`s. The client map is taken out for the loop and put back unchanged, so the
    /// handlers can take `&mut self`; none of them touches `self.lsp`.
    fn poll_clients(
        &mut self,
        id: DocumentId,
        focused_path: Option<&Path>,
        snapshot: &[u8],
        status: &mut Status,
    ) -> Drained {
        let mut drained = Drained::default();
        let mut clients = std::mem::take(&mut self.lsp);
        for (key, client) in clients.iter_mut() {
            let polled = client.poll();
            for params in polled.diagnostics {
                if focused_path.is_some()
                    && Some(lsp::uri_to_path(&params.uri).as_path()) == focused_path
                {
                    self.diagnostics
                        .insert(id, lsp::protocol::to_diags(snapshot, &params));
                }
            }
            for (rid, result) in polled.responses {
                self.on_response(key, rid, result, snapshot, status, &mut drained);
            }
            // Server `workspace/applyEdit` requests: stash (key, reply-id, parsed edit) to apply + reply
            // after render (applying needs `&mut ws`).
            for (reply_id, params) in polled.apply_edits {
                let edit = params
                    .get("edit")
                    .map(lsp::protocol::parse_workspace_edit)
                    .unwrap_or_default();
                self.pending_server_edits
                    .push((key.clone(), reply_id, edit));
            }
        }
        self.lsp = clients;
        drained
    }

    /// Dispatch one response by the pending request it answers (removing it). Results that open a buffer
    /// or edit are stashed for after render; completion and resolve go to `drained` for `apply_drained`.
    fn on_response(
        &mut self,
        key: &str,
        rid: i64,
        result: Value,
        snapshot: &[u8],
        status: &mut Status,
        drained: &mut Drained,
    ) {
        match self.pending.remove(&(key.to_string(), rid)) {
            Some(LspKind::Hover) => self.hover_panel = lsp::protocol::parse_hover(&result),
            // Defer the jump until AFTER render — it may open a buffer (mutating `highlighters`), which
            // the frame's live `spans` borrow forbids here.
            Some(LspKind::Goto) => self.goto_jump = lsp::protocol::parse_definition(&result),
            // Convert TextEdits (UTF-16 ranges) to byte ranges; applied after render.
            Some(LspKind::Format) => {
                self.pending_edits = lsp::protocol::parse_text_edits(&result)
                    .into_iter()
                    .map(|((sl, sc), (el, ec), text)| {
                        (
                            BytePos(lsp::model::lsp_pos_to_byte(snapshot, sl, sc)),
                            BytePos(lsp::model::lsp_pos_to_byte(snapshot, el, ec)),
                            text,
                        )
                    })
                    .collect();
            }
            // A WorkspaceEdit spans files; keep the raw per-file UTF-16 edits and convert each against
            // its own bytes when applied after render (offsets are per-document).
            Some(LspKind::Rename) => {
                self.pending_rename = lsp::protocol::parse_workspace_edit(&result);
            }
            // Completion: stash for after the drain (ingested against the post-drain cursor).
            Some(LspKind::Completion(req_rev)) => {
                drained.completions.push((rid, req_rev, result));
            }
            // Resolve: stash for after the drain (merged after the live-filter re-request).
            Some(LspKind::Resolve(req_rev, idx)) => {
                drained.resolves.push((rid, req_rev, idx, result));
            }
            // executeCommand result is ignored — the effect returns via `workspace/applyEdit`.
            Some(LspKind::ExecuteCommand) => {}
            // References: stash the locations; the loop opens its picker after render.
            Some(LspKind::References) => {
                let locs = lsp::protocol::parse_locations(&result);
                if locs.is_empty() {
                    status.echo("no references");
                } else {
                    status.echo(format!("{} reference(s)", locs.len()));
                    self.pending_refs = Some(locs);
                }
            }
            // Code actions: stash the edit-bearing actions; the loop opens its picker after render.
            Some(LspKind::CodeAction) => {
                let actions = lsp::protocol::parse_code_actions(&result);
                if actions.is_empty() {
                    status.echo("no code actions");
                } else {
                    status.echo(format!("{} code action(s)", actions.len()));
                    self.pending_actions = Some(actions);
                }
            }
            None => {}
        }
    }

    /// The apply phase, after the drain: ingest completion responses, re-request completion for a
    /// live-filter keystroke, merge resolve responses, then send the lazy resolve for the selection.
    fn apply_drained(
        &mut self,
        ws: &Workspace,
        files: &Files,
        revision: Revision,
        snapshot: &[u8],
        drained: Drained,
    ) {
        let cur = ws.focused().view.cursor().0.min(snapshot.len());
        for (rid, req_rev, result) in drained.completions {
            self.ingest_completion(rid, req_rev, &result, cur, snapshot, revision);
        }
        // Live-filter: a word char / backspace edited the buffer while the pum was open — re-request now the
        // edit is synced (the sync phase's didChange bumped the revision). One request per keystroke; stale
        // responses are dropped by the completion_req id + revision guard in ingest_completion.
        if self.refilter {
            self.refilter = false;
            if self.completion.is_some() {
                self.request_completion(ws, files, snapshot, revision);
            }
        }
        // Merge resolve responses (detail/documentation/additionalTextEdits) into the pum item.
        for (rid, req_rev, idx, result) in drained.resolves {
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
}
