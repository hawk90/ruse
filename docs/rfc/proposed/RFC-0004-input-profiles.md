---
doc: rfc
project: ruse
title: "RFC-0004: Input Profiles & Command Layer"
summary: >
  Locks the input philosophy and the ecosystem's editing ABI: three official, versioned input
  profiles (Vim / Emacs / Native) that share one Semantic Command Layer. Keymaps resolve onto
  namespaced typed commands; profiles never share a key space; a fixed-principle priority ABI and a
  context key resolver decide bindings. Crucially, the atomic-command model does NOT express Vim's
  operator+motion grammar — the editing-language composition engine (C-EDITLANG, D-025) is a
  first-class core concern, and `editor.delete_selection` is a sibling of operator+motion, not its
  generalization. Calls out the register/kill-ring (D-026) and positions-history (D-027) models as
  blocking-open.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../architecture/architecture.md
  - ../../parity/vim.md
  - ../../parity/emacs.md
  - ../../parity/native-style.md
  - ../../parity/README.md
  - ../../invariants/reference-invariants.md
  - ../../../spec/DECISIONS.md
---

# RFC-0004: Input Profiles & Command Layer

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-05
- **Decision link:** D-006, D-008 (principle) — this RFC also depends on the open decisions D-025,
  D-026, D-027, which **block** its L2 obligations.

> This RFC records a hard-to-reverse decision: the product's input philosophy and the semantic layer
> that plugins, configs, macros, and keymaps all bind against. It does **not** restate the source
> material — it cites [architecture.md](../../architecture/architecture.md) §1–2, the parity set
> ([vim.md](../../parity/vim.md), [emacs.md](../../parity/emacs.md),
> [native-style.md](../../parity/native-style.md), [README.md](../../parity/README.md)), the
> reference invariants ([reference-invariants.md](../../invariants/reference-invariants.md)), and
> the decision record ([DECISIONS.md](../../../spec/DECISIONS.md)).

## Summary

ruse ships **three official, versioned input profiles** — Vim Style, Emacs Style, Native Style —
that share **one Semantic Command Layer**. A profile is not a keymap preset: each profile is an
input grammar and state machine that **resolves onto** namespaced, typed semantic commands
(D-006 / INV-CMD-SEMANTIC). Profiles are **isolated** — bindings from different profiles never
share a key space (INV-PROFILE-ISOLATION) — and a **fixed-principle priority ABI** plus a **context
key resolver** decide which binding fires (INV-PRIORITY, D-008). The load-bearing correction this
RFC formalizes: the atomic-command model **cannot express Vim's `operator + count + motion|text-object`
grammar**. That composition is a first-class core layer — the **editing-language engine**
(`C-EDITLANG`, D-025) — and `editor.delete_selection` is a **sibling** of operator+motion (used by
Emacs/Native selection editing), not its generalization. Two data models the profiles depend on —
the unified register/kill-ring (D-026) and positions-history (D-027) — remain **open and blocking**
for L2 parity.

## Motivation / Problem

The Neovim ecosystem's fragility comes less from any single feature than from **input and command
coupling**: mappings collide, load order decides winners, `<Plug>` indirection substitutes for a
real command contract, and plugins reach into internals. ruse's goal (see
[architecture.md §0.6](../../architecture/architecture.md)) is "a platform that grows an ecosystem yet
breaks less than Neovim from day one." That requires locking two things early, before an input
engine or real plugins exist to drag the design around:

1. **The input philosophy** — how many input languages exist, whether they mix, and whether a
   profile is a first-class versioned artifact or a bag of key overrides (architecture.md §1).
2. **The ecosystem ABI** — the semantic command names and versioning policy *beneath* keymaps, so a
   user picks an input language while a plugin author barely thinks about Vim/Emacs differences
   (architecture.md §2).

This RFC is the decision record for both, and for the correction that the semantic layer alone is
**insufficient** to model the Vim editing language.

