---
doc: github-workflow
project: ruse
title: "ruse — GitHub Operating Model"
summary: >
  How ruse represents the Spec-Gated Iterative Development model on GitHub: the load-bearing principle
  (Label = nature, Project field = state, Milestone = stage-proof), a prefixed label taxonomy, one Project
  with fixed fields (Status/Type/Area/Stage/Priority/Risk/Spec-ID/Evidence/Target/Blocking), milestones as
  stage proofs (M0–M6), views, issue dependencies, Discussions-vs-Issues, automation, and branch/PR/commit
  conventions. The process definitions are canonical in development-model.md; this is the GitHub mechanics.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - development-model.md
  - definition-of-done.md
  - ci-cd-and-release.md
  - ../../CONTRIBUTING.md
---

# ruse — GitHub Operating Model

> **Load-bearing principle:** **Label = *nature* · Project field = *state* · Milestone = *stage proof*.**
> Never mix the three; never stuff metadata into issue **titles**.

The **process** (work-item types, Status meanings, stage gates, DoD) is defined once in
[development-model.md](development-model.md) — this doc only maps it onto GitHub. Labels are single-homed in
[`.github/labels.yml`](../../.github/labels.yml) (synced by an action; don't create labels in the UI).

## 1. Labels (nature) — prefixed, per-axis

| Prefix | Axis | Cardinality | Values |
| --- | --- | --- | --- |
| `type/` | Work-item type (see development-model.md) | exactly one | `capability` `rfc` `spike` `slice` `bug` `debt` `chore` `dependency` |
| `area/` | Subsystem(s) | one or more | `core` `document` `input` `render` `terminal` `remote` `language` `debug` `plugin` `infra` `docs` |
| `impact/` | Quality affected | 0+ | `correctness` `performance` `reliability` `security` `compatibility` `readability` |
| `community/` | Signposting | optional | `good-first-issue` `help-wanted` |
| `needs/` | What blocks readiness | 0+ | `reproduction` `design` `benchmark` `decision` |

**Not labels** (they are Project fields / milestones): `priority/*`, `status/*`, `size/*`, `target/*`.

## 2. The Project (state) — fields

| Field | Values |
| --- | --- |
| **Status** | `Inbox` → `Clarifying` → `Ready` → `Active` → `Review` → `Proven` → `Done` (meanings: development-model.md §Workflow) |
| **Type** | mirrors the `type/` label (Capability/RFC/Spike/Slice/Bug/Debt/Chore/Dependency) |
| **Area** | mirrors `area/` |
| **Stage** | `0`–`9` (build-order stage this belongs to) |
| **Priority** | `P0` data-loss/security/build-broken · `P1` blocks current stage · `P2` important for current stage · `P3` prep for next stage · `P4` idea/improvement |
| **Risk** | `Low` · `Medium` · `High` · `Experimental` (High: Document mutation, persistence format, remote protocol, plugin API, security boundary, cross-platform process) |
| **Spec ID** | linked `CAP-*` / `F-*` / `D-*` / `RFC-*` / `INV-*` |
| **Evidence** | how it will be Proven: `Test` · `Benchmark` · `Demo` · `Dogfood` · `Review` (see testing-and-benchmarks.md) |
| **Target** | current Milestone |
| **Blocking** | boolean — is this blocking other work |

> `Size` (XS–XL, complexity not time; `XL` = must split before Ready) may be added later; keep it a field,
> never a label.

## 3. Milestones = stage proofs (not sprints)

`M0`–`M6` (+`Someday`), each with a one-sentence exit condition — the canonical table is in
[development-model.md](development-model.md) §Milestones. A milestone answers "which stage proof", not
"which feature" (use `area/*` for that).

## 4. Views

| View | Type | Filter |
| --- | --- | --- |
| **Inbox** | Table | Status = Inbox or Clarifying |
| **Current** | Board (by Status) | Target = current milestone |
| **Roadmap** | Roadmap | grouped by Milestone / Target date |
| **Architecture** | Table | `type/rfc` OR `type/debt` OR `impact/readability` |
| **Remote** | Table | `area/remote` |
| **Reliability** | Table | `impact/correctness` OR `impact/reliability` OR `impact/security` |
| **Dependencies** | Table | `type/dependency` |
| **Proving** | Table | Status = Active or Review or Proven (evidence in flight) |

## 5. Issue dependencies

A **Slice** is the smallest mergeable behavior; a big Capability is a **parent** issue with a tasklist of
Slices (or GitHub native issue dependencies):

```md
### Remote Workspace MVP  (parent, type/capability area/remote, Stage 6)
- [ ] #NN Agent bootstrap
- [ ] #NN Handshake
- [ ] #NN Filesystem
- [ ] #NN Watcher / Search / PTY / Reconnect / LSP host
```

Tasks attach to a Slice (checklist inside it), never as standalone roadmap items (development-model.md).

## 6. Discussions vs Issues

- **Discussions** — questions, half-formed ideas, "should we…", open design chatter.
- **Issues** — anything **actionable**: reproducible bug, approved Capability/Slice, approved RFC/Spike.
- **Promote** a Discussion to an Issue once concrete (scope + acceptance evidence known → `Ready`).

## 7. Automation

- New **issue** → Status **Inbox**.
- New **PR** → Status **Review**.
- Issue/PR **closed** (incl. `Closes #NN` on squash-merge) → Status **Done**.
- Path-based `area/*` labels via [`.github/labeler.yml`](../../.github/labeler.yml) (advisory).
- Archive **Done** cards after ~14–30 days.

> Note: "Proven" is a human/CI transition (evidence obtained), not an automated one — a PR merges to Done
> only after its Evidence is recorded (definition-of-done.md).

## 8. Branches · PRs · Commits

**Branches** (short-lived; `main` always validation-green): `feat/<issue>-<name>` · `fix/<issue>-<name>` ·
`spec/<issue>-<name>` · `rfc/<issue>-<name>` · `spike/<issue>-<name>`.

**One PR = one kind of change**: a spec change · an RFC/decision · a spike result · a vertical Slice · a bug
fix · a mechanical refactor. Never combine a spec change + big refactor + new feature. **Squash-merge**; the
PR title is the durable changelog entry. PR checklist = [definition-of-done.md](definition-of-done.md).

**Commit / PR-title types** (Conventional-ish, ruse subset):

| Type | Meaning |
| --- | --- |
| `spec` | normative state change (spec/) |
| `rfc` | architectural proposal / RFC |
| `feat` | product/platform capability |
| `fix` | defect fix |
| `refactor` | behavior-preserving structural change |
| `test` | automated verification |
| `bench` | performance evidence |
| `docs` | supporting explanation / research |
| `build` | Cargo / CI / toolchain |
| `chore` | maintenance |

Write **why**, not the filename. Bad: `update document.rs`. Good:
`fix(core): reject transactions based on stale revisions`.

Release notes are generated by label ([`.github/release.yml`](../../.github/release.yml)), so PRs need the
right `type/*` label.
