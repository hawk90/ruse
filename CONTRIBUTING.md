# Contributing to ruse

ruse is a **spec-first** Rust TUI editor: the design (`spec/`, `docs/`) is the source of truth; code is a
reference implementation that proves it. Read [`spec/CONTEXT.md`](spec/CONTEXT.md) first. The dev process is
[`docs/operations/development-model.md`](docs/operations/development-model.md) (Spec-Gated Iterative
Development); GitHub mechanics (labels/fields/milestones) are in
[`github-workflow.md`](docs/operations/github-workflow.md); completion is
[`definition-of-done.md`](docs/operations/definition-of-done.md).

**New here?** Start at the [contributor hub](docs/contributing/README.md) — it routes you by persona and by
change type ([which process is my change?](docs/contributing/change-paths.md)). AI-assisted work is welcome
and tool-agnostic: [ai-assisted-development.md](docs/contributing/ai-assisted-development.md). Be kind:
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Getting help: [SUPPORT.md](SUPPORT.md).

## Workflow (idea → merge)

1. **Idea.** Open a **Discussion** (question / half-formed idea) or a **Feature/Bug issue** (actionable).
   Promote a Discussion to an issue once it is concrete. See "Discussions vs Issues" in the workflow doc.
2. **Clarifying.** A maintainer applies `type/*` + `area/*` labels and sets Project fields (Status, Type,
   Stage, Priority, Risk, Spec-ID, Evidence). Titles carry *no* metadata — labels and fields do.
3. **Decide if PRD/design is needed.** If the change touches product scope or a hard-to-reverse boundary,
   it needs a spec update *before* code: update [`spec/PRD.yaml`](spec/PRD.yaml) (product) and/or open an
   **RFC** (`type/rfc`). Small, obvious changes skip straight to Ready.
4. **Ready.** Issue has clear acceptance criteria and required spec in place → Status **Ready**.
5. **Draft PR early.** Branch, push, open a **Draft PR** that references the issue with `Closes #NN`.
   Draft-early makes CI and discussion visible from the start.
6. **CI.** Required checks must pass (below).
7. **Review.** At least one review pass (code **and** spec consistency, boundaries, test evidence, doc
   drift, compatibility, perf assumptions). Required approvals are **0 for now** (solo-friendly).
8. **Proven.** Record the completion **evidence** (test/benchmark/demo/dogfood — see
   [definition-of-done.md](docs/operations/definition-of-done.md)); "compiles" is not Done.
9. **Squash-merge.** Merge with squash only. `Closes #NN` moves the card to **Done** and advances the
   milestone (stage proof).
10. **Release notes.** Merged PRs under a milestone feed the release notes for that milestone.

### Design changes (extra loop)

An RFC (`type/rfc`) → decide → **update the spec**: record the choice in
[`spec/DECISIONS.md`](spec/DECISIONS.md) (a `D-*` record; add/refresh an RFC under
[`spec/templates/rfc.md`](spec/templates/rfc.md) when hard to reverse) **and** the affected spec file
(PRD / POLICY / capabilities / dependencies) → **split** the work into implementation issues → PRs.
Spec lands before or with the first implementation PR, never after.

## Branches

- `feat/<issue>-<name>` — product/platform capability (a Slice)
- `fix/<issue>-<name>` — bug fix
- `spec/<issue>-<name>` — normative spec change
- `rfc/<issue>-<name>` — architectural proposal
- `spike/<issue>-<name>` — time-boxed experiment

Short-lived; squash-merge only (the PR title is the durable changelog); delete the branch after merge.
Commit/PR-title types: `spec rfc feat fix refactor test bench docs build chore` (see
[github-workflow.md](docs/operations/github-workflow.md) §8).

## Keep PRs small

Draft early, keep the diff reviewable. **Split** a PR when it mixes any of:

- more than one product feature,
- architecture change **and** a persistence/protocol format change,
- a **new dependency** **and** a large change,
- large **generated** diff (codegen, lockfile churn) mixed with hand-written logic.

Land the split pieces behind a parent tracking issue (see issue dependencies in the workflow doc).

## Required checks

Every PR:

```
python3 tools/spec-validate.py        # doc-system integrity — always
```

When `crates/` is touched, also:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI enforces these; run them locally before pushing.

## New dependency?

Follow the dependency policy — do not add a crate ad hoc:

1. Read **D-034** in [`spec/DECISIONS.md`](spec/DECISIONS.md) (own semantics, wrap by default, tiers 0–4,
   budget by cost type; never hand-roll crypto/unicode/escape-parsing/SSH).
2. Add the entry to [`spec/dependencies.yaml`](spec/dependencies.yaml) with `allowed_layers`.
3. Fill the **"New dependency?"** section of the PR template (tier, usage, exit strategy, exposure).

## Security

Do **not** open a public issue for a vulnerability. See [`SECURITY.md`](SECURITY.md).
