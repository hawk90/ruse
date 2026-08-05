---
doc: contributing-hub
project: ruse
title: "ruse — Contributor Hub"
summary: >
  Entry point for contributing to ruse. Routes every kind of contribution to the ONE process it belongs to
  in under a minute: four contributor paths (question / bug / idea / capability / architecture / spec /
  security), four contributor personas (User, First-time contributor, General contributor, Design
  contributor), and the documentation model (state in spec/, explanation in docs/, work in Issues/Projects,
  Q&A in Discussions — GitHub Wiki is not authoritative). Deep process detail lives in ../operations/.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - change-paths.md
  - ai-assisted-development.md
  - quickstart.md
  - ../operations/development-model.md
  - ../operations/definition-of-done.md
  - ../operations/github-workflow.md
  - ../operations/testing-and-benchmarks.md
  - ../../CONTRIBUTING.md
  - ../../spec/CONTEXT.md
---

# ruse — Contributor Hub

> **One-minute goal:** find *which process your change belongs to* before you write code. If you only read
> one file next, read [change-paths.md](change-paths.md).

ruse is **spec-first**: the design (`spec/`, `docs/`) is the maintained source of truth; code is a reference
implementation that proves it. The right first move depends on *what kind* of change you have — not on how
big it feels.

## You want to… → Start with…

| You want to… | Start with… |
| --- | --- |
| Ask a question / get help | **GitHub Discussions** |
| Report reproducible incorrect behavior | **Bug report issue** |
| Suggest an early / half-formed idea | **GitHub Discussions** |
| Implement a scoped capability | **GitHub Issue** (Slice) |
| Change architecture or a public contract | **RFC** (`type/rfc`) |
| Change normative project state | **Specification PR** (spec/) |
| Report a security problem | **[SECURITY.md](../../SECURITY.md)** (private, never a public issue) |

## The four contributor personas

Pick the row that matches you today. Each persona has exactly one starting point per intent — do not invent
a fifth path.

### User
You use ruse and want to interact, not (yet) build.
- Question → **Discussions**
- Reproducible incorrect behavior → **Bug report issue**
- Early idea → **Discussions**
- Security problem → **[SECURITY.md](../../SECURITY.md)**

### First-time contributor
You want your first merged change to succeed. **Start in the low-risk surface**, where feedback is fast and
the blast radius is small:
- docs, tests, error messages, validator improvements, small TUI behavior, reproducible bug fixtures.
- Filter for labels: `good-first-issue`, `help-wanted`, `area/docs`, `area/testing`, `area/infra`.
- **Do NOT** start in `editor-core` or `plugin-protocol` — those changes are contract-level and need an RFC
  path (see Design contributor).
- Follow [quickstart.md](quickstart.md) to reach your first PR.

### General contributor
You are taking a clear, already-scoped **Slice**. The loop:

```
Issue (acceptance criteria) → acceptance evidence → short branch → Draft PR → CI → review → squash merge
```

- Every Slice needs at least one **observable acceptance condition** (see
  [../operations/definition-of-done.md](../operations/definition-of-done.md)).
- GitHub mechanics (labels, Project fields, milestones) are in
  [../operations/github-workflow.md](../operations/github-workflow.md).

### Design contributor
You want to change architecture or a public contract. **This never starts with code:**

```
Discussion → RFC → Spec delta → Decision (D-*) → Implementation Slice
```

- The RFC is the design's home; the Decision records what was chosen; only then does a Slice implement it.
- See [change-paths.md](change-paths.md) for the exact process per change level.

## Documentation model

ruse keeps **one fact in one home**:

- **`spec/`** — authoritative *state*: vision, requirements, enforced rules, decisions (LLM-first, minimal).
- **`docs/`** — *explanation*: long-form design, parity research, anti-patterns, contributor guides (this
  set).
- **Issues / Projects** — track *work* (what is being done and its state).
- **Discussions** — host *Q&A* and early ideas.
- **The GitHub Wiki is NOT used as an authoritative source** (see DECISIONS **D-035**). If it exists, treat
  it as scratch only; canonical content belongs in `spec/` or `docs/`.

## Before contributing

1. Read [`spec/CONTEXT.md`](../../spec/CONTEXT.md) (the compact context pack), then the specific
   capability / requirement / decision / RFC your change touches.
2. Check open **Issues / PRs / RFCs / Discussions** for overlap — do not duplicate in-flight work.

## Where to go next

- [change-paths.md](change-paths.md) — the one-minute "which process is my change" guide.
- [ai-assisted-development.md](ai-assisted-development.md) — the tool-agnostic AI policy.
- [quickstart.md](quickstart.md) — clone → first PR.
- [../operations/development-model.md](../operations/development-model.md) — Spec-Gated Iterative Development.
- [../operations/definition-of-done.md](../operations/definition-of-done.md) — when a change is done.
- [../operations/github-workflow.md](../operations/github-workflow.md) — labels, Project fields, milestones.
- [../operations/testing-and-benchmarks.md](../operations/testing-and-benchmarks.md) — evidence standards.
- [../../CONTRIBUTING.md](../../CONTRIBUTING.md) — the top-level contributing summary.
- [../../spec/CONTEXT.md](../../spec/CONTEXT.md) — read this first.
