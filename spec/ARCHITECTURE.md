---
doc: architecture-map
project: ruse
title: "ruse — Architecture Map (ARCH-*)"
summary: >
  Concise canonical map of layers, primary flow, ownership, forbidden dependencies, and open structural
  decisions. Home of the stable ARCH-* IDs referenced from POLICY/PRD/DECISIONS. Depth lives under
  docs/design/; invariants in docs/invariants/reference-invariants.md.
audience: [maintainers, contributors, llm-agents]
status: canonical
related:
  - ../docs/architecture/architecture.md
  - ../docs/invariants/reference-invariants.md
  - POLICY.yaml
  - PRD.yaml
---

# Architecture

> Concise canonical map: layers, primary flow, ownership, forbidden dependencies, open decisions. Stable
> IDs (`ARCH-*`) are referenced from POLICY/PRD/DECISIONS. Depth lives in
> [`../docs/design/`](../docs/architecture/architecture.md); invariants in
> [`../docs/invariants/reference-invariants.md`](../docs/invariants/reference-invariants.md).
>
> This map states the target. The v0 editor is two crates, `crates/core` and `apps/tui` (D-039), so stages and
> owners marked *(deferred)* below do not exist in code yet.

## ARCH-LAYER-001 — Layers (dependency flows downward only)

```
Kernel                (Document, Transaction, Command, Anchor, Undo; Query (deferred, #970); Health, Scheduler (deferred))
  → Built-in Services (Workspace, Render, Terminal platform, LSP, Git)
    → Bundled Extensions (core-git, core-search — built on the same stable API)
      → Third-party Plugins (isolated, versioned protocol)
```

Frontends (TUI / GUI / Web / Remote client) are **clients of the Kernel**, not part of it. The TUI is the
first client, not the editor itself.

## ARCH-FLOW-001 — Primary flow (each stage inspectable via `:debug`)

```
Input
  → Input Profile        (Vim / Emacs / Native)
    → Semantic Command
      → Transaction
        → Document (revision++)
          → View Model (deferred, D-039)
            → Render Tree (semantic paint IR; deferred, D-039, the TUI paints directly)
              → Frontend Lowering (ANSI cell grid / Kitty image / GUI / Web)
```

Read side is separate (CQRS at boundaries): `Query → Snapshot/DTO` (never a live mutable object).

## ARCH-OWN-001 — Ownership

- **Document** owns text, encoding, and revision. Knows nothing about Views.
- **View** owns cursor, selection, viewport, folds (view-local state; v0 folds live in the frontend).
- **Scheduler** (deferred) owns background task execution, priority, cancellation, budgets.
- **Health Registry** (deferred) derives system-wide status from per-component state machines.
- **Capability ledger** owns detected terminal/host capabilities + user overrides.
- Long-lived references are **typed, generation-checked handles**, never raw pointers/offsets.

## ARCH-FORBID-001 — Forbidden dependencies

- Document must not depend on View.
- Kernel must not depend on a specific Terminal backend or frontend.
- Plugins must not mutate the Document directly (only via CommandRequest → Transaction).
- Renderer must not execute Commands.
- Views/plugins must not emit backend-specific bytes (escape sequences, GPU calls) — only Render nodes.
- Config/event flow must not let a workspace override security-sensitive settings.

Enforced by `ruse arch deps` (see [POLICY.yaml](POLICY.yaml) ENG-ARCH-001): the crate graph, plus a source
scan of [architecture.yaml](architecture.yaml)'s forbidden module edges (Document → View, kernel → terminal,
input → frontend) and of direct document mutation outside the transaction path (#983). The plugin and
renderer rules return with their deferred boundaries (D-039).

## ARCH-EXEC-001 — Execution model

- Core is a **single-threaded deterministic executor**; observable ordering is preserved.
- Direct call for required sequential steps; **events** for independent consumers; **scheduler tasks** for
  long/cancellable work. Not everything is an event bus; not everything is a direct call.
- Async responses carry request-id + revision; stale results are dropped.

## ARCH-RENDER-001 — Render model

- One semantic **Render Tree** (deferred, D-039, D-054) lowered per capability to multiple backends.
- Two renderer tiers (compatibility / enhanced), **pinned per client-view**, not flipped on probe noise.
- Images degrade: native → Unicode preview → placeholder → external open.

## ARCH-OPEN — Open decisions (tracked in DECISIONS.md)

- External plugin transport specifics (WASM vs process) — D-009 (direction decided, details open).
- Multi-client workspace support — D-012.
- Offline remote editing / reconnect conflict policy — D-013.
- Save/recovery tuning — D-005 (design decided).
- Log PII/redaction field list — D-017.

## Cross-references

| ID | Depth |
| --- | --- |
| ARCH-LAYER-001 | [../docs/architecture/architecture.md](../docs/architecture/architecture.md) §4, [../README.md](../README.md) layout |
| ARCH-FLOW-001 | [../docs/design/render-and-frontends.md](../docs/design/render-and-frontends.md) §5 |
| ARCH-OWN-001 / ARCH-FORBID-001 | [../docs/invariants/reference-invariants.md](../docs/invariants/reference-invariants.md) |
| ARCH-EXEC-001 | [../docs/architecture/architecture.md](../docs/architecture/architecture.md) §8 |
| ARCH-RENDER-001 | [../docs/design/render-and-frontends.md](../docs/design/render-and-frontends.md) §3–4 |
