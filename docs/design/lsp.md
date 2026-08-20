# Built-in LSP (F-014) — design

Status: **slices 1–7 landed** (local diagnostics + hover/goto + format + rename + completion + references + code-actions, cross-platform; orchestration in `app/lsp_coordinator.rs`). Owner capabilities:
`CAP-LSP-COORD` (client + normalized model), `CAP-LSP-CODEC` (`DEP-SERDE`); `DEP-LSP-SERVER`. Spec:
`spec/PRD.yaml` F-014.

## Why

`ruse` had tree-sitter highlighting but no language intelligence. F-014 wants a Language Service host that
manages LSP processes and **normalizes their results into one model the UI reads instead of raw protocol**.
Slice 1 delivers local **diagnostics**: spawn a server for the focused file, handshake, keep it in sync, and
render the errors/warnings it reports. It reuses the async seam the terminal work built (child process +
reader thread + `mpsc` + the gated `event::poll` loop) — but over stdio **pipes**, so it is cross-platform.

## The layers (`apps/tui/src/lsp/`)

```
 std::process (pipes)     reader thread              session (main loop)
 ┌───────────────┐  stdout ┌──────────────┐  Value   ┌──────────────────────────────┐
 │ rust-analyzer │────────▶│ codec: frame │──mpsc───▶│ LspClient.poll(): handshake, │
 │  (child)      │◀──stdin─│  + parse     │          │  reply to server requests,   │
 └───────────────┘   LspClient.send        │         │  collect publishDiagnostics  │
                                                       └───────────┬──────────────────┘
                                          protocol::to_diags(bytes) │  (UTF-16 → byte)
                                                       ┌───────────▼──────────────────┐
                                                       │ model: Diag (byte range) —    │
                                                       │  the ONLY thing the UI reads  │
                                                       └───────────────────────────────┘
```

- **`codec.rs`** — JSON-RPC framing (`Content-Length` + body); `spawn_reader` parses frames off the server's
  stdout on a thread (mirrors `pty::spawn_reader`); `write_message` frames outgoing.
- **`protocol.rs`** — the minimal serde types we consume/produce; `to_diags(bytes, params)` converts an LSP
  `publishDiagnostics` into normalized byte-range diagnostics. Raw protocol never leaves `lsp/`.
- **`client.rs`** — `LspClient`: one server process; `spawn` (→ `None` if the binary is missing, so a missing
  server is a silent no-op); the `initialize`→`initialized`→`didOpen` handshake (notifications before ready are
  queued); `poll()` drains messages via the pure `classify`, **replies to server→client requests** so the
  server never blocks, and returns diagnostics; `did_open`/`did_change`; `Drop` = shutdown/exit + reap + join.
