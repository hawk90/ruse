---
doc: rfc
project: ruse
title: "RFC-0001: Project Vision & Non-Goals"
summary: >
  Locks the top-level product identity of ruse: a spec-first ("Architecture > Code"), TUI-first,
  remote-first Rust editor targeting Vim/Neovim/Emacs feature parity — a redesign, not a port. Records
  the maturity ladder (Spec → Reference Architecture → Reference Implementation → Production) and points
  at the single-homed non-goals. This is a decision record; depth lives in the linked design docs.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../README.md
  - ../../../spec/PROJECT.md
  - ../../architecture/design-charter.md
  - ../../architecture/architecture.md
  - ../../invariants/reference-invariants.md
  - ../../../spec/PRD.yaml
  - ../../../spec/DECISIONS.md
---

# RFC-0001: Project Vision & Non-Goals

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-05
- **Decision link:** [D-020](../../../spec/DECISIONS.md) (scope/non-goals), [D-021](../../../spec/DECISIONS.md) (doc system)

<!-- RFCs are only for hard-to-reverse decisions. Vision and non-goals are the most hard-to-reverse
     decision of all: everything downstream inherits them. This RFC ratifies what is already stated in
     spec/PROJECT.md and the design charter; it does not re-derive it. -->

## Summary

`ruse` is a Rust, terminal-first, remote-first, extensible code editor built **as a specification with a
reference implementation**, not as "an editor written in Rust." It targets **feature parity** with
Vim/Neovim (editing language) and Emacs (command/buffer/extension model) — a redesign that grows an
ecosystem yet **breaks less than Neovim**, explicitly *not a port*. This RFC locks five commitments —
spec-first ("Architecture > Code"), feature parity (not compatibility), TUI-first, the maturity ladder,
and a bounded v1 scope — and defers the authoritative non-goals list to a single home. See
[`spec/PROJECT.md`](../../../spec/PROJECT.md) Vision/Principles and the
[Design Charter](../../architecture/design-charter.md) for the full statements this record ratifies.

## Motivation / Problem

A project of this size ("roughly Neovim + Helix + Emacs + VSCode Remote + Zed, redesigned") fails in one
of two ways: **scope collapse** — accepting too much future at once until core semantics and boundaries
blur ([Design Charter](../../architecture/design-charter.md) risk statement) — or **analysis paralysis**. Both
are prevented by writing the identity down once, as a decision, so it is not re-litigated per feature or
per contributor. The cost analysis in
[architecture.md §0](../../architecture/architecture.md) (~60–140 person-years for a faithful Neovim port; a
solo full-compat port "likely never finishes") is the concrete evidence that forces the "parity, not
port" framing. This RFC exists to make that framing a locked contract rather than a recurring debate.

## Guide-level explanation

Five commitments define what ruse *is*:

1. **Spec-first — "Architecture > Code."** The architecture is meant to outlive the language. People study
   Git's object model, SQLite's pager, Redis's single-threaded loop — not the host language's syntax
   ([docs/README.md](../../README.md) Philosophy). The Rust implementation is a **proof** of the design,
   not its source. Contracts (invariants, terminology, command IDs, protocols, trust boundaries) are
   locked; implementations (data structures, algorithms, backends) stay free.

2. **Feature parity, not compatibility.** ruse inherits the *editing language* of Vim/Neovim and the
   *command/buffer/extension model* of Emacs at the **feature** level
   ([parity/](../../parity/README.md)). It does **not** run Vimscript/Elisp/Lua plugins (compatibility
   level L3 is a non-goal) — a redesign, not a port.

3. **TUI-first (remote-first).** v1 ships a terminal frontend only; the semantic Render IR
   (INV-RENDER-IR) keeps GUI/Web possible later. The client/remote boundary is a first-class type
   distinction from day one (INV-REMOTE-FIRST), not bolted on.

4. **The maturity ladder.** Every artifact advances through
   `Specification → Reference Architecture → Reference Implementation → Production`
   ([docs/README.md](../../README.md)). A future re-implementation in another language is conformant iff
   it upholds the [Reference Invariants](../../invariants/reference-invariants.md) — regardless of
   internal structure.

