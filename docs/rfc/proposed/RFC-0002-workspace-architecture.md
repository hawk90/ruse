---
doc: rfc
project: ruse
title: "RFC-0002: Workspace Architecture"
summary: >
  Establishes the workspace as ruse's UI substrate: a strict downward-only layer model
  (Kernel → Built-in Services → Bundled Extensions → Third-party Plugins), the "everything is a
  workspace view/buffer" model, the Buffer ≠ View ≠ Window ≠ File separation, virtual-document kinds
  keyed to INV-BUFFER-KIND, and the interactive-view write-back contract that turns buffer edits into
  typed domain CommandRequests rather than text transactions.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/ARCHITECTURE.md
  - ../../architecture/architecture.md
  - ../../parity/workspace.md
  - ../../invariants/reference-invariants.md
---

# RFC-0002: Workspace Architecture

- **Status:** proposed
- **Author(s):** hawking90a@gmail.com
- **Created:** 2026-08-05
- **Decision link:** <D-xxx once accepted>

<!-- Hard-to-reverse: this fixes the layer model, the buffer/view/window/file boundary, and the
     mutation contract for non-text surfaces. Ecosystem code depends on all three. -->

## Summary

The workspace is ruse's UI substrate. This RFC fixes three hard-to-reverse decisions: (1) a strictly
downward layer model — Kernel → Built-in Services → Bundled Extensions → Third-party Plugins — with
forbidden reverse dependencies ([ARCH-LAYER-001](../../../spec/ARCHITECTURE.md), [ARCH-FORBID-001](../../../spec/ARCHITECTURE.md));
(2) "everything is a workspace view or buffer" over a shared semantic view model, keeping
Buffer ≠ View ≠ Window ≠ File distinct; and (3) a per-*kind* mutation contract where interactive
and generated surfaces write back through **typed domain CommandRequests**, not text transactions.
It does not introduce new invariants; it restates and binds together the invariants that
[design §7](../../architecture/architecture.md) and [parity/workspace.md](../../parity/workspace.md)
already depend on.

## Motivation / Problem

Neovim's instability comes largely from surfaces reaching sideways or upward: plugins mutate core
state, views emit terminal bytes directly, and everything that is not a file gets forced into a text
buffer (or, conversely, grows a bespoke UI engine per feature). ruse needs a workspace model that
(a) lets every surface — file tree, Git, search, diagnostics, terminal, help, debugger, AI, image,
hex, remote file ([parity WS-1..WS-11](../../parity/workspace.md)) — be a first-class navigable
view, while (b) preventing the dependency tangle and the "one giant `EditorState`" that make such
systems break. The load-bearing question is *what a "buffer edit" means* when the buffer is a Magit
status view, not a text file. Answering it wrong forces either a fake text layer under every view or
an ad-hoc side channel per feature.

## Guide-level explanation

**Layers (dependency flows downward only, [ARCH-LAYER-001](../../../spec/ARCHITECTURE.md)).**

```
Kernel                (Document, Transaction, Command, Query, Anchor, Undo, Health, Scheduler)
  → Built-in Services (Workspace, Render, Terminal platform, LSP, Git)
    → Bundled Extensions (core-git, core-search — same stable API as third parties)
      → Third-party Plugins (isolated, versioned protocol)
```

The **Workspace** is a Built-in Service, not part of the Kernel: the Kernel owns Documents and
Transactions and knows nothing about views, panes, or layout. Frontends (TUI/GUI/Web/remote client)
are *clients of the Kernel*, never members of a layer. Bundled extensions (`core-git`, `core-search`)
are built on the exact same stable API third-party plugins get — a bundled extension has no private
door into a lower layer. This is what keeps the API honest.

**Everything is a workspace view/buffer.** Every surface is a *view over a shared semantic view model*
([parity/workspace.md](../../parity/workspace.md)), lowered to output through the single Render Tree
(INV-RENDER-IR), never a hand-drawn cell grid and never a bespoke UI engine per feature. A file tree,
a Git status buffer, a problems list, and a source file are the same *kind of thing* to the layout
engine, differing only in their buffer *kind* and their command set.

**Buffer ≠ View ≠ Window ≠ File** ([D-003](../../../spec/DECISIONS.md), [ARCH-OWN-001](../../../spec/ARCHITECTURE.md)).
A **File** is bytes on a filesystem. A **Buffer/Document** is loaded content with revision and encoding
(one File may back zero or many). A **View** owns cursor, selection, viewport, and folds — one Buffer
opens in many Views with independent cursors ([F-007](../../../spec/PRD.yaml)). A **Window/pane** is a
layout slot that hosts a View. Layout lifecycle is not view lifecycle: closing a pane must not keep the
Document or its background tasks alive.

