---
doc: rfc
project: ruse
title: "RFC-0014: The Emacs input profile (F-012) — resolving CONCEPT-COUNT-VS-PREFIX and CONCEPT-POSITION-HISTORY"
summary: >
  F-012 adds Emacs Style as the second input profile. Almost all of it is already DECIDED substrate: D-045
  made the keymap router an ordered layer stack whose Emacs configuration is the nine buffer-selected tiers
  (unsealed), D-026 made the kill ring a view/policy over the unified C-REGISTER store, and D-027 made the
  mark ring one C-POSHIST container. Two tensions were left `pending` and block F-012: CONCEPT-COUNT-VS-PREFIX
  (Vim's count is an engine multiplier; Emacs's `C-u` is an opaque prefix ARGUMENT the command interprets)
  and CONCEPT-POSITION-HISTORY (jumplist vs mark ring). This RFC resolves both — count/prefix as
  PROFILE-SCOPED over one raw prefix channel (the D-047 change-intent already carries it; only the
  INTERPRETATION differs per profile), and position-history as UNIFIED on C-POSHIST's pluggable
  membership/traversal policies — and defines the minimal-honest Emacs slice (the nine-tier stack, core
  motions, kill ring + yank-pop, `C-u`, mark ring, `C-x` prefix maps, `M-x`). Unblocks F-012; running Elisp
  stays a non-goal (D-020, D-007 L3).
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/parity/concepts/irreconcilable.yaml
  - ../../../spec/parity/contracts/keymap-layers.yaml
  - ./RFC-0004-input-profiles.md
  - ../../design/register-model.md
  - ../../design/positions-history.md
  - ../../design/input-engine.md
---

# RFC-0014: The Emacs input profile (F-012)

- **Status:** proposed
- **Decision link:** D-049 (proposed by this RFC; not yet recorded)
- **Resolves:** CONCEPT-COUNT-VS-PREFIX, CONCEPT-POSITION-HISTORY (`spec/parity/concepts/irreconcilable.yaml`).
- **Unblocks:** F-012; C-COMMAND (prefix-arg), C-POSHIST / C-ANCHOR (mark ring).
- **Builds on:** D-045 (layer router), D-026 (kill ring over C-REGISTER), D-027 (C-POSHIST), D-047 (change-intent count channel), RFC-0004 (input profiles).

## Summary

Ship **Emacs Style** as a CONFIGURATION of machinery ruse already decided, not a second engine:

- **Keymap** — D-045's ordered layer stack, in its Emacs arrangement: the nine tiers
  (`overriding-terminal-local-map` → … → `global-map`), **unsealed**, selected by what the BUFFER is. The
  Vim profile is the same stack at depth-1 sealed; the Emacs profile just installs nine layers and consults
  them in order. `C-x`/`C-c` are ordinary keymaps a key resolves INTO (a prefix map), not a special case.
- **Kill ring** — D-026's view/policy over the one `C-REGISTER` store: consecutive kills coalesce, `C-y`
  yanks the head, `M-y` rotates in a transient post-yank state. No second store.
- **Prefix argument** (`C-u`, `M-<digit>`) — resolves **CONCEPT-COUNT-VS-PREFIX** as **profile-scoped**: the
  kernel carries ONE raw prefix value on the D-047 change-intent; the Vim profile folds it as a MULTIPLIER,
  the Emacs profile passes it OPAQUE to the command (`C-u C-SPC` pops the mark, no repetition).
- **Mark ring** — resolves **CONCEPT-POSITION-HISTORY** as **unified on C-POSHIST**: the Emacs mark ring and
  global mark ring are `Ring` containers with pop-rotate membership; Vim's jumplist is a `CursoredList`.
  Both are the same anchor-backed `Selection` primitive with different membership/traversal policies —
  exactly D-027's design.

`M-x` reuses the existing C-COMMAND registry (F-004) to invoke any command by name.

## Motivation / Problem

`gov implementable F-012` is NOT-READY on two pending concepts and a parity repoint. The concepts are the
real blockers; both were left pending on purpose (decide the kernel shape before a profile hardens it), and
both are now decidable because the substrate that would decide them already shipped.

