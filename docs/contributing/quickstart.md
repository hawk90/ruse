---
doc: contributing-quickstart
project: ruse
title: "ruse — Contributor Quickstart"
summary: >
  Clone to first PR in a few steps: clone, validate the spec (python3 tools/spec-validate.py), check Rust
  format and tests (cargo fmt --all --check && cargo test --workspace — crates are stubs today), pick a
  good-first-issue, branch feat/<issue>-<name>, open a Draft PR early with Closes #NN, satisfy the Definition
  of Done, and squash merge. Intentionally short.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - change-paths.md
  - ../operations/definition-of-done.md
  - ../operations/github-workflow.md
---

# Contributor Quickstart

From zero to your first PR. For *which* change to make, see [change-paths.md](change-paths.md).

1. **Clone.**
   ```bash
   git clone <your-fork-url> ruse && cd ruse
   ```

2. **Validate the spec.**
   ```bash
   python3 tools/spec-validate.py
   ```
   This checks the `spec/` set (IDs, cross-references) and should pass before you start.

3. **Check format and tests.**
   ```bash
   cargo fmt --all --check && cargo test --workspace
   ```
   > Note: the Rust crates are **stubs today** — the workspace builds and tests run, but there is little
   > implementation yet. That is expected.

4. **Pick a `good-first-issue`.** Filter Issues for `good-first-issue` / `help-wanted` and the low-risk
   areas (`area/docs`, `area/testing`, `area/infra`). See the First-time contributor persona in
   [README.md](README.md).

5. **Branch.** Use a short, prefixed name:
   ```bash
   git checkout -b feat/<issue>-<name>   # e.g. feat/42-status-line-clamp
   ```

6. **Open a Draft PR early.** Push and open a **Draft** PR that references the issue:
   ```
   Closes #NN
   ```
   Early Draft PRs get CI and feedback sooner.

7. **Satisfy the Definition of Done.** Behavior defined, tested, traced to a spec ID, documented without
   duplication, acceptance evidence recorded — see
   [../operations/definition-of-done.md](../operations/definition-of-done.md). Fill the PR **AI assistance**
   block ([ai-assisted-development.md](ai-assisted-development.md)).

8. **Squash merge.** Once approved and green, the PR is **squash merged** into one clean commit.
