---
doc: development-model
project: ruse
title: "ruse Development Model — Spec-Gated Iterative Development"
summary: >
  How ruse is built. NOT "waterfall-agile" — a specialized model: long-term direction is staged, execution
  is iterative, change approval is spec-based, and a stage ends on evidence not a date. The build order is a
  dependency direction, not a one-way waterfall; implementation evidence may revise earlier specs via an RFC.
  Defines work-item types (Capability/RFC/Spike/Slice/Task), the workflow, the five stage gates, iteration
  cadence (work loop + stage review), milestones, and what the project deliberately does NOT optimize for.
  Details split into definition-of-done.md, github-workflow.md, testing-and-benchmarks.md, ci-cd-and-release.md.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - definition-of-done.md
  - github-workflow.md
  - testing-and-benchmarks.md
  - ci-cd-and-release.md
  - spec-validate.md
  - ../../spec/PROJECT.md
  - ../../spec/DECISIONS.md
---

# ruse Development Model — Spec-Gated Iterative Development

Korean: **사양 게이트형 반복 개발.** This is not "hyper waterfall-agile" (not an established method — usually a
misname for Hybrid Waterfall–Agile). ruse needs something more specialized:

```
Long-term direction : staged
Task execution      : iterative
Change approval     : spec-based
Stage completion    : evidence-based (not calendar-based)
```

The loop: **Spec-first → thin implementation → executable evidence → spec revision → next feature slice.**

## Build order is a dependency direction, not a waterfall

The staged order is kept, but it is **not** "finish editor-core completely, then move to input-engine":

```
RFCs → editor-core → input-engine → tui-client → workspace → plugin-api → remote-runtime → GUI → Marketplace → AI
```

Progress is by **thin vertical slices that connect stages early**, and evidence can flow *back* into an
earlier stage's spec:

```
editor-core minimal slice   (Document → Transaction → Command → Undo)
  → input-engine minimal link   (key input → command → transaction)
    → TUI minimal link          (input → edit → render)
      → problems found feed back into the editor-core spec (via RFC/decision)
```

Stages are ordered, but verification results may reopen an earlier stage.

## Work-item types

| Type | Meaning |
| --- | --- |
| **Capability** | A long-lived ability the project must provide (`CAP-*` in [`spec/capabilities.yaml`](../../spec/capabilities.yaml)). |
| **RFC** | A proposal that changes architecture / public contract / protocol / core data model / plugin compat / remote protocol / persistent format / security boundary / a hard-to-reverse choice / structure across ≥2 crates. |
| **Spike** | A time-boxed experiment resolving uncertainty (rope perf, renderer latency, SSH reconnect, WASM sandbox cost). Its output is an RFC / Decision / Benchmark / Rejected-alternative / new question — **never** product architecture-by-accident. |
| **Slice** | The **smallest mergeable end-to-end behavior** — cut by *behavior*, not crate/layer. Good: "empty Document → Insert command → Transaction recorded → Undo → headless test passes." Bad: "write Document struct, write Command trait, tests later." |
| **Task** | A small implementation step to finish a Slice — a checklist item *inside* a Slice, not an independent roadmap item. |

> Tasks attach to a Slice. File/crate-shaped issues ("Create document.rs", "Add workspace crate") are
> forbidden — they hide the resulting behavior.

## Workflow

```
Inbox → Clarifying → Ready → Active → Review → Proven → Done
```

- **Inbox** — a thought/bug/proposal; not implemented yet.
- **Clarifying** — why is it needed? which Capability? spec change needed? Spike needed? what proves it done?
- **Ready** — has a linked Capability, clear scope, confirmed deps, defined completion *evidence*, RFC-need
  decided, finishable in one/few PRs.
- **Active** — actually being worked. Solo cap: **≤2 Active** (1 implementation + 1 RFC/Spike). Starting
  many implementations at once turns spec-first into *unfinished-first*.
- **Review** — reviews code **and** spec consistency, architecture boundaries, test evidence, doc drift,
  compatibility, performance assumptions.
- **Proven** — not "code written" but **defined evidence obtained**: golden/property tests pass, benchmark
  meets baseline, headless demo runs, TUI dogfood works, remote reconnect test passes. (What proves what:
  [testing-and-benchmarks.md](testing-and-benchmarks.md).)
- **Done** — implementation + tests + linked spec IDs + updated docs + validator green + follow-ups split.

## Stage gates (evidence, not dates)

Each build stage ends on **Exit Criteria**, progressing through five gates:

```
Specified → Executable → Integrated → Dogfoodable → Stable
```