## Guide-level explanation

### Three profiles, one command core

A user selects one profile — `core.vim@1`, `core.emacs@1`, or `core.native@1` (versioned; see
architecture.md §11.1). Each profile has its **own** input grammar and state machine
(architecture.md §1.1):

- **Vim Style** — modal; `operator/count/motion|text-object` grammar (parity: vim.md VIM-OP,
  VIM-MOT, VIM-TOBJ).
- **Emacs Style** — prefix keys + region + named commands (parity: emacs.md EMACS-KEYMAP,
  EMACS-CMD, EMACS-REGION).
- **Native Style** — ruse's own third language: *modal grammar for text, command grammar for
  actions, context-specific action grammar for special screens* (parity: native-style.md). Native
  Style **is** the redesign of the best of Vim and Emacs — not a "Hybrid" mode
  (architecture.md §1, §1.5).

All three resolve onto the **same** Semantic Command Layer. A plugin registers **one** semantic
command (e.g. `org.example.git.stage`); each profile wires a key/gesture to it
(architecture.md §2.1):

```
Vim Style    → s   or  <leader>gs
Emacs Style  → C-c g s
Native Style → Space g s
Palette      → Git: Stage Selection
RPC / macro  → org.example.git.stage
```

### A binding's identity is a tuple, not a key

Because the same key means different things in different views/modes (architecture.md §1.3), a
binding is identified by `(profile, sequence, context, priority)`, not by the key alone. Two
bindings on the same key with **mutually exclusive contexts** (`view.kind == 'text'` vs
`view.kind == 'git-status'`) are **not** a conflict; the **context key resolver** disambiguates
them. A real conflict is narrow and **detected statically** (INV-PROFILE-ISOLATION):

```
same profile + same key sequence + overlapping context + same priority
```

On a real conflict the editor **does not** let the last-loaded plugin silently win; it disables the
new binding and asks the user to resolve (architecture.md §1.2). Because profiles are isolated, a
Vim binding and an Emacs binding **cannot** be that conflict — the Emacs binding never enters the
Vim resolver at all.

### The correction: atomic commands do not model Vim editing

