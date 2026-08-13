---
doc: rfc-index
project: ruse
title: "ruse RFC Index"
summary: >
  Index of ruse RFCs. RFCs are only for hard-to-reverse decisions (save format, plugin protocol, command
  semantics, document/transaction boundary, remote protocol, compatibility policy). Each follows
  spec/templates/rfc.md and ends with Alternatives / Rejected approaches / Trade-offs / Re-evaluation +
  Reference Invariants. Small changes go in PR descriptions, not RFCs.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../README.md
  - ../../spec/DECISIONS.md
  - ../../spec/templates/rfc.md
---

# ruse RFC Index

Folders: `proposed/` (under discussion) · `accepted/` (ratified) · `rejected/` (kept forever so decisions
aren't re-litigated). An RFC records *why*; the design docs record *how* (linked, not duplicated); the
compact decision lives in [`spec/DECISIONS.md`](../../spec/DECISIONS.md).

## Proposed

| RFC | Title | Decisions | Design detail |
| --- | --- | --- | --- |
| [RFC-0001](proposed/RFC-0001-project-vision.md) | Project Vision & Non-Goals | D-020, D-021 | [spec/PROJECT.md](../../spec/PROJECT.md), [design-charter](../architecture/design-charter.md) |
| [RFC-0002](proposed/RFC-0002-workspace-architecture.md) | Workspace Architecture | D-003 | [architecture §7](../architecture/architecture.md), [parity/workspace](../parity/workspace.md) |
| [RFC-0003](proposed/RFC-0003-plugin-api.md) | Plugin API & Lifecycle | D-004, D-009, D-010 | [architecture §4](../architecture/architecture.md), [protocols](../protocols/versioning-and-evolution.md) |
| [RFC-0004](proposed/RFC-0004-input-profiles.md) | Input Profiles & Command Layer | D-006, D-008, D-025/026/027 | [architecture §1–2](../architecture/architecture.md), [parity](../parity/README.md) |
| [RFC-0005](proposed/RFC-0005-terminal-capability.md) | Terminal Capability | D-015 | [parity/terminal](../parity/terminal.md), [architecture §6](../architecture/architecture.md) |
| [RFC-0006](proposed/RFC-0006-remote-runtime.md) | Remote Runtime & Agent | D-011/012/013/024/029/030/031/032 | [remote-runtime](../design/remote-runtime.md), [parity/remote](../parity/remote.md) |
| [RFC-0007](proposed/RFC-0007-transaction-engine.md) | Transaction Engine | D-001, D-005, D-025 | [architecture §3](../architecture/architecture.md), [stability](../design/stability-and-observability.md) |
| [RFC-0008](proposed/RFC-0008-document-model.md) | Document Model & Coordinates | D-003, D-023 | [architecture §3](../architecture/architecture.md) |
| [RFC-0009](proposed/RFC-0009-render-model.md) | Render Model & Frontends | D-014, D-015, D-012 | [render-and-frontends](../design/render-and-frontends.md), [parity/terminal](../parity/terminal.md) |
| [RFC-0010](proposed/RFC-0010-stability-observability.md) | Stability & Observability | D-016, D-017 | [stability-and-observability](../design/stability-and-observability.md) |
| [RFC-0011](proposed/RFC-0011-layer-axis-terminology.md) | Layer-axis terminology (build_stage / architecture_tier) | D-036 | [glossary](../../spec/glossary.yaml), [spec-validate](../operations/spec-validate.md) |
| [RFC-0014](proposed/RFC-0014-emacs-input-profile.md) | The Emacs input profile (F-012) | D-049 | [register-model](../design/register-model.md), [positions-history](../design/positions-history.md) |
| [RFC-0015](proposed/RFC-0015-emacs-caret-gravity.md) | Caret gravity (Emacs point is between-character) | D-050 | [emacs-cursor-and-mark-fidelity](../design/emacs-cursor-and-mark-fidelity.md) |

## Rejected (do-not-relitigate)

| RFC | Title | Replaced by |
| --- | --- | --- |
| [RFC-R001](rejected/RFC-R001-rust-dylib-plugin-abi.md) | Rust dynamic-library plugin ABI | WASM / versioned protocol (D-004, RFC-0003) |

All ten planned RFCs (0001–0010) are drafted as **proposed**; RFC-R001 is the sole rejected record. Next
lifecycle step is review → `accepted/`. New hard-to-reverse decisions get a new RFC via
[`spec/templates/rfc.md`](../../spec/templates/rfc.md).
