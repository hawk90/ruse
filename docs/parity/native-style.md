---
doc: parity-native-style
project: ruse
title: "Parity: Native Style (ruse's Third Input Language)"
summary: >
  Native Style is a first-class, versioned input profile — a redesign of the best of Vim and Emacs, not a
  mix of their keys. One rule: modal grammar for text, command grammar for actions, context-specific action
  grammar for special screens. Text uses Vim-style operator/motion; command discovery uses Emacs-style named
  commands; special views use Magit-style transient actions; input lines use Readline editing; multiple
  selection uses a Helix/Kakoune model.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - vim.md
  - emacs.md
  - ../architecture/architecture.md
---

# Parity: Native Style

Native Style is **not** a compatibility target with an external editor — it is ruse's own third input
language, declared as an official versioned profile (`native-profile@1`). Building it as "some Vim keys +
some Emacs keys" would just produce conflicts (anti-pattern PROFILE-15). It has its own principles.

## The One Rule

> **Modal grammar for text, command grammar for actions, context-specific action grammar for special
> screens.**

| Domain | Input model | Borrows the *model* from |
| --- | --- | --- |
| Text editing | Vim-style modal / operator+motion | [vim.md](vim.md) VIM-OP/VIM-MOT/VIM-TOBJ |
| Command discovery | Emacs-style named commands + prefix discovery | [emacs.md](emacs.md) EMACS-CMD |
| Special views | Magit-style transient actions | [emacs.md](emacs.md) EMACS-TRANSIENT |
| Search / input line | Readline/Emacs line editing | [emacs.md](emacs.md) EMACS-MINI |
| Multiple selection | Helix/Kakoune selection model | — |
| Workspace | VSCode-style command/context | [workspace.md](workspace.md) |

This selects the right input model per domain; it is a new grammar, not a blend.

## Illustrative bindings

```
Text buffer
  d + object       delete
  c + object       change

Command layer (leader)
  Space f          Files
  Space g          Git
  Space l          Language
  Space d          Debug

Transient Git layer (special view)
  s                stage
  u                unstage
  c                commit
  p                push
```

| ID | Element | Target |
| --- | --- | --- |
| NAT-1 | Modal text editing (operator/motion/text-object) | L1 |
| NAT-2 | Leader-based command layer with prefix discovery (which-key style) | L1 |
| NAT-3 | Transient action maps in special views (git/debug/picker) | L1 |
| NAT-4 | Readline-style input/search line | L1 |
| NAT-5 | Multiple-selection model | L2 (post-MVP; single selection must not block extension) |

## Design constraints

- Native Style is a **versioned profile package** (`native-profile@N`); changing default behavior ships a
  new version, existing users are not auto-migrated (architecture §11.1).
- All bindings resolve onto **semantic commands** (INV-CMD-SEMANTIC); Native Style provides its own
  recommended keymap, not a private command set.
- Selection model is designed so a single caret/selection can extend to multi-selection later without a
  type rewrite (design-requirements §4; anti-pattern — "single selection becomes unextensible").

## Reference Invariants
INV-PROFILE-ISOLATION, INV-PRIORITY, INV-CMD-SEMANTIC.