A tempting simplification (and the one architecture.md §1.1's table originally implied) is that
`dw` / `diw` / `dd` are surfaces of one atomic `editor.delete_selection`. **This is wrong for Vim**
(architecture.md §1.1 correction box, verification V-1): there is no selection at `dw`. Vim composes
`operator + count + motion|text-object`, where the motion produces a **typed range**
(`char/line/block`, inclusive/exclusive) that undergoes the exclusive→inclusive→linewise
**promotion** (vim.md VIM-MOT-PROMOTE) *before* the operator consumes it. That composition — plus
the dot-repeat change-intent record and the plugin-registrable operator (`g@`/`operatorfunc`) — is a
**first-class core concern**, the **editing-language engine** `C-EDITLANG` (D-025), not a keymap
tier and not a command argument convention. `editor.delete_selection` is a **sibling** of
operator+motion (the path Emacs/Native selection editing take), never its generalization.

## Reference-level explanation

### 1. Profiles are versioned packages over a shared command layer

The final model (architecture.md §12):

```
                Stable Semantic Command Layer
                            │
         ┌──────────────────┼──────────────────┐
     Vim Profile        Emacs Profile      Native Profile
     versioned          versioned          versioned
         └──────────────────┼──────────────────┘
                            │
                    Context Key Resolver
                            │
                   Active View / Buffer
```

- A profile is a **versioned artifact** (`core.vim@1`, `core.native@2`). Changing a default binding
  is a compatibility break; ship a new version, do not auto-migrate users (architecture.md §11.1;
  parity native-style.md "Design constraints"). Profiles are covered by INV-PROTOCOL-VERSIONED
  ("… and profiles are all versioned with deprecation windows").
- A profile owns an **input grammar + state machine**, not just a key table (architecture.md §1.1).
  Vim/Native carry operator-pending as a *transient state* (vim.md VIM-MODE-6); Emacs carries prefix
  state; the resolver's top priority tier is exactly these transient states (§3 below).

### 2. Commands are the ecosystem ABI (D-006 / INV-CMD-SEMANTIC)

Per architecture.md §2 and the command contract (§2.3):

- IDs are **stable, namespaced** (`core.editor.delete`, `org.example.git.stage`); renames provide an
  **alias + deprecation window** (D-006). Command IDs are effectively the ecosystem ABI — configs,
  keymaps, macros, and *other plugins* depend on them.
- Commands take **typed arguments**, not `Vec<String>`; are decoupled from any keybinding or
  command-line string (INV-CMD-SEMANTIC); declare `undoable` / `non-undoable`; do not touch UI or
  arbitrary global state directly; leave no partial mutation on failure (transaction boundary,
  D-001 / INV-TXN); declare side effects and execution location (local/client/remote) in the
  manifest; and generate their own docs/completion from metadata.
- Keymaps and the palette are **convenience layers over the command registry** — this is exactly the
  Emacs M-x model (emacs.md EMACS-CMD-1: "invoke by name independent of any key"). `<Plug>`-style
  indirection is replaced by semantic command IDs (vim.md VIM-MAP-1).

### 3. Priority ABI — principle-locked, tiers provisional (D-008)

Key resolution follows a **fixed priority ABI** (INV-PRIORITY). D-008 is **decided in principle,
open in tiers** — this distinction is normative and must not be collapsed:

- **Locked (principle):** profiles are isolated (never share a key space); user overrides beat
  plugin bindings; **plugins cannot force a global key** (they *suggest*; the user accepts via
  install flow/preset — architecture.md §1.4); real conflicts are detected statically. A plugin
  may own keys **only inside its own special view**.
- **Open (do NOT lock):** the exact 8-tier ordering — especially *plugin-explicit vs
  plugin-suggested* — is **provisional**. It cannot be validated until an input engine (F-003) and
  real plugins (F-016) exist; locking it now would violate D-010 ("don't stabilize the
  unvalidated"). This RFC therefore records the ordering as **provisional guidance**, not a frozen
  contract:

  1. Temporary state (Vim operator-pending, Emacs prefix, popup nav)
  2. Active widget/view (git, tree, debugger, picker)
  3. Buffer-local mode — **an ordered sub-list, not flat** (verification V-28): stacked minor modes
     form an ordered sub-list, and text-span/overlay keymaps rank just above the major-mode map.
     This mirrors Emacs lookup precedence exactly (emacs.md EMACS-KEYMAP-2: "transient > **ordered**
     minor > major > global"). The ABI must carry that ordered sub-list.
  4. Workspace override → 5. User profile override → 6. Plugin explicit → 7. Plugin suggested →
  8. Built-in profile default.

  (Lower number = higher priority.)

### 4. Context key resolver

The resolver evaluates a binding's `ContextExpression` (e.g.
`view.kind == 'text' && input.mode == 'normal'`) against the **active view/buffer** state
(architecture.md §1.3). This is the runtime, compositional keymap stack that Emacs describes as a
minor mode shadowing a major-mode key without either knowing (emacs.md EMACS-KEYMAP "Semantic
model" → "ruse's Context Key Resolver + priority ABI"). The resolver is what makes "most conflicts
disappear": distinct contexts are not conflicts; only overlapping context at the same priority in
the same profile is.

### 5. C-EDITLANG — the editing-language composition engine (D-025, OPEN — blocking Vim L2)

The Semantic Command Layer is **atomic**: one command, typed args, executed and done. Vim's editing
language is **compositional** and cannot be expressed that way. D-025 adds a first-class core layer,
sitting *between* the profile's input engine and the command layer, that the atomic layer cannot
express:

- **Operator-pending state** + a typed `Motion → Range{kind: char|line|block, inclusivity}` IR, with
  the **exclusive→inclusive→linewise promotion** applied before the operator consumes the range
  (vim.md VIM-MOT-PROMOTE; governs `d}`, `d/pat`). Forced-motion overrides (`v`/`V`/`C-v` after an
  operator) toggle/force the range kind.
- A re-parameterizable **change-intent** record `(operator, object, count, inserted-text)` for
  **dot-repeat** — `.` must capture the full last change including inserted text and honor a new
  count (vim.md VIM-REPEAT-DOT, VIM-CNT-INS). Dot-repeat is distinct from transaction replay.
- A **plugin-registrable operator** kind for `g@`/`operatorfunc` (vim.md VIM-OP-9) — user/plugin
  operators are dot-repeatable and compose with the same motion IR.

`editor.delete_selection` is a **sibling** of operator+motion, not its generalization: it is the
path Emacs region editing (emacs.md EMACS-KILL / EMACS-REGION) and Native selection editing
(native-style.md NAT-1/NAT-5) take, where a selection/region already exists. Vim's `dw` has **no**
selection; it runs the operator over a freshly computed, promoted range. The two paths converge on
the same *effect* (text removed, register updated) but are **not** the same command surface. This
keeps the semantic layer honest (a `delete` command with a range argument) while the composition
grammar lives where it belongs — in C-EDITLANG, exercised by the Vim and Native profiles.

`C-EDITLANG` is **OPEN** (D-025): the exact IR and how it re-enters the input engine (also covering
`:normal` / `:global`, V-9) are undecided. It is **blocking for Vim L2** and must close before F-003
reaches L2.

### 6. Two shared data models the profiles depend on — OPEN & blocking

Profiles do not each own their own store for these; the design unifies them:

- **Unified register / kill-ring — D-026 (OPEN, blocking COM-11).** One store reproducing both
  surfaces exactly: typed slots (char/line/block governing paste geometry — vim.md VIM-REG-TYPE) +
  a numbered shift-ring (`"1`–`"9`, `"0` yank-only, `"-` small-delete — vim.md VIM-REG-RING) + a
  separate interaction state for Emacs consecutive-kill **coalescing** and post-yank **yank-pop**
  valid only right after a yank (emacs.md EMACS-KILL-2/3), plus an optional OS-clipboard bridge.
  Introduces `C-REGISTER`. The Vim *surface* and the Emacs *surface* map onto one superset model.
  **Blocking** for Vim/Emacs register parity (F-003/F-012 L2).
- **Positions-history — D-027 (OPEN).** One positions-history model over the anchor store (D-023 /
  INV-ANCHOR) with **pluggable membership/traversal policies** per surface: Vim jumplist (`n` is a
  jump, `j` is not) + `m{A-Z}` global-persistent marks (vim.md VIM-MARK-1); Emacs per-buffer +
  global mark rings (emacs.md EMACS-REGION-2); Helix/Kakoune selection **sets** (native-style.md
  NAT-5). Must state how point-rings and selection-sets coexist. **Blocking** for Vim marks
  (F-003 L2) and Native multi-selection (NAT-5).

These are called out as **blocking-open** here so that RFC acceptance does not imply they are
settled: the profile *surfaces* in this RFC are only implementable once D-026 and D-027 land.

## Reference Invariants

This RFC depends on and enforces:

- **INV-CMD-SEMANTIC** — a Command has a stable, namespaced ID, typed arguments, and is decoupled
  from any keybinding or command-line string; keymaps resolve *onto* commands. (D-006.)
- **INV-PROFILE-ISOLATION** — bindings from different profiles never share a key space; a real
  conflict requires same profile + same sequence + overlapping context + same priority, detected
  statically. (D-008 principle.)
- **INV-PRIORITY** — key resolution follows the fixed priority ABI; plugins cannot force global
  keys. (D-008 principle; exact tiers provisional per D-008 open.)

Related invariants it leans on (not introduced here): INV-PROTOCOL-VERSIONED (profiles/commands are
versioned), INV-TXN / INV-UNDO (command mutations go through transactions), INV-ANCHOR / INV-POS-TYPED
(positions-history over anchors, D-027), INV-ASYNC-ORDER (commands run on the deterministic executor,
D-002). No new `INV-*` is minted here; new invariants must be added to
[reference-invariants.md](../../invariants/reference-invariants.md) in the same change.

## Failure modes & Recovery

- **Real key conflict (same profile/sequence/overlapping-context/priority):** the new binding is
  kept **disabled** until the user resolves (Keep / Replace / Reassign / Context-scope). Safe
  default = do not silently override (architecture.md §1.2).
- **Command ID removed/renamed:** alias + deprecation window (D-006); a keymap/macro referencing a
  removed ID surfaces a typed, coded error (INV-ERR-CLASS), not a silent no-op.
- **Plugin attempts a forced global key:** rejected by the priority ABI (INV-PRIORITY); the plugin
  may only *suggest*, or bind within its own special view.
- **Unknown context predicate / profile version mismatch:** the binding is inert (additive-evolution
  reader tolerance, INV-ADDITIVE) rather than crashing the resolver.

## Security impact

- Plugins cannot seize global input; input capability is gated the same way as other capabilities
  (INV-TRUST-1). A plugin's keys live only in its own view unless the user accepts a suggestion.
- Command side effects and execution location (local/client/remote) are **declared in the manifest**
  and capability-gated (architecture.md §2.3, §10); a command cannot escalate by being bound to a
  key. Workspace/profile overrides respect precedence and do not let project settings silently
  override user settings (EMACS-VAR precedence; SEC-3).

## Performance impact

- Conflict detection is **static** (load/registration time), off the input hot path.
- Command dispatch must not **duplicate implementations per profile** (architecture.md §9); one
  command, three input surfaces. The editing-language engine (C-EDITLANG) is the only per-grammar
  cost and is shared by the Vim and Native profiles.
- Context predicates are evaluated against a small active-view state; the resolver must not re-index
  the whole keymap per keystroke.

## Compatibility & Migration

- Profiles are versioned packages (`core.vim@1`); a new editing philosophy ships as `@2` and users
  are **not** auto-switched (architecture.md §11.1). Config pins the profile:
  `[input] profile = "core.vim@1"`.
- Command IDs evolve **additively** with alias/deprecation windows (D-006, INV-ADDITIVE). The
  priority-tier ordering is **provisional** (D-008 open) and may change before F-003/F-016 without a
  profile major bump, because it is documented as not-yet-stable.

## Observability

- The status line shows the current **mode/prefix/operator-pending** state (architecture.md §7).
- Command metadata generates discovery/help; key→command-as-currently-bound and command→binding
  introspection mirror Emacs's live help (emacs.md EMACS-HELP-1). Conflict decisions and disabled
  bindings are inspectable, not hidden.

## Alternatives

1. **One profile with user-configurable keymaps (the Neovim/`vim.keymap.set` model).** Rejected: it
   collapses back into the load-order/override fragility this RFC exists to prevent, and offers no
   isolation guarantee (INV-PROFILE-ISOLATION).
2. **Commands carry a `Vec<String>` argv (Ex-command-style).** Rejected in favor of typed arguments
   (INV-CMD-SEMANTIC, architecture.md §2.3) — argv defeats static validation, docs generation, and
   the contract-first invariant (INV-CONTRACT-FIRST).
3. **Model operator+motion as a command taking a "range" argument, fully inside the semantic layer.**
   Rejected: it cannot represent operator-pending as a live state, motion-type promotion, forced
   motions, or dot-repeat's change-intent — see Rejected approaches and D-025.
4. **Lock the full 8-tier priority ABI now.** Rejected: the tiers are unvalidated pre-input-engine;
   D-008 keeps them open. We lock the *principles* only.
5. **`<Plug>`-style indirection for plugin keymaps.** Rejected: replaced by semantic command IDs as
   the indirection point (vim.md VIM-MAP-1), which are versioned and discoverable.

## Rejected approaches

- **Profiles as mere keymap presets.** A profile is an input grammar + state machine, not a key
  table (architecture.md §1.1). Treating it as a preset loses operator-pending/prefix state, makes
  isolation impossible to guarantee, and reintroduces cross-profile key collisions. *Rejected.*
- **Native Style = a mix of Vim keys + Emacs keys.** "Some Vim keys + some Emacs keys" is a bundle
  of conflicts (architecture.md §1.5; parity anti-pattern PROFILE-15). Native Style has its **own**
  principles — modal text, command actions, transient special views — and is a versioned profile in
  its own right (native-style.md). Naming it "Hybrid" would keep it perpetually dragged by
  Vim/Emacs compatibility. *Rejected.*
- **Modeling `dw` / `diw` / `dd` as surfaces of one atomic `delete` command.** There is no selection
  at `dw`; Vim composes operator + count + motion|text-object with a typed, promoted range
  (vim.md VIM-MOT-PROMOTE). The atomic-command model cannot express this. `editor.delete_selection`
  is a **sibling** of operator+motion, not its generalization; the grammar lives in the
  editing-language engine C-EDITLANG (D-025), not in the command layer. *Rejected* (this is the V-1
  correction, now formalized).

## Trade-offs

- **Three profiles + a separate composition engine is more surface than one configurable keymap.**
  Accepted: it buys profile isolation, a stable command ABI, and a correct Vim editing model — the
  properties the whole ecosystem stability argument rests on (architecture.md §1–2).
- **Provisional priority tiers create temporary ambiguity.** Accepted: locking unvalidated tiers is
  worse (D-010). The *principles* (isolation, user-override, no forced global keys, static conflict
  detection) are enough to be safe now.
- **C-EDITLANG duplicates some notion of "range" that the command layer also has.** Accepted and
  deliberate: the sibling relationship keeps the atomic command layer simple while the compositional
  grammar stays where Vim/Native need it.
- **Blocking on D-025/026/027 delays Vim/Emacs L2.** Accepted: shipping the surfaces without the
  shared models would produce subtly-wrong register/mark/motion behavior — exactly the "emulator got
  it slightly wrong" failure the parity corpus exists to prevent (parity vim.md checklist).

## Re-evaluation conditions

- **Priority tiers (D-008 open):** finalize once F-003 (input engine) and F-016 (plugins) exist and
  the plugin-explicit vs plugin-suggested distinction can be validated on real plugins.
- **C-EDITLANG (D-025):** close before F-003 reaches L2; if the motion IR cannot re-enter the input
  engine cleanly (incl. `:normal`/`:global`), revisit the profile/engine boundary.
- **Command contract (D-006):** additive only; a need to change command *meaning* (not add) would
  reopen the ABI.
- **Register/kill-ring (D-026), positions-history (D-027):** close before the respective L2 parity
  milestones; a superset model that cannot reproduce both surfaces exactly reopens the "unify vs
  keep separate" question.

## Open questions

- **D-025 — editing-language composition engine (OPEN, blocking Vim L2):** the exact `Motion → Range`
  IR and how it re-enters the input engine (also `:normal`/`:global`, V-9).
- **D-026 — unified register/kill-ring (OPEN, blocking COM-11):** the superset data model + per-surface
  mapping tables + differential tests (introduces `C-REGISTER`).
- **D-027 — positions-history model (OPEN):** the model + pluggable membership/traversal rules for
  jumplist / mark-rings / selection-sets, and how point-rings and selection-sets coexist.
- **D-008 open tail:** the exact 8-tier ordering, especially plugin-explicit vs plugin-suggested, and
  how the tier-3 ordered sub-list (V-28) is encoded in the ABI.
