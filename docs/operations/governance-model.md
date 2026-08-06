---
doc: governance-model
project: ruse
title: "ruse — Repository Governance Model"
summary: >
  ruse is not only a spec-first editor; it is a reusable Repository Governance Plane meant to be applied to
  other repositories. A per-change execution harness (plan → run → verify → sync, à la MoAI-ADK) answers
  "is this change good?"; a large multi-contributor / human+agent repository additionally needs "is the
  repository evolving safely?" This doc defines the seven-layer governance model, maps ~25 methodologies
  onto what ruse already enforces vs what is planned, and states the portability contract: policy is
  per-repo YAML under spec/, the engine is the reusable checkers under tools/.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - change-workflow.md
  - ../../spec/change-kinds.yaml
  - ../../spec/architecture.yaml
  - ../../spec/waivers.yaml
  - ../../spec/review-axes.yaml
  - ../../spec/dependencies.yaml
  - ../../spec/DECISIONS.md
---

# Repository Governance Model

> **The thesis.** A change-execution harness controls *the agent producing a change*. A governance plane
> controls *the evolution of a repository that humans and agents change together*. ruse aims to be the
> second, layered on top of the first — so it is not a clone of a per-change harness but a level above it.

## Harness vs. governance plane

| | Per-change harness (e.g. MoAI-ADK) | Repository Governance Plane (ruse) |
| --- | --- | --- |
| Question | Is *this* change correct, tested, traceable? | Is the *repository* evolving safely over time? |
| Unit | one PR / SPEC | the whole repo: structure, churn, ownership, risk |
| Strength | plan→run→sync, TDD/DDD, TRUST gates, role split | architecture-as-code, impact, risk, repo health, waivers |
| Blind spot | integration friction that only shows in aggregate | (delegates single-change execution to the harness) |

ruse uses the harness pattern for execution and adds the governance plane above it. The two are
complementary, not the same.

## Portability contract

The plane is designed to be adopted by other repositories:

- **Policy = per-repo YAML under `spec/`** — `change-kinds.yaml`, `architecture.yaml`, `waivers.yaml`,
  `dependencies.yaml`, `review-axes.yaml`, `PRD.yaml`, `POLICY.yaml`. A new repo ships its own.
- **Engine = reusable checkers under `tools/`** — `ruse.py` + `rusekit/` + `change/ verify/ arch/ gov/
  docs/`. Repo-agnostic; they read the policy.
- **Enforcement = the same three surfaces** — `ruse verify` (local), lefthook (pre-commit/push), CI.

To govern another repo: copy `tools/`, author that repo's `spec/*.yaml` policy, wire the three surfaces.

## The seven layers

Each methodology is tagged **[live]** (enforced now), **[partial]** (declared / partially enforced), or
**[planned]** (needs code, history, or infra that the spec-first phase does not yet have).

### 1. Intent layer — *what may change and why*
| Methodology | Status | Where |
| --- | --- | --- |
| Spec-Driven Development | live | [change-workflow.md](change-workflow.md), spec/PRD.yaml, spec-validate |
| Repository Constitution (agent-invariant principles) | partial | spec/POLICY.yaml (ENG-*), CLAUDE.md/AGENTS.md, [change-workflow.md](change-workflow.md) roles → explicit `constitution.yaml` planned |
| Decision Records + revisit triggers | partial | spec/DECISIONS.md (Re-evaluate lines) → fitness-linked auto-revisit planned |
| Contract-Driven Development | partial | docs/protocols, D-010 promotion ladder → per-contract files + consumer-driven tests planned |

### 2. Structural layer — *the shape must hold*
| Methodology | Status | Where |
| --- | --- | --- |
| Architecture as Code | **live** | [spec/architecture.yaml](../../spec/architecture.yaml) + `ruse arch deps` (ARCH-LAYER-001 / ARCH-FORBID-001) |
| Dependency Governance | live | spec/dependencies.yaml + `ruse arch deps` (cycles, allowed_layers) + PR-template dep gate |
| Architecture Fitness Functions | partial | static (cycles=0, direction) live via `arch deps`; quantitative (build/size/API) planned |
| API Lifecycle Governance | partial | D-010 Internal→Experimental→Preview→Stable → `api-diff` planned (verify NOT_YET) |
| Ownership / Team Topologies (Conway) | planned | CODEOWNERS exists; ownership fitness planned |

