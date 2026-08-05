---
doc: rfc
project: ruse
title: "RFC-0010: Stability & Observability"
summary: >
  Failure paths are first-class. This RFC locks the three-way separation of failure classes that
  must never be mixed — invariant violation → assert/panic, expected failure → typed coded error,
  external failure → timeout/retry/isolate — and the machinery that hangs off it: typed errors with
  stable ecosystem-API error codes (DOC-001…), context chains logged once at ownership boundaries,
  per-component status as a restricted state machine aggregated into SystemHealth that only the UI
  renders, traceability (every async op an ID, every mutation an origin), bounded blast radius with
  supervisors and safe mode, fail-fast vs graceful degradation, boundary-only panic capture,
  preflight (block-before-apply), and debug surfaces as product features. It records the decision
  (D-016) and defers the PII/redaction field list and retention (D-017). It does not duplicate the
  design model it links.
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - ../../design/stability-and-observability.md
  - ../../architecture/architecture.md
  - ../../invariants/reference-invariants.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/POLICY.yaml
---

# RFC-0010: Stability & Observability

- **Status:** proposed
- **Author(s):** ruse core
- **Created:** 2026-08-05
- **Decision link:** D-016 (Status / Error / Panic are separate); relates to D-017 (log PII / redaction, open)

<!-- Hard-to-reverse: this fixes the error model, the status model, and the panic policy — contracts
     that plugin authors, tests, and diagnostic tooling all bind to. Changing them later breaks error
     codes, status subscriptions, and every test that asserts on a code. -->

## Summary

In system software the goal is not "never fail" but: **when it fails, know exactly where and why,
and recover safely.** ruse designs failure paths as first-class as success paths. This RFC locks the
one discipline the rest depends on — **three failure classes that are never mixed** (D-016): an
internal **invariant violation** is an assert/panic, an **expected** user/environment failure is a
typed error with a stable code, an **external** component failure is handled by timeout/retry/
isolation and degradation. From that separation follow the load-bearing contracts: typed errors
carry **stable error codes** (`DOC-001`…) that are part of the ecosystem API; errors gain context
while propagating and are **logged once, at the ownership boundary**; every async operation carries
an ID and every mutation an **origin**, so any change is back-traceable; **status is a per-component
state machine** with restricted transitions, aggregated into `SystemHealth`, and the UI only renders
a subscribed Health Registry; blast radius is bounded by isolation boundaries, supervisors, and a
**safe mode**; core-invariant failures **fail fast** while external failures **degrade gracefully**;
panics are captured **only at boundaries**; risky operations are **preflighted** (blocked before
apply, never applied-then-recovered); and debug surfaces (`:debug …`) are shipped **product
features**. The full model lives in
[stability-and-observability.md](../../design/stability-and-observability.md); this RFC records the
decision and its boundaries, not the design.

## Motivation / Problem

Neovim's failure story is diffuse: a bad plugin, an LSP crash, or a lost SSH link can degrade or hang
the whole editor, and "why did this happen / what state am I in now?" has no single answer.
ruse's premise is the opposite — failure is a designed subsystem. Concretely, five guarantees are
impossible unless the failure classes are separated up front:

1. **Real bugs stay visible.** If an invariant violation is swallowed as an ordinary error and
   execution continues, the editor runs on corrupted state and does *more* damage. Asserts must not
   be reachable through the error channel.
2. **Errors are actionable and testable.** A bare `Err("failed".to_string())` cannot be matched,
   recovered from, searched, or asserted on. Tests must key on a **stable code**, not a string.
3. **A change is back-traceable.** "Why was this character deleted?" is answerable only if every
   mutation carries an `origin` and every async op a correlation/request ID (RFC-0007 §5).
4. **One failure does not kill the editor.** Plugin, LSP, remote, terminal, and image failures must
   be independently isolated and degrade the feature only — the blast radius is bounded, not hidden.
5. **The UI reflects truth without owning it.** A single global `is_ok` bool cannot express partial
   degradation and forces the status bar to invent state; the source of truth must be a per-component
   Health Registry the UI subscribes to.

Collapsing the classes, or bolting observability on afterward, quietly breaks all five. This RFC
locks the separation so they cannot regress (POLICY **ENG-ERR-001**, **ENG-FAIL-001**,
**ENG-OBS-001**).

## Guide-level explanation

From a contributor's or plugin author's view there are three distinct channels, and choosing the
right one is the whole discipline:

