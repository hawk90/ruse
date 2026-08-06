# ruse

A **Rust, terminal-first, remote-first, extensible code editor** targeting feature parity with Vim/Neovim
(editing language) and Emacs (command/buffer/extension model) — designed **specification-first**: the
architecture is meant to outlive the language, and the code is a *reference implementation* that proves the
spec, not the source of it.

> **Status: spec-first / pre-code.** The design is near-complete and self-validating; product implementation
> has not started (the `crates/` are compiling stubs). This is deliberate — see the build order below.

## Where to start

The repo has **two documentation layers on purpose** — a normative source of truth and its supporting prose:

| You want… | Go to |
| --- | --- |
| **The compact orientation pack** (read this first) | [`spec/CONTEXT.md`](spec/CONTEXT.md) |
| **The authoritative state** (vision, requirements, decisions, policy) | [`spec/`](spec/PROJECT.md) — normative, machine-managed |
| **The supporting reference** (deep design, parity research, RFCs, anti-patterns) | [`docs/`](docs/README.md) — prose |
| **The reference implementation** | [`crates/`](Cargo.toml) (engine) + `apps/` (thin frontends) |

**One rule ties them together:** *state* (records with fields) lives as YAML in `spec/`; *explanation and
research* live as prose in `docs/`; `spec/` IDs point **into** `docs/`, and no fact has two homes. This split
is enforced by [`tools/spec-validate.py`](tools/spec-validate.py).

## Layout

```
spec/     Source of truth (state). PROJECT · glossary · capabilities · dependencies · PRD · ARCHITECTURE ·
          POLICY · DECISIONS · CONTEXT · context-profiles · templates/       (YAML where it's data, MD where prose)
docs/     Reference (prose). README (hub) · architecture/ · design/ (subsystems) · parity/ · invariants/ ·
          protocols/ · operations/ · anti-patterns/ · reviews/ · rfc/
crates/   Reference implementation — engine: core, render-model, terminal-platform, workspace,
          workspace-runtime, plugin-protocol
apps/     Thin frontends: tui, remote-agent, gui
tools/    spec-validate.py — the doc-system checker (D-022 reference)
.github/  Issue forms, PR template, labels, workflows (spec-check runs today)
```

## Build order (spec → reference implementation)

```
0 RFCs → 1 editor-core (Document, Transaction, Command) → 2 input-engine (Vim) → 3 tui-client
→ 4 workspace (local buffers, save/recovery) → 5 plugin-api → 6 remote-runtime → 7 GUI → 8 Marketplace → 9 AI
```

## Development model

ruse uses **Spec-Gated Iterative Development**: a specification-first process with staged architectural goals
and short, evidence-driven implementation loops. The build order above is a **dependency direction, not a
one-way waterfall** — each stage is built as small vertical slices, and implementation evidence may revise
earlier specs through an RFC.

Principles:
- The **specification defines the product**; the Rust implementation is a *reference implementation* that
  proves it.
- **Architecture advances in stages; implementation advances in slices.** A stage is done not when its files
  exist but when its required behavior is **executable and verified**.
- **Every significant change is traceable** (Capabilities/requirements/RFCs/decisions/PRs/tests/docs linked
  by stable IDs) and **evidence closes work** (a test, benchmark, demo, compatibility check, or dogfood).
- **Feedback may reopen earlier decisions** — specs are authoritative but not immutable.
- **State has one home** — structured state in `spec/`, explanation in `docs/`.

Full model: [docs/operations/development-model.md](docs/operations/development-model.md) · Definition of Done:
[definition-of-done.md](docs/operations/definition-of-done.md) · GitHub mechanics:
[github-workflow.md](docs/operations/github-workflow.md) · Tests & benchmarks:
[testing-and-benchmarks.md](docs/operations/testing-and-benchmarks.md).

## Validate

```sh
python3 tools/spec-validate.py                       # doc-system integrity (refs, layers, links, enums)
cargo fmt --all --check && cargo test --workspace    # reference implementation (stubs today)
```

## Development and contribution

ruse is developed with **AI-assisted pair programming**, but not through unreviewed "vibe coding." AI may
analyze, propose, implement, test, and document changes; **humans retain full ownership** of direction,
architecture, correctness, security, compatibility, and final acceptance. The governing rule: *AI may
multiply implementation capacity — it does not replace engineering judgment.* AI assistance is **optional**;
no specific model, provider, or coding agent is required. Every accepted change must be understood by its
author, connected to an explicit work item, and supported by evidence — never accepted merely because it
compiles. Full details: [docs/contributing/](docs/contributing/README.md).

**Contribution paths** — use the one matching your change's maturity:

| You want to… | Start with |
| --- | --- |
| Ask a question / get help | GitHub Discussions · [SUPPORT.md](SUPPORT.md) |
| Report reproducible incorrect behavior | Bug report Issue |
| Suggest an early idea | GitHub Discussions |
| Implement a scoped capability | GitHub Issue (a **Slice**) |
| Change architecture or a public contract | an **RFC** ([docs/rfc/](docs/rfc/README.md)) |
| Change normative project state | a **spec** PR (`spec/`) |
| Report a security problem | [SECURITY.md](SECURITY.md) |

**Change levels** (not every change needs the same process): docs/typos/small isolated tests → **direct PR**;
clear bug fix → Issue recommended; feature Slice → **Issue + observable acceptance**; public API/protocol →
**Discussion → RFC before code**; core data model → **RFC + Decision**; security/compat → **RFC + separate
review**. Spec lands *before or with* the first implementation PR, never after.
[Change-paths guide](docs/contributing/change-paths.md).

**Git workflow** — protected, continuously-integrated `main`; short-lived branches
(`feat/123-…` `fix/…` `spec/…` `rfc/…` `spike/…`) → **Draft PR** (`Closes #NN`) → review + CI +
`spec-validate` + `gate` → **squash-merge** → release tag at a release boundary.

**Documentation model** — authoritative state in [`spec/`](spec/PROJECT.md); explanation/research in
[`docs/`](docs/README.md); Issues/Projects track work; Discussions host Q&A. **GitHub Wiki is not used as an
authoritative source** ([D-035](spec/DECISIONS.md)) — no fact has two homes.

See also: [CONTRIBUTING.md](CONTRIBUTING.md) · [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) ·
[SECURITY.md](SECURITY.md) · [SUPPORT.md](SUPPORT.md).

## License
[MIT](LICENSE).