- **`model.rs`** — the UI-facing `Diag { start, end, severity, message }` and `lsp_pos_to_byte` (LSP positions
  are UTF-16 line/character — an emoji is 2 units; this walks a line's UTF-16 units to a byte offset).
- **`mod.rs`** — `server_for_ext` (rust → `rust-analyzer`, one process per server) and `path_to_uri`.

## Session integration + render

The session keeps `lsp: HashMap<serverKey, LspClient>` + `lsp_docs: DocumentId → (uri, version, rev)` +
`diagnostics: DocumentId → Vec<Diag>`. Each frame it syncs the **focused** file buffer (spawn once, `didOpen`
once, `didChange` full-document on a revision change), then `poll()`s every client and stores diagnostics whose
uri matches the focused buffer. The poll-gate that keeps the loop responsive for a terminal is extended to
"terminal OR any LSP client" (a slower ~10fps tick when only an LSP is live) so async diagnostics render
without a keypress. Render **underlines** the diagnostic ranges (via `paint_pane`'s new underline set +
`CellStyle.underline` from the terminal slice) and appends an `[E:n W:n]` count to the status line.

## Determinism boundary (F-022)

LSP I/O is external and non-deterministic: it never mutates a `Document`, is not recorded as `Command`s, and
`--replay` ignores it. Diagnostics are presentation state, layered over the buffer, not edits.

## Request-response (slice 2)

`K` (hover) and `<C-]>` (goto-definition) established the request-response infra every later feature reuses:
`LspClient::request(method, params) -> id`; `poll()` returns a `Polled { diagnostics, responses: [(id,
Value)] }`; the session keys a pending map by `(serverKey, id)` to dispatch each reply. `byte_to_lsp_pos`
sends the cursor position; `parse_hover`/`parse_definition` normalize the cross-shape results. Hover renders in
the bottom **overlay-rows panel** (dismissed on the next key); goto jumps the cursor (same file → move; other
file → `open_file_into_buffer` then move), **deferred until after render** because opening a buffer mutates
`highlighters` which the frame's live `spans` borrow forbids.

## Slicing

- **Slice 1 (landed):** local diagnostics for Rust; underline + status count; full-document `didChange`.
- **Slice 2 (landed):** hover (`K`) + goto-definition (`<C-]>`) + the request-response infra.
- **Slice 3 (landed):** format (`:fmt`) — `textDocument/formatting` → `TextEdit[]` → byte ranges →
  `Workspace::apply_edits` (core `EditorState::apply_edits`, one `TransactionOrigin::Lsp` undo group per F-005).
  Verified end-to-end against real rust-analyzer (diagnostics + hover + formatting) in the `#[ignore]` smoke.
- **Slice 4 (landed):** rename (`:rename {new}` / `:rn {new}`) — `textDocument/rename` at the cursor →
  a `WorkspaceEdit` (`parse_workspace_edit` reads both the `changes` map and the `documentChanges` array;
  resource ops are skipped — this slice only rewrites text) whose per-file edits are applied across **every**
  affected buffer. Each file is focused in turn (an already-open buffer is reused, else opened — deferred
  after render, like goto, since opening mutates `highlighters`), its UTF-16 edits are mapped to byte offsets
  against **its own** bytes, and applied as one `TransactionOrigin::Lsp` undo group (per-file undo). Focus is
  restored afterwards; opened files become modified buffers the user saves (`:wa`). The smoke test extends to
  assert a real rust-analyzer rename returns a ≥2-edit `WorkspaceEdit`.
- **Slice 5 (landed):** completion — the first **floating** overlay (a cursor-anchored popup menu / pum, vs
  the bottom-panel overlays). In Vim/Native **Insert** mode `<C-x><C-o>` (authentic Vim omni; a frontend
  two-key prefix `pending_omni`, gated to non-Emacs) requests `textDocument/completion`; `parse_completion`
  normalizes both `CompletionList`/`CompletionItem[]` (insert = `textEdit.newText` → `insertText` → `label`,
  snippet items fall back to the clean `label`). The pum is drawn into the cell grid at the cursor
  (`cursor_cell` anchor, flips above when it would overflow) — the existing diff emits it and repaints the
  covered cells on dismiss. `<C-n>/<C-p>`/`↓↑` move, `<CR>/<Tab>` accept (a single edit replacing the typed
  identifier prefix via `Workspace::apply_edits`, staying in Insert), `<Esc>`/typing dismisses. Verified
  end-to-end (the `live_lsp_pipeline` smoke asserts ≥1 real item).
- **Slice 6 (landed):** references (`:references` / `:refs` / `:ref`) — `textDocument/references` at the cursor
  → `parse_locations` (every `Location`/`LocationLink`; `parse_definition` is now `parse_locations(..).next()`)
  → a **references picker** (`ui/ref_picker.rs`, a `Picker<(uri,line,char)>` reusing the F-004/F-013 overlay
  infra) listing `relpath:line:col`. Enter jumps to the selected location (same file → move; other file →
  `open_file_into_buffer` then move — the goto path, run INLINE in the picker's accept since the `spans` borrow
  is already released post-render); Esc closes. The picker is OPENED after render (setting it mid-frame would
  clash with `cmd_line`'s borrow — deferred via `pending_refs`, like `goto_jump`). Verified end-to-end.
- **Slice 7 (landed):** code-actions (`:codeaction` / `:ca`) — `textDocument/codeAction` at the cursor with a
  reconstructed `context.diagnostics` (the normalized `Diag`s overlapping the cursor, round-tripped to LSP via
  `Severity::to_lsp` + `byte_to_lsp_pos`, so the server offers their quickfixes alongside assists) →
  `parse_code_actions` (edit-bearing actions only; command-only actions that need `workspace/executeCommand`
  are dropped for now) → an **action picker** (`ui/action_picker.rs`, a `Picker<CodeAction>`) listing titles;
  Enter applies the selected action's `WorkspaceEdit` via the shared multi-file `apply_workspace_edit` (the
  same path as rename). The live smoke asserts the request/response wire round-trips.
- **Coordinator (2026-08-20, #306):** all app-side LSP orchestration now lives in `app/lsp_coordinator.rs`
  (`LspCoordinator`) — the session loop just calls its methods. `lsp/` stays the pure client. Future LSP work
  goes in the coordinator.
- **Diagnostics list (landed):** `:diagnostics` / `:diags` / `:diag` opens a picker (`ui/diag_picker.rs`) over
  the FOCUSED buffer's collected diagnostics — rows `line:col [E/W/I/H] message`; Enter jumps to the
  diagnostic's byte offset. No server round-trip (reads the already-stored model). *Workspace-wide* (all open
  buffers) needs a per-buffer bytes API to map each publish's UTF-16 ranges → that buffer's bytes (the
  coordinator only has the focused snapshot today) — deferred, as is multi-SOURCE merge (LSP + tree-sitter +
  compiler) by namespace.
- **Completion live-filter (landed):** while the pum is open, a word char / `<C-Backspace>` edits the buffer
  AND keeps the pum open — the coordinator re-requests completion once the edit syncs (`refilter` flag →
  `request_completion` in `sync_and_poll`). A response is applied only when it is the LATEST request
  (`completion_req` id) at the CURRENT revision (`LspKind::Completion(Revision)`) — stale/out-of-order
  responses are discarded; the pum refreshes preserving the selected item by label; an empty prefix / no
  matches closes it. `ingest_completion` is the pure core (mock-tested, no server); `isIncomplete` is moot
  (we re-request on every input change). Non-word keys still dismiss.
- **Later:** command-only code actions (`workspace/executeCommand`); snippet placeholder expansion;
  `completionItem/resolve` lazy docs + a docs side-panel; signature help; trigger-character auto-popup; a
  source-preview line in the references picker; a floating name-input prompt + resource-op renames; a jumplist
  for `<C-o>` back-navigation; merge LSP with tree-sitter/compiler diagnostics by namespace; a diagnostics list
  / quickfix UI; more languages (config-driven); incremental `didChange`; remote (C-AGENT); `C-SCHEDULER`.

## Verification

Pure unit tests are the CI coverage (a real server is not assumed present): codec frame round-trip + back-to-back
frames; `lsp_pos_to_byte` for ASCII / multibyte / astral (surrogate-pair) columns; `publishDiagnostics` →
byte-range `Diag`; the `classify` handshake/dispatch cases. Manual: open a `.rs` file with a real error under
`rust-analyzer` — the range underlines and the status shows `E:1`; fix it and it clears (proves `didChange`).