- **Something that can never happen if the code is correct** — an undo node with a missing parent, a
  duplicate `DocumentId`, a revision that failed to increase, a delete range outside the document, a
  freed-generation handle. That is an **invariant violation**: `debug_assert!` / panic, not an error.
  Swallowing it is forbidden.
- **Something that plausibly happens because of the user or environment** — file not found,
  permission denied, bad encoding, config parse failure, an SSH drop, an LSP crash. That is a **typed
  error** carrying a **stable code** (`DOC-001`, `DOC-004`, `REM-003`, `PLG-007`, `TRM-012`, …). You
  match on the code, recover, and surface a short user message plus a fix.
- **An external component being slow or unavailable** — a plugin timing out, a remote link dropping,
  a renderer failing. That is handled with **timeout / retry / isolation** and **graceful
  degradation**: the feature drops a level (image → Unicode, LSP → plain text, Git → plain files,
  remote → read-only snapshot), the rest keeps working.

You never build your own status string or your own log-and-rethrow ladder. You **return a typed
error**, and it is **logged once** at the ownership boundary as it crosses out of your subsystem;
lower layers add context (`WorkspaceOpen → ReadConfig → path → PermissionDenied`) but do not each
log. You **report status by driving a state machine** (`Ready → Degraded → Recovering → …`), not by
setting a flag the UI reads. And you get **debug surfaces for free**: `:debug transactions`,
`:debug keymap g s`, `:debug render-tree`, `:debug capabilities` are product commands, not ad-hoc
`eprintln!`.

Everything risky is **preflighted**. A remote save, a plugin edit, or an AI proposal is *checked
before it applies* (connection valid? base revision matches? range in bounds? capability permitted?
inverse producible?). If any check fails, the operation is **refused** with a typed error and a
diagnostic — never applied-and-recovered, so there is no partial or lost state.

## Reference-level explanation

Language-independent contract (illustrative types live in the design doc; not re-specified here).

### 1. Three classes, never mixed (D-016, INV-ERR-CLASS)

```
Invariant violation        Expected failure             External failure
   ↓                          ↓                            ↓
Assert / Panic             Typed error (+ code)         Timeout / Retry / Isolation
   ↓                          ↓                            ↓
Crash report + snapshot    context-propagate → recover  Degraded operation
```

The mapping is a **hard rule**, not a guideline: user/environment failure → typed error; internal
impossible state → assert/panic; external component → isolate + degrade. An assert must never be
reachable via the error channel, and an external failure must never be reported as a bug. This is
D-016 verbatim (Error = event; Status = state machine; Panic = invariant violation captured only at
boundaries), enforced by ENG-ERR-001 (`no-duplicate-log-check`, error-code registry).

### 2. Typed errors carry stable codes (STAB-1, STAB-2)

Errors are **structured types**, not strings, and separate the human message, the machine context
fields, the **severity**, and the **recovery policy** (design §2). Each carries a **stable
`ErrorCode`** from a namespaced registry (`DOC-`, `REM-`, `PLG-`, `TRM-`, …). Codes are an
**ecosystem-API surface**: users search them, tests assert on them, and they are treated with
command-ID-grade stability (INV-CMD-SEMANTIC discipline; additive-only, deprecation windows). Layer
codes but do **not** over-subdivide (design-requirements §11 / `STAB` addendum).

### 3. Context chains, logged once (STAB-3, STAB-4)

Errors **gain context while propagating** — the top of a chain reads as a nested cause list, not a
bare leaf. Boundary discipline: libraries return explicit typed errors; the app top assembles the
context chain; the user UI shows a short message + fix; the diagnostic log holds the full chain and
internal fields. **An error is logged exactly once, at the ownership boundary it crosses.** Logging
on receipt and re-returning (log-and-rethrow at every layer) is prohibited — it produces duplicate
noise and no single source of truth (`no-duplicate-log-check`). Log **levels** have fixed meanings
(trace/debug/info/warn/error/fatal); logs are **structured events** with fixed recommended fields
(session/request/transaction/workspace/document/view ID, revision, command/plugin ID, execution
location, remote target, duration, error code, retry count), never sentences.

### 4. Traceability: async IDs + mutation origins (INV-ORIGIN, INV-ASYNC-ORDER)

**Every async operation has an ID and every mutation an origin.** The correlation chain
`input(correlation_id) → command → transaction → revision → frame` (design §5) makes any change
back-traceable, and `origin` (UserInput | Macro | Plugin | Lsp | AiAgent | RemotePeer) is the audit
hook for AI/plugin review before apply (architecture §10). Async responses carry a request ID +
revision so **stale results are dropped**, not applied (INV-ASYNC-ORDER; RFC-0007 §2/§5). The
transaction metadata that carries this is owned by RFC-0007; this RFC only requires that the
observability substrate consume it.

