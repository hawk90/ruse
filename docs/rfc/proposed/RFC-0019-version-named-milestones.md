---
doc: rfc
project: ruse
title: "RFC-0019: Version-named delivery milestones"
summary: >
  Each phase in spec/phases.yaml carries a release version, and that version titles its GitHub Milestone.
  Phases remain the spec axis and milestones remain a one-way mirror. Records D-059.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/DECISIONS.md
  - ../../../spec/phases.yaml
---

# RFC-0019: Version-named delivery milestones

## Problem

Milestones titled by phase do not state which release a feature ships in.

## Design

- `spec/phases.yaml`: every phase has `version: vX.Y.Z`. `tools/phases.py` rejects a missing, malformed or
  non-increasing version.
- `tools/phase_sync.py`: the milestone title is the version. The hidden phase marker in the description stays
  the identity, so a version change renames the milestone instead of creating another.
- `tools/gen_roadmap.py`: the generated phase table gains a Version column.

| Phase | Version |
| --- | --- |
| `core-editing` | v0.1.0 |
| `usable-vim-tui` | v0.2.0 |
| `ide-grade` | v0.3.0 |
| `ecosystem` | v1.0.0 |
| `breadth` | v2.0.0 |

The ladder keeps the roadmap's definition of 1.0 as `ide-grade` plus `ecosystem`.

## Non-goals

Tagging releases, and changing `Cargo.toml` versions. Neither is decided here.
