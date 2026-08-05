---
doc: verification
project: ruse
title: "ruse Design Verification Report (v1)"
summary: >
  Synthesis of three adversarial reviews of the spec/docs set — parity coverage, internal consistency,
  and MVP feasibility/decision risk — before writing code. Gives per-subsystem go/no-go, ranked findings,
  the blocking open decisions, and a remediation log. Verdict: storage/boundary axes are solid;
  editing-language composition and the invariant registry need work before implementation.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
  - ../../spec/PRD.yaml
---

# ruse Design Verification Report (v1)

> **Status: point-in-time adversarial-review synthesis (v1).** This is a report, not a living canonical
> source — build order, decisions, and invariants are owned by `spec/` + `docs/invariants/`. Read it for the
> `V-*` findings and remediation log; don't treat its restated conclusions as a third source of truth.

Three independent adversarial reviews read the full `spec/` + `docs/` set: (A) parity coverage vs
architecture, (B) internal consistency, (C) MVP feasibility & decision risk. This report synthesizes them,
gives a per-subsystem go/no-go, and logs remediation. Findings are IDed `V-<n>`.

## Verdict per subsystem

| Subsystem | Verdict | Basis |
| --- | --- | --- |
| Core state & transaction (storage) | **GO** | Document≠View, transactions, anchors, revisions are coherent (all reviewers). |
| **Editing-language composition** (operator/motion, registers, marks, undo grouping) | **NO-GO until redesigned** | Reviewer A: the atomic-command model can't express Vim's grammar; register/kill-ring and marks/selection unified only by assertion. |
| Input profiles & command layer (boundaries) | **GO with fixes** | Priority ABI & isolation solid; but keymap tier flattening + prefix-arg/context evaluator gaps. |
| Plugin protocol / versioning / isolation | **GO** | Coherent and complete for L1 (all reviewers). |
| Terminal capability & rendering | **GO** | Best-covered area; probe/ledger/degradation mechanized. |
| Remote | **GO with open decisions** | Architecture sound; D-012/D-013 correctly open. |
| Stability & observability | **GO** | First-class and self-consistent (modulo INV-UNDO tension V-4). |
| **Invariant registry & spec integrity** | **NO-GO until unified** | Reviewer B: POLICY cites 9 INV IDs not in the registry + duplicate IDs; components have no `depends_on` so order isn't machine-checkable. |
| MVP plan / build order | **GO after reorder** | Reviewer C: Command placed contradictorily; Workspace unplaced; two premature locks. |

## Ranked findings

### Blocking (must resolve before/at implementation start)
- **V-1 Operator+motion grammar not expressible by atomic commands.** `architecture.md §1.1` models
  `dw/diw/dd` as surfaces of `editor.delete_selection` — that's the *selection* model, not Vim's two-level
  operator + typed-range grammar (VIM-MOT-PROMOTE). Needs a first-class operator-pending / `Motion →
  Range{kind,inclusivity}` composition engine. → **D-025 (new, open)** + architecture correction.
- **V-2 Register ⇄ kill-ring "unified model" asserted, never designed.** Vim's typed named-map + numbered
  shift-ring vs Emacs's single coalescing ring + post-yank `yank-pop` are structurally different. →
  **D-026 (new, open)** + `C-REGISTER` component.
- **V-3 Invariant registry fragmented.** POLICY.yaml/DECISIONS cite `INV-CONTRACT-FIRST, INV-ADDITIVE,
  INV-PROMOTION, INV-RENDER-IR, INV-RENDER-PROFILE, INV-QUERY-SNAPSHOT` (absent from the registry) and the
  stability doc defines duplicate IDs (`INV-ERR-1/2`, `INV-CAP-1`, `INV-FAIL-1`, `INV-OBS-1/2`) for facts
  the registry already names. Self-violates ENG-DOC-001. → unify (remediated below).
- **V-4 INV-TXN vs INV-UNDO contradiction for streaming/PTY/large-file buffers.** "Every mutation is a
  transaction" + "every transaction is undoable" is wrong for append-only/terminal/log buffers (F-011,
  WS-5, COM-12). → carve an explicit ephemeral-buffer exception (remediated below).

### Major
- **V-5 Undo grouping is open (D-005) yet F-005 is MVP-must; chronological `g-`/`g+` traversal has no
  mechanism** (needs a temporal index over the tree, not just parent/child).
- **V-6 Marks ⇄ jumplist ⇄ mark-ring ⇄ multi-selection unified by assertion** (different kinds: cursored
  list w/ membership rules, position rings, selection set). → **D-027 (new, open)**.
- **V-7 Dot-repeat/`operatorfunc` not modeled as a re-parameterizable change-intent** distinct from
  transaction replay or macro command-lists. → folded into D-025.
- **V-8 Vim regex dialect (`\zs \ze \@<= …`) has no owning component or decision.** → **D-028 (new,
  open)** + `C-REGEX`.
- **V-9 `:global` two-pass / `:normal` require command→input re-entry** inverting ARCH-FLOW-001. → note in
  architecture (input engine drivable as a library).
- **V-10 Build-order contradiction (Reviewer C R1):** Command is kernel in ARCH-LAYER-001 but step 4 in
  README build order, after Input (step 2) which depends on it. → reorder (remediated).
- **V-11 Workspace unplaced (R2):** F-007/F-008 (`must`) depend on `C-WORKSPACE` absent from the build
  order and bundled into the remote-stage crate. → add early workspace stage + split crate (remediated).
