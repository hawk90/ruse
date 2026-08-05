---
doc: design-requirements
project: ruse
title: "ruse Long-Horizon Design Requirements"
summary: >
  Normative design requirements across 20 domains that cause failures 2–5 years out (not at first
  implementation): spec-vs-implementation separation, parity meaning, persistence & crash consistency,
  determinism & replay, background scheduler, cache, IDs/generations/time, multi-client concurrency,
  plugin governance, config/profile/feature-pack, extended error/status, security/trust boundaries,
  cross-platform semantics, terminal UX, render-IR risks, API-stability paradox, performance stability,
  CI/CD, contributor sustainability, and product scope. Each domain lists what to add; the mirror
  anti-patterns live in anti-patterns.md.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - stability-and-observability.md
  - render-and-frontends.md
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
> real user flows. Each numbered domain below is a normative checklist (what to add); the mirror
> anti-patterns are in [../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md).

> **Requirement IDs.** Each requirement is tagged `DR-<CODE>-<n>` (Design Requirement) — `<CODE>` is the enclosing domain's code, `<n>` its position within that domain in document order — plus a priority tier P0–P3 (P0 foundational/data-integrity/core-path · P1 major-subsystem correctness · P2 quality · P3 long-horizon/polish).
> These `DR-`-prefixed IDs are the positive "do" mirror of the same-code anti-pattern "don'ts" in [../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md); the `DR-` prefix keeps them in a distinct namespace (e.g. `DR-SPEC-1` here vs. the unrelated `SPEC-1` anti-pattern).

