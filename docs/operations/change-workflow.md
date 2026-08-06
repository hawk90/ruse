---
doc: change-workflow
project: ruse
title: "ruse — Change Workflow (Classify → Impact → Plan → Approve → Execute → Verify → Reconcile → Merge)"
summary: >
  The layer above Git Flow for a spec-first, AI-paired project: before an agent starts, the change declares
  its kind and blast radius, and the required documents/verification/approval follow from that kind. A human
  owns Approve and Merge; the AI amplifies Plan and Execute; scripts verify the result independently. This
  documents the single entry point `python3 tools/ruse.py` and its P0 subcommands, the Change Contract
  (.ruse/work/<id>/change.yaml), and how classification can only raise risk, never lower it.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../contributing/change-paths.md
  - ../contributing/ai-assisted-development.md
  - ../../spec/change-kinds.yaml
  - ../../spec/templates/change.yaml
  - ../../spec/DECISIONS.md
  - spec-validate.md
---

# Change Workflow

Git Flow answers *how code lands*. It does not answer *what kind of change this is, and therefore what
process it needs*. Treating an architecture change and a typo the same way is either too heavy (everything
becomes an RFC) or too dangerous (an AI silently reshapes the architecture through a stream of "small" PRs).
This workflow sits above Git Flow and picks the right process from the change's declared kind.

```
idea / bug / requirement
      ↓
Classify → Impact → Plan → Approve → Execute → Verify → Reconcile → Merge
```

**The division of responsibility is fixed:** the AI may do **Plan** and **Execute**; a human owns **Approve**
and **Merge**; **Verify** is scripts, not a model's assurance. A model saying "tests pass" is not evidence
(see [ai-assisted-development.md](../contributing/ai-assisted-development.md)).

## One command

Contributors remember one command; everything else is behind it.

```bash
python3 tools/ruse.py <command>
```

| Command | What it does |
| --- | --- |
| `change start --issue <id> --kind <k> --area <crate>` | Scaffold `.ruse/work/<id>/` (the Change Contract) |
| `change classify [--base <ref>] [--files ...] [--kind <k>]` | Compute the **minimum** required kind from the diff |
| `impact --from <ID>` / `impact --changed` | What a spec ID or the diff ripples into |
| `context build --issue <id>` / `context check` | Task-scoped context pack for an AI + staleness lock |
| `plan validate [path]` | Check a plan covers every required perspective |
| `verify --changed` / `verify --full` | Run only the checks the diff needs; record real evidence |
| `spec validate` / `spec generate` | Reference checker + change-workflow extensions / (P1) generator |
| `pr render` / `pr check` | Generate the PR body / run the merge gate |
| `status` | Show the active change workspace |

## Change kinds (the risk ladder)

The authoritative taxonomy is [spec/change-kinds.yaml](../../spec/change-kinds.yaml) — one fact, one home.
It mirrors the human one-minute guide in [change-paths.md](../contributing/change-paths.md).

| Kind | Risk | Required process |
| --- | --- | --- |
| `docs-editorial` | 0 | direct PR (CI only) |
| `docs-semantic` | 1 | link the spec/Decision ID; PR |
| `implementation` | 2 | Issue + one observable acceptance condition |
| `spec` | 2 | Issue + spec diff (spec lands before/with the first impl PR) |
| `architecture` | 3 | RFC + impact analysis + human approval; Decision when hard to reverse |
| `contract` | 3 | RFC + compatibility/migration + separate review |

**Load-bearing rule:** the classifier may **raise** the required risk above what the author declared; it may
**never lower** it. Declaring `docs-editorial` while touching `spec/ARCHITECTURE.md` **fails**. Declaring
`architecture` for a one-line README change only **warns**. Whether a docs edit is editorial or semantic —
and whether something is "really" an architecture change — stays a human judgment; the tool only forces a
floor when a path is unambiguously high-risk, and hard-fails a hand-edit of a generated file (ENG-DOC-001).

## The Change Contract

`change start` scaffolds a small, **local** agreement at `.ruse/work/<id>/change.yaml`
(template: [spec/templates/change.yaml](../../spec/templates/change.yaml)). `.ruse/` is gitignored — only
the final RFC / Decision / PRD change / PR is permanent. The contract declares the kind, the affected spec
IDs (`CAP-*`/`F-*`/`C-*`/`INV-*`/`D-*`), the crates, whether a stability boundary is crossed, the
`allow_paths` / `forbid_paths` blast radius, and the closing evidence. It is not a prompt — it is the set of
permissions and verification conditions the implementer (human or agent) works within.

