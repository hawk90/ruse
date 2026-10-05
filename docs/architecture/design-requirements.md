---
doc: design-requirements
project: ruse
title: "ruse Long-Horizon Design Requirements"
summary: >
  Why ruse carries 111 long-horizon design requirements (DR-*) across 20 domains, the failures 2–5 years out
  (not at first implementation) that each domain guards against: spec-vs-implementation separation, parity
  meaning, persistence & crash consistency, determinism & replay, background scheduler, cache,
  IDs/generations/time, multi-client concurrency, plugin governance, config/profile/feature-pack, extended
  error/status, security/trust boundaries, cross-platform semantics, terminal UX, render-IR risks, the
  API-stability paradox, performance stability, CI/CD, contributor sustainability, and product scope. The
  requirement list, priorities and statuses live in spec/design-requirements.yaml; the mirror anti-patterns live
  in anti-patterns.md.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - stability-and-observability.md
  - render-and-frontends.md
  - ../../spec/design-requirements.yaml
  - ../protocols/versioning-and-evolution.md
  - ../operations/ci-cd-and-release.md
  - ../anti-patterns/anti-patterns.md
---

# ruse Long-Horizon Design Requirements

> The biggest risk is not too few features — it is **accepting too much future at once and blurring the
> core semantics and boundaries.** Sustainability here is not "make many abstractions"; it is the ability
> to precisely separate *contracts to lock* from *implementations not yet to lock*.
>
> These are the areas that bite 2–5 years later: many features but inconsistent meaning; a stable API over
> a wrong abstraction; recovery that recovers corrupted state; many plugins with uncontrolled quality/
> security; multi-platform support with divergent per-platform behavior; many tests that don't guarantee
> real user flows. Each numbered domain below explains why it matters. The mirror anti-patterns are in
> [../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md).

> **The requirement list and its status live in [`spec/design-requirements.yaml`](../../spec/design-requirements.yaml)**
> (상태·목록의 정본은 spec/design-requirements.yaml). Each requirement keeps its original `DR-<CODE>-<n>` id as
> `legacy_ids` (registry id `<CODE>-<NNN>`, e.g. `DR-PERSIST-3` → `PERSIST-003`), with priority tier P0–P3
> (P0 foundational/data-integrity/core-path · P1 major-subsystem correctness · P2 quality · P3
> long-horizon/polish). The `DR-` prefix is the positive "do" mirror of the same-code anti-pattern "don'ts"
> and keeps them in a distinct namespace (e.g. `DR-SPEC-1` vs the unrelated `SPEC-1` anti-pattern). Schema
> and update rules: [design-requirements-registry.md](design-requirements-registry.md).

## Status summary (verified against code, 2026-10-05)

| | done | partial | todo | total |
|---|---|---|---|---|
| P0 | 4 | 10 | 0 | 14 |
| P1 | 9 | 36 | 2 | 47 |
| P2 | 8 | 29 | 10 | 47 |
| P3 | 0 | 2 | 1 | 3 |
| **all** | **21** | **77** | **13** | **111** |

Most requirements are *partial*: designed in `docs/design/`, `spec/` or an RFC, but not yet implemented or
enforced. The done set is concentrated in spec discipline (§1), determinism/replay (§4), and product scope
(§20).

## 1. Specification ↔ Implementation (`SPEC`)
The code is a reference implementation that proves the spec, not the source of it. Normative statements
("must be X") stay apart from descriptions of the current Rust implementation ("does Y"). Protocols carry
wire-level meaning, ambiguity is marked as unspecified on purpose, and spec tests assert observable results.
That way a second implementation, or a rewrite, can be judged against the spec.

## 2. Parity Meaning (`PAR`)
Parity is **levels of compatibility**, not a flat feature list:
`Syntax parity · Semantic parity · Observable-behavior parity · Workflow parity · Plugin parity · Bug
compatibility`. Each feature is tagged **Exact · Equivalent · Adapted · Unsupported · Intentionally
different**. Same-named Vim and Emacs behaviors are not forced into one command. Parity % is weighted by
usage and importance, not by counting features.
(See [../parity/README.md](../parity/README.md); this taxonomy governs the parity files.)

## 3. Persistence & Crash Consistency (`PERSIST`)
The state at the moment of a crash matters more than the running state. Document revision, saved revision,
the externally observed file version, and the journal position are different things. Autosave, swap, journal
and backup play different roles. The journal is append-only and self-checking, recovery never silently
overwrites the original, and saves are atomic where the platform allows. Full design:
[../design/persistence-and-recovery.md](../design/persistence-and-recovery.md).

## 4. Determinism & Replay (`DET`)
Deterministic replay is how we know exactly where something broke. The core does not reach for the wall
clock, randomness or OS state. Inputs, commands and key async results become ordered, replayable events,
so a crash report, a fuzz failure or a bug report can be replayed into the same document state, with only
the data needed and a redaction policy.

## 5. Background Scheduler & Resource Control (`SCHED`)
A central scheduler knows about **all** background work. User input and screen refresh always come first.
Redundant parse/index work is coalesced and superseded work is cancelled. Budgets are per service and per
plugin. Under load, features degrade step by step
(`full semantic index → current-file index → visible-range only`) instead of stalling the cursor. Full
design: [../design/scheduler.md](../design/scheduler.md).

