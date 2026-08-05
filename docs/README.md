---
doc: docs-index
project: ruse
title: "ruse — Documentation Hub (Specification-first)"
summary: >
  Entry point and philosophy for the ruse documentation set. ruse is designed as a specification with
  a reference implementation, not as "an editor written in Rust." Architecture, invariants, protocols,
  anti-patterns, and parity are first-class, language-independent artifacts; code proves them. This hub
  defines the spec-first philosophy, the RFC process, the stable glossary, the build order, the target
  repository layout, and the document map.
audience: [maintainers, contributors, llm-agents]
status: draft
---

# ruse — Documentation Hub

> **Two layers.** [`spec/`](../spec/PROJECT.md) is the **maintained source of truth for state** (vision,
> requirements, enforced rules, decisions — minimal, LLM-first, YAML-where-it's-state). This `docs/` tree is
> the **reference/detail layer**: long-form design, parity research, and the full anti-pattern catalog that
> spec IDs point into. One fact, one home — *state* lives in `spec/`; *explanation/research* lives here.
> Start at [`spec/CONTEXT.md`](../spec/CONTEXT.md) for the compact context pack.

## Philosophy: Architecture > Code

The goal of ruse is **a design that remains a reference even after the implementation language changes.**
People study Git's object model, LLVM's SSA/passes, SQLite's pager/B-tree, Redis's single-threaded event
loop, and Linux's "everything is a file" — not the host language's syntax. ruse aims to be that kind of
project: if in 20 years Rust gives way to something else, a re-implementation should still follow this
design. **The Rust implementation is a *reference implementation* — a proof of the design, not its source.**

The maturity ladder:

```
Specification → Reference Architecture → Reference Implementation → Production Implementation
```

Consequences that shape everything here:
- **Stable terminology** (glossary below) is treated like paper terminology — it should not drift for years.
- **Reference Invariants** ([invariants/reference-invariants.md](invariants/reference-invariants.md)) are
  language-independent rules; every RFC restates the invariants it enforces.
- Design documents come **before** code. For a project this large (roughly "Neovim + Helix + Emacs +
  VSCode Remote + Zed, redesigned"), writing production code too early means throwing most of it away as
  the design shifts. Avoid the opposite failure too — *analysis paralysis* — by using the RFC process to
  make decisions final and move on.

## RFC Process

Hard-to-reverse decisions are captured as RFCs (as in Rust, Swift, Kubernetes): ~3–10 pages, each ending
with **Alternatives / Rejected Ideas / Trade-offs** + a **Reference Invariants** section, so a debate isn't
re-litigated later. `rejected/` matters as much as `accepted/`. **The RFC index, status, and process live in
[`docs/rfc/README.md`](rfc/README.md)** (single home); small changes go in PR descriptions, not RFCs.

## Stable Glossary (paper terminology)

Canonical glossary is single-homed in [`spec/glossary.yaml`](../spec/glossary.yaml) (machine-managed,
multi-language) — see [`spec/PROJECT.md` §Terminology](../spec/PROJECT.md). Fixed vocabulary across all
docs/RFCs; never rename a term id.

## Build Order (avoid building the final product first)

A solo/small-team effort must not aim at the final product on the first pass. Sequence:

```
0. RFCs
1. editor-core        (Document, Transaction, Command, Query, Anchor, Undo, Registers)  # Command is kernel (ARCH-LAYER-001)
2. input-engine       (Vim first; operator/motion composition)                          # depends on Command → must follow stage 1
3. tui-client         (Render IR, ANSI/Unicode, terminal capability)
4. workspace          (buffers, save/recovery — LOCAL only, no remote)                  # F-007/F-008 need this before plugins/remote
5. plugin-api
6. remote-runtime
7. GUI
8. Marketplace
9. AI
```

> Reconciles verification V-10 (Command was contradictorily placed at step 4 while Input at step 2 depends
> on it) and V-11 (Workspace was unplaced yet F-007/F-008 need it). Command lives in editor-core; a local
> Workspace stage precedes plugin/remote. **Machine-checkable ordering = `spec/PRD.yaml` component
> `build_stage`/`depends_on`** (this prose is the narrative view).

Render backends specifically sequence: (1) Semantic Render Tree → (2) ANSI/Unicode backend → (3) virtual
terminal tests → (4) Kitty image backend → (5) remote protocol → (6) GUI backend. Do **not** implement
ANSI + Kitty + SIXEL + GUI + Web all at once.

## Target Repository Layout

Big boundaries only at first; `document/transaction/command/anchor` start as **modules inside one core
crate**, not separate crates. Do not confuse well-named folders with public package boundaries.

```
project/
├── crates/
│   ├── core/            # document, transaction, command, query, anchor, undo, registers, context, health
│   ├── render-model/    # node, layout, decoration, protocol
│   ├── terminal-platform/ # input, capability, ansi, image, virtual-terminal
│   ├── workspace/       # LOCAL: buffers, filesystem, save, recovery journal   (stage 4 — no remote)
│   ├── workspace-runtime/ # remote runtime: process, remote, lsp, plugin-host  (stages 5–6)
│   └── plugin-protocol/
├── apps/                # tui, remote-agent, gui   (thin; engine is reusable)
├── extensions/          # core-git, core-search, examples
├── docs/                # see doc map below
├── tests/               # command-parity, terminal-matrix, plugin-compat, remote-scenarios
└── tools/               # inspector, protocol-dump, render-diff, diagnostic-bundle
```

Large sample outputs / test corpora belong in a separate repo or Git LFS, not the main tree.

## Document Map

Current docs (this set) map onto the spec-first structure as follows:

Reorganized by *kind* (holistic): cross-cutting architecture/governance, per-subsystem design specs, and
reviews are separate folders.

| Area | Location |
| --- | --- |
| **Vision / hub** | this file · [`spec/PROJECT.md`](../spec/PROJECT.md) |
| **Architecture & governance** | [architecture/](architecture/architecture.md) — `architecture.md` (deep map), `design-requirements.md` (20 long-horizon domains), `design-charter.md` (governance) |
| **Subsystem design specs** | [design/](design/editing-language.md) — editing-language, register-model, positions-history, vim-regex, persistence-and-recovery, render-and-frontends, stability-and-observability, remote-runtime, delivery-and-dependencies |
| **Invariants** | [invariants/reference-invariants.md](invariants/reference-invariants.md) |
| **Protocols** | [protocols/versioning-and-evolution.md](protocols/versioning-and-evolution.md) |
| **Parity** | [parity/README.md](parity/README.md) → vim, neovim, emacs, terminal, remote, native-style, workspace, plugin-ecosystem, roadmap, common |
| **Anti-patterns** | [anti-patterns/anti-patterns.md](anti-patterns/anti-patterns.md) |
| **Operations** | [operations/](operations/ci-cd-and-release.md) — development-model, definition-of-done, testing-and-benchmarks, github-workflow, ci-cd-and-release, spec-validate, review-axes |
| **Contributing** | [contributing/](contributing/README.md) — hub, change-paths, ai-assisted-development, quickstart (+ root CONTRIBUTING/SUPPORT/CODE_OF_CONDUCT/SECURITY) |
| **Reviews** | [reviews/verification.md](reviews/verification.md) (point-in-time design-verification report) |
| **RFCs** | [rfc/README.md](rfc/README.md) — `proposed/` (0001–0011), `rejected/` (R001) |

> The old "one PRD.md per folder" idea is superseded by the single [`spec/PRD.yaml`](../spec/PRD.yaml).

> Convention for all docs here: English, LLM-agent-parseable (frontmatter, stable headings/IDs, tables,
> cross-references). See the standing preference in project memory.