### 5. Status is a per-component state machine (INV-STATUS, STAB-7, STAB-8)

Status is **not** a string and **not** a global bool. Each component runs a small state machine
(`Stopped → Starting → Ready`, `→ Degraded → Recovering → Ready`, `→ Failed`) with **restricted
transitions**, which removes "Ready but internally dead" contradictions. Per-component states
(Document Engine, Renderer, Git, LSP, Plugin Host, Remote, Terminal Capability) aggregate into a
single `SystemHealth { overall, components[] }` — the **Health Registry**. Health includes
**freshness** (`LSP: Ready, checked 200ms ago`; `Remote: Unknown, no heartbeat 20s` — design-req
§11). **Status transitions are themselves log events** (old→new, reason, cause event) so "why did
that feature turn off?" is traceable. The status bar is a **view**: it **subscribes to** the Health
Registry and renders it; the UI never owns or manages status (INV-STATUS; architecture §7). Status
(persistent state) and Error (a past event) are separate concerns and are never conflated
(design-req §11).

### 6. Bounded blast radius: isolation, supervisors, safe mode (INV-FAIL-BOUNDED, INV-CAP-DEGRADE)

Failure-prone components (plugin host, LSP, remote runtime, terminal job) sit behind **isolation
boundaries** — a plugin panic stops only that plugin, an LSP crash restarts while editing continues,
a remote drop keeps the local snapshot. Each is managed by a **supervisor** (start, health check,
timeout, restart policy, backoff, crash count, disable threshold) with escalating restart so a crash
loop cannot eat CPU (design §9). A **safe mode** boots even with bad config (ENG-FAIL-001;
design-req §10). System stability is defined as **bounding each error's blast radius, not hiding
errors**.

### 7. Fail-fast vs graceful degradation (INV-FAIL-BOUNDED, INV-CAP-DEGRADE)

These are not opposites — they apply to different classes. **Fail-fast** for internal bad state
(undo-history corruption, invalid transaction ordering, impossible handle generation): detect and
stop immediately → save a recovery file → produce a diagnostic snapshot → safe shutdown or restart.
A dedicated minimal **crash path** on fatal invariant failure depends less on allocation/locks
(design-req §11). **Graceful degradation** for external feature failure: lower the feature level
only. An unsupported capability **degrades, it does not disappear** (INV-CAP-DEGRADE).

### 8. Panic policy: boundary-only capture (INV-PLUGIN-ISOLATED, STAB-6)

Panic policy is documented per component: Core panic = invariant violation; plugin-host panic =
isolated plugin crash; worker panic = task failure the supervisor records and restarts; a panic must
**not cross an FFI boundary**; release unwind-vs-abort is decided per component. Neither blanket
`panic=abort` (kills crash reports/recovery) **nor** blanket `catch_unwind` (hides corrupted state)
is permitted — **catch only at boundaries, never internally**.

### 9. Preflight — block before apply (design §13)

Risky operations are checked before they apply. Remote save, plugin transaction, and AI proposal
each have preflight checks (connection/base-revision/permission/conflict; range validity; capability;
inverse producible; external-change/binary/large-file). Any failure **refuses** the operation with a
typed error (§2) + diagnostic — never "apply and hope to recover." This upholds INV-FAIL-BOUNDED and
INV-TXN jointly (owned in detail by RFC-0007 §5).

### 10. Roles are separated: log / status / metric / trace / diagnostic

Five distinct concerns answer five distinct questions — *what happened (log) · what state now
(status) · how often/long (metric) · how did one request flow (trace) · what should the user fix
(diagnostic)*. One log line must not try to serve all five. **Debug surfaces are product features**:
`:debug state|document|transactions|render-tree|keymap|capabilities|plugins|remote` make conflicts
and state problems debuggable by inspection (an image-placement bug localizes to "Kitty lowering,"
not "the terminal"). A **diagnostic bundle** (`:diagnostics export`) collects version/platform/
capabilities/profile/plugins/remote/recent-events/crash-report at once, **redacted by default**.

### 11. PII / redaction (D-017, open)

Direction (locked): **designate never-log fields by default**; diagnostic bundles **redact by
default with a preview** before export (D-017; design §10). The exact never-log field list and
retention windows are **open** and must close before any telemetry or diagnostic-export ships
(see Open questions).

