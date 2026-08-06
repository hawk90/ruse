---
doc: change-paths
project: ruse
title: "ruse — Which Process Is My Change?"
summary: >
  The one-minute decision guide: given a change, which process does it require? A change-level-by-risk table
  maps each kind of change (docs, bug fix, feature Slice, public API/protocol, core data model, security/
  compatibility) to its required process (direct PR, Issue, RFC, Decision, separate review). The load-bearing
  rule: the spec lands before or with the first implementation PR, never after.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - ../operations/development-model.md
  - ../operations/definition-of-done.md
  - ../../spec/DECISIONS.md
  - ../../spec/PRD.yaml
  - ../../spec/templates/rfc.md
---

# Which Process Is My Change?

> **Read this in one minute.** Find the row that matches the *riskiest* thing your change does, then follow
> its required process. When in doubt, pick the heavier row — under-processing a contract change is the
> expensive mistake.

## Change level by risk

| Change | Required process |
| --- | --- |
| Docs / typos / small isolated tests | **direct PR** |
| Build / CI / dependency bump | **direct PR** (CI must stay green) |
| Clear bug fix | **Issue recommended, PR ok** |
| Feature Slice | **Issue required** + an observable acceptance condition |
| Public API / protocol | **Discussion → RFC** (before implementation) |
| Core data model | **RFC + Decision (D-\*)** |
| Security / compatibility change | **RFC + separate review** |

## What each row means

- **Docs / typos / small isolated tests — direct PR.** No Issue needed. The change is self-evident and
  isolated; CI is the only gate. If a "doc fix" implies a behavior change, it is not this row.

- **Build / CI / dependency bump — direct PR.** Changes to build config, CI workflows, `.github/`,
  or dependency/hook manifests (`Cargo.toml`/`Cargo.lock`/`pyproject.toml`/`lefthook.yml`) that don't change
  product behavior, spec, or doc meaning. This is the `build` change-kind (risk 1); CI staying green is the
  gate. Trusted automation (Dependabot) opens these without a gate block — the server re-derives the kind from
  the diff and still enforces artifacts, so a bot PR that strays into code is rejected, not waved through.

- **Clear bug fix — Issue recommended, PR ok.** Reproducible, understood, no contract change. An Issue helps
  tracking and links a fixture, but a well-described PR that reproduces the bug and adds a regression test is
  acceptable. If the "fix" changes intended behavior, it is a Feature Slice.

- **Feature Slice — Issue required + observable acceptance condition.** New capability within existing
  contracts. The Issue must state at least one **observable acceptance condition** (what a reviewer can watch
  succeed) before work starts. See [../operations/definition-of-done.md](../operations/definition-of-done.md).

- **Public API / protocol — Discussion → RFC before implementation.** Anything other code (plugins, remote
  peers, downstream tools) depends on. Start a Discussion to shape it, then write an **RFC** using
  [../../spec/templates/rfc.md](../../spec/templates/rfc.md). Code follows the accepted RFC, not the reverse.

- **Core data model — RFC + Decision.** Changes to load-bearing internal models (buffer/text, positions,
  registers, save format). These are hard to reverse: they need an RFC **and** a recorded Decision in
  [../../spec/DECISIONS.md](../../spec/DECISIONS.md) (a `D-*` entry) so the choice and its rationale are
  permanent.

- **Security / compatibility change — RFC + separate review.** Anything affecting the security posture or
  backward/forward compatibility. Requires an RFC and a **separate, dedicated review** distinct from normal
  code review. Report *vulnerabilities* privately via [SECURITY.md](../../SECURITY.md) — do not open a public
  issue.

## The load-bearing rule

**Spec lands before or with the first implementation PR — never after.**

If a change requires an RFC, PRD update, or Decision, that spec change is merged **before** the implementing
PR, or **in** the same PR — but the implementation is never merged first with a promise to document it later.
Product scope lives in [../../spec/PRD.yaml](../../spec/PRD.yaml); hard-to-reverse choices live as `D-*`
entries in [../../spec/DECISIONS.md](../../spec/DECISIONS.md); the design itself lives in an RFC
([template](../../spec/templates/rfc.md)).

See [../operations/development-model.md](../operations/development-model.md) for the full Spec-Gated
Iterative Development model.