**Virtual-document kinds** ([parity/workspace.md](../../parity/workspace.md#virtual-document-kinds)).
Surfaces reuse view primitives but carry an explicit *kind* that decides the mutation contract:

| Kind | Example | Mutation contract |
| --- | --- | --- |
| Editable Document | source file | text Transaction (INV-TXN / INV-UNDO) |
| Read-only Document | Git revision, LSP virtual doc | none (rejected at preflight) |
| Generated Document | help, command output | regenerate from source; no text Txn |
| Streaming Document | logs, build output | append-only, bounded/absent undo (INV-BUFFER-KIND) |
| Interactive View | git status, file tree, debugger | typed domain CommandRequest (write-back contract, below) |

**Interactive-view write-back.** Editing a `dired`/`wdired` rename, or a Magit stage/unstage, *looks*
like editing a buffer but is **not** a text Transaction against a Document. The view defines a contract
that translates the buffer edit into a **typed domain CommandRequest** — `rename`, `stage`, `delete`,
`step` — each run through preflight and its own Built-in Service, then re-renders from the resulting
domain state. Buffer *kind* (INV-BUFFER-KIND) is precisely what selects "text Transaction" vs "domain
CommandRequest," which is why kind is a first-class property and not cosmetic.

## Reference-level explanation

**Ownership** ([ARCH-OWN-001](../../../spec/ARCHITECTURE.md)). Document owns text/encoding/revision and
never references a View (INV-DOC-VIEW). View owns view-local state. References across the boundary are
typed, generation-checked handles (INV-HANDLE), never raw pointers or offsets.

**Write-back is CQRS-shaped.** An interactive view's read side is `Query → immutable snapshot/DTO`
(INV-QUERY-SNAPSHOT) — the view renders from a snapshot of domain state (the Git index, the filesystem
tree), never a live mutable object. Its write side is a `CommandRequest` carrying typed arguments, a
declared origin (INV-ORIGIN), and idempotency where the domain allows. The request is validated at
preflight before any service touches state; failure leaves no partial mutation. After the service
mutates the domain, the view re-queries and re-renders. There is no text buffer under a Magit view
that must be kept "in sync" — the buffer content *is* a projection of domain state.

**Text transactions are unchanged** by this RFC: editing an Editable Document still goes through
INV-TXN with `base_revision`, and generates undoable logical-unit history (INV-UNDO). The novelty is
only that non-editable kinds have a *different, explicit* contract instead of being coerced into text.

**Output.** All kinds lower through one semantic Render Tree (INV-RENDER-IR); no view or plugin emits
escape sequences or GPU calls. The status line is a *rendered* Health Registry subscription, not
UI-owned state (INV-STATUS). Async results carry request-id + revision and stale ones are dropped
(INV-ASYNC-ORDER), so a slow `stage` result cannot clobber a newer view state.

**Forbidden dependencies** ([ARCH-FORBID-001](../../../spec/ARCHITECTURE.md)) that this model enforces:
Document must not depend on View; Kernel must not depend on a frontend/terminal backend; plugins must
not mutate the Document directly (only via CommandRequest → Transaction); the renderer must not execute
Commands; views/plugins must not emit backend-specific bytes; a workspace must not override
security-sensitive settings. These are checked by architecture/dependency lint (POLICY ENG-ARCH-001).

## Reference Invariants

This RFC introduces no new invariants; it depends on and binds together:

- **INV-DOC-VIEW** — Document ≠ View ≠ Window ≠ File; one Document, many Views; no view-local state in
  the Document. The spine of the Buffer/View/Window separation.
- **INV-BUFFER-KIND** — A buffer's kind (editable / read-only / generated / streaming / interactive)
  determines its mutation contract; ephemeral/append-only kinds are an explicit exception to
  INV-TXN/INV-UNDO. This is what makes the write-back table above legitimate.
- **INV-STATUS** — Status is a per-component state machine aggregated into a Health Registry; the status
  bar only renders a subscription and never owns the state.
- **INV-RENDER-IR** — All surfaces lower through one semantic Render Tree; no backend-specific bytes.

Also relied upon: INV-HANDLE, INV-QUERY-SNAPSHOT, INV-ORIGIN, INV-ASYNC-ORDER, INV-TXN, INV-UNDO
(see [reference-invariants.md](../../invariants/reference-invariants.md)).

## Failure modes & Recovery

- **Stale write-back** — a CommandRequest computed against an outdated snapshot: rejected at preflight
  on revision/generation mismatch (INV-ASYNC-ORDER, INV-HANDLE); the view re-queries and re-renders.
- **Dangling view** — a closed pane leaking its Document/tasks: layout lifecycle is decoupled from view
  lifecycle; closing a View releases its handles and cancels its scheduler tasks.
- **Service failure mid-write-back** — e.g. `git stage` fails: typed error (INV-ERR-CLASS), no partial
  mutation, view re-renders unchanged domain state; blast radius bounded to the surface (INV-FAIL-BOUNDED).

## Security impact

Interactive views route mutation through services whose side effects are capability-gated and
trust-scoped (INV-TRUST-1): a workspace or plugin cannot smuggle a filesystem/Git write by "editing a
buffer," because the write-back is an explicit, validated CommandRequest, not a raw text edit. A
workspace must not override security-sensitive settings (ARCH-FORBID-001). Terminal-output and AI
principals stay untrusted; AI-proposed edits are reviewed before apply.

## Performance impact

Read side is snapshot-bounded (INV-QUERY-SNAPSHOT): views re-render from immutable projections, no deep
document clones, decoration providers bounded to the visible range. Write-back is coarse-grained (one
domain CommandRequest per user action), avoiding chatty per-cell IPC. Output uses render-diff, not
full-frame reassembly. Streaming kinds append with bounded/absent undo (INV-BUFFER-KIND) so log/build
buffers do not accrue unbounded transaction history.

## Compatibility & Migration

New subsystem; nothing to migrate. Buffer *kind* becomes part of the view-model contract that plugins
program against; adding kinds is additive (INV-ADDITIVE). The write-back contract is a stable,
namespaced-command surface (INV-CMD-SEMANTIC, [D-006](../../../spec/DECISIONS.md)); interactive views
supplied by third parties reach it through the same versioned protocol as bundled extensions
([D-004](../../../spec/DECISIONS.md)).

## Observability

Each stage of the primary flow is inspectable via `:debug` (ARCH-FLOW-001). Every write-back carries an
origin (INV-ORIGIN) and request-id + revision, so a surface's mutations are auditable. Per-surface
health feeds the Health Registry (INV-STATUS).

## Alternatives

- **Text-buffer substrate with an overlay side channel** — keep a real text buffer under every view and
  bolt domain actions on via keymaps. Rejected below.
- **Bespoke UI per surface** — let each feature draw its own widget tree. Rejected below.
- **Cloned VSCode pane/panel layout ported to the TUI** — Rejected below.
- **Kind as a soft hint** — a single mutable buffer type with kind advisory only. Rejected: it collapses
  the very distinction (INV-BUFFER-KIND) that makes write-back safe; nothing then stops a text edit from
  reaching a Git index.

## Rejected approaches

- **VSCode layout cloned to the TUI verbatim.** Rejected: a fixed sidebar/panel/editor grid does not
  degrade into a narrow terminal, and it couples layout to a specific frontend, violating the
  Kernel-independent-of-frontend rule (ARCH-FORBID-001). ruse uses priority-based degradation over a
  semantic view model instead ([parity principles](../../parity/workspace.md#principles)).
- **Everything forced into a text buffer.** Rejected: representing a Git status view or a debugger as
  editable text means every non-text action becomes text parsing, undo semantics become nonsensical
  (what does "undo" mean on a stage line?), and it directly contradicts INV-BUFFER-KIND. The
  interactive-view write-back contract exists precisely so these surfaces are *not* text.
- **Bespoke UI per view.** Rejected: a custom widget/render path per feature reproduces Neovim's
  divergence, forces per-surface backend code (violating INV-RENDER-IR), and prevents a single command
  system across TUI/GUI/Web. All surfaces share the semantic view model and lower through one Render Tree.

## Trade-offs

- **More concepts up front.** Authors must know a surface's *kind* and, for interactive views, define a
  write-back mapping — heavier than "just write to a buffer." The payoff is that mutation is typed,
  validated, and trust-scoped rather than implicit.
- **Two mutation contracts** (text Transaction vs domain CommandRequest) rather than one. This is
  deliberate: unifying them would either fake text under domain views or weaken transactional guarantees
  on real documents. INV-BUFFER-KIND makes the split explicit instead of accidental.
- **Snapshot re-render on write-back** costs a re-query versus in-place buffer mutation, in exchange for
  a single source of truth (domain state) and no sync-drift bugs.

## Re-evaluation conditions

- A multi-client workspace decision ([D-012](../../../spec/DECISIONS.md)) that requires per-client view
  state semantics beyond what View-local ownership provides.
- The semantic-view-model ↔ Render IR boundary ([D-014](../../../spec/DECISIONS.md)) shifting as the GUI
  backend (F-018) lands, if it changes how interactive views are lowered.
- Evidence that the two-contract model blocks a needed surface — which would reopen INV-BUFFER-KIND,
  not just this RFC.

## Open questions

- Exact schema of the write-back contract descriptor (how an interactive view declares its
  edit → CommandRequest mapping) — to be pinned before the first interactive view (WS-1/WS-2) ships.
- Whether Generated Documents that are cheaply *partially* regenerable warrant an incremental
  re-render path, or always full regenerate.
- Undo affordance for interactive views: whether a domain-level "undo last action" is offered per
  service and how it relates to (and stays separate from) INV-UNDO document history.