## Reference Invariants

This RFC depends on and enforces these registry invariants (defined in
[reference-invariants.md](../../invariants/reference-invariants.md); not redefined here):

- **INV-ERR-CLASS** — Expected failures are typed errors with stable `ErrorCode`; impossible states
  are assertions; the two are never interchanged; errors gain context while propagating and are
  logged once, at ownership boundaries. (§1–§3)
- **INV-FAIL-BOUNDED** — Stability means bounding each error's blast radius, not hiding errors;
  core-invariant failure triggers a recovery snapshot and safe shutdown, external failure degrades
  only. (§6, §7, §9)
- **INV-STATUS** — Status is a per-component state machine with restricted transitions; overall
  health is an aggregate; the UI only renders a subscribed Health Registry and never owns the state.
  (§5)
- **INV-ORIGIN** — Every mutation has an explicit origin. (§4)

Also relied upon (owned by other docs): **INV-ASYNC-ORDER** (async IDs + stale-result drop, §4),
**INV-CAP-DEGRADE** (degrade not disappear, §7), **INV-PLUGIN-ISOLATED** (plugin panic never crosses
a boundary, §8), **INV-CMD-SEMANTIC** (error-code stability discipline, §2), **INV-TXN** (preflight
loss-safety, §9 — owned by RFC-0007).

## Failure modes & Recovery

- **Invariant violation** (orphaned undo parent, revision did not increase, delete range outside
  document, freed-generation handle): **fail-fast** — stop editing → recovery file/journal →
  diagnostic snapshot → safe shutdown, via the minimal crash path (§7, INV-FAIL-BOUNDED). Never
  swallowed as an error.
- **Expected typed error** (file not found `DOC-001`, permission denied, concurrent modification
  `DOC-004`, config parse failure): matched on code, recovered where possible, logged once at the
  boundary, surfaced as a short message + fix (§2, §3).
- **External component failure** (plugin timeout `PLG-007`, LSP crash, remote drop, renderer fail):
  isolated and supervised (restart/backoff/disable-threshold); the feature **degrades** (§6, §7).
  A crash loop is bounded by escalating restart, not left to consume CPU.
- **Preflight refusal**: the risky op is blocked before apply with a typed error + diagnostic, leaving
  no partial state (§9).

## Security impact

`origin` (§4) is the enforcement point for the trust model: plugin/AI/remote actions are
capability-gated at preflight and AI-origin actions are held for review before apply (architecture
§10). Because every mutation and async op is attributable, a wrong or malicious change is traceable
to its exact source. Diagnostic bundles are the main exfiltration risk and are therefore **redacted
by default with preview** (§11, D-017); the never-log field designation exists precisely so document
contents, paths, env vars, and tokens do not leak into logs or bundles. The final field list is
gated behind D-017 before any export ships.

## Performance impact

Structured events, correlation IDs, per-component state machines, and diagnostic snapshots cost
upfront plumbing and bytes — accepted, because this is the substrate the whole platform is debuggable
and recoverable through (Architecture > Code). Mitigations are first-class, not afterthoughts:
per-component **ring buffers** of recent logs and a **trace-sampling** policy bound steady-state cost
(design-req §11); logging is structured-field emission on failure paths, not hot-path narration
(hot-path detail is `trace`, off by default). The fatal-invariant crash path is deliberately minimal
(less allocation/lock dependence) so recovery works even when the process is unhealthy.

## Compatibility & Migration

New cross-cutting subsystem; no data migration. Two surfaces are **forward compatibility
commitments**: the **error-code registry** (`DOC-`, `REM-`, `PLG-`, …) is additive-only with
deprecation windows and stability on par with command IDs (INV-ADDITIVE; codes are never re-used or
silently deleted), and the **status state-machine set** + Health Registry schema are the contract the
status bar and any external monitor bind to. The **diagnostic-bundle format** is versioned. The
never-log field list and retention windows (D-017) are the one deferred piece and must be pinned
before diagnostic-export or telemetry ships.

## Observability

This RFC *is* the observability contract; it is self-hosting. Every failure emits a structured event
with a stable code and correlation fields; every status transition is a logged event with reason and
cause; `SystemHealth` is queryable; `:debug …` surfaces expose each pipeline stage; `:diagnostics
export` produces a redacted bundle. The observability module layout (`error / event / logging /
tracing / metrics / status / health / diagnostics / crash_report`) exists from day one (design §12),
so the machinery is present before the first feature "needs" it.

## Alternatives

