---
doc: rfc
project: ruse
title: "RFC-0009: Render Model & Frontends"
summary: >
  Locks ruse's rendering architecture: a reusable core with thin frontends (the TUI is the first client,
  not the editor); CQRS command/query applied at mutation/remote/plugin boundaries; and ONE semantic
  Render Tree (≡ Render IR) that is lowered per capability to ANSI / Kitty / SIXEL / GUI / Web. Fixes the
  Semantic View Model vs backend-neutral Render IR boundary (plugins target the view model, never the IR),
  pins the compatibility-vs-enhanced render profile per client-view, and makes every pipeline stage
  dumpable via :debug. This is a decision record ratifying D-014/D-015; depth lives in the linked design doc.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../design/render-and-frontends.md
  - ../../parity/terminal.md
  - ../../invariants/reference-invariants.md
  - ../../architecture/architecture.md
  - ../../protocols/versioning-and-evolution.md
  - ../../../spec/DECISIONS.md
---

# RFC-0009: Render Model & Frontends

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-05
- **Decision link:** [D-014](../../../spec/DECISIONS.md) (Semantic View vs Render IR boundary), [D-015](../../../spec/DECISIONS.md) (terminal capability fallback + pinned render profile); relates to [D-012](../../../spec/DECISIONS.md) (multi-client, open)

<!-- RFCs are only for hard-to-reverse decisions. The render boundary and the IR contract are hard to
     reverse: every view, plugin, and future frontend inherits them. This RFC ratifies the design in
     docs/design/render-and-frontends.md and DECISIONS D-014/D-015; it does not re-derive them. -->

## Summary

ruse is structured as **core → command/query → one Render Tree → many frontends**. A reusable Rust core
owns the document/edit/render model; frontends (TUI first, GUI/Web/remote later) are thin clients. Reads
and writes cross the boundary as **Queries** (immutable snapshots) and **Commands** (validated mutations).
All visible output is produced by lowering **a single semantic Render Tree** — the Render IR — per
capability to ANSI cell grids, Kitty images, SIXEL, GPU, or Web canvas. Plugins and views target the
**Semantic View Model**, not the IR; the IR is deliberately backend-neutral (not the union of backends).
Each client-view **pins** one render profile (compatibility | enhanced) and does not flip it on probe
noise. This RFC ratifies [D-014](../../../spec/DECISIONS.md) and [D-015](../../../spec/DECISIONS.md); the
full model with diagrams lives in
[docs/design/render-and-frontends.md](../../design/render-and-frontends.md).

## Motivation / Problem

A TUI-first editor that later wants GUI/Web/remote fails predictably if the terminal *is* the editor:
frontend assumptions leak into the core, every view and plugin emits its own escape sequences, and the
screen becomes an unstable mix of half-supported features. Neovim's history is the cautionary case —
UI logic entangled with core state, and capability handled as scattered booleans keyed off `TERM`.

Three problems must be solved once, structurally, so they are not re-litigated per feature:

1. **Reuse without rewrite.** GUI/Web/remote must attach later without touching core semantics — so the
   TUI must be *a client*, not *the editor* ([architecture.md §3.1](../../architecture/architecture.md)).
2. **One output path, many backends.** Capability differences (a plain terminal vs Kitty vs a GPU window)
   must be a *lowering* concern, not something every producer re-implements — otherwise degradation
   (INV-CAP-DEGRADE) is impossible and plugins race to paint the screen.
3. **A bounded contract.** The stable render contract must not drift into an unbounded legacy DOM that is
   the union of every backend's features (D-014's explicit fear).

## Guide-level explanation

Five commitments define ruse's render model:

1. **Core / frontend separation — the TUI is the first client.** The core owns Document, Transaction,
   Command, Query, Undo, Anchor, Workspace. Frontends sit on top and hold no core state. *The TUI is not
   the editor — it is the first client of the core*
   ([render-and-frontends.md §1](../../design/render-and-frontends.md)). This is what lets GUI/Web attach
   later without a core rewrite (INV-DOC-VIEW).

2. **CQRS at the boundaries.** Mutation (Command) is split from read (Query). Plugins never touch the
   document: `Plugin → CommandRequest → Validation → Transaction → State update`. Queries return a
   **snapshot/DTO**, never a live mutable object. This is applied in its strong form *only* at mutation,
   remote, and plugin boundaries — in-process reads stay direct, to avoid over-designing every getter into
   a message ([render-and-frontends.md §2](../../design/render-and-frontends.md)).

