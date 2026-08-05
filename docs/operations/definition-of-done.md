---
doc: definition-of-done
project: ruse
title: "ruse Definition of Done"
summary: >
  The completion checklist for every Slice/PR under the Spec-Gated Iterative Development model. A change is
  not done because it compiles — it is done when its behavior is defined, tested (normal + failure +
  invariant), traced to a spec ID, documented without duplication, and its acceptance evidence is recorded.
  Extra gates apply to performance-sensitive and protocol/plugin-API changes.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - development-model.md
  - testing-and-benchmarks.md
  - github-workflow.md
  - spec-validate.md
---

# ruse Definition of Done

"Done" ≠ "compiles". Under [development-model.md](development-model.md), a Slice/PR reaches **Proven → Done**
only when its **evidence** exists. This is the checklist (mirrored in
[`.github/pull_request_template.md`](../../.github/pull_request_template.md)).

## Every Slice / PR

- [ ] Linked to a **Capability or Requirement** (`CAP-*` / `F-*`).
- [ ] **Observable behavior** is defined (not "added a struct").
- [ ] **Normal-path test** exists.
- [ ] **Failure-path test** exists.
- [ ] **Invariant violation** is tested (the relevant `INV-*`).
- [ ] Required **spec change** is included in the *same* PR (no document-after-implement).
- [ ] Explanation docs are **not duplicated** (one fact, one home).
- [ ] Any **new dependency** is justified (purpose · why-not-implement · tier · exposure · exit — see
      [`spec/dependencies.yaml`](../../spec/dependencies.yaml), D-034).
- [ ] Errors are **diagnosable** (typed error + code, not `Err(String)`).
- [ ] `cargo fmt`, `clippy`, `test` pass.
- [ ] **`python3 tools/spec-validate.py`** passes.
- [ ] The **acceptance evidence** used to accept the change is recorded in the PR.

## Performance-sensitive work — add

- [ ] A reproducible **benchmark baseline** exists.
- [ ] **Allocation / copy cost** is examined.
- [ ] Every performance **claim is backed by a measurement** (p50/p95/p99, not an average).

See [testing-and-benchmarks.md](testing-and-benchmarks.md) for methodology and the fixed-baseline-machine
gating rule (PR = trend + warn; main/nightly = gate).

## Protocol / plugin-API work — add

- [ ] A **versioning policy** ([`protocols/versioning-and-evolution.md`](../protocols/versioning-and-evolution.md)).
- [ ] An **unknown-field / unknown-variant** handling policy.
- [ ] A **compatibility test** (old-SDK / prior-version fixture).
- [ ] A **malformed-input test**.

## What "Proven" means (the evidence)

The evidence must match the work — one or more of: golden test · property test · differential-parity
fixture · benchmark vs baseline · executable headless/e2e demo · TUI dogfood · compatibility check ·
fault-injection · deterministic-replay. Never close work with a sleep-based flaky fix or a mock-only pass.
Formats: [testing-and-benchmarks.md](testing-and-benchmarks.md).

## Machine-checked subset

Some DoD items are enforced automatically today by `spec-validate` (the DoD readiness rule: every `mvp`/
`must` feature has a resolving `trace.design` + non-empty `acceptance`) and by CI (`fmt`/`clippy`/`test`,
`spec-check`). The rest are review-gated until the corresponding tooling/fixtures exist.