1. **Chosen: three separated failure classes (D-016)** with typed coded errors, log-once context
   chains, per-component status aggregated into a Health Registry, boundary-only panic capture,
   supervisors + safe mode, and preflight. Locks debuggability and bounded blast radius in one model.
2. **Exceptions/`anyhow`-style dynamic errors everywhere.** Ergonomic, but erases the class boundary
   and the stable code — you cannot statically tell an invariant violation from an expected failure,
   and tests key on strings. Typed errors + `ErrorCode` chosen instead (§2).
3. **Centralized event bus that all failures and status flow through, UI included.** Rejected as the
   *source of truth* shape because it re-introduces the "UI owns status" failure (§5) and hides where
   an error is owned; status ownership stays per-component with the UI as a pure subscriber.

## Rejected approaches

*Recorded so they are not re-litigated (RFC process).*

- **Rejected: `Result<_, String>` (or `Err("...".to_string())`) everywhere.** Fast to write,
  impossible to match/test/recover on; loses codes, context, severity, and recovery policy. →
  typed errors + stable `ErrorCode` (§2). (STAB-1)
- **Rejected: one global `is_ok: bool`.** Cannot express partial degradation (LSP recovering while
  Git is degraded and editing is fine); the UI ends up inventing and owning truth. → per-component
  state machine aggregated into `SystemHealth`, UI subscribes only (§5). (STAB-7, STAB-8)
- **Rejected: blanket `panic=abort`** (destroys crash reports + recovery) **and, equally, blanket
  `catch_unwind`** (hides corrupted state and lets execution continue on it). → boundary-only capture,
  per-component unwind/abort, panic never crosses FFI (§8). (STAB-6)
- **Rejected: log-and-rethrow at every layer.** Duplicate log noise, no single source of truth,
  correlation drowned. → log **once** at the ownership boundary; lower layers add context only
  (§3). (STAB-3, STAB-4)
- **Rejected: the UI owns/derives status** from scattered flags. → status is owned by components and
  rendered by the UI; the status bar is a view over the Health Registry (§5, INV-STATUS).
- **Rejected: swallowing an assert as an ordinary error and continuing.** Runs on corrupted state and
  causes larger damage. → invariant violations are asserts/fail-fast, never routed through the error
  channel (§1, §7). (STAB-2)
- **Rejected: apply-then-recover for risky operations.** Leaves partial/lost state. → preflight
  refuses before apply (§9).

## Trade-offs

- **Upfront plumbing.** Codes, structured events, IDs, state machines, snapshots, and preflight cost
  engineering and bytes before any feature demands them — accepted; it is the debuggable/recoverable
  substrate the project is built on (design trade-offs; Architecture > Code).
- **Deliberate fail-fast.** Treating core-invariant corruption as a crash means intentionally
  stopping a running editor — mitigated by recovery-file + diagnostic snapshot on a minimal crash
  path before shutdown (§7).
- **Discipline over convenience.** Choosing the right failure class and logging exactly once is a
  standing review burden (ENG-ERR-001 checklist/lint), rejecting the easy `anyhow`-everywhere path.
- **Two-model surface.** Errors and status are separate models that both touch nearly every
  component; keeping them separate (not fusing into one "event") is the cost that buys the five roles
  in §10 staying untangled.

## Re-evaluation conditions

- **D-016:** not expected to reopen; would only be revisited if the three-class separation proved
  unworkable in practice (no current signal).
- **D-017 (open):** the never-log field list and retention windows are finalized **before** any
  telemetry or diagnostic-export ships; that is a blocking gate, not an optional refinement.
- **Ring-buffer / sampling budgets:** revised with real workload data (relates to D-018/D-019 once
  post-MVP features exercise them).
- **Error-code taxonomy:** revisit the namespace granularity if layering proves too coarse or too
  subdivided in real ecosystem use (design-req §11 caution), without breaking existing codes.

## Open questions

1. **D-017 — log PII / redaction (open).** The exact set of never-log fields (document contents,
   paths, env vars, tokens, remote targets?) and per-surface **retention windows**, plus the
   redaction-preview UX for `:diagnostics export`. Must close before telemetry/diagnostic-export.
2. **Error-code registry governance.** Who allocates namespaces for third-party plugins, and how plugin
   error codes coexist with core codes under the same stability guarantee (§2, relates to
   INV-PROTOCOL-VERSIONED).
3. **Health freshness semantics.** Standard staleness thresholds and heartbeat cadence per component
   before a `Ready` is downgraded to `Unknown` (design-req §11) — needs real remote/LSP latency data.