- **V-12 Premature ABI locks (R3):** D-008 (8-tier keymap ABI) and D-018 (scheduler budgets) locked before
  any input engine/plugin/workload exists — violates the project's own D-010/APIX rule. → reclassify
  specifics to open (remediated).
- **V-13 Multi-client render pinning (Reviewer A + C):** NVIM-UI-1 targets multi-client at L1, but D-012 is
  open and per-*session/view* profile pinning is undefined for two clients of differing capability. →
  pin per *client-view*; descope multi-client to post-MVP (remediated in render/neovim docs).
- **V-14 Interactive-buffer write-back undefined:** dired/wdired, Magit-stage edits aren't text
  transactions — they diff-then-apply to fs/git. → interactive-view contract note (workspace.md).

### Minor / correctly-open
- **V-15** `SCOPE-forbidden` is a dangling anti-pattern ref in `ENG-FAIL-001` → `SCOPE-7` (remediated).
- **V-16** `D-018` "INV — SCHED domain" placeholder → add `INV-SCHED-1` (remediated).
- **V-17** Non-goals duplicated across PROJECT.md / PRD.yaml / charter with membership drift → PRD.yaml
  `mvp.non_goals` is the single home (remediated).
- **V-18** Priority inversion: `must` F-006 depends on `should` F-010 → promote F-010 to `must` (remediated).
- **V-19** Missing decisions: anchor-based positioning (charter #04) and remote version-skew (charter #13)
  had no D-entry → **D-023, D-024 (new)**.
- **V-20** Missing MVP contracts: config/keymap loading (`C-CONFIG`), context/`when` evaluator
  (`C-CONTEXT`) → add components (remediated).
- **V-21** Components declare no `depends_on` → order not machine-checkable by `spec validate` → add
  component edges (remediated).
- **V-22** Enforcement mechanisms are aspirational pre-code → POLICY header note + prioritize D-022.
- **V-23** Terminology drift: "Render Tree" vs "Render IR" vs "Semantic View Model" → glossary + declare
  Render Tree ≡ Render IR (remediated).
- **V-24** Tier naming: "compatibility/enhanced" vs "Tier 0/1" → map explicitly (remediated).
- **V-25** `ENG-TRUST-001` has no trust invariant + only human enforcement → add `INV-TRUST-1` (remediated).
- **V-26** Decoration providers (NVIM-EXT-7) clash with CQRS/snapshot → bounded snapshot-scoped provider
  note. **V-27** Narrowing ownership (document-level restriction) → note. **V-28** minor-mode keymap
  ordering flattened → ordered sub-list note.

## Blocking open decisions before MVP
1. **D-005** (save/recovery journal + undo grouping) — gates `must` F-005 & F-008; close first.
2. **D-025** (editing-language composition: operator/motion/range IR + change-intent) — gates Vim L2.
3. **D-026** (unified register/kill-ring model) — gates COM-11 / Vim & Emacs L2.

## Remediation log (this pass)
Applied: invariant registry unified (V-3, added INV-CONTRACT-FIRST/ADDITIVE/PROMOTION/RENDER-IR/RENDER-
PROFILE/QUERY-SNAPSHOT/SCHED-1/TRUST-1; stability doc references registry IDs); POLICY invariant lists +
`SCOPE-forbidden` fixed (V-3,V-15); INV-TXN/INV-UNDO ephemeral-buffer exception (V-4); build order reordered
& workspace stage added, crate split (V-10,V-11); D-008/D-018 specifics reclassified open (V-12); F-010
promoted to must (V-18); new decisions D-023..D-028 (V-1,V-2,V-6,V-8,V-19); `C-CONFIG/C-CONTEXT/C-REGISTER/
C-REGEX` + component `depends_on` edges added (V-20,V-21); non-goals single-homed (V-17); terminology + tier
naming reconciled (V-23,V-24); architecture §1.1 corrected + notes for V-9,V-13,V-14,V-26,V-27,V-28.
Deferred to implementation phase (captured as open decisions/notes): full editing-language engine (D-025),
unified register model (D-026), positions-history model (D-027), Vim-regex engine (D-028), chronological
undo index (D-005 scope).

## Resolution follow-up (v1.1)

The three **blocking** gaps now have concrete designs (decisions promoted to *decided (design)*), so the
subsystem verdicts move **Editing-language composition: NO-GO → GO-with-design**:

- **V-1 / V-7 / V-9** (operator+motion grammar, dot-repeat intent, `:normal`/`:global` re-entry) →
  [editing-language.md](../design/editing-language.md) (`C-EDITLANG`), **D-025 decided (design)**.
- **V-2** (register ⇄ kill-ring unified model) → [register-model.md](../design/register-model.md) (`C-REGISTER`),
  **D-026 decided (design)**.
- **V-4 / V-5** (INV-TXN↔INV-UNDO for ephemeral buffers; undo grouping + chronological index) →
  [persistence-and-recovery.md](../design/persistence-and-recovery.md) (`C-PERSIST`) + INV-BUFFER-KIND,
  **D-005 decided (design)**.

Also now design-resolved: **V-6** positions-history → [positions-history.md](../design/positions-history.md)
(`C-POSHIST`, **D-027 decided**); **V-8** Vim-regex → [vim-regex.md](../design/vim-regex.md) (`C-REGEX`, **D-028
decided**). Remaining open (non-blocking for MVP): per-decision "open (details/tuning)" items and D-012/
D-013/D-017/D-022/D-024/D-032. A Cargo workspace skeleton (`crates/*`, `apps/*`) compiles and gates on
`spec validate` + `cargo fmt/clippy/test` via `.github/workflows/{spec-check,ci}.yml`.