- **A — Specified:** terms in glossary; Capability↔requirement linked; key invariants present; public
  contracts described; unknowns explicitly listed. (Not all questions answered — you *know what you don't
  know*.)
- **B — Executable:** minimal implementation compiles; core flow runs; automated tests exist; failure shape
  is observable.
- **C — Integrated:** actually connected to the prior stage (not mock-only); lifecycle verified; error &
  recovery flows verified.
- **D — Dogfoodable:** a developer can use it for real work; critical defects filed as issues; debugging/
  diagnostics exist; the basic flow works without manual workarounds.
- **E — Stable:** public-contract change rules exist; compatibility policy exists; performance baseline
  exists; regression tests exist; docs match implementation.

A stage may intentionally stop at an earlier gate (an experimental subsystem may be *Executable* but not
*Stable*). Example — `editor-core` isn't "done" because the crate exists; it must **prove**
`Document → Command → Transaction → Undo/Redo → deterministic replay` via headless tests to pass its gate.

## Iteration cadence

- **Work Loop (≈1 week or a natural completion unit):** (1) define this loop's evidence → (2) write the RFC/
  spec delta → (3) implement the minimal Slice → (4) automated verification → (5) fold findings back → (6)
  merge. Not finishing in a week is **not** a failed sprint — instead check whether the Slice is too big.
- **Stage Review (at stage end):** which assumptions were wrong? which spec was un-implementable? which API
  was frozen too early? new dependencies? enough evidence for the next stage? what debt was deliberately
  kept? **Gate retros matter more than weekly retros.**

## Milestones = stage proofs (not sprints)

Each milestone has a **one-sentence exit condition**:

| Milestone | Exit condition (example) |
| --- | --- |
| M0 Spec foundation | The normative spec, invariants, and validation exist and pass. |
| M1 Headless editor-core | A deterministic headless editor can execute commands, record transactions, and replay undo/redo. |
| M2 Vim editing kernel | Operator+motion/registers/dot-repeat pass the Vim L2 differential corpus. |
| M3 Dogfoodable terminal editor | A developer can edit real files in the TUI without manual workarounds. |
| M4 Local workspace | Buffers/views/splits + atomic save + crash recovery work end-to-end. |
| M5 Plugin proof | An isolated plugin registers a command and requests a transaction over the versioned protocol. |
| M6 Remote editing proof | An auto-bootstrapped SSH agent serves fs/search/PTY with reconnect. |

Never write "implement editor-core" — write the behavior that proves the stage.

## What ruse does NOT optimize for

Not story points, velocity targets, mandatory sprint boundaries, or issue counts. Progress is measured by:
**architectural risks removed · vertical slices proven · stage gates passed · automated invariants added ·
end-to-end flows made dogfoodable · compatibility contracts made explicit.** Also avoided: forced sprints
(never mark an incomplete Slice Done because a date passed); full-spec-before-any-implementation (un-proven
spec accumulates — every area's spec must connect to a small executable proof); document-after-implement
(spec/ stops being the source of truth); premature generalization of Plugin API / remote protocol / GUI
abstraction before ≥2 real use cases.

## Automation — the workflow that runs itself

Realized by four existing mechanisms (see [spec-validate.md](spec-validate.md)):
- **`spec-validate` = always-on gate** — refs/layers/links/enums, `trace.design` resolves, and the **DoD
  readiness rule** (every `mvp`/`must` feature has a resolving design + acceptance). "Product-ready" is
  machine-checked, not a vibe.
- **Traceability chain** `Capability/F-* → parity → design → verify` is the backbone: any item's missing
  stage is a query (no design → write it; no fixtures → add them).
- **Agent orchestration** = the parallel execution engine used throughout this project: per subsystem, fan
  out design → lock decision → implement → **adversarially verify** → integrate.
- **CI + RFC** = the gates around it.

## Where the details live (one fact, one home)
- **This doc** = the model. **[definition-of-done.md](definition-of-done.md)** = the DoD checklist.
  **[github-workflow.md](github-workflow.md)** = GitHub mechanics (Project fields, labels, branches, PRs,
  commits). **[testing-and-benchmarks.md](testing-and-benchmarks.md)** = test/bench strategy & evidence
  formats. **[ci-cd-and-release.md](ci-cd-and-release.md)** = release process. Enforced principles that need
  teeth live in [`spec/POLICY.yaml`](../../spec/POLICY.yaml).

## Reference Invariants
This process enforces the locked contracts; it mints no new `INV-*`. See
[../invariants/reference-invariants.md](../invariants/reference-invariants.md).
