# Built-in LSP (F-014) — design

Status: **slice 1 landed** (local diagnostics, cross-platform). Owner capabilities: `CAP-LSP-COORD` (client +
normalized model), `CAP-LSP-CODEC` (`DEP-SERDE`); `DEP-LSP-SERVER`. Spec: `spec/PRD.yaml` F-014.

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

## Slicing

- **Slice 1 (this):** local diagnostics for Rust; underline + status count; full-document `didChange`.
- **Later:** hover / definition / rename / format / completion (request-response); merge LSP with
  tree-sitter/compiler diagnostics by namespace; a diagnostics list / quickfix UI; more languages
  (config-driven); incremental `didChange`; remote servers (C-AGENT); `C-SCHEDULER` integration.

## Verification

Pure unit tests are the CI coverage (a real server is not assumed present): codec frame round-trip + back-to-back
frames; `lsp_pos_to_byte` for ASCII / multibyte / astral (surrogate-pair) columns; `publishDiagnostics` →
byte-range `Diag`; the `classify` handshake/dispatch cases. Manual: open a `.rs` file with a real error under
`rust-analyzer` — the range underlines and the status shows `E:1`; fix it and it clears (proves `didChange`).
