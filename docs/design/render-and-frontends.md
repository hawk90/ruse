---
doc: render-and-frontends
project: ruse
title: "ruse Render Model & Frontend Separation"
summary: >
  How ruse separates a reusable core engine from thin frontends, splits Command (mutation) from Query
  (read), and lowers a single semantic Render Tree (paint IR) to multiple backends (ANSI, Kitty, SIXEL,
  GUI, Web). Includes the compatibility-vs-enhanced render profile (pinned per client-view), the full
  transformation pipeline, and the thin-app/engine split.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - ../protocols/versioning-and-evolution.md
  - ../invariants/reference-invariants.md
  - ../parity/terminal.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Render Model & Frontend Separation

ruse is structured as **core → command/query → render IR → multiple frontends**: a Rust core owns
document/edit/render, and TUI/GUI/Web/remote frontends sit on top.

## 1. Core / Frontend Separation

The core owns Document, Transaction, Command, Query, Undo, Anchor, Workspace. Frontends (TUI, GUI, Web,
Remote Client) sit on top.

```
Editor Core                    Frontends
├─ Document                    ├─ TUI
├─ Transaction                 ├─ GUI
├─ Command                     ├─ Web
├─ Query                       └─ Remote Client
├─ Undo
├─ Anchor
└─ Workspace
```

**Governing principle:** *the TUI is not the editor — it is the first client of the core.* Holding this
lets GUI/Web attach later without rewriting the core. (Invariant: core has no knowledge of a specific
frontend — see [architecture.md](../architecture/architecture.md) §3.1; guards CORE-6/8/18/20.)

## 2. Command / Query Separation (CQRS, applied at boundaries)

The core splits **mutation (Command)** from **read (Query)**.

| Command (mutation) | Query (read) |
| --- | --- |
| `insert_text`, `delete_range`, `change_selection`, `open_document`, `split_view` | `get_visible_lines`, `get_cursor_position`, `get_diagnostics`, `get_render_snapshot`, `get_available_commands` |

Plugins never touch the document directly:

```
Plugin → CommandRequest → Validation → Transaction → State update
```

