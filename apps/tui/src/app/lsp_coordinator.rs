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
    /// A completion request; carries the byte length of the identifier prefix already typed before the
    /// cursor, so the accepted item replaces exactly that prefix (F-014 #5).
    Completion(usize),
    /// A references request; the response's locations are stashed for the loop to open its picker.
    References,
    /// A code-action request; the response's actions are stashed for the loop to open its picker.
    CodeAction,
}

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
    // Deferred results — set while polling (inside the frame's `spans` borrow) and drained AFTER render.
    goto_jump: Option<(String, u32, u32)>,
    pending_edits: Vec<(usize, usize, String)>,
    pending_rename: Vec<(String, Vec<lsp::protocol::LspTextEdit>)>,
    pending_refs: Option<Vec<(String, u32, u32)>>,
    pending_actions: Option<Vec<lsp::protocol::CodeAction>>,
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
            goto_jump: None,
            pending_edits: Vec::new(),
            pending_rename: Vec::new(),
            pending_refs: None,
            pending_actions: None,
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

    /// Send a request to `key`'s client and record its pending kind. Silent no-op if the client is gone.
    fn send(&mut self, key: &str, method: &str, params: Value, kind: LspKind) {
        if let Some(client) = self.lsp.get_mut(key) {
            let rid = client.request(method, params);
            self.pending.insert((key.to_string(), rid), kind);
        }
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
                    // Completion: open the pum from the parsed items, tightened to the typed prefix (RA
                    // already filters; this drops any stragglers). Empty → a status note, no menu.
                    Some(LspKind::Completion(prefix_len)) => {
                        let cur = ws.focused().view.cursor().min(snapshot.len());
                        let prefix =
                            String::from_utf8_lossy(&snapshot[cur.saturating_sub(prefix_len)..cur])
                                .to_lowercase();
                        let items: Vec<_> = lsp::protocol::parse_completion(&result)
                            .into_iter()
                            .filter(|it| it.label.to_lowercase().starts_with(&prefix))
                            .collect();
                        if items.is_empty() {
                            *status = "no completions".to_string();
                        } else {
                            self.completion = Some(CompletionMenu {
                                items,
                                selected: 0,
                                prefix_len,
                            });
                        }
                    }
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
    }

    /// The references locations resolved this frame (the loop opens its generic picker from these).
    pub(crate) fn take_refs(&mut self) -> Option<Vec<(String, u32, u32)>> {
        self.pending_refs.take()
    }

    /// The code actions resolved this frame (the loop opens its generic picker from these).
    pub(crate) fn take_actions(&mut self) -> Option<Vec<lsp::protocol::CodeAction>> {
        self.pending_actions.take()
    }

    /// Apply a selected code action's `WorkspaceEdit` across every affected file (the same multi-file apply
    /// as rename). Called from the action picker's accept, where the `spans` borrow is already released.
    pub(crate) fn apply_code_action(
        &self,
        edit: Vec<(String, Vec<lsp::protocol::LspTextEdit>)>,
        ws: &mut Workspace,
        files: &mut Files,
        highlighters: &mut Highlighters,
        status: &mut String,
    ) {
        let (files_n, edits_n) = apply_workspace_edit(edit, ws, files, highlighters);
        *status = format!("applied: {edits_n} edit(s) across {files_n} file(s)");
    }

    /// The working directory, for relativizing picker paths.
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Any key dismisses a shown hover panel (a fresh `K` re-populates it after its response).
    pub(crate) fn clear_hover(&mut self) {
        self.hover_panel = None;
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
            return true;
        }
        if is_ctrl(key, 'p') || key.code == KeyCode::Up {
            menu.selected = (menu.selected + n - 1) % n;
            return true;
        }
        if key.code == KeyCode::Esc {
            self.completion = None;
            return true;
        }
        if matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
            let item = menu.items[menu.selected].clone();
            let prefix_len = menu.prefix_len;
            self.completion = None;
            let cursor = ws.focused().view.cursor();
            let start = cursor.saturating_sub(prefix_len);
            ws.apply_edits(
                &[(start, cursor, item.insert.clone())],
                TransactionOrigin::Lsp,
            );
            ws.place_focused_cursor(start + item.insert.len());
            *status = format!("completed: {}", item.label);
            return true;
        }
        self.completion = None; // any other key: dismiss, then fall through to type it
        false
    }

    /// `<C-x><C-o>` in Vim/Native Insert opens the omni-completion pum (a frontend two-key prefix). Gated
    /// to non-Emacs insert. Returns whether the key was consumed.
    pub(crate) fn on_omni_key(
        &mut self,
        key: KeyEvent,
        ws: &Workspace,
        files: &Files,
        snapshot: &[u8],
        emacs_profile: bool,
        status: &mut String,
    ) -> bool {
        if !(matches!(ws.focused().view.mode(), Mode::Insert) && !emacs_profile) {
            return false;
        }
        if self.pending_omni {
            self.pending_omni = false;
            if is_ctrl(key, 'o') {
                if let Some((key_s, uri)) = self.focused_server(ws, files) {
                    let cursor = ws.focused().view.cursor().min(snapshot.len());
                    // The identifier prefix already typed before the cursor (bytes to replace).
                    let prefix_len = snapshot[..cursor]
                        .iter()
                        .rev()
                        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'_')
                        .count();
                    let (line, ch) = lsp::model::byte_to_lsp_pos(snapshot, cursor);
                    self.send(
                        key_s,
                        "textDocument/completion",
                        lsp::protocol::completion_params(&uri, line, ch),
                        LspKind::Completion(prefix_len),
                    );
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
