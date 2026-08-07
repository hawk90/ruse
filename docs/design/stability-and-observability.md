---
doc: stability-and-observability
project: ruse
title: "ruse Stability & Observability Model"
summary: >
  How ruse classifies, propagates, isolates, and records failure — and how it exposes live state.
  Covers assert-vs-error separation, typed errors + stable error codes, context chains, structured
  logging, transaction/correlation IDs, isolation boundaries, fail-fast vs graceful degradation,
  panic policy, supervisors, diagnostic bundles, and the log/status/metric/trace/diagnostic split.
  In system software, the priority is not "never fail" but "when it fails, know exactly where/why
  and recover safely." Failure paths are designed as first-class as success paths.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - ../invariants/reference-invariants.md
  - ../anti-patterns/anti-patterns.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Stability & Observability Model

> GoF patterns describe *structure*; in system software what matters more is: **it fails, but you
> immediately know where and why, and it recovers safely.** In an editor where core, plugins, remote,
> terminal, and LSP interleave, errors are unavoidable — so classification, propagation, isolation, and
> recording must exist from day one. **Failure paths are designed as first-class as success paths.**

Design ordering (before reaching for any GoF pattern):

```
Invariants → Ownership → Error model → Failure boundaries → Observability → Recovery → (then patterns)
```

Patterns (Command, State, Observer, Strategy, Adapter, Supervisor) emerge where needed — do not pick
pattern names first; that is over-design.

## v0 scope — the two-crate editor ([D-040](../../spec/DECISIONS.md))

This model was written for the *full* architecture (plugins, LSP, remote runtime, terminal jobs, a service
supervisor, a multi-component Health Registry). RFC-0012 collapsed ruse to a **two-crate editor**
(`editor-core` + the `ruse` TUI); most of those components do not exist yet. This section scopes which of
the contract is **live now** and which is **deferred with the boundary that needs it** — it changes no rule,
it records what applies today so v0 code is built against the real contract, not the full one. (Building it
all now would be exactly the "pattern names first" over-design warned against above.)

**Live now (v0 code is built against this):**
- **§0/§1 three failure classes; assert vs error.** `editor-core` uses `debug_assert!` for internal
  invariants (revision strictly increases, a delete range within bounds, a freed-generation handle — §1's
  exact examples) and typed `Result` errors for expected failures (`TxnError`, `EditError`, `TraceError`,
  file IO). The two are never swapped (anti-pattern **STAB-2 [P0]** — swallowing an assert as an error and
  continuing on corrupted state).
- **§2 typed, structured errors**, never `Err(String)` (anti-pattern **STAB-1**). The `ErrorReport` /
  `ErrorCode` *ecosystem-API* scheme (§2.1) is **deferred** — v0 keeps the typed enums.
- **§6/§8 recovery on a core-invariant break + panic policy.** A core panic *is* an invariant violation
  (§8). The TUI installs a panic hook that first saves the unsaved buffer to `<file>.ruse-recovered` (a
  recovery snapshot, §6) and then lets the panic unwind (`TermGuard` restores the terminal) — it does
  **not** `catch_unwind`-swallow (anti-pattern **STAB-6 [P1]**) nor blanket `panic=abort` (**STAB-5 [P1]**).
- **§7 fail-fast (internal) vs graceful (external).** Internal bad state → fail-fast (assert). External
  failure only lowers the feature level: a tree-sitter parse failure drops to no highlighting; a file-IO
  failure surfaces a status message; neither crashes editing.
- **§13 loss-safe preflight** — already upheld: `Document::apply` checks `base_revision` and range bounds
  *before* any mutation and rejects atomically, so there is no partial-apply state (anti-pattern
  **CORE-16 [P1]**). This is §13's "transaction preflight" for the editor.
- **§3/§4 structured logging via `tracing`** (in the frontend), logged **once at the ownership boundary**:
  the TUI logs errors / panics / recoveries as structured events when `RUSE_LOG` is set. The diagnostic log
  is kept **separate from the replay `Trace`** (anti-pattern **TRACE-1 [P2]**): the `Trace` is a domain
  record/replay artifact, not the debug log.
- **§11.4 the status bar is a view.** v0 renders one status line; it is not the source of truth for any
  subsystem state (INV-STATUS holds trivially with ~one component).

