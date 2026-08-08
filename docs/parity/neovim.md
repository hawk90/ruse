---
doc: parity-neovim
project: ruse
title: "Parity: Neovim Additions"
summary: >
  Feature-parity target set for what Neovim adds over Vim: embedded Lua, built-in LSP,
  Tree-sitter, diagnostics, extmarks/namespaces, MessagePack-RPC API, external UI protocol,
  jobs/channels/libuv, :terminal, floating windows, the fast-event/textlock model, providers,
  ShaDa. Grounded in neovim.io/doc. Focus: the client-observable guarantees ruse must re-provide.
audience: [maintainers, contributors, llm-agents]
status: draft
source_of_truth: false
verified_against_upstream: false
sources_root: https://neovim.io/doc/user/
related:
  - README.md
  - vim.md
  - ../architecture/architecture.md
  - ../anti-patterns/anti-patterns.md
---

# Parity: Neovim Additions

> **⚠️ NOT THE SOURCE OF TRUTH (D-043).** This file is hand-authored and has never been checked
> against a pinned upstream revision. The parity source is the machine-derived census in
> [`spec/parity/inventory/`](../../spec/parity/inventory/), generated from the SHA pins in
> [`spec/parity/upstreams.yaml`](../../spec/parity/upstreams.yaml). These tables survive as *human
> annotation* — reading, grouping and intent — and are migrating onto census IDs. Do not add rows
> here to record a newly discovered upstream feature: humans classify, they do not enumerate.


Classic Vim's editing language is in [vim.md](vim.md). This file covers what **Neovim adds** and, more
importantly, the **client/plugin-observable guarantees** ruse must reproduce with its *own* stable API
(ruse does not target running Lua plugins — L3 — but it does target the *capabilities* Neovim's Lua API
delivers). Neovim's charter — "an API-first, RPC-controllable editor," "the RPC API in particular should
never break" — is exactly ruse's [Semantic Command Layer + versioned plugin protocol](../architecture/architecture.md) thesis, arrived at independently.

## NVIM-DESIGN — Foundational divergence
Neovim = API-first; the built-in TUI is just one API client; the RPC API is a versioned, additive-only
stability contract. **ruse mirrors this** via the Semantic Command Layer (architecture §2) and a
versioned plugin protocol (architecture §4.3). Parity target: the *architecture property*, not the wire format.

## NVIM-LSP — Built-in LSP client
Source: `lsp.txt`.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| NVIM-LSP-1 | In-core LSP client (no plugin needed): start/config/enable, stdio+rpc transport | L1 | Equivalent | high |
| NVIM-LSP-2 | `buf.*`: hover, definition, references, rename, code_action, format, signature_help, document_symbol, calls | L1 | Equivalent | high |
| NVIM-LSP-3 | LSP-driven completion; inlay hints (as decorations); semantic tokens (`@lsp.*`) | L1 | Equivalent | med |
| NVIM-LSP-4 | Composable autocmds: `LspAttach/Detach/Progress/Request/TokenUpdate` | L1 | Equivalent | med |
| NVIM-LSP-5 | Diagnostics via publishDiagnostics → diagnostics framework | L1 | Equivalent | high |

Design note: the value is that the client is **in-core so plugins compose rather than replace it**. ruse
exposes LSP through the stable plugin API (architecture §4.2), never as a per-plugin reimplementation
(guards PERF-16, "duplicate per-plugin LSP processes").

## NVIM-TS — Tree-sitter
Source: `treesitter.txt`.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| NVIM-TS-1 | Incremental concrete-syntax parsing (C tree-sitter), replacing regex `syntax` | L1 | Equivalent | high |
| NVIM-TS-2 | Structural highlighting via `highlights.scm` capture groups, applied as decorations at priority | L1 | Equivalent | high |
| NVIM-TS-3 | Language injections (`injections.scm`: content/language, combined/include-children) | L1 | Equivalent | med |
| NVIM-TS-4 | Query API (`iter_captures/iter_matches`, predicates `eq?/match?`, directives `set!/offset!`) | L2 | Equivalent | med |
| NVIM-TS-5 | Tree-sitter folds and indent; `LanguageTree` incremental re-parse | L1 | Equivalent | med |

Design note: the parser must read a **snapshot**, not the live buffer (guards TEXT-16), and re-parse
incrementally, not per-keystroke full-parse (guards PERF-3).

## NVIM-DIAG — Diagnostics framework
Source: `diagnostic.txt`.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| NVIM-DIAG-1 | Producer/consumer split: any source (LSP/linter/compiler) sets diagnostics by **namespace** | L1 | Equivalent | high |
| NVIM-DIAG-2 | Item model: `lnum/col/end_*` (0-indexed), message, severity, source, code | L1 | Equivalent | med |
| NVIM-DIAG-3 | Pluggable render handlers: virtual_text, virtual_lines, signs, underline | L1 | Adapted | med |
| NVIM-DIAG-4 | Navigation + quickfix/loclist population; cascading config (ephemeral > ns > global) | L1 | Equivalent | med |