```
.ruse/work/<id>/
├── change.yaml       # the contract
├── plan.md           # Planner output (plan validate checks completeness)
├── context.md        # context build output (what to read)
├── context-lock.json # source hashes → context check detects staleness
├── impact.json       # impact --out record
└── evidence.json     # verify records real command results here
```

### The gate: local preflight vs. the CI gate of record

`.ruse/` is gitignored **on purpose**, and CI never trusts it — a self-reported `evidence.json` cannot be a
merge gate (it is trivially fabricated). So the gate is two-tier:

- **Local `ruse pr check`** — a *preflight*: it reads your `.ruse/` and tells you whether the gate will pass.
  Convenience, not authority.
- **CI `change-policy` job** — the *gate of record* ([`.github/workflows/change-policy.yml`](../../.github/workflows/change-policy.yml)).
  It reads only the author's DECLARED contract from the `ruse-gate:v1` block that `ruse pr render` embeds in
  the PR body, then **re-derives** the observed kind + blast radius from the real diff and relies on the
  re-run verify jobs (`spec-check.yml`) for evidence:
  `ruse pr check --pr-body <body> --base origin/<base>`. Nothing on the contributor's machine is trusted.

The author declares; CI verifies against the diff. Under-declaring the kind, or straying outside
`allow_paths`, fails in CI regardless of what the local run reported.

## Typical flows

**Typo / editorial docs** — the light path:

```bash
python3 tools/ruse.py change start --issue fix-typo --kind docs-editorial
python3 tools/ruse.py verify --changed
```

**Feature slice / implementation:**

```bash
python3 tools/ruse.py change start --issue 123 --kind implementation --area core --goal "..."
# edit change.yaml: affected IDs, allow_paths
python3 tools/ruse.py context build --issue 123     # what to read (bounded)
python3 tools/ruse.py impact --issue 123 --out .ruse/work/123/impact.json
# ... implement within allow_paths ...
python3 tools/ruse.py verify --changed               # records evidence.json
python3 tools/ruse.py pr render --issue 123 > pr.md
python3 tools/ruse.py pr check --issue 123           # the gate
```

**Architecture / contract change** — never "refactor X to Y" straight to an agent. First
`impact --from <ID>` to compute the direct + transitive blast radius, write an **RFC** (status
`proposed` → `provisional` → `accepted` → `implemented`; do not mark `accepted` before a vertical slice
proves it), get human approval, then implement. `pr check` requires the RFC and recorded impact.

## What is automated vs. human

**Scripts (never miss):** changed paths, spec-ID reference resolution, declared-vs-observed kind, generated
file edits, blast-radius escape, missing evidence, dependency direction (P1), public-API/protocol diff (P1).

**Humans (decide meaning):** whether a change is truly architectural, whether a trade-off is sound, whether
to freeze something as a public contract, whether the implementation evidence is sufficient, and final
approval + merge.

## AI roles

Do not let one AI session plan, implement, and self-review — it rationalizes its own decisions. Split the
roles: **Planner** (read-only, writes the plan), **Implementer** (codes within the approved `allow_paths`),
**Reviewer** (fresh context, reports but does not edit), **Verifier** (scripts, not a model), **Human**
(approval, architecture, merge). The tool is model-agnostic; what matters is that the same session does not
own plan + implement + final review.

## Status & roadmap

Implemented (P0): the entry point, `change start`/`classify`, `impact`, `context build`/`check`,
`plan validate`, `verify`, `spec validate` extensions, `pr render`/`check`.

Implemented (P1): `docs check` (anchor / frontmatter / normative-leak hygiene, complementary to
spec-validate) and `arch deps` / `dependency-check` (spec/dependencies.yaml `allowed_layers` placement +
crate dependency-cycle detection; enforces an explicit `crate_layers:` direction map if one is added to
spec/dependencies.yaml). Both are wired into `verify`.

Deferred (P1+, reported as "not built yet" by `verify`): `public-api-diff` and `protocol-compat` (need a
git baseline and real Rust/protocol surface — the crates are still skeletons), and the `spec generate`
derived-artifact generator (tracked by D-022). These raise the floor of what `verify --full` and `pr check`
can enforce as the reference implementation grows.