3. **One semantic Render Tree (≡ Render IR), lowered per capability.** Views and plugins produce semantic
   nodes (`Text`, `Image`, `Table`, `Tree`, `Diff`, `Overlay`); the frontend **lowers** them by capability
   to ANSI / Kitty / SIXEL / GUI / Web. A plugin emits an `ImageNode`, never a Kitty escape sequence; the
   TUI backend then chooses real image → Unicode preview → placeholder along the degradation ladder shared
   with [parity/terminal.md](../../parity/terminal.md) (TERM-GFX). Features degrade in *quality*, never
   *disappear* (INV-CAP-DEGRADE).

4. **Semantic View Model vs Render IR are two distinct layers.** Plugins and views target the **Semantic
   View Model** (a higher-level, extension-facing model). The **Render IR** underneath is
   *backend-neutral* — it is intentionally **not the union of backends**. Backend-specific concessions live
   in an isolated *capability namespace*, not smeared through the IR
   ([D-014](../../../spec/DECISIONS.md)). This keeps the IR from becoming an unbounded DOM.

5. **Profile pinned per client-view; pipeline dumpable.** Each client-view probes capability once, decides
   compatibility vs enhanced, records it in the capability ledger (with user override), and **freezes** it.
   The whole transformation `Input → Command → Transaction → Document State → Render Tree → backend output`
   is inspectable stage-by-stage via `:debug`, so a rendering fault is *localized* ("Kitty lowering:
   placement wrong") rather than blamed on "the terminal being weird".

## Reference-level explanation

**The layered contract (top is extension-facing, bottom is bytes):**

```
Semantic View Model        ← plugins/views target THIS (D-014)
        ↓  (compose)
Render Tree  ≡  Render IR   ← the stable, versioned, backend-NEUTRAL contract (schemaVersion, additive)
        ↓  (lower, per capability + pinned profile)
Backend output
  ├─ ANSI cell grid   (compatibility path: Unicode, 256-color, legacy keyboard)
  ├─ Kitty image      ┐
  ├─ SIXEL            ├ enhanced path: truecolor, inline images, synchronized output, Kitty keyboard
  ├─ GUI GPU          │
  └─ Web canvas       ┘
```

- **Render IR node set (illustrative, not the wire type):** `Text | Image | Table | Tree | Diff | Overlay`
  ([render-and-frontends.md §3](../../design/render-and-frontends.md)). The IR carries a `schemaVersion`
  and evolves **additively** per
  [protocols/versioning-and-evolution.md](../../protocols/versioning-and-evolution.md) (INV-ADDITIVE);
  readers handle unknown node/field variants gracefully.

- **Render IR discipline (the RIR-* constraints this RFC locks):**
  - *Backend-neutral, not union-of-backends.* A node exists in the IR only if it has a semantic meaning
    that *every* backend can lower (however coarsely). Backend-only affordances live in a segregated
    capability namespace, never as first-class IR nodes.
  - *No plugin escapes / no pixel coordinates in the IR.* The IR speaks in semantic and cell-relative
    terms; a plugin cannot inject raw escape sequences, and it cannot pin content to absolute pixel
    coordinates that only one backend understands (guards TERMOUT-10, PLUGIN-8/9).
  - *Stable resource handles.* Images and other large resources are referenced by **stable handles**
    (INV-HANDLE-style IDs), not inlined bytes or backend-local addresses — so lowering and caching are a
    backend concern and the IR stays small and diffable.
  - *No callbacks in the IR.* The IR is **data**, not behavior: no closures, function pointers, or
    host-callbacks embedded in nodes. Interaction is expressed as semantic commands flowing back through
    the Command boundary, keeping the IR serializable, snapshot-able, and remote-friendly.

- **Command / Query wire shape:** Commands are semantic, namespaced, typed (INV-CMD-SEMANTIC) and are the
  *only* path to document change (INV-TXN). Queries — `get_visible_lines`, `get_render_snapshot`,
  `get_diagnostics`, `get_available_commands`, … — return immutable DTOs (INV-QUERY-SNAPSHOT). Any
  per-redraw decoration provider is bounded to a **visible-range snapshot** and runs *outside* the paint
  critical section.

- **Profile selection & pinning (D-015):** probe capabilities
  ([terminal.md](../../parity/terminal.md) TERM-PROBE, DA1-fenced — no arbitrary timeouts) → decide tier →
  record in the capability ledger `{value, source, confidence}` with user override → **freeze for the
  client-view**. Re-evaluate *only* on explicit events (resize to a new terminal, override change,
  reconnect). On an unsupported element or runtime lowering failure, the client-view is pinned to the
  **compatibility** path rather than flipping backends mid-frame.

- **Multi-client (V-13):** the profile is pinned per **client-view**, not per session/document. Two clients
  of differing capability attached to one document (e.g. Kitty + a plain terminal) each lower the *shared*
  Render Tree at their own tier. Multi-client attach itself is **post-MVP and gated by
  [D-012](../../../spec/DECISIONS.md) (open)**; per-client-view pinning is the mechanism that makes it
  coherent when D-012 is decided, so nothing in the render model forecloses it.

- **Engineering anchor:** ENG-RENDER-001 is the implementation obligation behind this contract (the
  render-model crate and terminal-client lowering); this RFC is its reference-level statement.

## Reference Invariants

This RFC **depends on and restates** the following IDs from
[reference-invariants.md](../../invariants/reference-invariants.md). It introduces no new INV IDs (new
invariants are minted only in that registry, per D-022).

- **INV-RENDER-IR** — All output is produced by lowering a single semantic Render Tree; no view or plugin
  emits backend-specific bytes (escape sequences, GPU calls) directly. *(Guards TERMOUT-10, PLUGIN-8/9,
  RIR-*.)* — the core commitment (§3, §4).
- **INV-RENDER-PROFILE** — A render profile (compatibility | enhanced) is pinned per **client-view** and
  not switched mid-session on probe noise; with multiple clients, each client-view lowers the shared Render
  Tree at its own tier. *(Guards RENDER-1/3.)* — the pinning rule (§5).
- **INV-QUERY-SNAPSHOT** — Queries return immutable snapshots/DTOs, never live mutable core objects; any
  per-redraw decoration provider is bounded to a visible-range snapshot and runs outside the paint critical
  section. *(Guards CMD-14/15, PLUGIN-2.)* — the read side of CQRS (§2).
- **INV-CAP-DEGRADE** — An unsupported capability degrades (lower quality / fewer features), it does not
  disappear; capability is a confidence ledger with user override, never a bare bool, and never inferred
  from `TERM` alone. *(Guards TERMOUT-11/15/17, TERMIN-1.)* — the degradation ladder (§3) and profile
  fallback (§5).

Reaffirmed (owned by other RFCs): **INV-DOC-VIEW** (core knows no frontend), **INV-CMD-SEMANTIC** /
**INV-TXN** (Command is the only mutation path), **INV-PLUGIN-NO-CORE** (plugins see only the view
model/handles/snapshots), **INV-ADDITIVE** / **INV-PROTOCOL-VERSIONED** (the IR schema evolves additively).

## Failure modes & Recovery

- **Backend lowering failure at runtime** (e.g. a graphics payload rejected mid-frame). *Recovery:* the
  client-view falls back to the **compatibility** path (INV-RENDER-PROFILE); it does not tear the screen by
  flipping backends. The affected node degrades along the ladder (native image → Unicode preview →
  placeholder), never vanishes (INV-CAP-DEGRADE).
- **Unknown IR node/field** (newer producer, older lowerer, or vice versa). *Recovery:* additive-evolution
  reader rules — unknown variants are skipped/rendered as a safe placeholder, not a crash (INV-ADDITIVE).
- **Probe noise / capability wobble.** *Recovery:* pinning absorbs it; the ledger only updates on explicit
  renegotiation events. `TERM`-only inference is disallowed (INV-CAP-DEGRADE).
- **Plugin attempts a raw escape / pixel-pinned output.** *Recovery:* rejected at the view-model boundary —
  there is no IR node to carry it (RIR constraints), so the fault is contained to the plugin, not the
  screen (INV-PLUGIN-NO-CORE, INV-PLUGIN-ISOLATED).

## Security impact

Constraining plugins to the Semantic View Model with **no escapes and no callbacks in the IR** removes a
class of terminal-injection attacks: a plugin cannot smuggle control sequences into the output stream, and
untrusted terminal output cannot become behavior. This complements the paste-neutralization and
host-mediated graphics rules in [parity/terminal.md](../../parity/terminal.md) (TERM-PASTE-2, TERMOUT-10)
and the workspace-trust model (INV-TRUST-1). The IR being pure data also means a remote client renders it
without executing anything the runtime sent.

## Performance impact

An IR + lowering layer adds one indirection and a schema to version. Accepted, and bounded: the IR is
**small and diffable** (stable resource handles instead of inlined bytes; no closures), so redraws diff the
tree rather than re-serializing payloads. Decoration providers are snapshot-bounded to the visible range
and run **outside the paint critical section** (INV-QUERY-SNAPSHOT), keeping plugin work off the hot path.
Synchronized output (TERM-SYNC) is applied at lowering time for tear-free large redraws. Per-stage latency
budgets are governed by [D-019](../../../spec/DECISIONS.md)/ENG-PERF-001, not set here.

## Compatibility & Migration

No prior public render contract exists; nothing to migrate. Forward-compatibility is structural: the Render
IR carries `schemaVersion` and evolves additively
([protocols/versioning-and-evolution.md](../../protocols/versioning-and-evolution.md); INV-ADDITIVE), and
the Semantic View Model is the promotion surface plugins bind to — so backend growth (GUI at F-018, Web
later) is additive lowering, not an IR break. TUI-first does **not** foreclose GUI/Web precisely because of
INV-RENDER-IR.

## Observability

The transformation pipeline is dumpable at every stage — debug surfaces are **product features**, not
ad-hoc logging ([stability-and-observability.md](../../design/stability-and-observability.md)
§Debug-Surfaces):

```
Input → Semantic Command   (:debug keymap / :debug command)
      → Transaction        (:debug transactions)
      → Document State      (:debug document — revision, ranges)
      → Semantic Render Tree (:debug render-tree)
      → Backend output      (:debug capabilities + lowering)
```

The capability ledger and pinned profile are themselves inspectable (`:debug capabilities`), making
"why did this render at the compatibility tier?" a queryable fact.

## Alternatives

- **A1 — Frontends talk to the document model directly.** Rejected (see below). Fast initially, fatal to
  reuse.
- **A2 — Each view/plugin emits its own terminal escapes.** Rejected (see below). Blocks degradation.
- **A3 — Dynamically pick the best renderer per element.** Rejected (see below). Visual instability.
- **A4 — Render IR as an unbounded DOM (union of all backends).** Rejected (see below). D-014's core fear.
- **A5 — Full CQRS everywhere (every getter is a message).** Rejected as over-design; the strong form is
  applied only at mutation/remote/plugin boundaries, in-process reads stay direct
  ([render-and-frontends.md §2](../../design/render-and-frontends.md)).
- **A6 — One profile per session/document (not per client-view).** Rejected: it cannot serve two clients of
  differing capability on one document coherently (V-13); per-client-view pinning is required to keep D-012
  open.

## Rejected approaches

- **Frontends talk to the document model directly.** Rejected: couples every frontend to core internals,
  makes view-local state leak into the document (violates INV-DOC-VIEW), and blocks GUI/Web/remote from
  ever attaching without a core rewrite. → command/query boundary + Render IR. *Recorded so "just let the
  TUI read the rope directly for speed" is not re-proposed.*
- **Each view/plugin emits its own terminal escapes.** Rejected: produces an unstable screen (competing
  writers, conflicting cursor state), makes capability degradation impossible, and is a terminal-injection
  vector. Violates INV-RENDER-IR and TERMOUT-10. → single Render Tree + host-mediated backend lowering.
- **Dynamically pick the "best" renderer per element mid-session.** Rejected: mixing tiers within one screen
  gives visual instability and un-reasoned-about output; a probe wobble would visibly flip elements.
  Violates INV-RENDER-PROFILE. → pin one profile per client-view; re-evaluate only on explicit events.
- **Render IR as an unbounded DOM (the union of every backend's features).** Rejected: it becomes a legacy
  web-DOM — impossible to version, impossible to lower coherently to a weak backend, and a magnet for
  backend-specific leakage. This is exactly the failure D-014 exists to prevent. → backend-*neutral* IR +
  a separate Semantic View Model for plugins + an isolated capability namespace for backend concessions.

## Trade-offs

- **IR + lowering adds indirection and a schema to version.** Accepted: it is the substrate for
  multi-frontend parity, capability degradation, per-stage debuggability, and remote rendering — the point
  of the architecture, not overhead ([render-and-frontends.md §3](../../design/render-and-frontends.md)).
- **Two layers (View Model + IR) is more concept than one.** Accepted: collapsing them is precisely how the
  IR becomes an unbounded DOM (A4/D-014). The seam is where "plugin-facing richness" and "backend-neutral
  minimalism" are allowed to differ.
- **Pinning can leave a client on a lower tier after the environment improves.** Accepted: screen stability
  beats opportunistic upgrades; the explicit renegotiation events (resize/override/reconnect) are the
  escape hatch, and user override always wins.
- **CQRS at boundaries adds message/DTO types.** Accepted only at mutation/remote/plugin edges; in-process
  reads stay direct to avoid over-design.

## Re-evaluation conditions

- **D-014 (View Model ↔ IR boundary)** — revisit as the **GUI backend arrives (F-018)**: a real second
  backend is the first true test of "backend-neutral, not union-of-backends." If a GUI need cannot be met
  without leaking backend specifics into the IR, the capability-namespace mechanism is re-examined — the IR
  staying neutral is not.
- **D-015 (fallback + pinning)** — revisit on the defined renegotiation events (resize/reconnect/override)
  as behavior, not as a policy change; the *policy* re-opens only if pinning demonstrably causes worse UX
  than controlled mid-session upgrades on real terminals.
- **D-012 (multi-client)** — when multi-client attach is decided (before F-017 hardening), confirm
  per-client-view pinning still expresses the chosen optimistic-vs-authoritative sequencing.
- Superseding this RFC requires superseding D-014 and/or D-015 in the same change (never editing this RFC
  alone), and updating [render-and-frontends.md](../../design/render-and-frontends.md) to match.

## Open questions

- **Exact Render IR node set and `schemaVersion` evolution rules** — the illustrative node enum here is not
  the frozen wire type; the concrete schema and its additive-evolution rules are owned by ENG-RENDER-001 and
  [protocols/versioning-and-evolution.md](../../protocols/versioning-and-evolution.md), not decided here.
- **Where the Semantic View Model ends and the Render IR begins** for borderline nodes (e.g. tables/trees
  that some backends render natively) — to be validated against a second backend (D-014 re-evaluation).
- **Multi-client sequencing** (optimistic vs authoritative) for a shared Render Tree — deferred to
  [D-012](../../../spec/DECISIONS.md)/F-017; this RFC only guarantees per-client-view pinning keeps it open.
- **Decoration-provider budget** within the snapshot-bounded, off-critical-section contract — tied to the
  scheduler budgets that are open under [D-018](../../../spec/DECISIONS.md).

## Addendum A (2026-08-20) — Partial single-frontend reactivation for rich in-buffer rendering (F-031)

Sections 1–7 (and RFC-0012) **defer** the full multi-frontend Render Tree "until a second frontend exists."
**F-031 (rich in-buffer rendering — Markdown/Org faces, conceal, virtual text, inline images)** does not add a
second frontend; it adds a second render **tier within the one TUI**. That is a distinct RFC-0012 re-boundary
trigger, and it reintroduces **only** the piece that tier actually needs: a **TUI-local layout pass** from
*(buffer bytes + decorations)* to *display cells*, plus the **decoration model** (the in-line facet of this
RFC's Semantic View Model). Scope is fixed by [D-054](../../../spec/DECISIONS.md):

- **Reactivated (now):** the decoration model (`face | conceal | virt_text | virt_lines | image-handle`,
  anchored, priority-resolved) and the layout pass that owns the buffer↔display coordinate mapping consumed by
  both painting and the caret. This is an internal lowering step, **not** the versioned wire IR.
- **Still deferred (until F-018 GUI):** the backend-neutral, serializable, `schemaVersion`-carrying
  multi-frontend Render Tree; GUI/Web lowering; plugin-authored decorations (F-016). The decoration model is a
  strict **subset** of the eventual IR, so nothing here forecloses the full model — it is the same node
  vocabulary (`Text`/`Image`/`Overlay` → faces/images/virtual), realized single-frontend first.
- **Capability continuity:** the `Image` node + degradation ladder (§3) and INV-CAP-DEGRADE / INV-RENDER-PROFILE
  are honored as specified; inline-image detection extends the F-010 capability ledger with an `InlineGraphics`
  rung ladder (Kitty > Sixel > iTerm2 > None) per [D-053](../../../spec/DECISIONS.md) and RFC-0005. Decoration
  providers remain snapshot-bounded and off the paint critical section (INV-QUERY-SNAPSHOT).

Depth lives in [docs/design/rich-rendering.md](../../design/rich-rendering.md). This addendum records the
scoping only; it neither supersedes nor edits the deferral in §§1–7 — those reactivate in full when the GUI
backend (F-018) lands, at which point the layout pass is re-expressed as the TUI lowering of the shared tree.
