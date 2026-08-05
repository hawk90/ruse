---
doc: design-charter
project: ruse
title: "ruse Design Charter — Required Docs, Template, Decisions to Lock"
summary: >
  The governance charter: the 15 core design documents required before coding, the common section
  template every design doc/RFC must follow, and the 20 decisions that must be locked before
  implementation starts. Encodes the project's central discipline — separate contracts to lock from
  implementations not yet to lock.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../README.md
  - architecture.md
  - design-requirements.md
  - ../invariants/reference-invariants.md
---

# ruse Design Charter

> One-sentence risk statement: **the project is more likely to fail by accepting too much future at once —
> blurring core semantics and boundaries — than by lacking features.** Sustainability = precisely
> separating *contracts to lock* from *implementations not yet to lock*.

## 15 Required Design Concerns (before coding)

At minimum these 15 concerns must be covered before implementation begins. **Where each lives is mapped in
[../README.md](../README.md) §Document Map** (do not maintain a second location table here):

01 Vision & Non-Goals · 02 Domain Terminology · 03 Core Invariants · 04 State Ownership · 05 Command &
Transaction Semantics · 06 Persistence & Crash Recovery · 07 Input Profiles & Compatibility Levels ·
08 Plugin API Lifecycle · 09 Remote Client/Runtime Protocol · 10 Terminal Capability & Rendering ·
11 Error/Logging/Status/Diagnostics · 12 Security & Workspace Trust · 13 Performance Budgets ·
14 Compatibility & Deprecation Policy · 15 CI/CD & Release Governance.

## Common Document Template

Every design doc / RFC should carry these sections (in this order where applicable):

```
Problem
Goals
Non-goals
Terminology
Invariants
Proposed design
Failure modes
Recovery behavior
Security impact
Performance impact
Compatibility impact
Observability
Alternatives
Rejected approaches
Migration strategy
Test strategy
Open questions
```

This forces every proposal to state not just "how success works" but where it can fail, where errors go,
how far the blast radius reaches, and how it recovers — the stability discipline from
[stability-and-observability.md](../design/stability-and-observability.md).

## Decisions to Lock Before Implementation

The ~20 hard-to-reverse decisions and their live status/rationale/re-evaluation are **single-homed in
[`spec/DECISIONS.md`](../../spec/DECISIONS.md)** (records `D-001…D-020`+). Do not restate them here — a
static table rots as decisions get superseded. Each hard decision becomes (or feeds) an RFC using the
template above.

## Non-Goals

Single-homed: prose in [`spec/PROJECT.md` §Non-goals](../../spec/PROJECT.md), machine list in
[`spec/PRD.yaml` `mvp.non_goals`](../../spec/PRD.yaml); rationale in
[design-requirements.md §20](design-requirements.md).

## The Discipline in One Line

> Lock the **contracts** (invariants, terminology, command IDs, protocols, trust boundaries). Keep the
> **implementations** (data structures, algorithms, backends, optimizations) free to change. The reference
> implementation proves the contracts; it is not the contract.