**Deferred (returns with its boundary — RFC-0012 re-boundary triggers):**
- §2.1 the `ErrorCode` ecosystem API — with the plugin protocol (asserting-on-a-code needs an ecosystem).
- §5 full transaction/correlation-ID *trace propagation* — with async/LSP (v0 records the metadata but does
  not propagate a distributed trace).
- §6 isolation boundaries, §9 the **supervisor**, §11 the full **Health Registry** (per-component state
  machine + `SystemHealth` aggregate), §10 the **diagnostic bundle** — all need the failure-prone
  components (plugins, LSP, remote, terminal jobs) v0 does not have. They land with the LSP/async slice,
  where a supervisor + health registry first earn their place.

## 0. Three Failure Classes — never mixed

```
Invariant violation        Expected failure            External failure
   ↓                          ↓                           ↓
Assert / Panic             Typed Error                 Timeout / Retry / Isolation
   ↓                          ↓                           ↓
Crash report + snapshot    context-propagate → recover  Degraded operation
```

Keeping these three separate is the core discipline. Collapsing them (an assert swallowed as an error, an
external failure treated as a bug) either corrupts state or hides real defects.

## 1. Assert vs Error

**Assert / panic** = a state that can never occur if the program is written correctly (internal invariant).

```rust
debug_assert!(range.start <= range.end);
debug_assert!(range.end <= document.len_bytes());
debug_assert_eq!(transaction.base_revision, document.revision());
```

Representative invariant violations (assert, not error):
- An undo-tree node's parent does not exist.
- A duplicate `DocumentId` was created.
- Revision did not increase after applying a transaction.
- A cycle appeared in the layout tree.
- A delete range points outside the document.
- Internal code used a handle from an already-freed generation.

**Error** = a situation that can plausibly happen (user/environment): file not found, permission denied,
encoding error, SSH drop, plugin timeout, LSP crash, external file change, config parse failure,
unsupported image protocol.

Rule: **user/environment failure → Error; internal invariant violation → Assert/Panic.** Swallowing an
assert failure as an ordinary error and continuing runs on corrupted state and causes larger damage.

## 2. Errors Are Not a Single String

Anti: `Err("failed to open file".to_string())`. Internal errors are **structured types**:

```rust
pub enum DocumentError {
    NotFound { path: WorkspacePath },
    PermissionDenied { path: WorkspacePath },
    InvalidEncoding { path: WorkspacePath, encoding: TextEncoding, offset: u64 },
    ConcurrentModification { path: WorkspacePath, expected: FileVersion, actual: FileVersion },
}
```

Separate the display string, log fields, and recoverability:

```rust
pub struct ErrorReport {
    pub code: ErrorCode,
    pub severity: Severity,
    pub message: String,          // human-facing
    pub context: ErrorContext,    // machine fields
    pub recovery: RecoveryPolicy,
}
```

### 2.1 Error codes are part of the ecosystem API

```
DOC-001   File not found
DOC-004   Concurrent modification
REM-003   Remote protocol mismatch
PLG-007   Plugin execution timeout
TRM-012   Terminal capability probe failed
```

Users can search a code; tests assert on the **code**, not a whole string. Treat code stability like
command-ID stability (see [architecture.md](../architecture/architecture.md) §2.2).

## 3. Propagation Adds Context

A bare `Permission denied` bubbling up is useless. The top must read:

```
Workspace open failed
└─ read project settings failed
   └─ open remote file failed
      └─ /opt/project/editor.toml
         Permission denied
```

Boundary discipline in Rust:
- **Library:** explicit typed errors.
- **App top:** collect errors into a context chain.
- **User UI:** short message + how to fix.
- **Diagnostic log:** full chain + internal fields.

```rust
let content = workspace
    .read_file(&path)
    .await
    .map_err(|source| WorkspaceOpenError::ReadConfig { path: path.clone(), source })?;
```

**Log an error once, at the ownership boundary.** Logging on receipt and re-returning stacks duplicate logs.

## 4. Logs Are Structured Events, Not Sentences

Anti: `"Something went wrong." / "Plugin failed."` Emit structured fields:

```
event="plugin.command.failed"
plugin_id="org.example.git"   plugin_version="2.3.1"
command_id="org.example.git.stage"
workspace_id="ws-37"          request_id="req-12891"
duration_ms=1532              error_code="PLG-007"
remote="ssh://build-server"
```

```rust
tracing::error!(
    plugin_id = %plugin.id(),
    command_id = %command_id,
    request_id = %request_id,
    error_code = "PLG-007",
    elapsed_ms = elapsed.as_millis(),
    error = ?error,
    "plugin command failed"
);
```

Recommended fields (to connect async flows): session ID · request/transaction ID · workspace/document/view
ID · revision · command ID · plugin ID/version · execution location (client/workspace) · remote target ·
duration · error code · retry count.

### 4.1 Log levels have fixed meaning

| Level | Meaning |
| --- | --- |
| trace | hot-path detail flow |
| debug | dev / problem-analysis info |
| info | normal, significant lifecycle change |
| warn | recovered, but possible user impact |
| error | a request or component failed |
| fatal | core invariant corrupted; cannot safely continue |

Logging `error` and continuing normally is fine; recording the *same* error at multiple layers is not
(see §3). Level ladder: low-level creates error → mid-level adds context → ownership boundary logs once →
UI boundary shows a user message.

## 5. Transaction ID Is the Center of Traceability

Every change carries an ID; the chain must be reconstructable end-to-end:

```
Input event (correlation_id=42)
  → Command (command_id=core.editor.delete)
    → Transaction (transaction_id=991)
      → Document revision (120 → 121)
        → Render frame (frame_id=837)
```

So "why was this character deleted?" is answerable by back-tracing.

```rust
pub struct TransactionMetadata {
    pub transaction_id: TransactionId,
    pub correlation_id: CorrelationId,
    pub origin: TransactionOrigin,
    pub command_id: CommandId,
    pub base_revision: Revision,
    pub timestamp: SystemTime,   // injected, not sampled inside pure logic
}

pub enum TransactionOrigin {
    UserInput,
    Macro,
    Plugin(PluginId),
    Lsp,
    AiAgent(AgentId),
    RemotePeer(ClientId),
}
```

**Every mutation has an origin** — when an AI agent or plugin makes a wrong change, its source is exact.
This is also the audit hook for "AI changes reviewed before apply" ([architecture.md](../architecture/architecture.md) §10).

## 6. Error Isolation Boundaries

One failure must not kill the whole editor. Define boundaries:

```
Editor Core
├─ Plugin Host A     (isolated)
├─ Plugin Host B     (isolated)
├─ LSP Process       (isolated)
├─ Remote Runtime    (isolated)
└─ Terminal Job      (isolated)
```

| Failure | Effect |
| --- | --- |
| Plugin panic | only that plugin stops |
| LSP crash | restart; editing continues |
| Remote disconnect | keep local snapshot |
| Image renderer failure | text fallback |
| Git service failure | only the Git view is unavailable |

But when a **core invariant** breaks, do not force continuation. Instead: stop editing → save a recovery
file → produce a diagnostic snapshot → safe shutdown or workspace restart.

## 7. Fail-Fast vs Graceful Degradation (not opposites)

- **Fail-fast** for internal bad state: undo-history corruption, invalid transaction ordering, impossible
  handle generation → detect and stop immediately.
- **Graceful degradation** for external feature failure: lower the feature level only.

```
image  → Unicode fallback
LSP    → plain text editing
Git    → plain file editing
Remote → read-only snapshot
Plugin → that feature disabled
```

**System stability is not hiding all errors — it is bounding the blast radius of each error.**

## 8. Panic Policy (documented)

| Component | Policy |
| --- | --- |
| Core | panic = invariant violation |
| Plugin host | panic = plugin crash, isolated |
| Worker | panic = task failure; supervisor records + restarts |
| FFI boundary | panic must not cross the boundary |
| Release | unwind vs abort decided per component |

Do not blanket `panic=abort` for the whole program (kills crash reports/recovery); do not `catch_unwind`
every panic (hides corrupted state). **Catch only at boundaries; never abuse internally.**

## 9. Supervisor Structure

Failure-prone components (plugins, LSP, remote runtime) are managed by a supervisor:

```
Service Supervisor
├─ start
├─ health check
├─ timeout
├─ restart policy
├─ backoff
├─ crash count
└─ disable threshold
```