### CONCEPT-COUNT-VS-PREFIX

Vim's `[count]` is a multiplier the ENGINE applies (`2d3w` = 6); Emacs's `C-u`/`M-5` is an ARGUMENT the
COMMAND interprets (may mean repetition, or not — `C-u C-SPC` pops the mark ring; bare `C-u` = 4). A kernel
that models count as a number folded into the command loses Emacs semantics; one that passes it opaque
loses Vim's `count1 × count2` rule. **D-047 already decided the count CHANNEL** — count is part of the
change-intent — so what remains is only the cross-profile INTERPRETATION, which is a profile policy, not a
kernel shape. That makes it `profile-scoped`, not `pending`.

### CONCEPT-POSITION-HISTORY

In Emacs the mark is simultaneously position history AND the selection anchor; in Vim those are different
objects (jumplist vs visual anchor). **D-027 already decided C-POSHIST** as pluggable membership + traversal
policies over three containers (NamedMap / Ring / CursoredList) plus the live selection set, precisely so
these coexist: the mark ring is a `Ring` (pop-rotate), the jumplist a `CursoredList` (cursored traversal),
and *every entry is a `Selection`* so a bare mark is a degenerate one-caret selection. So the concept is
`unified` on C-POSHIST — the residual was only naming which container each surface uses, which this RFC does.

## Guide-level explanation

Selecting Emacs Style (`input.profile = emacs`, already in `config-schema`) changes which layers are
installed and how a prefix argument is interpreted — nothing else:

```
C-f / C-b / C-n / C-p / C-a / C-e     move   (global-map)
C-SPC                                 set-mark-command (push the mark ring)
C-u C-SPC                             pop-to-mark (rotate the mark ring)
C-w / M-w / C-y / M-y                 kill-region / kill-ring-save / yank / yank-pop
C-k                                   kill-line
C-x C-s                               save-buffer          (C-x is a prefix map)
C-x C-c                               save-buffers-kill-terminal
M-x <name> RET                        execute any command by name (C-COMMAND registry)
C-u 5 C-f                             move 5 (numeric prefix)
C-u C-u C-f                           move 16 (raw prefix, C-u multiplies by 4)
```

- **Prefix maps.** `C-x` is a keymap. Pressing `C-x` enters it; the next key resolves WITHIN it. This is the
  layer router walking one more level, not a bespoke "pending prefix" flag.
- **`M-y` is transient.** Yank-pop is valid only IMMEDIATELY after a `C-y`/`M-y` — a post-yank state on the
  C-REGISTER kill-ring view (D-026), exactly as Vim's `.`-repeat and `gv` are transient states.
- **The prefix argument is opaque to the command.** `C-u` sets a raw arg; the COMMAND decides what it means.
  `C-u C-f` repeats; `C-u C-SPC` does NOT repeat — it pops the mark. Vim's count could never express that,
  which is why this is a per-profile interpretation, not a shared rule.

## Reference-level explanation

### Keymap: nine tiers on the D-045 stack

The Emacs profile installs nine layers, unsealed, consulted highest-rank first until one binds
(`emacs.keymaptier.01…09`): `overriding-terminal-local-map` > `overriding-local-map` > text/overlay
`keymap` property > `emulation-mode-map-alists` > `minor-mode-overriding-map-alist` >
`minor-mode-map-alist` > `local-map` property > `current-local-map` (major mode) > `global-map`. Acceptance
#2 ("transient > ordered minor > major > global") is these ranks. A prefix map (`C-x`) is a binding whose
VALUE is another keymap; resolving it consults that keymap for the next key — the router already supports
this as "a layer may resolve to a sub-map," no new mechanism.

**MVP scope of the tiers.** The MVP installs all nine ranks but populates only `global-map` (core bindings)
and a `local-map` seam; the 613 census major-mode bindings and the minor-mode alists are the ECOSYSTEM
(post-MVP, and the reason CONCEPT-KEYMAP-DISPATCH exists). The nine-rank SHAPE is what F-012 must get right
so those populate additively later.

