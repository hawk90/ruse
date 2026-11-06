---
doc: github-workflow
project: ruse
title: "ruse — GitHub Operating Model"
summary: >
  How ruse represents the Spec-Gated Iterative Development model on GitHub: the load-bearing principle
  (Label = nature, Milestone = release, issue open/closed = state), a prefixed label taxonomy synced from
  .github/labels.yml, milestones named by release version, no maintained Project field structure, issue
  dependencies, Discussions-vs-Issues, automation, and branch/PR/commit conventions. The process definitions are
  canonical in development-model.md; this is the GitHub mechanics.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - development-model.md
  - definition-of-done.md
  - ci-cd-and-release.md
  - ../../CONTRIBUTING.md
---

# ruse — GitHub Operating Model

> **Load-bearing principle:** **Label = *nature* · Milestone = *release*.** Never mix the two; never stuff
> metadata into issue **titles** beyond the conventional type prefix.

The **process** (work-item types, Status meanings, stage gates, DoD) is defined once in
[development-model.md](development-model.md) — this doc only maps it onto GitHub. Labels are single-homed in
[`.github/labels.yml`](../../.github/labels.yml). The [`labels`](../../.github/workflows/labels.yml) workflow
applies that file on every push to `main` that changes it (and on manual dispatch): it creates missing labels and
updates colors and descriptions, and it never deletes a label the file does not list (GitHub's defaults such as
`bug`, and `parity`, stay). Add or change a label in the file, not in the UI.

## 1. Labels (nature) — prefixed, per-axis

| Prefix | Axis | Cardinality | Values |
| --- | --- | --- | --- |
| `type/` | Work-item type (see development-model.md) | at most one | `capability` `rfc` `spike` `slice` `bug` `debt` `chore` `dependency` |
| `area/` | Subsystem(s) | one or more | `core` `document` `input` `render` `terminal` `remote` `language` `debug` `plugin` `infra` `docs` |
| `impact/` | Quality affected | 0+ | `correctness` `performance` `reliability` `security` `compatibility` `readability` |
| `community/` | Signposting | optional | `good-first-issue` `help-wanted` |
| `needs/` | What blocks readiness | 0+ | `reproduction` `design` `benchmark` `decision` |

Who applies them: the path labeler ([`.github/labeler.yml`](../../.github/labeler.yml)) adds `area/*` to PRs from
the `crates/core` and `apps/tui/src` paths each area owns (`area/plugin` and `area/debug` have no rule until that
code exists; `tools/tests/test_labeler.py` checks every glob matches a tracked file);
a maintainer may add `type/*`, `impact/*`, `community/*` and `needs/*` to issues by hand. Nothing applies `type/*`
to PRs: a PR's type is the conventional prefix of its title (§8), which `tools/lint-commit-msg.py` enforces.

**Not labels:** `priority/*`, `status/*`, `size/*`, `target/*`.

## 2. State and Projects

There is no maintained GitHub Project and no Project field structure. State lives in three places:

- **Release:** the issue's milestone (§3).
- **Open / closed:** an issue closes when the PR that names it with `Closes #NN` squash-merges.
- **Spec state:** feature and requirement status lives in `spec/` (`PRD.yaml`, `capabilities.yaml`,
  `design-requirements.yaml`), checked by `spec-validate`, not in GitHub fields.

The work-item Status names in development-model.md (`Inbox` … `Done`) describe the process; they are not tracked
as a field.

## 3. Milestones = releases (not sprints)

Milestones are named by release version (`v0.1.0`, ...) and mirror `spec/phases.yaml` one-way through
`ruse phase sync` (D-037, D-059). The phase goal is the exit condition. A milestone answers "which release",
not "which feature" (use `area/*` for that).

Intermediate version milestones between phases (`v0.1.1`, ...) keep each release a plannable size (D-060). They
are created by hand, carry no `ruse-phase` marker, and hold issues only; `spec/phases.yaml` still owns features.

## 4. Issue dependencies

A **Slice** is the smallest mergeable behavior; a big Capability is a **parent** issue with a tasklist of
Slices (or GitHub native issue dependencies):

```md
### Remote Workspace MVP  (parent, type/capability area/remote)
- [ ] #NN Agent bootstrap
- [ ] #NN Handshake
- [ ] #NN Filesystem
- [ ] #NN Watcher / Search / PTY / Reconnect / LSP host
```

Tasks attach to a Slice (checklist inside it), never as standalone roadmap items (development-model.md).

## 5. Discussions vs Issues

- **[Discussions](https://github.com/hawk90/ruse/discussions)** — questions (Q&A), half-formed ideas (Ideas),
  "should we…", open design chatter.
- **Issues** — anything **actionable**: reproducible bug, approved Capability/Slice, approved RFC/Spike.
- **Promote** a Discussion to an Issue once concrete (scope + acceptance evidence known).

The repository Wiki is disabled (D-035).

## 6. Repository settings

- Squash merge only (merge commits and rebase merges are off); the head branch is deleted on merge.
- Branch protection on `main` requires the `gate`, `rust` and `spec-validate` checks and requires the branch to be
  up to date with `main`. There is no merge queue: GitHub offers it only to organization-owned repositories
  (see [ci-cd-and-release.md](ci-cd-and-release.md) §13).

## 7. Automation

- Path-based `area/*` labels on PRs via [`.github/labeler.yml`](../../.github/labeler.yml) (advisory; it has no
  `type/*` rules, because a path does not tell the change type).
- Labels synced from [`.github/labels.yml`](../../.github/labels.yml) by the `labels` workflow (never deletes).
- Release notes: [`.github/release.yml`](../../.github/release.yml) lists every merged PR in one group. It cannot
  group by title type and no `type/*` labels reach PRs, so the squash-merge title (`type(scope): …`) is what
  tells a reader the kind of change.

> "Proven" is a human/CI transition (evidence obtained), not an automated one — a PR merges only after its
> Evidence is recorded (definition-of-done.md).

## 8. Branches · PRs · Commits

**Branches** (short-lived; `main` always validation-green): `feat/<issue>-<name>` · `fix/<issue>-<name>` ·
`spec/<issue>-<name>` · `rfc/<issue>-<name>` · `spike/<issue>-<name>`.

**One PR = one kind of change**: a spec change · an RFC/decision · a spike result · a vertical Slice · a bug
fix · a mechanical refactor. Never combine a spec change + big refactor + new feature. **Squash-merge**; the
PR title is the durable changelog entry. PR checklist = [definition-of-done.md](definition-of-done.md).

**Commit / PR-title types** (Conventional-ish, ruse subset):

| Type | Meaning |
| --- | --- |
| `spec` | normative state change (spec/) |
| `rfc` | architectural proposal / RFC |
| `feat` | product/platform capability |
| `fix` | defect fix |
| `refactor` | behavior-preserving structural change |
| `test` | automated verification |
| `bench` | performance evidence |
| `docs` | supporting explanation / research |
| `build` | Cargo / CI / toolchain |
| `chore` | maintenance |

Write **why**, not the filename. Bad: `update document.rs`. Good:
`fix(core): reject transactions based on stale revisions`.