Example LSP restart escalation:

```
1st crash: restart immediately
2nd:       restart after 1s
3rd:       restart after 5s
5th:       auto-disable + notify user
```

This prevents an infinite crash loop from eating CPU.

## 10. Diagnostic Bundle (logs alone are not enough)

On a bug report, bundle everything at once:

```
diagnostic-bundle/
├── editor-version.json
├── platform.json
├── terminal-capabilities.json     # the capability ledger (see terminal parity)
├── active-profile.json
├── plugins.json
├── remote-session.json
├── recent-events.jsonl
├── crash-report.txt
└── redacted-config.toml
```

**Privacy first:** strip document contents, paths, env vars, tokens by default. Expose one command:
`:diagnostics export`.

## 11. Log vs Status vs Metric vs Trace vs Diagnostic

Similar-looking, but roles are separate:

| Concern | Question it answers |
| --- | --- |
| **Log** | What happened in the past? |
| **Status** | What state is the system in right now? |
| **Metric** | How often / how long? |
| **Trace** | How did one request pass through components? |
| **Diagnostic** | What should the user fix now? |

Worked example (a slow plugin):

```
Log:        plugin activation failed
Status:     plugin = degraded
Metric:     activation_duration_ms = 2400
Trace:      workspace_open → plugin_resolve → wasm_start → timeout
Diagnostic: "Git plugin is not responding. [Restart] [Disable] [View logs]"
```

Trying to solve all five with one log line tangles quickly.

### 11.1 Status is a state machine, not a string

```rust
pub enum ServiceStatus {
    Stopped,
    Starting,
    Ready,
    Degraded { reason: DegradedReason },
    Recovering { attempt: u32 },
    Failed { code: ErrorCode },
}
```

Transitions are explicitly restricted (reduces "Ready but internally dead" contradictions):

```
Stopped → Starting → Ready
                    ↘ Degraded
Starting → Failed
Degraded → Recovering → Ready
                      ↘ Failed
```

### 11.2 Status is per-component (no single `is_ok` bool)

```
Workspace
├─ Document Engine      Ready
├─ Renderer             Ready
├─ Git Service          Degraded
├─ LSP: rust-analyzer   Recovering
├─ Plugin Host          Ready
├─ Remote Runtime       Disconnected
└─ Terminal Capability  Limited
```

Overall health is an **aggregate** of component health:

```rust
pub struct SystemHealth {
    pub overall: HealthLevel,
    pub components: Vec<ComponentHealth>,
}
```

### 11.3 Status changes are log events

```
component="rust-analyzer"
old_status="ready"  new_status="recovering"
reason="process_exited"  restart_attempt=2
```

So "why did that feature suddenly turn off?" is traceable.

### 11.4 The status bar is a *view*, not the source

The TUI may show `LSP: OK`, `SSH: LOST` — but the UI must not manage that state. It **subscribes to the
Health Registry** and only renders it. (Guards UI managing state directly; see [architecture.md](../architecture/architecture.md) §7, invariant INV-STATUS.)

## 12. Early Observability Modules

```
observability/
├── error
├── event
├── logging
├── tracing
├── metrics
├── status
├── health
├── diagnostics
└── crash_report
```

## 13. Preflight (Loss-Safe)

Risky operations are **checked before they apply**, not applied-then-recovered. If preconditions fail,
block the operation instead of leaving partial/lost state.

**Remote save preflight** — connection valid? remote revision matches? write permission? conflict?

**Plugin transaction preflight** — base revision matches? range valid? capability permitted? undo record
producible?

**AI proposal preflight** — target document revision matches? file changed externally? change range outside
the workspace? binary/large file?

If any check fails, the operation is refused with a typed error (§2) and a diagnostic (§11), never
"apply and hope to recover." (Upholds INV-FAIL-BOUNDED and INV-TXN — see
[../invariants/reference-invariants.md](../invariants/reference-invariants.md).)

## 14. Debug Surfaces as Product Features

Every stage of the transformation pipeline ([render-and-frontends.md](render-and-frontends.md) §5) is
inspectable via first-class commands — not ad-hoc logging:

```
:debug state          :debug document       :debug transactions
:debug render-tree     :debug keymap         :debug capabilities
:debug plugins         :debug remote
```