### Prefix argument channel (resolves CONCEPT-COUNT-VS-PREFIX → profile-scoped)

The change-intent (D-047) carries a `prefix: PrefixArg` where `PrefixArg = None | Numeric(i64) | Raw(u32)`
(`Raw(n)` = `C-u` pressed n times = 4ⁿ). Resolution is TOTAL and per-profile:

- **Vim profile:** folds `prefix` into the engine multiplier (`count1 × count2`), as today. `Raw`/`None`
  collapse to a count. Nothing changes.
- **Emacs profile:** passes `prefix` OPAQUE to the resolved command via the Context. Each command interprets
  it (`forward-char` repeats; `set-mark-command` pops on `Raw`). The engine never multiplies.

One channel, two interpretation policies — INV-CMD-SEMANTIC holds (the command a key resolves to is
unchanged; only the argument's reading differs).

### Mark ring (resolves CONCEPT-POSITION-HISTORY → unified on C-POSHIST)

C-POSHIST (D-027) already provides the containers. The Emacs profile wires:

- **mark ring** — a per-buffer `Ring<Selection>`; `C-SPC` pushes point, `C-u C-SPC` pops-and-rotates.
- **global mark ring** — a global `Ring<Selection>` across buffers.
- The mark is a `Selection` (anchor-based), so it is ALSO the region anchor (Emacs's dual role) with no
  separate object — a bare mark is a one-caret degenerate selection (D-027 / NAT-5).

Vim's jumplist stays a `CursoredList`; nothing about the Emacs wiring touches it. The concept is `unified`:
one primitive, two membership/traversal policies.

## Reference Invariants

- **INV-CMD-SEMANTIC** — profile selects INTERPRETATION of the prefix arg, never the command's identity.
- **INV-PROFILE-ISOLATION** — Emacs and Vim layers never share a key space; `input.profile` selects the whole
  stack (D-008/D-046).
- **INV-ANCHOR** — mark-ring entries are anchor-based `Selection`s that survive edits (D-023/D-027).
- **INV-ADDITIVE** — the nine-tier stack populates additively; major-mode/minor-mode maps are later layers,
  not a reshape.

## Failure modes & Recovery

- **Prefix arg leaking Vim's multiplier into Emacs** (`C-u C-SPC` repeating). *Recovery:* the Emacs profile
  never multiplies; a differential fixture asserts `C-u C-SPC` pops the mark, does not repeat.
- **Two position-history objects drifting** (mark ring vs a second selection store). *Recovery:* there is one
  `Selection` primitive; the mark IS the anchor (D-027), so they cannot drift.
- **`C-x` modelled as a flag** (the mode-defect shape). *Recovery:* `C-x` is a keymap the router resolves
  into, asserted by a test that `C-x C-s` saves and `C-x` alone is pending-in-the-map, not a bespoke state.

## Drawbacks & Alternatives

- **Alternative: a separate Emacs engine.** *Rejected* — D-045/D-026/D-027 exist precisely so one kernel
  serves both; a second engine is the divergence those decisions prevent.
- **Alternative: leave the concepts pending and ship Emacs keys ad-hoc.** *Rejected* — that hardens a
  profile over an undecided kernel shape, the exact failure the concepts guard.
- **Drawback: the nine-tier stack is mostly empty in the MVP.** Accepted — the SHAPE is the deliverable; the
  ecosystem populates it (CONCEPT-KEYMAP-DISPATCH's absorption path).

## Unresolved questions

- Which minor-mode/major-mode maps (if any) ship in-tree vs. arrive via the plugin host (F-016) — a
  delivery question, not a shape question.
- `iminsert`-style per-buffer prefix-arg persistence and the exact `M-<digit>` accumulation edge cases —
  fixture-pinned at implementation, do not change the channel's shape.
- Elisp/Vimscript execution stays a NON-GOAL (D-020, D-007 L3): ruse reproduces Emacs editor SEMANTICS, not
  the Emacs runtime.