## 1. Specification ↔ Implementation (`SPEC`)
- **DR-SPEC-1** (P0) — Separate the **normative specification** from implementation docs ("must be X" vs "the current Rust impl
  does Y").
- **DR-SPEC-2** (P0) — Define a language-independent state machine + invariants per core concept.
- **DR-SPEC-3** (P1) — Protocols define **wire-level meaning**, not implementation examples.
- **DR-SPEC-4** (P1) — Mark ambiguous behavior deliberately as **unspecified / implementation-defined**.
- **DR-SPEC-5** (P1) — Write spec tests against **observable results**, not specific Rust types.
- **DR-SPEC-6** (P2) — Separate the reference-implementation path from production-optimization paths.
- **DR-SPEC-7** (P2) — RFCs record approval rationale **and** rejected alternatives **and** re-evaluation conditions.
- **DR-SPEC-8** (P2) — Maintain the terminology glossary independently (Buffer/Document/View/Workspace/Session/Client not mixed;
  see [../README.md](../README.md) glossary).

## 2. Parity Meaning (`PAR`)
Define **levels of compatibility**, not a flat feature list:
`Syntax parity · Semantic parity · Observable-behavior parity · Workflow parity · Plugin parity · Bug
compatibility`.
- **DR-PAR-1** (P1) — Tag each feature with a compatibility level: **Exact · Equivalent · Adapted · Unsupported · Intentionally
  different**.
- **DR-PAR-2** (P1) — When the same name means different things in Vim vs Emacs, do **not** force-merge into one command.
- **DR-PAR-3** (P1) — Parity tests include: cursor position, register/kill ring, mode, selection shape, undo grouping, error
  timing (not just final document).
- **DR-PAR-4** (P2) — Officially document Vim Style vs Native Style differences.
- **DR-PAR-5** (P2) — Decide **bug compatibility** explicitly, per behavior.
- **DR-PAR-6** (P2) — Auto-generate a **compatibility-impact report** when behavior changes.
- **DR-PAR-7** (P2) — Parity % is weighted by **usage frequency and importance**, not feature count.
(See [../parity/README.md](../parity/README.md); this taxonomy governs the parity files.)

## 3. Persistence & Crash Consistency (`PERSIST`)
The state at the moment of a crash matters more than the running state.
- **DR-PERSIST-1** (P0) — Track as distinct states: **Document revision · Saved revision · Externally observed file version ·
  Recovery-journal position**.
- **DR-PERSIST-2** (P1) — Separate the roles of autosave / swap / journal / backup.
- **DR-PERSIST-3** (P0) — Transaction journal starts **append-only**; records carry **checksum + schema version**; on truncation,
  recover up to the last valid record.
- **DR-PERSIST-4** (P0) — Recovery data **never auto-overwrites** the original file.
- **DR-PERSIST-5** (P1) — Crash recovery offers three outcomes: **current document · disk file · recoverable changes**.
- **DR-PERSIST-6** (P0) — Use **atomic replace** on save where the platform allows; define directory fsync / metadata / permission
  preservation per platform.
- **DR-PERSIST-7** (P2) — Large files use an **incremental journal**, not a full snapshot.
- **DR-PERSIST-8** (P1) — Workspace/session state has a **versioned persistence format**.
- **DR-PERSIST-9** (P2) — Recovery files have a retention period + PII-removal policy.

## 4. Determinism & Replay (`DET`)
Deterministic replay powers "know exactly where it broke."
- **DR-DET-1** (P0) — Restrict direct access to wall clock / randomness / OS state in core command processing; make time/random/
  environment **injectable services**.
- **DR-DET-2** (P1) — Record input events, commands, transactions, and key async results as **replayable events**; attach an
  **ordering sequence** to external async results.
- **DR-DET-3** (P1) — Replaying the same event log yields the same document state.
- **DR-DET-4** (P2) — Crash reports include the last N semantic events.
- **DR-DET-5** (P2) — Replay logs store the **minimum needed data + a redaction policy**, not full content.
- **DR-DET-6** (P2) — Fuzzing failures auto-save as replay fixtures.

## 5. Background Scheduler & Resource Control (`SCHED`)
A central scheduler is aware of **all** background work.
- **DR-SCHED-1** (P1) — Each task carries metadata: **priority · deadline · cost estimate · cancellation token · workspace ·
  document revision · owner/plugin**.
- **DR-SCHED-2** (P0) — User input and screen refresh always outrank background work.
- **DR-SCHED-3** (P1) — Coalesce duplicate parse/index requests per document; cancel superseded requests where only the latest
  result matters.
- **DR-SCHED-4** (P2) — Per-service and per-plugin **CPU / memory / I/O budgets**; separate idle-time from interactive work; a
  **bandwidth budget** in remote environments.
- **DR-SCHED-5** (P2) — Detect starvation and priority inversion; degrade feature quality under load
  (`full semantic index → current-file index → visible-range only`).

## 6. Cache (`CACHE`)
Caches are the most common source of inconsistency.
- **DR-CACHE-1** (P1) — Every cache names its **source data + invalidation source**.
- **DR-CACHE-2** (P1) — Cache keys include **revision, profile, capability, schema version**.
- **DR-CACHE-3** (P1) — Any cache is deletable at any time; guarantee a regeneration path on corruption.
- **DR-CACHE-4** (P0) — A cache hit must **not** change the semantic result.
- **DR-CACHE-5** (P2) — Distinguish the trust boundary of remote vs local caches.
- **DR-CACHE-6** (P2) — Make cache size + eviction observable; manage command-palette / syntax / LSP-position caches independently.

## 7. IDs, Generations, Time (`ID`)
- **DR-ID-1** (P1) — Scope IDs: **process-local · session-local · workspace-persistent · globally stable**.
- **DR-ID-2** (P1) — Reusable slot IDs carry a **generation**.
- **DR-ID-3** (P0) — Never persist process-local IDs in external protocols.
- **DR-ID-4** (P1) — Use **monotonic sequence** for ordering, not wall clock; separate user-display time from internal timeout
  time.
- **DR-ID-5** (P1) — Define ownership of Remote-Client ID / Workspace ID / Document ID and the collision policy for
  command/transaction IDs.

## 8. Multi-Client & Concurrency (`MULTI`)
Even if single-TUI first, design for remote/GUI/web clients.
- **DR-MULTI-1** (P1) — Decide whether multiple clients may attach to one workspace runtime.
- **DR-MULTI-2** (P1) — Cursor / viewport / input mode are **client/view-local**.
- **DR-MULTI-3** (P0) — Choose **optimistic concurrency** or **authoritative sequencing** for document changes; specify
  conflicting-transaction rules.
- **DR-MULTI-4** (P2) — Manage each client's capability + profile independently.
- **DR-MULTI-5** (P1) — **Backpressure** so a slow client can't block the whole runtime.
- **DR-MULTI-6** (P2) — On reconnect, recover missed events via snapshot or delta.
- **DR-MULTI-7** (P2) — Specify the target client for client-local actions (clipboard, notification, open-browser).

## 9. Plugin Ecosystem Governance (`GOV`)
A stable API does not imply a stable ecosystem.
- **DR-GOV-1** (P2) — Distinguish responsibility scope of official vs third-party plugins.
- **DR-GOV-2** (P2) — Marketplace verification levels: **Official · Verified · Community · Unreviewed · Deprecated ·
  Quarantined**.
- **DR-GOV-3** (P1) — Require re-approval when a plugin's permissions change; policy for malicious/abandoned packages.
- **DR-GOV-4** (P3) — Namespace ownership + package-name dispute policy; transfer procedure for orphaned plugins.
- **DR-GOV-5** (P2) — Service-sharing rules between same-capability plugins; dependency resolution via lockfile + checksum.
- **DR-GOV-6** (P3) — Plugin quality metrics: **crash rate · activation latency · API compatibility**.
- **DR-GOV-7** (P2) — Don't blanket-block plugins competing with core, but define the conflict boundary; ship an Extension SDK
  **conformance test kit**.

## 10. Config, Profile, Feature Pack (`CFG`)
- **DR-CFG-1** (P1) — Separate user / workspace / machine-local settings; security-sensitive settings are **not** overridable by
  a workspace.
- **DR-CFG-2** (P1) — Define merge rules per type: **replace · append · set-union · deep merge**.
- **DR-CFG-3** (P2) — Preserve **source provenance**; provide `:inspect config editor.tab_width` to show where a value came from.
- **DR-CFG-4** (P2) — Profiles may carry behavior policy (not just keymaps) but with **bounded scope** (no full core monkey-patch).
- **DR-CFG-5** (P1) — Feature packs are **declarative dependency bundles**; config migration separates auto-conversion from
  manual warnings; a **safe mode** runs even with bad config.

## 11. Extended Error / Log / Status (`STAB` addendum)
(Base model in [stability-and-observability.md](../design/stability-and-observability.md).)
- **DR-STAB-1** (P1) — Separate state from error (**Error = event, Status = persistent state**); log each transition's reason +
  cause event.
- **DR-STAB-2** (P2) — Layer error codes but don't over-subdivide; separate user-facing message from developer diagnostic.
- **DR-STAB-3** (P1) — Designate fields that must **never** be logged, by default.
- **DR-STAB-4** (P2) — Per-component **ring buffer** of recent logs; a trace-sampling policy.
- **DR-STAB-5** (P1) — A dedicated minimal **crash path** on fatal invariant failure that depends less on allocation/locks.
- **DR-STAB-6** (P2) — Health status includes **freshness** (`LSP: Ready, checked 200ms ago`; `Remote: Unknown, no heartbeat 20s`).

## 12. Security & Trust Boundary (`TRUST`)
- **DR-TRUST-1** (P0) — Treat each principal at a distinct trust level: **core · official plugin · third-party plugin · workspace
  repository · remote server · terminal output · AI agent**.
- **DR-TRUST-2** (P0) — Make a **trust decision before opening** a workspace; the client verifies remote-runtime binary integrity.
- **DR-TRUST-3** (P1) — Sanitize terminal escapes into a semantic terminal model; define env-var forwarding policy to plugins/shell.
- **DR-TRUST-4** (P1) — A **secret provider API** so plugins don't store plaintext secrets.
- **DR-TRUST-5** (P1) — Distinguish AI commands that may **execute** from those that may only **propose**.
- **DR-TRUST-6** (P1) — Package-signing key rotation + revoke; a forced plugin-block for security fixes; **redaction preview** when
  building a diagnostic bundle.

## 13. Cross-Platform Semantics (`XPLAT`)
- **DR-XPLAT-1** (P1) — Manage a list of **behavioral differences**, not just an OS abstraction.
- **DR-XPLAT-2** (P1) — Policies for filename case-sensitivity + normalization; model symlink/junction/UNC/WSL paths separately.
- **DR-XPLAT-3** (P2) — Handle executable bit / permission / ACL per platform; abstract process signal + termination semantics.
- **DR-XPLAT-4** (P1) — Do shell quoting via a real quoter, never string concatenation; specify newline/encoding/clipboard format.
- **DR-XPLAT-5** (P2) — Test macOS Unicode normalization; treat file-watcher missing/duplicate events as normal.
- **DR-XPLAT-6** (P2) — Split platform capability into **build-time** and **runtime** capability.

## 14. Terminal UX (`TUX` addendum)
(Base model in [../parity/terminal.md](../parity/terminal.md).)
- **DR-TUX-1** (P1) — Manage terminal capability separately from user preference; capability changes don't auto-switch
  mid-session — define a **renegotiation point**.
- **DR-TUX-2** (P2) — Specify the editor escape chord inside a terminal-buffer passthrough; per-profile key-ambiguity timeout.
- **DR-TUX-3** (P2) — Model escape wrapping by SSH/tmux nesting depth; define modal-transition policy during IME.
- **DR-TUX-4** (P2) — View priority + collapse rules when width shrinks; preserve selection/focus/accessibility even in image
  fallback; never convey state by color alone; core commands usable even on headless/dumb terminals.

## 15. Render-IR Risks (`RIR`)
A common IR is powerful but can become another giant legacy.
- **DR-RIR-1** (P1) — Separate the **Semantic View Model** from the low-level **Render IR**; the plugin API exposes only up to
  the semantic model where possible.
- **DR-RIR-2** (P2) — The Render IR is backend-neutral but **not the union of all backends**; isolate backend-specific
  extensions in a **capability namespace**.
- **DR-RIR-3** (P2) — IR version-migration tests; support **incremental diff**, not only whole-tree.
- **DR-RIR-4** (P2) — Resource references are **stable resource handles**, not raw file paths; specify image/font/binary resource
  lifecycle.

## 16. API-Stability Paradox (`APIX`)
The most dangerous thing is stabilizing a bad API too fast.
- **DR-APIX-1** (P1) — Promotion ladder **Internal → Experimental → Preview → Stable → Deprecated → Removed** (see
  [../protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md)).
- **DR-APIX-2** (P2) — ≥2 independent implementations/plugins before Stable; opt-in/local-only API telemetry; an **API surface
  budget**; distinguish convenience vs primitive APIs.
- **DR-APIX-3** (P1) — Every API can express failure/cancellation/partial success; each new API ships with versioning + migration.

## 17. Performance Stability (`PERFS`)
- **DR-PERFS-1** (P1) — Manage **p95/p99**, not averages; per-stage budgets (input→command→transaction→render).
- **DR-PERFS-2** (P2) — Measure cold vs warm start separately; benchmark with a **real plugin set**, not only an empty editor.
- **DR-PERFS-3** (P2) — Separate memory peak vs steady-state; simulate poor remote latency/bandwidth; per-feature degradation
  policy; track allocator/allocation-count regressions; export a per-device performance profile.

## 18. CI/CD Additions (`OPS` addendum)
(Base pipeline in [../operations/ci-cd-and-release.md](../operations/ci-cd-and-release.md).)
- **DR-OPS-1** (P2) — Change-impact analysis to run relevant tests fast, with periodic full CI; a **merge queue**.
- **DR-OPS-2** (P1) — Build the release artifact **once**; all channels reuse it; **rollback = re-publish a prior verified
  artifact**.
- **DR-OPS-3** (P2) — Ship a plugin-API compatibility report as a release asset; verify **binary reproducibility** where
  possible; long **soak tests** on nightly; **fault-injection CI** (disk full, permission loss, process
  crash, packet loss, truncated journal); test-quarantine entries carry expiry + owner; observe CI's own
  health.

## 19. Contributor Sustainability (`CONTRIB`)
- **DR-CONTRIB-1** (P2) — A new contributor can build/test within 30 minutes (bootstrap).
- **DR-CONTRIB-2** (P1) — Enforce architecture-boundary violations via lint / dependency rules.
- **DR-CONTRIB-3** (P2) — Separate good first-contribution areas from core-critical areas; an ownership map by **area**, not code.
- **DR-CONTRIB-4** (P2) — Clear criteria for what changes require an RFC; review checklist includes performance/compatibility/
  recovery/observability.
- **DR-CONTRIB-5** (P3) — Separate generated vs hand-written code; provide test-fixture generators; track maintainer bus factor; no
  single-person tacit knowledge on key design decisions.

## 20. Product Scope & Strategy (`SCOPE`)
The largest risk. Non-goals to hold (canonical: [`spec/PROJECT.md` §Non-goals](../../spec/PROJECT.md) /
[`spec/PRD.yaml` `mvp.non_goals`](../../spec/PRD.yaml)):
- **DR-SCOPE-1** (P1) — Don't complete Vim + Emacs + Native all in v1; don't build TUI + GUI + Web simultaneously.
- **DR-SCOPE-2** (P1) — Don't platformize Editor + IDE + OS shell + notebook + analyzer at once.
- **DR-SCOPE-3** (P2) — Don't build a Marketplace before there are users, or a Plugin SDK before real plugins validate the API.
- **DR-SCOPE-4** (P2) — Don't over-distribute local runtime/client into a distributed system before remote is needed.
- **DR-SCOPE-5** (P1) — Don't generalize every feature until simple file editing becomes complex.
- **DR-SCOPE-6** (P0) — Don't defer the MVP forever in the name of "sustainability"; don't refuse to use the current language's
  strengths for the sake of hypothetical future porting; don't perfect the architecture without real user
  feedback.

---

## How to Use
Each domain maps 1:1 to an anti-pattern category in
[../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md) (same code). Requirements here are the
"do"; anti-patterns are the "don't." The design-concern checklist + doc template are in
[design-charter.md](design-charter.md); the lock-before-coding decisions are in
[`spec/DECISIONS.md`](../../spec/DECISIONS.md).
