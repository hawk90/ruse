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

Each item below carries a stable `DOD-<n>` ID and an enforcement tag (`[machine]` | `[llm]` | `[manual]`,
using the review-axes `method` vocabulary). The `[machine]` items are the CI-enforced subset — decided by
the required checks in [CONTRIBUTING.md](../../CONTRIBUTING.md); the `[llm]` and `[manual]` items are
review-gated.

## Every Slice / PR

- [ ] **DOD-1** [llm] Linked to a **Capability or Requirement** (`CAP-*` / `F-*`).
- [ ] **DOD-2** [llm] **Observable behavior** is defined (not "added a struct").
- [ ] **DOD-3** [llm] **Normal-path test** exists.
- [ ] **DOD-4** [llm] **Failure-path test** exists.
- [ ] **DOD-5** [llm] **Invariant violation** is tested (the relevant `INV-*`).
- [ ] **DOD-6** [llm] Required **spec change** is included in the *same* PR (no document-after-implement).
- [ ] **DOD-7** [llm] Explanation docs are **not duplicated** (one fact, one home).
- [ ] **DOD-8** [llm] Any **new dependency** is justified (purpose · why-not-implement · tier · exposure · exit — see
      [`spec/dependencies.yaml`](../../spec/dependencies.yaml), D-034).
- [ ] **DOD-9** [llm] Errors are **diagnosable** (typed error + code, not `Err(String)`).
- [ ] **DOD-10** [machine] `cargo fmt`, `clippy`, `test` pass.
- [ ] **DOD-11** [machine] **`python3 tools/spec-validate.py`** passes.
- [ ] **DOD-12** [llm] The **acceptance evidence** used to accept the change is recorded in the PR.

## Performance-sensitive work — add

- [ ] **DOD-13** [machine] A reproducible **benchmark baseline** exists.
- [ ] **DOD-14** [manual] **Allocation / copy cost** is examined.
- [ ] **DOD-15** [llm] Every performance **claim is backed by a measurement** (p50/p95/p99, not an average).

See [testing-and-benchmarks.md](testing-and-benchmarks.md) for methodology and the fixed-baseline-machine
gating rule (PR = trend + warn; main/nightly = gate).

## Protocol / plugin-API work — add

- [ ] **DOD-16** [llm] A **versioning policy** ([`protocols/versioning-and-evolution.md`](../protocols/versioning-and-evolution.md)).
- [ ] **DOD-17** [llm] An **unknown-field / unknown-variant** handling policy.
- [ ] **DOD-18** [machine] A **compatibility test** (old-SDK / prior-version fixture).
- [ ] **DOD-19** [llm] A **malformed-input test**.

## What "Proven" means (the evidence)

The evidence must match the work — one or more of: golden test · property test · differential-parity
fixture · benchmark vs baseline · executable headless/e2e demo · TUI dogfood · compatibility check ·
fault-injection · deterministic-replay. Never close work with a sleep-based flaky fix or a mock-only pass.
Formats: [testing-and-benchmarks.md](testing-and-benchmarks.md).

## Machine-checked subset

Some DoD items are enforced automatically today by `spec-validate` (the DoD readiness rule: every `mvp`/
`must` feature has a resolving `trace.design` + non-empty `acceptance`) and by CI (`fmt`/`clippy`/`test`,
`spec-check`). The rest are review-gated until the corresponding tooling/fixtures exist.
