---
doc: rfc-index
project: ruse
title: "ruse RFC Index"
summary: >
  Index of ruse RFCs and their lifecycle (proposed/ → accepted/ when the linked D-* record is decided;
  rejected/ kept forever). RFCs are only for hard-to-reverse decisions (save format, plugin protocol, command
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

An RFC records *why*; the design docs record *how* (linked, not duplicated); the compact decision lives in
[`spec/DECISIONS.md`](../../spec/DECISIONS.md).

## Lifecycle

| Folder | Holds | Moves when |
| --- | --- | --- |
| `proposed/` | RFCs whose decision is still open | a new RFC starts here ([`spec/templates/rfc.md`](../../spec/templates/rfc.md)) |
| `accepted/` | RFCs whose `D-*` record in `spec/DECISIONS.md` is `decided` (including `decided (design)` and decided-but-deferred) | `git mv` from `proposed/` in the PR that marks the `D-*` record decided; set the RFC's `status: accepted` |
| `rejected/` | RFCs the maintainer turned down, kept forever so decisions are not re-litigated | `git mv` when rejected; the index row says what replaced it |

An RFC with more than one linked decision moves only when every one of them is decided; one open decision keeps
it in `proposed/`. An accepted RFC that a later decision supersedes or defers stays in `accepted/`, and its index
row says which decision did it. Each RFC is in exactly one row below.

## Proposed

| RFC | Title | Decisions | Waiting on | Design detail |
| --- | --- | --- | --- | --- |
| [RFC-0006](proposed/RFC-0006-remote-runtime.md) | Remote Runtime & Agent | D-011/024/029/030/031/032 | D-024 (version skew) and D-032 (debug backend detail) are open; D-039 defers remote | [remote-runtime](../design/remote-runtime.md), [parity/remote](../parity/remote.md) |

## Accepted

| RFC | Title | Decisions | Note | Design detail |
| --- | --- | --- | --- | --- |
| [RFC-0001](accepted/RFC-0001-project-vision.md) | Project Vision & Non-Goals | D-020, D-021 | D-039 deferred the remote-first commitment; `spec/PROJECT.md` holds the current vision | [spec/PROJECT.md](../../spec/PROJECT.md), [design-charter](../architecture/design-charter.md) |
| [RFC-0002](accepted/RFC-0002-workspace-architecture.md) | Workspace Architecture | D-003 | | [architecture §7](../architecture/architecture.md), [parity/workspace](../parity/workspace.md) |
| [RFC-0003](accepted/RFC-0003-plugin-api.md) | Plugin API & Lifecycle | D-004, D-009, D-010 | deferred by D-039 | [architecture §4](../architecture/architecture.md), [protocols](../protocols/versioning-and-evolution.md) |
| [RFC-0004](accepted/RFC-0004-input-profiles.md) | Input Profiles & Command Layer | D-006, D-008 | D-008 narrowed by D-046 | [architecture §1–2](../architecture/architecture.md), [parity](../parity/README.md) |
| [RFC-0005](accepted/RFC-0005-terminal-capability.md) | Terminal Capability | D-015 | | [parity/terminal](../parity/terminal.md), [architecture §6](../architecture/architecture.md) |
| [RFC-0007](accepted/RFC-0007-transaction-engine.md) | Transaction Engine | D-001 | recovery journal refined by D-064 (RFC-0023) | [architecture §3](../architecture/architecture.md), [stability](../design/stability-and-observability.md) |
| [RFC-0008](accepted/RFC-0008-document-model.md) | Document Model & Coordinates | D-003, D-023 | typed-position boundary set by D-063 (RFC-0022) | [architecture §3](../architecture/architecture.md) |
| [RFC-0009](accepted/RFC-0009-render-model.md) | Render Model & Frontends | D-014, D-015 | render IR deferred by D-039; D-054 scopes a second tier | [render-and-frontends](../design/render-and-frontends.md), [parity/terminal](../parity/terminal.md) |
| [RFC-0010](accepted/RFC-0010-stability-observability.md) | Stability & Observability | D-016 | v0 scope narrowed by D-040; D-017 (PII) still open | [stability-and-observability](../design/stability-and-observability.md) |
| [RFC-0011](accepted/RFC-0011-layer-axis-terminology.md) | Layer-axis terminology (build_stage / architecture_tier) | D-036 | | [glossary](../../spec/glossary.yaml), [spec-validate](../operations/spec-validate.md) |
| [RFC-0012](accepted/RFC-0012-collapse-to-two-crate-editor.md) | Collapse to a two-crate terminal modal editor | D-039 | | [spec/PROJECT.md](../../spec/PROJECT.md) |
| [RFC-0013](accepted/RFC-0013-lang-arg-translation-stage.md) | Lang-Arg translation stage | D-048 | | [input-engine](../design/input-engine.md) |
| [RFC-0014](accepted/RFC-0014-emacs-input-profile.md) | The Emacs input profile (F-012) | D-049 | | [register-model](../design/register-model.md), [positions-history](../design/positions-history.md) |
| [RFC-0015](accepted/RFC-0015-emacs-caret-gravity.md) | Caret gravity (Emacs point is between-character) | D-050 | | [emacs-cursor-and-mark-fidelity](../design/emacs-cursor-and-mark-fidelity.md) |
| [RFC-0016](accepted/RFC-0016-emacs-command-divergence.md) | Emacs-vs-Vim command divergence = distinct Commands | D-051 | | [emacs-cursor-and-mark-fidelity](../design/emacs-cursor-and-mark-fidelity.md) |
| [RFC-0017](accepted/RFC-0017-better-than-vanilla-defaults.md) | Better-than-vanilla defaults at the frontend layer | D-056 | | [config-model](../design/config-model.md) |
| [RFC-0018](accepted/RFC-0018-multi-buffer-arena.md) | Multi-buffer arena and workspace-global state | D-057 | | [buffer-arena-and-global-state](../design/buffer-arena-and-global-state.md) |
| [RFC-0019](accepted/RFC-0019-version-named-milestones.md) | Version-named milestones | D-059 | | [spec/phases.yaml](../../spec/phases.yaml) |
| [RFC-0020](accepted/RFC-0020-ex-edits-in-the-trace.md) | Ex edits are recorded in the trace as resolved commands | D-061 | | [command-engine §7.3](../design/command-engine.md) |
| [RFC-0021](accepted/RFC-0021-workspace-config-boundary.md) | The workspace config boundary (keymaps, `.ruse/` ownership, content-bound trust) | D-062 | | [config-model](../design/config-model.md) |
| [RFC-0022](accepted/RFC-0022-typed-positions-at-the-core-boundary.md) | Positions are typed at the core boundary | D-063 | | [reference-invariants](../invariants/reference-invariants.md) |
| [RFC-0023](accepted/RFC-0023-per-transaction-journal.md) | Per-transaction recovery journal (RJ01), state-dir location, three-way recovery | D-064 | | [persistence-and-recovery](../design/persistence-and-recovery.md) |

## Rejected (do-not-relitigate)

| RFC | Title | Replaced by |
| --- | --- | --- |
| [RFC-R001](rejected/RFC-R001-rust-dylib-plugin-abi.md) | Rust dynamic-library plugin ABI | WASM / versioned protocol (D-004, RFC-0003) |

New hard-to-reverse decisions get a new RFC in `proposed/` via [`spec/templates/rfc.md`](../../spec/templates/rfc.md).