5. **Bounded v1 scope.** Vim Style leads; Emacs/Native follow. No GUI/Web, Marketplace, WASM plugin host,
   or collaborative editing in v1 (see Non-goals below).

## Reference-level explanation

This RFC introduces no wire format. Its "contract" is the set of statements ratified as stable:

- **Vision & principles** — verbatim source: [`spec/PROJECT.md`](../../../spec/PROJECT.md) §Vision,
  §Principles. This RFC does not restate them authoritatively; it points to them.
- **Doc system** — governed by [D-021](../../../spec/DECISIONS.md): `spec/` YAML is the state source of
  truth; `docs/` is prose reference; one fact, one home. Every design doc / RFC follows the
  [Design Charter](../../architecture/design-charter.md) Common Document Template and restates the invariants it
  depends on (D-022, `spec validate`).
- **Non-goals** — the machine-readable v1 exclusion list is **single-homed** in
  [`spec/PRD.yaml`](../../../spec/PRD.yaml) `mvp.non_goals` (V-17): `gui`, `web`, `marketplace`,
  `wasm-plugin-host`, `collaborative-editing`, `running-vimscript-or-elisp`. That list is authoritative;
  this RFC and [`spec/PROJECT.md`](../../../spec/PROJECT.md) §Non-goals state the product-level intent and
  must stay consistent with it (D-020).

## Reference Invariants

This vision depends on (does not introduce) the following IDs from
[reference-invariants.md](../../invariants/reference-invariants.md). They are the concrete, testable rules
that make "Architecture > Code" more than a slogan; each is restated in the RFC that owns its domain.

- **INV-CONTRACT-FIRST** — contracts are defined independently of implementation types; changing an
  internal Rust type is not, by itself, an API change. *(Direct basis of spec-first.)*
