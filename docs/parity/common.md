---
doc: parity-common
project: ruse
title: "Parity: Common Editor Capabilities"
summary: >
  Cross-editor capabilities every profile shares: file open/save/reload, multiple buffers, splits, tabs/
  workspaces, search & substitute, regex, undo/redo + undo tree, session recovery, macros, unified
  clipboard/register/kill-ring model, large files, encoding/line endings.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - vim.md
  - emacs.md
  - ../architecture/architecture.md
---

# Parity: Common Editor Capabilities

Capabilities shared by Vim, Neovim, and Emacs — provided once in the core and surfaced through every
profile. Profile-specific keystrokes are in [vim.md](vim.md) / [emacs.md](emacs.md) / [native-style.md](native-style.md).

| ID | Capability | Vim | Neovim | Emacs | Target |
| --- | --- | --- | --- | --- | --- |
| COM-1 | Open / save / reload files | ✓ | ✓ | ✓ | L1 required |
| COM-2 | Multiple buffers | ✓ | ✓ | ✓ | L1 required |
| COM-3 | Window splits | ✓ | ✓ | ✓ | L1 required |
| COM-4 | Tabs / workspaces | ✓ | ✓ | ✓ | L1 required |
| COM-5 | Search & substitute | ✓ | ✓ | ✓ | L1 required |
| COM-6 | Regex search | ✓ | ✓ | ✓ | L1 required |
| COM-7 | Undo / redo | ✓ | ✓ | ✓ | L1 required |
| COM-8 | Undo tree (branching history) | ✓ | ✓ | ~ (concept differs) | L1 required |
| COM-9 | Session recovery | ✓ | ✓ | ✓ | L1 required |
| COM-10 | Macros | ✓ | ✓ | ✓ | L1 required |
| COM-11 | Clipboard / registers / kill ring | registers | registers | kill ring | unified model |
| COM-12 | Large files | ✓ | ✓ | ✓ | L1 required |
| COM-13 | Encoding / line endings | ✓ | ✓ | ✓ | L1 required |

## Notes on the hard ones

- **COM-8 Undo tree** — Vim/Neovim expose chronological branch traversal (`g-`/`g+`, `:earlier/:later`);
  Emacs's model differs (undo-as-undoable, plus the undo-tree package). ruse's transaction/undo engine
  (INV-UNDO) provides a branching history that both surfaces map onto. Undo grouping is by logical unit,
  not per keystroke (DECISIONS D-005 open for exact boundaries).
- **COM-11 Unified clipboard model** — ruse unifies **Vim registers** ([vim.md](vim.md) VIM-REG) and the
  **Emacs kill ring** ([emacs.md](emacs.md) EMACS-KILL) into one model with an optional OS-clipboard bridge
  and OSC 52 fallback ([terminal.md](terminal.md) TERM-OSC52). Each profile's surface reproduces its own
  semantics (register types / yank-pop) over the shared model.
- **COM-12 Large files** — not "a slow version of normal mode": a distinct degraded profile (syntax off,
  bounded undo, streaming search, binary detection, very-long-line handling). See design-requirements §6.
- **COM-13 Encoding / line endings** — kept separate from document data (INV — TEXT-19); detection order +
  BOM + `fileformat`/`fileencoding` semantics per [vim.md](vim.md) VIM-STATE.

## Reference Invariants
INV-TXN, INV-UNDO, INV-ANCHOR, INV-POS-TYPED (see
[../invariants/reference-invariants.md](../invariants/reference-invariants.md)).