### 3. Change layer — *each change, sized to its risk*
| Methodology | Status | Where |
| --- | --- | --- |
| Change Impact Analysis | live | `ruse impact` (spec cross-reference graph) → runtime/coverage graph planned |
| Risk-Based Verification | live | spec/change-kinds.yaml risk ladder + `ruse verify` scope selection |
| Evidence-Driven Completion | live | `.ruse/work/<id>/evidence.json` from `ruse verify` → structured artifact retention planned |
| Property / Model-Based / Differential testing | partial | anchor-store test plan (extmark parity, undo round-trip) declared; corpus absent |

### 4. Agent layer — *humans and agents, bounded*
| Methodology | Status | Where |
| --- | --- | --- |
| Separation of Duties (Planner/Implementer/Reviewer/Verifier/Human) | live | [change-workflow.md](change-workflow.md) §AI roles |
| Context Governance (manifest + freshness) | live | `ruse context build/check` + context-lock.json → `forbidden`/`freshness` fields planned |
| Least Privilege / Sandbox isolation | partial | worktree isolation available; tool-permission policy planned |
| AI Provenance & Reproducibility | partial | PR-template AI-assistance block → structured provenance record planned |
| Model-diverse review | planned | |

### 5. Repository layer — *the whole tree stays healthy*
| Methodology | Status | Where |
| --- | --- | --- |
| Repository Health / Hotspot / Integration-friction metrics | planned | needs git history |
| Technical Debt as a governed asset (budget, interest, expiry) | partial | anti-patterns + review-axes tiers → debt ledger + budget planned |
| Build / Test / Dependency budgets | planned | needs measured baselines |

### 6. Delivery layer — *safe past merge*
| Methodology | Status | Where |
| --- | --- | --- |
| Phased delivery planning + milestone sync | **live** | [spec/phases.yaml](../../spec/phases.yaml) (ordered phase ladder, refines PRD `stage`) → `tools/phases.py` (validated in spec-validate) + `ruse phase sync` (one-way GitHub-milestone mirror, dry-run default); D-037 |
| Progressive Delivery / canary / rollback | planned | needs release channels (nightly→beta→stable) |
| Compatibility gates | partial | protocol versioning policy present; fixtures planned |
| Continuous Verification (triggered/nightly/on-toolchain-change) | partial | PR/merge live via CI + lefthook; nightly/triggered planned |

### 7. Exception layer — *no indefinite escapes*
| Methodology | Status | Where |
| --- | --- | --- |
| Governance Waiver Workflow | **live** | [spec/waivers.yaml](../../spec/waivers.yaml) + `ruse gov waivers` (owned, dated, expiring; consulted by `pr check`) |

## The governance flow

The harness flow is *requirement → plan → implement → test → review → sync*. The governance flow wraps it:

```
define intent
  → check structural constraints (architecture.yaml)
    → compute impact (ruse impact)
      → score risk (change-kinds)
        → execute the change (harness)
          → verify contracts + properties (verify)
            → check repository health (planned)
              → preserve evidence (evidence.json)
                → continuous re-verification (CI/nightly)
                  → expire policy waivers (gov waivers)
```

## Priorities (honest status)

- **P0 — foundational.** Architecture as Code **[done]**, Governance Waiver **[done]**, Change Impact
  **[done]**, Risk-Based **[done]**, Context Manifest **[done]**; Fitness Functions (quantitative) and
  Contract files **[next]**.
- **P1 — before scale.** Ownership governance, API-diff, Decision revisit triggers, structured Evidence
  artifacts, Agent provenance, Tech-debt budget, Continuous Verification (nightly).
- **P2 — once large.** InnerSource, Progressive Delivery, Conway fitness, integration-friction metrics,
  Formal-methods-lite, model-diverse review, error/quality budgets.

> These layers are declared before they are all enforceable **on purpose**: in the spec-first phase the
> policy lands before the code it will constrain, and each checker reports `live` vs `not-yet` honestly
> rather than passing silently.