- **INV-PROMOTION** — no API reaches Stable without ≥2 independent users + migration strategy.
  *(Enforces the maturity ladder's Reference → Production step; see D-009.)*
- **INV-ADDITIVE** / **INV-PROTOCOL-VERSIONED** — compatible evolution is additive over versioned
  protocols; underpins "breaks less than Neovim" and the deferred WASM plugin host.
- **INV-REMOTE-FIRST** — client/remote boundary is a first-class type distinction from the start.
  *(Basis of remote-first.)*
- **INV-RENDER-IR** — all output lowers from one semantic Render Tree; no view/plugin emits
  backend-specific bytes. *(What keeps TUI-first from foreclosing GUI/Web.)*
- **INV-PLUGIN-ISOLATED** / **INV-FAIL-BOUNDED** — external failure degrades, it does not stop core
  editing; features degrade in quality, they do not disappear. *(Product principle → invariant.)*

## Failure modes & Recovery

Vision-level failure is drift, not a crash. Two modes and their recoveries:

- **Scope creep** — a feature re-argues a locked non-goal. *Recovery:* it is rejected by reference to
  `spec/PRD.yaml` `mvp.non_goals` + D-020; reopening requires the Re-evaluation conditions below, not an
  ad-hoc exception.
- **Drift between homes** — prose in this RFC / PROJECT.md diverges from `mvp.non_goals`. *Recovery:*
  `mvp.non_goals` wins (single home); `spec validate` (D-022) is the intended mechanical guard.

## Security impact

None directly. The vision *defers* the WASM/process plugin host (D-009) so an unproven extension surface
is not stabilized early, and preserves the workspace-trust model (INV-TRUST-1) as a locked contract rather
than a v1 deliverable. Deferring reduces near-term attack surface.

## Performance impact

None directly. "Reference implementation is a proof, not the source" explicitly permits — and the charter
demands — using Rust's strengths for the MVP rather than degrading it for hypothetical future porting
([Design Charter](../../architecture/design-charter.md) Non-Goals: "does not refuse to use Rust's strengths").

## Compatibility & Migration

No prior public contract exists; nothing to migrate. Forward-compatibility is structural: TUI-first is
non-foreclosing because of INV-RENDER-IR, and remote/plugin surfaces evolve additively
(INV-ADDITIVE, INV-PROTOCOL-VERSIONED). "Feature parity, not compatibility" is itself the compatibility
stance: ruse never promises to load Vimscript/Elisp/Lua.

## Observability

The vision is observable through the spec system: `spec/PRD.yaml` states/priorities, `spec/DECISIONS.md`
D-020/D-021, and the design-axis states in [`spec/CONTEXT.md`](../../../spec/CONTEXT.md)
(`Unexplored → Draft → Validated → Stable → Needs-revision`) make "are we still on-vision?" a queryable
fact rather than a matter of opinion.

## Alternatives

- **A1 — Neovim port / drop-in replacement.** Rejected; see below. Costed at ~60–140 person-years with
  no ecosystem improvement ([architecture.md §0](../../architecture/architecture.md)).
- **A2 — GUI-first (Zed-style) from v1.** Deferred, not rejected forever: TUI-first + INV-RENDER-IR keeps
  it open. Doing all frontends at once is a charter non-goal (scope trap).
- **A3 — All three input profiles (Vim + Emacs + Native) complete in v1.** Rejected for v1: Vim Style
  leads, others follow (D-020) — completing all three simultaneously is the classic scope trap.
- **A4 — Public plugin SDK / Marketplace in v1.** Rejected for v1 (D-009): stabilize the extension API
  only after official built-ins dogfood it and ≥2 independent users exist (INV-PROMOTION).
- **A5 — Code-first ("just build the editor, document later").** Rejected: for a design this large,
  premature production code is thrown away as the design shifts ([docs/README.md](../../README.md)).

## Rejected approaches

- **Full Vimscript/Elisp/Lua compatibility (running existing plugins, L3).** Rejected permanently for v1.
  The last 10–20% of behavioral compatibility can consume more than half the schedule
  ([architecture.md §0.1](../../architecture/architecture.md)), and "implementing only a similar protocol will
  not make existing clients fully compatible" (§0.2). Parity is measured as user-perceived functionality,
  not bug-for-bug fidelity. *Recorded so this is not re-proposed as "just add a Lua host."*
- **A single global mutable editor state modeled on Neovim's internals.** Rejected: violates
  INV-NO-GLOBAL-STATE and reproduces the entanglement that makes Neovim expensive to change. ruse is a
  redesign specifically to avoid inheriting that structure.
- **Treating the Rust code as the specification.** Rejected: it makes the design language-bound and
  un-reimplementable, defeating "Architecture > Code" and INV-CONTRACT-FIRST.

## Trade-offs

- **Not a drop-in replacement.** Existing Vim/Neovim/Emacs plugins do not run. We accept a smaller initial
  ecosystem in exchange for a core that breaks less and an API stabilized only when proven.
- **Spec-first has upfront cost.** Writing invariants/RFCs before code is slower to first commit; the
  RFC process (finalize, then move on) is the guard against this tipping into analysis paralysis.
- **TUI-first defers GUI users.** Mitigated by INV-RENDER-IR keeping GUI/Web strictly possible later; the
  cost is IR discipline now.
- **Bounded scope disappoints "do everything" expectations.** Deliberate: the charter's central risk is
  failing by accepting too much future at once, not by lacking features.

## Re-evaluation conditions

- **Scope (D-020)** — revisit after the MVP ships and real user feedback exists. Any single non-goal may
  be lifted then, edited in `spec/PRD.yaml` `mvp.non_goals` first (its single home), with this RFC updated
  to match — never the reverse.
- **Doc system (D-021)** — revisit when the `xtask` spec-context generator (D-022) is built.
- **Parity strategy** — revisit if a validated cross-language compatibility layer (e.g. WASM plugin host,
  D-009) demonstrably delivers real plugins meeting INV-PROMOTION.
- Superseding this RFC requires superseding D-020 and updating `spec/PROJECT.md` §Vision/§Non-goals in the
  same change.

## Open questions

- Exact **parity coverage bar** per input profile for "v1 done" — owned by RFC-0004 (Input Profile) and
  `docs/parity/`, not this RFC.
- Whether Native profile is a v1.0 or post-1.0 deliverable once Vim Style is stable (tracked as a
  design-axis state in [`spec/CONTEXT.md`](../../../spec/CONTEXT.md), not decided here).
