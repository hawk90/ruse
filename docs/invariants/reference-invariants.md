---
doc: reference-invariants
project: ruse
title: "ruse Reference Invariants"
summary: >
  Language-independent invariants that define ruse as a design, not as a Rust program. These are the
  paper-terminology rules meant to hold across any reference or production implementation (Rust today,
  something else in 20 years). Each RFC/design doc restates the invariants it enforces. Invariants are
  first-class artifacts: code is only a proof of them.
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - ../architecture/architecture.md
  - ../design/stability-and-observability.md
  - ../anti-patterns/anti-patterns.md
---

# ruse Reference Invariants

> These invariants are ruse's language-independent rules — a concrete implementation is a *proof* of them,
> not their source. (For the "Architecture > Code" philosophy behind this, see
> [docs/README.md §Philosophy](../README.md).)

Each invariant is language-independent, testable, and referenced by ID from RFCs, PRDs, POLICY, and
anti-patterns. **This file is the single registry for `INV-*` IDs:** design docs may *reference* invariant
IDs but must not privately define new ones — a new invariant is added here in the same change (enforced by
`spec validate`, D-022). Design docs' local "Reference Invariants" sections list the IDs they depend on;
they do not mint new numbering.

## State & Ownership

- **INV-DOC-VIEW** — A Document must not know about a View. Document ≠ View ≠ Window ≠ File; the same
  Document is openable in multiple Views, and view-local state is never stored in the Document.
  *Guards:* CORE-4, UI-6/7/8.
- **INV-NO-GLOBAL-STATE** — There is no single global mutable editor state; components own their state and
  communicate by messages/handles, never by shared `Arc<Mutex<_>>` over everything.
  *Guards:* CORE-2/3.
- **INV-HANDLE** — Long-lived references are stable IDs / typed handles (with generation), never raw
  pointers or offsets; using a freed-generation handle is an invariant violation (assert), not an error.
  *Guards:* CORE-14, and see [stability §1](../design/stability-and-observability.md).

## Text & Position

- **INV-POS-TYPED** — Positions are typed by unit (byte / char / grapheme / UTF-16 column / screen cell);
  coordinates are never an untyped `usize`. *Guards:* TEXT-1/2.
- **INV-ANCHOR** — Long-lived positions (cursors, diagnostics, decorations) are anchors with a boundary
  **bias** (`Before | After`, anchor-store D-023) that survive edits, never raw offsets; anchor update cost
  is not O(anchors × edits).
  *Guards:* TEXT-4/5/6, PERF-6.

## Transaction & Undo

- **INV-TXN** — Every mutation of an **editable Document** goes through a Transaction carrying
  `base_revision`; applying a transaction strictly increases the Document revision. Direct insert/delete
  scattered through the code is forbidden. *Guards:* TEXT-9/10; assert in [stability §1](../design/stability-and-observability.md).
- **INV-UNDO** — Every Transaction on an editable Document is undoable; undo is recorded by logical unit
  (not per keystroke), and the undo history is itself consistent (no orphaned parents). *Guards:* TEXT-12/13.
- **INV-BUFFER-KIND** — **Ephemeral/append-only buffers** (terminal/PTY, streaming logs, generated output,
  and large-file/degraded mode) are an explicit exception to INV-TXN/INV-UNDO: they use a bounded or absent
  undo log and an append path that is not a full transaction. A buffer's kind
  (editable / read-only / generated / streaming / interactive — see [parity/workspace.md](../parity/workspace.md))
  determines which mutation contract applies. *Guards:* the INV-TXN↔INV-UNDO contradiction for F-011/WS-5/COM-12 (V-4).
- **INV-ORIGIN** — Every mutation has an explicit origin (UserInput | Macro | Plugin | Lsp | AiAgent |
  RemotePeer). *Guards:* observability, AI-review (SEC-15).

## Command & Input

- **INV-CMD-SEMANTIC** — A Command is semantic: it has a stable, namespaced ID, typed arguments, and is
  decoupled from any keybinding or command-line string. Keymaps resolve *onto* commands.
  *Guards:* CMD-1/6/7/8.
- **INV-PROFILE-ISOLATION** — Bindings from different input profiles never share a key space; a real
  conflict requires same profile + same sequence + overlapping context + same priority, and is detected
  statically. *Guards:* PROFILE-1/4/5.
- **INV-PRIORITY** — Key resolution follows the fixed priority ABI (temporary state → active view →
  buffer-local mode → workspace → user → plugin-explicit → plugin-suggested → built-in). Plugins cannot
  force global keys. *Guards:* PROFILE-3/6/7.

## Plugin & Extension

- **INV-PLUGIN-NO-CORE** — A Plugin cannot mutate Core state directly and never receives internal types
  (Rope, View, slotmap, undo node, renderer). It sees only handles, snapshots, commands, events, typed UI
  models, and capabilities. *Guards:* PLUGIN-1/2/3, CORE-15.
- **INV-PLUGIN-ISOLATED** — A Plugin fails independently; a plugin panic/timeout never terminates the
  editor and never crosses an FFI/host boundary. *Guards:* PLUGIN-4/5, [stability §6/§8](../design/stability-and-observability.md).
