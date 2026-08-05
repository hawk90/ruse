# Project

> Source of truth for **vision, principles, non-goals**. Stable and slow-changing. Terminology is in
> [glossary.yaml](glossary.yaml).
> Feature state lives in [PRD.yaml](PRD.yaml); enforced rules in [POLICY.yaml](POLICY.yaml); decisions in
> [DECISIONS.md](DECISIONS.md); the big structure in [ARCHITECTURE.md](ARCHITECTURE.md). Long-form research
> and full catalogs are the reference layer under [`../docs/`](../docs/README.md). One fact, one home.

## Vision

`ruse` is a Rust, terminal-first, remote-first, extensible code editor targeting **feature parity** with
Vim/Neovim (editing language) and Emacs (command/buffer/extension model). It is designed as a
**specification with a reference implementation** — the architecture is meant to outlive the language.
Not a Neovim port; a redesign that grows an ecosystem yet **breaks less than Neovim**.

## Principles

- Document changes are tracked and reversible (Transaction is the only mutation path).
- External feature failure never stops core editing (bounded blast radius, not hidden errors).
- Primary execution flows are understandable by reading the code.
- Unstable APIs are not exposed early; contracts are locked, implementations stay free.
- Features degrade in quality, they do not disappear.
- One fact has one home; humans edit sources, tools generate derived files.

## Non-goals (v1)

> The machine-readable MVP exclusion list is single-homed in [`PRD.yaml`](PRD.yaml) `mvp.non_goals`
> (V-17). This prose states the product-level intent; it must stay consistent with that list.

- Full compatibility with every editor (no running Vimscript/Elisp/Lua plugins — L3).
- GUI or Web frontend in v1 (TUI-first; the Render IR keeps them possible later).
- Collaborative (multi-user) editing.
- A general-purpose editor framework, or platformizing editor+IDE+shell+notebook+analyzer at once.
- A Marketplace before there are users; a Plugin SDK before real plugins validate the API.
- WASM plugin host in the MVP (internal extension API first — see [DECISIONS.md](DECISIONS.md) D-009).

## Terminology (canonical glossary)

**Canonical, machine-managed source: [`glossary.yaml`](glossary.yaml)** — ID-keyed terms with `en`/`ko`
definitions and `ja`/`zh` labels. Do not maintain a term table here; the human-readable table is generated
from `glossary.yaml` (xtask, D-022). Fixed vocabulary across all docs, RFCs, and implementations; never
rename a term id.

## Design-axis status

Each big axis is tracked as a state, not "done": `Unexplored → Draft → Validated-by-implementation →
Stable → Needs-revision`. Current states live in [CONTEXT.md](CONTEXT.md). New problems are classified into
an axis, given a minimal current decision, and promoted to policy/design only if they recur.

## Document system (how these files relate)

```
spec/                       maintained source of truth (LLM-first, minimal)
├── PROJECT.md   (this)     vision · principles · non-goals              [stable]
├── glossary.yaml           canonical terminology (multi-language)        [state]
├── capabilities.yaml       capability inventory: delivery/impl/trust axes (D-033) [state]
├── dependencies.yaml       dependency inventory: tier/usage/exit-strategy (D-034)  [state]
├── config-schema.yaml       user-config schema (types/defaults/scope/merge/lock)  [state]
├── PRD.yaml                requirements: stage/priority/status/deps/acceptance [state]
├── ARCHITECTURE.md         layers · flow · ownership · forbidden deps · open   [boundaries]
├── POLICY.yaml             enforced principles → reference anti-pattern IDs     [rules]
├── DECISIONS.md            D-xxx: decision · reason · re-evaluation condition   [decisions]
├── CONTEXT.md              compact LLM context pack (manual now, generated later)
├── context-profiles.yaml   per-task context packs (manifest for future generator)
└── templates/              rfc · design-doc · prd-feature · policy-principle · decision
```

The `docs/` reference/detail layer is mapped in [`docs/README.md` §Document Map](../docs/README.md).
Rule of thumb: **state → YAML in `spec/`; explanation/research → prose in `docs/`.** spec IDs point into
docs; docs never duplicate spec state.