## 6. Cache (`CACHE`)
Caches are the most common source of inconsistency. Each cache names its source data and invalidation
trigger, keys on everything that can change its answer, can be deleted at any time, and must never change
the semantic result. Remote caches sit behind a different trust boundary from local ones.

## 7. IDs, Generations, Time (`ID`)
An ID's scope (process, session, workspace, global) decides where it may travel. Reusable slots carry
generations so stale handles fail loudly. Ordering uses monotonic sequences, never the wall clock, and
display time is separate from timeout time.

## 8. Multi-Client & Concurrency (`MULTI`)
Even a single-TUI-first editor should be designed for remote, GUI and web clients. Cursor, viewport and mode
belong to the client/view. Document changes follow one explicit concurrency model. A slow client must not
block the runtime, and reconnecting recovers missed state.

## 9. Plugin Ecosystem Governance (`GOV`)
A stable API does not imply a stable ecosystem. Responsibility, verification levels, permission
re-approval, namespace ownership, dependency locking and quality metrics are what keep the ecosystem healthy
once third parties arrive. Plugin research: [../parity/plugin-ecosystem.md](../parity/plugin-ecosystem.md).

## 10. Config, Profile, Feature Pack (`CFG`)
Settings have scopes (user, workspace, machine), per-type merge rules and provenance. Security-sensitive
settings cannot be overridden by a workspace. Profiles may carry bounded behavior policy. Feature packs are
declarative, and a safe mode always starts. Model: [../design/config-model.md](../design/config-model.md);
keys: [`spec/config-schema.yaml`](../../spec/config-schema.yaml).

## 11. Extended Error / Log / Status (`STAB` addendum)
(Base model in [stability-and-observability.md](../design/stability-and-observability.md).)
An error is an event and a status is persistent state. Some fields are never logged. Recent logs sit in
bounded ring buffers. A fatal invariant failure takes a minimal crash path. Health reports say how fresh
they are.

## 12. Security & Trust Boundary (`TRUST`)
Core, official plugins, third-party plugins, the workspace repository, remote servers, terminal output and
AI agents are each separate principals with their own trust level. Trust is decided before a workspace
opens. Terminal escapes are interpreted, never passed through. Secrets go through a provider. AI may
propose separately from executing.

## 13. Cross-Platform Semantics (`XPLAT`)
Abstracting the OS is not enough; ruse has to manage a list of **behavioral differences**: path case and
normalization, symlink/junction/UNC/WSL, permissions and ACLs, signals, shell quoting, newline/encoding,
and file-watcher quirks. Capabilities are split into build-time and runtime.

## 14. Terminal UX (`TUX` addendum)
(Base model in [../parity/terminal.md](../parity/terminal.md).)
Terminal capability and user preference are separate, and capability changes happen only at defined
renegotiation points. Escape chords, ambiguity timeouts, SSH/tmux nesting and IME are modeled explicitly.
Degraded terminals (narrow, no images, no color, dumb) keep the core usable.

## 15. Render-IR Risks (`RIR`)
A common IR is powerful, but it can become one more giant legacy. A semantic view model sits above a
backend-neutral Render IR that is **not** the union of all backends. Backend extensions live in capability
namespaces, the IR gets migration tests and incremental diffs, and resources are stable handles.

## 16. API-Stability Paradox (`APIX`)
The most dangerous thing is stabilizing a bad API too fast. APIs climb a promotion ladder
(**Internal → Experimental → Preview → Stable → Deprecated → Removed**, see
[../protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md)), need independent users
before Stable, stay within a surface budget, and can express failure, cancellation and partial success from
day one.

## 17. Performance Stability (`PERFS`)
Track p95/p99, not averages, against per-stage budgets (input → command → transaction → render). Measure cold
vs warm start, peak vs steady memory, and allocation counts, with real plugin sets and poor remote links,
because that is what users actually run.

## 18. CI/CD Additions (`OPS` addendum)
(Base pipeline in [../operations/ci-cd-and-release.md](../operations/ci-cd-and-release.md).)
Fast impact-scoped CI with periodic full runs and a merge queue. Release artifacts are built once and
rollback re-publishes a verified artifact. Reproducibility, soak and fault-injection runs, and quarantined
tests with an expiry date keep CI honest about the long tail.

## 19. Contributor Sustainability (`CONTRIB`)
The project has to outlive any one contributor's memory: a 30-minute bootstrap, architecture boundaries
enforced by tooling, area ownership, clear RFC triggers, and no key design knowledge held by one person
alone.

## 20. Product Scope & Strategy (`SCOPE`)
The largest risk. Non-goals to hold (canonical: [`spec/PROJECT.md` §Non-goals](../../spec/PROJECT.md) /
[`spec/PRD.yaml` `mvp.non_goals`](../../spec/PRD.yaml)): don't build every profile and frontend in v1, don't
platformize everything at once, no marketplace or SDK before real users and plugins, no distributed runtime
before remote is needed, and don't defer the MVP in the name of sustainability.

---

## How to Use
Each domain maps 1:1 to an anti-pattern category in
[../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md) (same code). Requirements are the
"do"; anti-patterns are the "don't." Look a requirement up by its old id with
`grep -n "DR-PERSIST-3" spec/design-requirements.yaml`. The design-concern checklist + doc template are in
[design-charter.md](design-charter.md); the lock-before-coding decisions are in
[`spec/DECISIONS.md`](../../spec/DECISIONS.md).
