---
doc: parity-workspace
project: ruse
title: "Parity: Workspace Surfaces (Everything Is a Buffer/View)"
summary: >
  Combining Emacs's "everything is a buffer" with the Neovim ecosystem: file explorer, Git (Magit-style),
  search results, diagnostics, terminal, help, debugger, AI, image, hex/binary, and remote files are all
  workspace views over a shared semantic view model — not bespoke UI per feature, nor everything forced
  into a text buffer.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - emacs.md
  - neovim.md
  - ../architecture/architecture.md
  - ../design/render-and-frontends.md
---

# Parity: Workspace Surfaces

Every workspace surface is a **view over a shared semantic view model** (not a cell grid, not a bespoke UI
engine per feature). This is Emacs's "everything is a buffer" ([emacs.md](emacs.md) EMACS-BUFFER) realized
through ruse's Render IR ([../design/render-and-frontends.md](../design/render-and-frontends.md)).

| ID | Surface | Target form | Target |
| --- | --- | --- | --- |
| WS-1 | File explorer | Tree/List buffer (dired/wdired-style editability) | L1 |
| WS-2 | Git status | Magit-style transient action buffer | L1 |
| WS-3 | Search results | Navigable results buffer | L1 |
| WS-4 | Diagnostics | Problems buffer (from the diagnostics framework, [neovim.md](neovim.md) NVIM-DIAG) | L1 |
| WS-5 | Terminal | PTY-backed buffer (Unix PTY + ConPTY) | L1 |
| WS-6 | Help | Documentation buffer (command↔doc coupling, EMACS-HELP) | L1 |
| WS-7 | Debugger | Stack / Variables / Console views | post-MVP |
| WS-8 | AI | Chat / Proposal / Review buffer (proposals reviewed before apply) | future |
| WS-9 | Image | Semantic media view (degradation ladder) | L1 |
| WS-10 | Hex / Binary | Typed binary view | post-MVP |
| WS-11 | Remote file | Remote workspace document ([remote.md](remote.md)) | L1 |

## Virtual-document kinds

Reuse common view primitives but distinguish semantics (design-requirements §12):

| Kind | Example |
| --- | --- |
| Editable Document | source file |
| Read-only Document | Git revision, LSP virtual document |
| Generated Document | help, command output |
| Streaming Document | logs, build output |
| Interactive View | git status, file tree, debugger |

> **Interactive-view write-back (verification V-14):** editing an interactive/generated buffer (dired/wdired
> rename, Magit stage/unstage) is **not** a text Transaction against a Document. Such a view defines a
> contract that turns buffer edits into **typed domain CommandRequests** (rename/stage/delete/…), each run
> through preflight ([../design/stability-and-observability.md](../design/stability-and-observability.md)
> §13) and its own service — separate from `INV-TXN` document transactions. The view then re-renders from
> the new domain state. This is why buffer *kind* (INV-BUFFER-KIND) determines the mutation contract.

## Principles

- Do not clone a VSCode layout onto the TUI verbatim; on a narrow screen apply **priority-based
  degradation**, not feature removal (UI-1/3/18).
- Neither force everything into a text buffer (UI-4) nor build a fully custom UI per view (UI-5) — provide a
  **semantic view model** (UI-10).
- Buffer ≠ View ≠ Window; one buffer opens in multiple views; view-local state stays in the view
  (INV-DOC-VIEW; UI-6/7/8).
- Same command system across TUI/GUI/Web (UI-11); status line shows current mode/prefix (UI-17); status is a
  rendered Health Registry, not owned by the UI (INV-STATUS).
- Layout lifecycle ≠ view lifecycle: a closed view must not keep holding its document/tasks
  (design-requirements §11).

## Reference Invariants
INV-DOC-VIEW, INV-RENDER-IR, INV-STATUS.
