---
doc: spec-validate
project: ruse
title: "spec validate — Doc-System Checker & Generator (D-022)"
summary: >
  Specification for the `spec validate` / `spec build` toolchain that keeps spec/+docs/ consistent and
  generates derived artifacts. A working reference implementation lives at tools/spec-validate.py and passes
  on the current tree. This is the enforcement mechanism behind ENG-DOC-001 (one fact, one home) and the
  cross-reference discipline; D-022 tracks promoting it to the real repo `xtask`.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../../spec/DECISIONS.md
  - ../../spec/POLICY.yaml
  - ci-cd-and-release.md
---

# spec validate — Doc-System Checker & Generator

The spec/docs discipline (one fact one home, spec IDs point into docs, records-as-YAML / prose-as-Markdown)
is only real if a tool enforces it. `spec validate` is that tool. A precise reference implementation is
[`tools/spec-validate.py`](../../tools/spec-validate.py) (passes on the current tree); D-022 promotes it to
the repo's `xtask` once code exists. Two subcommands:

- **`spec validate`** — checks; exit 0 = PASS, 1 = FAIL. Runs in fast PR CI ([ci-cd-and-release.md](ci-cd-and-release.md) §1).
- **`spec build`** — regenerates derived artifacts (below); CI fails if regeneration would change a
  committed generated file (i.e. someone hand-edited it or forgot to rebuild).

## Inputs

| Kind | Files | Role |
| --- | --- | --- |
| State (YAML) | `spec/PRD.yaml`, `spec/POLICY.yaml`, `spec/context-profiles.yaml`, `spec/glossary.yaml` | parsed as data |
| ID registries (Markdown) | `docs/invariants/reference-invariants.md` (INV-*), `docs/anti-patterns/anti-patterns.md` (CATEGORY-n), `spec/DECISIONS.md` (D-*), `spec/ARCHITECTURE.md` (ARCH-*) | IDs extracted for cross-ref checks |
| Link surface | all `*.md` under `spec/` and `docs/` | relative links resolved |

### ID-extraction rules (deterministic)

| ID class | Source | Pattern |
| --- | --- | --- |
| `INV-*` | reference-invariants.md | bold `**INV-XXX**` |
| `ENG-*` | POLICY.yaml | `principles:` map keys |
| `D-*` | DECISIONS.md | heading `## D-NNN` |
| `ARCH-*` | ARCHITECTURE.md | token `ARCH-WORD-NNN` |
| `F-*` / `C-*` | PRD.yaml | `features:` / `components:` keys |
| anti-pattern `CODE-n` | anti-patterns.md | `## CODE — Name` (ALL-CAPS code) then a numbered list ⇒ valid range `CODE-1..CODE-N` |

Anti-pattern categories are ALL-CAPS by convention, so section headers like `## Critical-15 — …` or
`## Priorities — …` are correctly excluded from the category set.

## Checks (validate)

1. **YAML parses**; PRD `stage/priority/status` and POLICY `strength/status` are within their closed enum
   sets; no duplicate `F-`/`C-`/`D-` IDs.
2. **PRD component `depends_on`** resolve, and respect **build_stage order** `kernel < input < tui < workspace <
   plugin < remote` (a component may not depend on a later-stage component; D-036). This makes the build order
   machine-checkable (V-10/V-11 could not have slipped past this).
3. **PRD feature `depends_on`** resolve to a component or feature.
4. **POLICY `invariants:`** resolve to the INV-* registry.
5. **POLICY `antipattern.refs`** resolve to a real anti-pattern catalog ID (`CODE-n` in range).
6. **context-profiles `include`** resolve to a known ID (F/C/ENG/ARCH/D/INV), an anti-pattern ID, or an
   existing file.
7. **Every Markdown relative link** under `spec/` and `docs/` resolves.

The v1 implementation is intentionally **precise (zero false positives)**: it does not fuzzily parse
free-text `Refs:` lines in DECISIONS or parity IDs (those are reviewed). Planned strict additions as the
registries stabilize: DECISIONS `Refs:` INV/ENG/D/F/C resolution, POLICY exception-expiry, illegal
status-transition detection, and parity-ID resolution.

## Generated artifacts (build)

Authored source → generated output (never hand-edit the outputs — D-021):

| Output | From |
| --- | --- |
| `spec/CONTEXT.md`, `.context/<profile>.md` | PROJECT + PRD + ARCHITECTURE + POLICY + DECISIONS + context-profiles.yaml |
| human glossary table (in PROJECT.md or a generated page) + `glossary.json` | `spec/glossary.yaml` |
| `anti-patterns.index.yaml` (id → category → label) | `docs/anti-patterns/anti-patterns.md` |

## Output & exit codes

```
registries: INV=29 ENG=17 D=32 ARCH=6 F=21 C=24 anti-pattern-categories=37 glossary-terms=20
md files checked: 43
spec validate: PASS
```

Exit `0` PASS, `1` FAIL (lists each error). Warnings do not fail the build.

## CI integration

- **PR (fast CI):** `spec validate` must pass to merge (blocks dangling refs, build_stage violations, broken
  links, bad enums, duplicate IDs).
- **PR:** `spec build` then `git diff --exit-code` on generated files (catches hand-edited or stale
  generated artifacts).
- Wire into [ci-cd-and-release.md](ci-cd-and-release.md) §1 (`ci-fast.yml`).

## Status

Reference implementation `tools/spec-validate.py` **passes** on the current tree. Promotion to the repo
`xtask` (Rust) with the planned strict additions is tracked by **[DECISIONS D-022](../../spec/DECISIONS.md)**.
