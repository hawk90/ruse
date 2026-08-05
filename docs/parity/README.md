---
doc: parity-index
project: ruse
title: "ruse Feature-Parity Reference — Index"
summary: >
  Index and methodology for ruse's feature-parity target set against Vim, Neovim, and Emacs.
  Parity is defined at three levels (L1 feature / L2 interaction / L3 config-plugin-compat).
  Each editor/domain has its own file; upstream sources are cited per file.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md
  - ../anti-patterns/anti-patterns.md
---

# ruse Feature-Parity Reference — Index

The product goal: **inherit the editing language and dev features of Vim/Neovim, and the
command/buffer/extension model of Emacs, as feature-parity targets. Do NOT replicate their internal
structures or script runtimes.** Nano is not a compatibility target — it is only a reference for a
future "Simple Style" for new users who find modes heavy.

## Parity Levels

Parity is not "clone the implementation." It is graded:

| Level | Name | Meaning | Initial scope |
| --- | --- | --- | --- |
| **L1** | Feature parity | The capability exists and is usable | Primary target for MVP–1.0 |
| **L2** | Interaction / behavior parity | Specific key sequences and observable behavior match | Only for a few core areas early |
| **L3** | Config / plugin compatibility | Existing configs/plugins/scripts run | Explicitly out of scope early |

Start L1-centric; take only select core flows to L2. L3 (running actual Vimscript/Elisp/Lua plugins)
is a non-goal for the redesign — the plugin story is ruse's own stable API (see
[../architecture/architecture.md](../architecture/architecture.md) §4 and [plugin-ecosystem.md](plugin-ecosystem.md)).

### Compatibility Dimensions & Per-Feature Levels

Parity is measured along dimensions — **Syntax · Semantic · Observable-behavior · Workflow · Plugin · Bug
compatibility** — not a flat feature list. Each feature is additionally tagged with a compatibility level:

| Level | Meaning |
| --- | --- |
| **Exact** | byte-for-byte same observable behavior |
| **Equivalent** | same outcome, different mechanism |
| **Adapted** | intentionally reshaped to fit ruse's model |
| **Unsupported** | not provided |
| **Intentionally-different** | deliberately diverges (documented) |

Behavior tests assert cursor, register/kill ring, mode, selection shape, undo grouping, and error timing —
not just final text. Parity % is weighted by **usage frequency and importance**, not feature count. Bug
compatibility is decided explicitly per behavior. See
[../architecture/design-requirements.md](../architecture/design-requirements.md) §2 and the CI parity gate in
[../operations/ci-cd-and-release.md](../operations/ci-cd-and-release.md) §3.

## Files

| File | Scope |
| --- | --- |
| [common.md](common.md) | Cross-editor common capabilities (files, buffers, windows, search, undo, macros, clipboard) |
| [vim.md](vim.md) | Vim editing language: modes, operator+motion, text objects, registers, marks, folds, quickfix, Ex |
| [neovim.md](neovim.md) | Neovim additions: LSP, Tree-sitter, extmarks, jobs, RPC/UI protocol, Lua API, `:terminal` |
| [emacs.md](emacs.md) | Emacs interaction & extension model: prefix keys, M-x, mark/region, kill ring, modes, minibuffer |
| [native-style.md](native-style.md) | ruse's own third input language (Native Style) |
| [workspace.md](workspace.md) | Everything-is-a-buffer/view workspace surfaces (git, tree, diagnostics, terminal, debugger, AI) |
| [remote.md](remote.md) | Remote-development parity (SSH/WSL/containers), TUI-first |
| [terminal.md](terminal.md) | Terminal input/rendering capability parity |
| [plugin-ecosystem.md](plugin-ecosystem.md) | Ecosystem-parity foundations (stable command IDs, versioned API, lockfile, compat CI) |
| [roadmap.md](roadmap.md) | MVP / 1.0 / later sequencing of the parity set |

## Reading the Feature Tables

Editor-specific files use a common table shape:

```
| ID | Feature | One-line | Example | Target level | Notes |
```

- **ID**: stable, e.g. `VIM-OP-3`, `EMACS-KILL-2` — reference these from PRDs and PRs.
- **Target level**: L1/L2/L3 as above.
- **Notes**: edge cases a compatibility layer must get exactly right.

## Methodology

Editor feature inventories are grounded in authoritative upstream sources (official manuals, `:help`,
and source repos), researched per file and cited inline. Where the design already takes a position
(e.g. unified kill-ring/register model), the parity file links to the relevant
[architecture.md](../architecture/architecture.md) section and notes the semantic model to reproduce —
not just the keystroke.