Example — keymap resolution (`:debug keymap g s`):

```
Sequence: g s
Profile: vim@1
Context: git-status
Resolved command: org.example.git.stage

Resolution chain:
1. temporary map      none
2. view-local map     org.example.git.stage
3. user override      none
4. plugin suggestion  ignored
5. builtin profile    core.search.symbol
```

`:debug render-tree` shows semantic node → terminal lowering, so an image-placement bug is localized to
"Kitty lowering," not blamed on "the terminal." This makes conflicts and state problems debuggable by
inspection rather than guesswork, and is the product-facing side of the observability model.

**Offline counterparts (the `tools/` CLIs).** The same surfaces are reachable without a running editor, for
bug reports and CI regression capture (`tools/`, see [docs/README.md](../README.md) §Target Repository
Layout):
- `protocol-dump` — decodes a captured **framed protocol stream** (remote runtime ↔ agent, or host ↔
  plugin) into human-readable, versioned records: one line per frame with `session-id`, direction,
  `protocol-version`, message kind, and transaction id. Unknown/newer variants are shown, not dropped
  (`INV-ADDITIVE`), so a version-skew bug is visible as an unhandled frame rather than a silent desync. It
  reads a `.jsonl`/binary capture (or `--follow` a live socket) and never needs the document contents —
  same redaction defaults as the diagnostic bundle (§10).
- `render-diff` — diffs two semantic Render Trees (golden vs actual) for the terminal-matrix tests (§ testing).
- `inspector` / `diagnostic-bundle` — the interactive state inspector and the §10 bundle packer.

These are thin wrappers over the same query/serialization the `:debug` commands use — one model, two entry
points (in-editor command, offline CLI), never a divergent second implementation.

## Reference Invariants (this doc)

This doc depends on these registry invariants (defined in
[../invariants/reference-invariants.md](../invariants/reference-invariants.md); not redefined here):

- **INV-ERR-CLASS** — Expected failures are typed errors; impossible states are assertions; errors gain
  context while propagating and are logged once, at ownership boundaries. (§1, §3)
- **INV-ORIGIN** / **INV-ASYNC-ORDER** — Every mutation has an origin; every async operation has an ID. (§5)
- **INV-FAIL-BOUNDED** — External components fail independently; a core invariant failure triggers a
  recovery snapshot rather than continued execution. (§6, §7)
- **INV-CAP-DEGRADE** — Unsupported capabilities degrade, they do not disappear. (§7)
- **INV-STATUS** — Status is a per-component state machine; the UI only renders the Health Registry. (§11)
- Logs are structured and privacy-aware. (§4, §10)

The ten project-level stability rules (canonical list): (1) Expected failures are typed errors. (2)
Impossible states are assertions. (3) Errors gain context while propagating. (4) Errors are logged once, at
ownership boundaries. (5) Every asynchronous operation has an ID. (6) Every document mutation has an origin.
(7) External components fail independently. (8) Core invariant failures trigger recovery snapshots. (9)
Unsupported capabilities degrade, not disappear. (10) Logs are structured and privacy-aware.

## Alternatives / Rejected Ideas / Trade-offs

- **Rejected: `Result<_, String>` everywhere.** Fast to write, impossible to match/test/recover on;
  loses codes and context. → typed errors + `ErrorCode` (§2).
- **Rejected: one global `is_ok: bool`.** Cannot express partial degradation; UI ends up owning truth. →
  per-component `ServiceStatus` + aggregated `SystemHealth` (§11.2).
- **Rejected: blanket `panic=abort`** (no crash report/recovery) **and blanket `catch_unwind`** (hides
  corruption). → boundary-only capture, per-component unwind/abort (§8).
- **Rejected: log-and-rethrow at every layer.** Duplicate noise, no single source of truth. → log once at
  boundary (§3, §4).
- **Trade-off:** structured events + IDs + snapshots cost upfront plumbing and bytes. Accepted: this is the
  observability substrate the whole platform is built to be debuggable through — the point of the project
  (Architecture > Code).
- **Trade-off:** fail-fast on core invariants means deliberately crashing a running editor. Mitigated by
  recovery-file save + diagnostic snapshot before shutdown (§6).