In ruse this surfaces as a **Problems buffer** (see [workspace.md](workspace.md)).

## NVIM-EXT — Extmarks & namespaces (the anchor/decoration model)
Source: `api.txt` (extmarks). **This is the single most important Neovim primitive to reproduce.**

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| NVIM-EXT-1 | Persistent, gravity-aware buffer positions grouped by **namespace**, auto-tracking edits | L1 | Equivalent | high |
| NVIM-EXT-2 | Range highlights (`hl_group`, `end_row/col`, `priority`, `hl_mode` replace/combine/blend) | L1 | Equivalent | high |
| NVIM-EXT-3 | Virtual text: `virt_text_pos` = eol/overlay/right_align/**inline** (inlay hints) | L1 | Equivalent | med |
| NVIM-EXT-4 | Virtual lines (block hints, inline diffs) | L1 | Equivalent | med |
| NVIM-EXT-5 | **Signs as extmarks** (`sign_text`, `line_hl_group`, `number_hl_group`) — unified sign column | L1 | Equivalent | med |
| NVIM-EXT-6 | Gravity (`right_gravity`, `end_right_gravity`), `invalidate`, `ephemeral`, `conceal`, `url` | L1 | Equivalent | med |
| NVIM-EXT-7 | Decoration providers (`set_decoration_provider` on_start/on_win/on_line) for per-redraw marks | L2 | Adapted | med |

**Client-observable guarantee to reproduce:** *extmark stability* — anchors survive edits with defined
gravity so decorations don't drift; namespaces isolate producers. This is ruse's **anchor model** (TEXT-4,
TEXT-5, TEXT-6; architecture §3.2). Anchor updates must not be O(anchors × edits) (guards PERF-6).

> **Decoration providers (NVIM-EXT-7, verification V-26):** Neovim runs a plugin callback synchronously per
> visible line during redraw, which would put plugin code inside the paint pass — conflicting with
> INV-QUERY-SNAPSHOT / INV-PLUGIN-NO-CORE. ruse instead exposes a **bounded, visible-range,
> snapshot-scoped** decoration-provider API that returns ephemeral marks and runs *outside* the paint
> critical section. Target L2.

## NVIM-RPC — MessagePack-RPC API (auto-generated)
Source: `api.txt`.
- Entire public API auto-exported as RPC methods with machine-readable metadata (`version`, `api_level`,
  `functions`, `ui_events`, `error_types`), discoverable via `nvim_get_api_info`/`--api-info`.
- Buffer/Window/Tabpage are opaque, type-discriminated EXT handles.
- **ruse equivalent:** the Semantic Command Layer is the stable surface; command/plugin protocol metadata
  is generated (architecture §2.3 "command metadata generates docs and autocompletion"; guards CMD-20).
- Target: **the property** (stable, versioned, introspectable, additive-only) at L1; not the msgpack wire format.

## NVIM-UI — External UI protocol
Source: `ui.txt`, `api-ui-events.txt`.
- Any process becomes the display by `nvim_ui_attach(w,h,opts)` and rendering batched `redraw` events;
  paint only on `flush`.
- `ext_linegrid` events: `grid_resize/grid_line/grid_scroll/grid_cursor_goto/grid_clear/hl_attr_define/
  default_colors_set/mode_change`. `ext_multigrid`: per-window grids (`win_pos/win_float_pos/win_hide`).
- Semantic externalization options: `ext_cmdline/popupmenu/tabline/messages/hlstate/termcolors`.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| NVIM-UI-1 | External/remote UI attach + render a cell grid | L1 · **multiple clients: post-MVP (D-012)**, each client-view pins its own render tier (V-13) | Equivalent | med |
| NVIM-UI-2 | Batched redraw with **flush** boundary; ordered events; stable grid ids | L1 | Equivalent | high |
| NVIM-UI-3 | Semantic externalization of cmdline/popupmenu/messages (not drawn into the grid) | L1 | Adapted | med |

**Guarantee to reproduce:** nothing paints before flush; events applied in batch order. This validates
ruse's **semantic view model** + synchronized output (UI-10, architecture §6.2, §7). ruse generalizes
`ext_*` into "everything is a semantic view," so TUI/GUI/Web share one command+view system (guards UI-11, PLUGIN-11).

## NVIM-JOB — Jobs, channels, libuv loop
Source: `channel.txt`.
- Four channel kinds: stdio (`--headless`), job (`jobstart`), PTY, socket. `chansend`, `rpcnotify/rpcrequest`,
  `jobresize`, `vim.system`, `vim.uv`.
- Channels carry raw bytes or msgpack-RPC.
- **ruse note:** jobs/channels are core infra (architecture §5, §8), single-loop and async; do not bolt on later (guards CORE-19).

## NVIM-TERM — `:terminal`
Source: `terminal.txt`. Full libvterm emulator inside an ordinary buffer; Terminal-mode; `TermOpen/Enter/
Leave/Close/Request` autocmds; `b:channel` for input. In ruse this is a **PTY-backed workspace buffer**
([workspace.md](workspace.md)); needs ConPTY handling on Windows (see [terminal.md](terminal.md)).

## NVIM-AU — Autocommands (Lua API + new events)
Source: `autocmd.txt`. `nvim_create_autocmd` with structured event objects; Neovim-notable events:
`TextYankPost`, `RecordingEnter/Leave`, `DirChanged`, `ModeChanged`, `Term*`, `UIEnter/Leave`, `Chan*`,
`WinScrolled/Resized/Closed`, `SearchWrapped`, `Signal`, `Vim{Resume,Suspend}`, plus subsystem events
`LspAttach`, `DiagnosticChanged`. ruse: typed event model with an ordering contract (architecture §8; guards ASYNC-5, ASYNC-18).

## NVIM-WIN — Floating windows & handle model
Source: `api.txt`. `nvim_open_win` (relative editor/win/cursor/mouse, anchor, border, zindex, title, style)
powers hover/completion/diagnostic popups. Buffer/Window/Tabpage = opaque stable handles (`0`=current).
ruse: typed handles, not long-lived references (guards CORE-14).

## NVIM-ASYNC — Event loop, textlock, fast-event model (architecture-critical)
Source: `api.txt` (api-fast), `luvref.txt`.
- Single-threaded over libuv. Async producers (RPC, uv callbacks, timers, job output) arrive in a
  restricted **fast** context that may touch Lua state but **not editor state** (textlock).
- `vim.in_fast_event()` detects it; `vim.schedule(fn)` / `vim.schedule_wrap` / `vim.defer_fn` bridge back
  to the main loop; only `api-fast`-flagged functions are legal in fast context.
- Per-channel request ordering is preserved; `rpcrequest` is blocking/ordered, `rpcnotify` is fire-and-forget.

**This is the crux Neovim learned the hard way and ruse designs for up front:** preserve observable
ordering with a **single-threaded deterministic executor**; never funnel arbitrary async events into the
state machine; carry request-id + revision and drop stale results (architecture §8; guards ASYNC-1/6/9/17).
Parity target: the *safety property and ordering guarantee*, at L1.

## NVIM-PROV — Provider model
Source: `provider.txt`. Optional external programs supply clipboard/python/node/ruby/perl; remote plugins
run as separate host processes over RPC. ruse: capability/permission-gated external providers (architecture
§4.4, §10); OSC 52 clipboard fallback (see [terminal.md](terminal.md) TERM-OSC52).

## NVIM-SHADA — ShaDa (shared data)
Source: `starting.txt#shada`. MessagePack, **mergeable across concurrent instances**, forward-compatible,
XDG-located; stores history, registers, marks, search patterns, buffer list, globals. ruse: versioned,
mergeable session store; keep encoding/state separate from document data (guards TEXT-19).

## NVIM-LUA — Vimscript ↔ Lua coexistence
Source: `lua.txt`. Full Vimscript retained; Lua layered alongside with bidirectional interop (`vim.cmd`,
`vim.fn`, `v:lua`, `luaeval`). **Out of scope for ruse** (L3) — ruse's extension story is its own stable
protocol, not embedded Lua/Vimscript. Recorded so the parity gap is explicit.

---

## Client/Plugin-Observable Guarantees ruse Must Re-provide (dependency surface)

| Guarantee | Neovim origin | ruse mechanism | Anti-patterns guarded |
| --- | --- | --- | --- |
| Stable, versioned, additive-only extension API | RPC design goal | Semantic Command Layer + versioned protocol | ECO-1/2/3, CMD-6 |
| Introspectable API metadata (codegen clients) | `nvim_get_api_info` | generated command/protocol metadata | CMD-20 |
| Per-channel request ordering; sync vs async calls | event loop | deterministic executor; typed request/response | ASYNC-3/5 |
| Nothing paints before flush; ordered redraw; stable grid ids | UI protocol | semantic view model + synchronized output | UI-10, TERMOUT-9 |
| Extmark stability + gravity + namespace isolation | extmarks | anchor model with affinity + namespaces | TEXT-4/5/6, PERF-6 |
| Fast-context/textlock contract + schedule bridge | api-fast | single-threaded deterministic executor + revisions | ASYNC-1/6/9/15/17 |
| Opaque, stable, typed handles | Buffer/Window/Tabpage EXT | typed handles / stable IDs | CORE-14 |
