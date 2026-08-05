---
doc: operations-review-axes
project: ruse
title: "Review Axes — the inspection rubric"
summary: >
  How ruse's review-axis catalog works: a machine-checkable list of the dimensions a review should cover,
  tagged by assessment method and priority tier, validated as part of the single verification entry point.
audience: [maintainers, contributors, llm-agents]
status: draft
---

# Review Axes

`spec/review-axes.yaml` is the durable catalog of **what a review of ruse should check** — 566 axes across 20
domains plus a `RUSE` domain of 10 cross-cutting P0 priorities. It captures the *questions* (definitions),
never the *answers*: a point-in-time assessment lives in a dated report under [`docs/reviews/`](../reviews/verification.md).

## Model

- **Method** — each axis is honest about how it can be assessed: `machine` (a tool/test/CI gate decides),
  `llm` (an LLM rubric assesses from the repo), `manual` (human judgement), `mixed`. Only ~24% are
  `machine`; the rest are judgement. This is deliberate — it exposes the **spec-vs-enforcement gap**
  (`RA-RUSE-001`) instead of hiding it.
- **Tier** — `P0` (ruse-critical, the `RUSE` domain — check first) · `P1` core correctness/architecture ·
  `P2` quality · `P3` long-horizon/polish.
- **Inheritance** — a domain sets `default_method`/`default_tier`; an axis overrides only when it differs.
  Adding an axis is one line.
- **Stable IDs** — `RA-<DOMAIN>-NNN`. Never rename or renumber (`RA-LLM-013`). `refs:` cross-link related
  axes; the `RUSE` axes add a sharpened `q:` question and `refs:` into the domains they summarize.

## Tool

`tools/review_axes.py` (reference implementation):

```
python3 tools/review_axes.py                 # validate the catalog (structure + refs); exit 1 on error
python3 tools/review_axes.py --stats         # counts by domain/tier/method + machine-automatable share
python3 tools/review_axes.py --list --tier P0 --domain ARCH --method machine
python3 tools/review_axes.py --json          # fully-resolved catalog as JSON (generated view; not committed)
```

The catalog is validated as part of [`spec-validate`](spec-validate.md) (imported, not shelled out) so
there is a **single verification entry point** (`RA-CICD-002`): `python3 tools/spec-validate.py` reports
`review-axes=<n>` and fails on any duplicate id, bad enum, malformed id, or dangling `ref`. The catalog thus
dogfoods its own rules (`RA-LLM-013/014/015/016`) — the review system reviews itself.

## Running a review

1. Pick a scope — usually **P0 first** (`--list --tier P0`), then the domains relevant to the change.
2. For `machine` axes, wire or run the gate; for `llm`/`manual`, assess against the repo.
3. Record findings in a **dated report** under `docs/reviews/` (id + verdict + evidence), never back into
   the catalog. Keep the JSON view (`--json`) out of git — regenerate on demand to avoid drift.

## Extending

Add an axis: one `{ id, title }` line under the right domain (override `method`/`tier`/add `refs` if
needed). Add a domain: a new block with `id`, `title`, `default_method`, `default_tier`, `axes`. Re-run
`spec-validate` — new ids are checked immediately.
