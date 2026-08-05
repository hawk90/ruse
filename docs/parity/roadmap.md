---
doc: parity-roadmap
project: ruse
title: "Parity: Roadmap & Sequencing"
summary: >
  Narrative sequencing of the parity set across MVP / 1.0 / later, reconciled with the build order and the
  Non-goals. Per-feature state is owned by spec/PRD.yaml (stage/status) — this file is the reference
  narrative and references F-IDs. Vim Style leads; TUI-first; no WASM plugin host in the MVP.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - ../../spec/PRD.yaml
  - ../../spec/DECISIONS.md
---

# Parity: Roadmap & Sequencing

> **Source of truth for per-feature state is [`spec/PRD.yaml`](../../spec/PRD.yaml)** (`stage`/`status`).
> This file is the narrative; it references `F-*` IDs and must not duplicate their status. Sequencing
> follows the build order in [`docs/README.md`](../README.md) and the Non-goals in
> [`spec/PROJECT.md`](../../spec/PROJECT.md). It reconciles (and supersedes) earlier rough roadmaps: Vim
> Style leads, TUI-first, and the public plugin host is **post-MVP** (D-009).

## Build order (recap)

```
0 RFCs → 1 editor-core (Document, Transaction, Command) → 2 input-engine (Vim) → 3 tui-client
→ 4 workspace (local buffers, save/recovery) → 5 plugin-api → 6 remote-runtime → 7 GUI → 8 Marketplace → 9 AI
```
(Command is part of editor-core; a local workspace stage precedes plugins — see verification V-10/V-11.)

## MVP — a usable Vim-style TUI editor on a solid core

Goal: prove the core contracts end-to-end (key → command → transaction → document → render).

- Core: **F-001** transactional editing, **F-002** document & coordinate model, **F-005** undo grouping.
- Input: **F-003** Vim profile (Normal/Insert, operator+motion, counts, basic text objects).
- Commands: **F-004** semantic command engine + palette.
- UI: **F-006** TUI rendering (ANSI/Unicode, render-diff), **F-007** buffers/views/windows.
- Files: **F-008** open/save + crash-recovery journal.
- Extras: **F-009** search & substitute, **F-010** terminal capability detection.
- Foundations present but minimal: observability (health/status), single-thread deterministic executor,
  internal extension API (no public plugin host yet).

Explicitly **not** in MVP: Emacs/Native profiles complete, LSP, tree-sitter, public plugin host/WASM,
remote, GUI, marketplace, AI (Non-goals; D-020).

## 1.0 — a genuinely useful editor with an ecosystem

- Vim: broaden operation parity toward the L2 checklist ([vim.md](vim.md)).
- Emacs: **F-012** profile — kill ring, mark/region, prefix keys, universal argument, major/minor modes.
- Language: **F-014** built-in LSP, **F-015** tree-sitter highlighting.
- Workspace: **F-011** PTY terminal buffer; Git / search / diagnostics buffers ([workspace.md](workspace.md)).
- Ecosystem: **F-016** plugin protocol + host (versioned, isolated); per-profile recommended keymaps;
  compatibility CI.
- Remote: **F-017** SSH/WSL runtime with reconnect; image fallback ladder.

## Later — platform breadth

- **F-013** Native Style advancement (incl. multi-selection).
- DAP/debugger views; container remote; **F-018** GUI client (same command system + Render IR).
- **F-019** marketplace (signing, verification levels, lockfile); **F-020** AI (proposals reviewed before
  apply); large plugins of the Org/Magit *kind* (built on the stable API, not ported).

## Parity-level expectations by milestone

| Milestone | Vim | Emacs | Native | Plugin |
| --- | --- | --- | --- | --- |
| MVP | L1 core + some L2 | — | text-editing only | internal API |
| 1.0 | L2 core checklist | L1 + key L2 | L1 | stable public API |
| Later | broad L2 | L1 broadened | L1 + multi-select L2 | marketplace + governance |

Parity % is weighted by usage/importance and computed from CI fixtures, not doc tables
([../operations/ci-cd-and-release.md](../operations/ci-cd-and-release.md) §9).