Queries return a **snapshot / DTO**, never a live mutable object — this raises stability (a stale/aliased
mutable handle can't corrupt core state).

> **Do not over-apply CQRS.** Turning every getter into a message adds complexity. Apply the strong form
> only at **mutation, remote, and plugin boundaries**; in-process reads can stay direct.
> (Guards CMD-14/15, PLUGIN-2/3; upholds INV-CMD-SEMANTIC, INV-PLUGIN-NO-CORE.)

## 3. Common Render IR (Semantic Render Tree)

ruse places a **common paint IR before any output backend**. Plugins and views produce semantic nodes;
backends lower them per capability.

```
Semantic View
    ↓
Render Tree (paint IR)        ← this is the stable, versioned contract
    ↓
Frontend Lowering
    ├─ ANSI Cell Grid
    ├─ Kitty Image
    ├─ SIXEL
    ├─ GUI GPU
    └─ Web Canvas
```

```rust
pub enum RenderNode {
    Text(TextNode),
    Image(ImageNode),
    Table(TableNode),
    Tree(TreeNode),
    Diff(DiffNode),
    Overlay(OverlayNode),
}
```

**Plugins emit `ImageNode`, never a Kitty escape sequence.** The TUI backend then lowers by capability:

```
Kitty            → real image
plain terminal   → Unicode preview
low-capability   → image-info placeholder
```

This is the same degradation ladder as the terminal parity doc ([parity/terminal.md](../parity/terminal.md)
TERM-GFX) and is a **core product differentiator**: features degrade in quality, never disappear
(INV-CAP-DEGRADE; guards TERMOUT-10/11, PLUGIN-8/9).

The Render Tree carries a `schemaVersion` and evolves additively — see
[protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md).

## 4. Compatibility Path vs Enhanced Path (pinned per client-view)

Two renderer tiers, chosen once and held:

| Compatibility Renderer | Enhanced Renderer |
| --- | --- |
| ANSI, Unicode, 256 colors, legacy keyboard | true color, Kitty keyboard, synchronized output, inline images |

**Key rule:** rather than mixing features and getting an unstable screen, **pin a render profile per
client-view.** Once `terminal profile = compatible` is decided for a client-view, it uses the ANSI/Unicode
path stably even if a later capability probe result wobbles — the backend does **not** keep switching.

> **Multi-client (verification V-13):** the profile is pinned per **client-view**, not per session/document —
> so two clients of differing capability (e.g. Kitty + a plain terminal) attached to the same document each
> lower the *shared Render Tree* at their own tier. Multi-client attach itself is post-MVP and gated by
> **DECISIONS D-012**; per-client-view pinning is the mechanism that makes it coherent.

On unsupported elements or runtime failure, the document is pinned to the compatibility path. (Guards the
RENDER anti-pattern "flipping render backend mid-session"; upholds INV-CAP-DEGRADE.)

Selection logic: probe capabilities ([parity/terminal.md](../parity/terminal.md) TERM-PROBE) → decide tier
→ record in the capability ledger with a user override → **freeze for the client-view**. Re-evaluate only
on explicit events (resize to a new terminal, user override change, reconnect).

## 5. The Transformation Pipeline (each stage dumpable)

The editor is a multi-stage transformation system; every intermediate result must be inspectable:

```
Input
  → Semantic Command        (:debug keymap / :debug command)
    → Transaction           (:debug transactions)
      → Document State       (:debug document — revision, ranges)
        → Semantic Render Tree   (:debug render-tree)
          → Terminal-specific Output  (:debug capabilities + lowering)
```

Benefit: when something looks wrong, you don't say "the terminal is weird" — you localize precisely:

```
Command:      OK
Transaction:  OK
Document rev: OK
Render Tree:  OK
Kitty lowering: placement is wrong   ← found it
```

Debug surfaces are **product features**, not ad-hoc logging — specified in
[stability-and-observability.md](stability-and-observability.md) §Debug-Surfaces.

## 6. Thin App + Reusable Engine

Keep apps thin so the TUI doesn't become a monolith; the engine is reusable.

```
workspace-kernel          editor-tui                 editor-gui
├─ document               ├─ terminal input          ├─ native window
├─ command                ├─ ANSI rendering          ├─ GPU rendering
├─ plugin                 ├─ clipboard               └─ OS integration
├─ remote                 └─ process bootstrap
└─ render-model
```

Maps to the repo layout in [../README.md](../README.md): `crates/*` = engine, `apps/*` = thin frontends.

## 7. Structural Guardrails

- **No too-wide monorepo.** A root that mixes core, web studio, browser/IDE extensions, mobile, npm,
  samples, and generated outputs becomes unmanageable. ruse splits strictly early (`crates/ apps/
  extensions/ sdk/ tests/ docs/ tools/`) and keeps sample outputs / large corpora in a separate repo or
  Git LFS.
- **Too many backends early.** Follow the render sequencing in §4 of [../README.md](../README.md).
- **Folder ≠ crate.** Start with big crate boundaries (`editor-core`, `terminal-client`,
  `workspace-runtime`, `plugin-protocol`); keep `document/transaction/command/anchor` as modules inside
  `core` until a real boundary demands a split. (Guards CORE-11/12/13.)

## Reference Invariants (this doc)

- **INV-RENDER-IR** — All output is produced by lowering a single semantic Render Tree; no view or plugin
  emits backend-specific bytes (escape sequences, GPU calls) directly. (Guards TERMOUT-10, PLUGIN-8/9.)
- **INV-RENDER-PROFILE** — A render profile (compatibility | enhanced) is pinned per client-view and not
  switched mid-session on probe noise. (Guards RENDER anti-patterns; upholds INV-CAP-DEGRADE.)
- **INV-QUERY-SNAPSHOT** — Queries return immutable snapshots/DTOs, never live mutable core objects.
  (Guards CMD-14/15.)
- Reaffirms **INV-DOC-VIEW** (core knows no frontend) and **INV-CAP-DEGRADE** (features degrade, not vanish).

## Alternatives / Rejected Ideas / Trade-offs

- **Rejected: frontends talk to the document model directly.** Fast initially; couples every frontend to
  core internals and blocks GUI/Web/remote. → command/query boundary + render IR.
- **Rejected: each view/plugin emits its own terminal escapes.** Produces unstable screens and blocks
  capability degradation. → single Render Tree + backend lowering.
- **Rejected: dynamically pick the best renderer per element.** Visual instability; hard to reason about. →
  pin one profile per client-view.
- **Trade-off:** an IR + lowering layer adds indirection and a schema to version. Accepted: it is the
  substrate for multi-frontend parity, capability degradation, and per-stage debuggability — the point of
  the architecture.
- **Trade-off:** CQRS at boundaries adds message types. Accepted only at mutation/remote/plugin edges;
  in-process reads stay direct to avoid over-design.
