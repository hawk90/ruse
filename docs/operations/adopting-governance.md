---
doc: adopting-governance
project: ruse
title: "ruse — Adopting the Governance Plane in Another Repository"
summary: >
  How to apply ruse's Repository Governance Plane to a different repository. The plane splits cleanly into a
  reusable ENGINE (the checkers under tools/, driven by one CLI and one verification registry) and per-repo
  POLICY (the spec/*.yaml files each checker reads). Adopting it means copying the engine, authoring your
  repo's policy, and wiring the three enforcement surfaces (ruse verify, lefthook, CI). This is the concrete
  companion to governance-model.md, which describes WHAT the plane is; this describes HOW to take it elsewhere.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - governance-model.md
  - change-workflow.md
  - ../../spec/verification.yaml
---

# Adopting the Governance Plane

The plane is built to be portable. See [governance-model.md](governance-model.md) for the model; this is the
adoption procedure.

## Engine vs. policy

| Layer | What | In another repo |
| --- | --- | --- |
| **Engine** (reusable) | `tools/ruse.py` + `tools/rusekit/` + the checkers under `tools/{change,verify,arch,gov,docs}/` | copy as-is |
| **Policy** (per-repo) | the `spec/*.yaml` files each checker reads | author your own |
| **Surfaces** | `ruse verify` (local) · lefthook (pre-commit/push) · CI | wire all three |

A checker never hard-codes policy; it reads a `spec/*.yaml`. Missing a policy file degrades to a reported
skip, never a crash — so you adopt incrementally, one policy file at a time.

## The per-repo policy files

Author these for your repo (start with the first three; the rest are optional/incremental):

| Policy file | Declares | Checker |
| --- | --- | --- |
| `spec/verification.yaml` | the ONE list of verify steps all three surfaces read | `ruse verify` |
| `spec/architecture.yaml` | crate/module dependency contract (may_depend_on, forbidden edges) | `ruse arch deps` |
| `spec/change-kinds.yaml` | the change-kind risk ladder + path triggers | `ruse change classify` |
| `spec/constitution.yaml` | standing invariant articles (CON-*) linked to enforcement | `ruse gov constitution` |
| `spec/fitness.yaml` | architecture fitness thresholds (FIT-*) | `ruse gov fitness` |
| `spec/waivers.yaml` | owned, dated, expiring rule exceptions | `ruse gov waivers` |
| `spec/contracts/*.yaml` | API/protocol/format contracts, declared contract-first | `ruse gov contracts` |

Each file ships with a header comment documenting its schema; copy ruse's as a template and replace the
entries. Governance checkers are **auto-discovered**: dropping a `tools/gov/<x>.py` module makes
`ruse gov <x>` and `ruse gov check` pick it up with no wiring.

## Wiring the three surfaces

1. **`ruse verify`** — reads `spec/verification.yaml`; run `python3 tools/ruse.py verify --full`.
2. **lefthook** — copy `lefthook.yml`; `commit-msg` validates the subject convention, `pre-commit` runs the
   fast checks, `pre-push` runs `ruse verify --full`. `lefthook install`.
3. **CI** — one job runs `python3 tools/ruse.py verify --full`; because CI reads the same
   `spec/verification.yaml`, CI and local cannot drift (the RA-RUSE-006 property).

## What is ruse-specific vs. generic

The **engine is generic**. The shipped **policy references ruse's own IDs** (ARCH-LAYER-001, ENG-*, D-*,
CAP-*, crate names) — that is exactly the part you replace. A checker that names ruse crates
(`architecture.yaml`) or ruse decisions (`contracts/*.yaml`) is showing you the *shape*; swap the values.

## Adoption checklist

- [ ] Copy `tools/` (engine) and `lefthook.yml`.
- [ ] Author `spec/verification.yaml` (steps for your stack) + `spec/architecture.yaml` (your modules).
- [ ] Add `spec/change-kinds.yaml`, `spec/waivers.yaml`; then constitution / fitness / contracts as needed.
- [ ] `lefthook install`; add a CI job calling `ruse verify --full`.
- [ ] Confirm `python3 tools/ruse.py gov check` and `verify --full` pass on a clean tree.

## Non-goals

- This is not a packaged distribution yet (no `pip install`); adoption is copy-in until `pyproject.toml`
  lands (tracked). The engine's only runtime dependency is PyYAML.
- The plane governs repository evolution; it does not replace a per-change execution harness — it sits
  above one (see [governance-model.md](governance-model.md)).