- **INV-PROTOCOL-VERSIONED** — The extension surface is a versioned protocol (WASM/external process), never
  a Rust dynamic-library ABI; API, command IDs, config schema, and profiles are all versioned with
  deprecation windows. *Guards:* PLUGIN-1/14, ECO-2, CMD-6.

## Capability, Remote, Terminal

- **INV-CAP-DEGRADE** — An unsupported capability degrades (lower quality / fewer features), it does not
  disappear; capability is a confidence ledger with user override, never a bare bool, and never inferred
  from `TERM` alone. *Guards:* TERMOUT-11/15/17, TERMIN-1.
- **INV-REMOTE-FIRST** — *(DEFERRED by [D-039] / RFC-0012 — downgraded from an active invariant to a deferred
  design commitment; ruse is a terminal modal editor first, and "remote-first from day one" is xi-editor's
  failed shape. Re-boundary trigger: ≥2 months of local dogfooding. Retained here for when it is re-earned as
  a new invariant.)* The client/remote boundary is a first-class type distinction (local path ≠ workspace
  path) present from the start, not bolted on; remote runtime and client negotiate versions and never assume
  identical builds. *Guards:* CORE-19, REMOTE-1/3/7/11.

## Failure & Observability

- **INV-ERR-CLASS** — Expected failures are typed errors (with stable `ErrorCode`); impossible states are
  assertions; the two are never interchanged. Errors gain context while propagating and are logged once,
  at ownership boundaries. *Guards:* stability §1–§4; SEC.
- **INV-FAIL-BOUNDED** — System stability means bounding each error's blast radius, not hiding errors.
  Core-invariant failure triggers a recovery snapshot and safe shutdown; external failure degrades only.
  *Guards:* stability §6/§7.
- **INV-STATUS** — Status is a per-component state machine with restricted transitions; overall health is an
  aggregate; the UI (status bar) only renders a subscribed Health Registry and never owns the state.
  *Guards:* UI, stability §11.
- **INV-ASYNC-ORDER** — Observable ordering is preserved by a single-threaded deterministic executor;
  arbitrary async events are never funneled directly into the state machine; every async response carries a
  request ID + revision and stale results are dropped. *Guards:* ASYNC-1/6/9/17.

---

## Protocol & Evolution

- **INV-CONTRACT-FIRST** — Contracts are defined independently of implementation types; changing an
  internal Rust type is not, by itself, an API change. *Guards:* SPEC, ECO-1/17.
- **INV-ADDITIVE** — Compatible evolution is additive; readers handle unknown variants/fields/capabilities
  gracefully; breaking changes require a major bump. *Guards:* ECO-2/3, APIX.
- **INV-PROMOTION** — No API reaches Stable without ≥2 independent users and a migration strategy.
  *Guards:* APIX-1.

## Render

- **INV-RENDER-IR** — All output is produced by lowering a single semantic Render Tree; no view or plugin
  emits backend-specific bytes (escape sequences, GPU calls) directly. *Guards:* TERMOUT-10, PLUGIN-8/9,
  RIR-*.
- **INV-RENDER-PROFILE** — A render profile (compatibility | enhanced) is pinned per **client-view** and not
  switched mid-session on probe noise; with multiple clients, each client-view lowers the shared Render
  Tree at its own tier. *Guards:* RENDER-1/3.
- **INV-QUERY-SNAPSHOT** — Queries return immutable snapshots/DTOs, never live mutable core objects; any
  per-redraw decoration provider is bounded to a visible-range snapshot and runs outside the paint critical
  section. *Guards:* CMD-14/15, PLUGIN-2.

## Scheduler

- **INV-SCHED-1** — All background work is owned by a central scheduler; user input and screen refresh
  always outrank background tasks; duplicate per-document index/parse requests are coalesced and superseded
  requests cancelled. *Guards:* SCHED-1/3/4/8. (Priority/deadline/budget *specifics* are open — D-018.)

## Trust

- **INV-TRUST-1** — No code executes before a workspace-trust decision; principals (core/official-plugin/
  third-party-plugin/workspace-repo/remote/terminal-output/AI) carry distinct trust levels; new permissions
  are never auto-approved; side effects are capability-gated; AI changes are reviewed before apply.
  *Guards:* TRUST-1/4/5/6, SEC-2/15.

## How Invariants Are Used

1. **RFCs restate them.** Every RFC's "Reference Invariants" section lists the INV-IDs it depends on or
   introduces (see [docs/README.md](../README.md) RFC process).
2. **Anti-patterns map to them.** Each anti-pattern is a concrete way to break one or more invariants; the
   `Guards:` lines above are the cross-reference.
3. **Tests assert them.** Property/differential tests are written against invariants (e.g. INV-TXN: revision
   strictly increases; INV-ANCHOR: anchors survive random edit sequences) — see anti-patterns TEST-*.
4. **They outlive the language.** A future re-implementation in another language is conformant iff it
   upholds these invariants, regardless of internal structure.
