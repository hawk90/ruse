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

## 1. Specification ↔ Implementation (`SPEC`)
- Separate the **normative specification** from implementation docs ("must be X" vs "the current Rust impl
  does Y").
- Define a language-independent state machine + invariants per core concept.
- Protocols define **wire-level meaning**, not implementation examples.
- Mark ambiguous behavior deliberately as **unspecified / implementation-defined**.
- Write spec tests against **observable results**, not specific Rust types.
- Separate the reference-implementation path from production-optimization paths.
- RFCs record approval rationale **and** rejected alternatives **and** re-evaluation conditions.
- Maintain the terminology glossary independently (Buffer/Document/View/Workspace/Session/Client not mixed;
  see [../README.md](../README.md) glossary).

## 2. Parity Meaning (`PAR`)
Define **levels of compatibility**, not a flat feature list:
`Syntax parity · Semantic parity · Observable-behavior parity · Workflow parity · Plugin parity · Bug
compatibility`.
- Tag each feature with a compatibility level: **Exact · Equivalent · Adapted · Unsupported · Intentionally
  different**.
- When the same name means different things in Vim vs Emacs, do **not** force-merge into one command.
- Parity tests include: cursor position, register/kill ring, mode, selection shape, undo grouping, error
  timing (not just final document).
- Officially document Vim Style vs Native Style differences.
- Decide **bug compatibility** explicitly, per behavior.
- Auto-generate a **compatibility-impact report** when behavior changes.
- Parity % is weighted by **usage frequency and importance**, not feature count.
(See [../parity/README.md](../parity/README.md); this taxonomy governs the parity files.)

## 3. Persistence & Crash Consistency (`PERSIST`)
The state at the moment of a crash matters more than the running state.
- Track as distinct states: **Document revision · Saved revision · Externally observed file version ·
  Recovery-journal position**.
- Separate the roles of autosave / swap / journal / backup.
- Transaction journal starts **append-only**; records carry **checksum + schema version**; on truncation,
  recover up to the last valid record.
- Recovery data **never auto-overwrites** the original file.
- Crash recovery offers three outcomes: **current document · disk file · recoverable changes**.
- Use **atomic replace** on save where the platform allows; define directory fsync / metadata / permission
  preservation per platform.
- Large files use an **incremental journal**, not a full snapshot.
- Workspace/session state has a **versioned persistence format**.
- Recovery files have a retention period + PII-removal policy.

## 4. Determinism & Replay (`DET`)
Deterministic replay powers "know exactly where it broke."
- Restrict direct access to wall clock / randomness / OS state in core command processing; make time/random/
  environment **injectable services**.
- Record input events, commands, transactions, and key async results as **replayable events**; attach an
  **ordering sequence** to external async results.
- Replaying the same event log yields the same document state.
- Crash reports include the last N semantic events.
- Replay logs store the **minimum needed data + a redaction policy**, not full content.
- Fuzzing failures auto-save as replay fixtures.

## 5. Background Scheduler & Resource Control (`SCHED`)
A central scheduler is aware of **all** background work.
- Each task carries metadata: **priority · deadline · cost estimate · cancellation token · workspace ·
  document revision · owner/plugin**.
- User input and screen refresh always outrank background work.
- Coalesce duplicate parse/index requests per document; cancel superseded requests where only the latest
  result matters.
- Per-service and per-plugin **CPU / memory / I/O budgets**; separate idle-time from interactive work; a
  **bandwidth budget** in remote environments.
- Detect starvation and priority inversion; degrade feature quality under load
  (`full semantic index → current-file index → visible-range only`).

## 6. Cache (`CACHE`)
Caches are the most common source of inconsistency.
- Every cache names its **source data + invalidation source**.
- Cache keys include **revision, profile, capability, schema version**.
- Any cache is deletable at any time; guarantee a regeneration path on corruption.
- A cache hit must **not** change the semantic result.
- Distinguish the trust boundary of remote vs local caches.
- Make cache size + eviction observable; manage command-palette / syntax / LSP-position caches independently.

## 7. IDs, Generations, Time (`ID`)
- Scope IDs: **process-local · session-local · workspace-persistent · globally stable**.
- Reusable slot IDs carry a **generation**.
- Never persist process-local IDs in external protocols.
- Use **monotonic sequence** for ordering, not wall clock; separate user-display time from internal timeout
  time.
- Define ownership of Remote-Client ID / Workspace ID / Document ID and the collision policy for
  command/transaction IDs.

## 8. Multi-Client & Concurrency (`MULTI`)
Even if single-TUI first, design for remote/GUI/web clients.
- Decide whether multiple clients may attach to one workspace runtime.
- Cursor / viewport / input mode are **client/view-local**.
- Choose **optimistic concurrency** or **authoritative sequencing** for document changes; specify
  conflicting-transaction rules.
- Manage each client's capability + profile independently.
- **Backpressure** so a slow client can't block the whole runtime.
- On reconnect, recover missed events via snapshot or delta.
- Specify the target client for client-local actions (clipboard, notification, open-browser).

## 9. Plugin Ecosystem Governance (`GOV`)
A stable API does not imply a stable ecosystem.
- Distinguish responsibility scope of official vs third-party plugins.
- Marketplace verification levels: **Official · Verified · Community · Unreviewed · Deprecated ·
  Quarantined**.
- Require re-approval when a plugin's permissions change; policy for malicious/abandoned packages.
- Namespace ownership + package-name dispute policy; transfer procedure for orphaned plugins.
- Service-sharing rules between same-capability plugins; dependency resolution via lockfile + checksum.
- Plugin quality metrics: **crash rate · activation latency · API compatibility**.
- Don't blanket-block plugins competing with core, but define the conflict boundary; ship an Extension SDK
  **conformance test kit**.

## 10. Config, Profile, Feature Pack (`CFG`)
- Separate user / workspace / machine-local settings; security-sensitive settings are **not** overridable by
  a workspace.
- Define merge rules per type: **replace · append · set-union · deep merge**.
- Preserve **source provenance**; provide `:inspect config editor.tab_width` to show where a value came from.
- Profiles may carry behavior policy (not just keymaps) but with **bounded scope** (no full core monkey-patch).
- Feature packs are **declarative dependency bundles**; config migration separates auto-conversion from
  manual warnings; a **safe mode** runs even with bad config.

## 11. Extended Error / Log / Status (`STAB` addendum)
(Base model in [stability-and-observability.md](../design/stability-and-observability.md).)
- Separate state from error (**Error = event, Status = persistent state**); log each transition's reason +
  cause event.
- Layer error codes but don't over-subdivide; separate user-facing message from developer diagnostic.
- Designate fields that must **never** be logged, by default.
- Per-component **ring buffer** of recent logs; a trace-sampling policy.
- A dedicated minimal **crash path** on fatal invariant failure that depends less on allocation/locks.
- Health status includes **freshness** (`LSP: Ready, checked 200ms ago`; `Remote: Unknown, no heartbeat 20s`).

## 12. Security & Trust Boundary (`TRUST`)
- Treat each principal at a distinct trust level: **core · official plugin · third-party plugin · workspace
  repository · remote server · terminal output · AI agent**.
- Make a **trust decision before opening** a workspace; the client verifies remote-runtime binary integrity.
- Sanitize terminal escapes into a semantic terminal model; define env-var forwarding policy to plugins/shell.
- A **secret provider API** so plugins don't store plaintext secrets.
- Distinguish AI commands that may **execute** from those that may only **propose**.
- Package-signing key rotation + revoke; a forced plugin-block for security fixes; **redaction preview** when
  building a diagnostic bundle.

## 13. Cross-Platform Semantics (`XPLAT`)
- Manage a list of **behavioral differences**, not just an OS abstraction.
- Policies for filename case-sensitivity + normalization; model symlink/junction/UNC/WSL paths separately.
- Handle executable bit / permission / ACL per platform; abstract process signal + termination semantics.
- Do shell quoting via a real quoter, never string concatenation; specify newline/encoding/clipboard format.
- Test macOS Unicode normalization; treat file-watcher missing/duplicate events as normal.
- Split platform capability into **build-time** and **runtime** capability.

## 14. Terminal UX (`TUX` addendum)
(Base model in [../parity/terminal.md](../parity/terminal.md).)
- Manage terminal capability separately from user preference; capability changes don't auto-switch
  mid-session — define a **renegotiation point**.
- Specify the editor escape chord inside a terminal-buffer passthrough; per-profile key-ambiguity timeout.
- Model escape wrapping by SSH/tmux nesting depth; define modal-transition policy during IME.
- View priority + collapse rules when width shrinks; preserve selection/focus/accessibility even in image
  fallback; never convey state by color alone; core commands usable even on headless/dumb terminals.

## 15. Render-IR Risks (`RIR`)
A common IR is powerful but can become another giant legacy.
- Separate the **Semantic View Model** from the low-level **Render IR**; the plugin API exposes only up to
  the semantic model where possible.
- The Render IR is backend-neutral but **not the union of all backends**; isolate backend-specific
  extensions in a **capability namespace**.
- IR version-migration tests; support **incremental diff**, not only whole-tree.
- Resource references are **stable resource handles**, not raw file paths; specify image/font/binary resource
  lifecycle.

## 16. API-Stability Paradox (`APIX`)
The most dangerous thing is stabilizing a bad API too fast.
- Promotion ladder **Internal → Experimental → Preview → Stable → Deprecated → Removed** (see
  [../protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md)).
- ≥2 independent implementations/plugins before Stable; opt-in/local-only API telemetry; an **API surface
  budget**; distinguish convenience vs primitive APIs.
- Every API can express failure/cancellation/partial success; each new API ships with versioning + migration.

## 17. Performance Stability (`PERFS`)
- Manage **p95/p99**, not averages; per-stage budgets (input→command→transaction→render).
- Measure cold vs warm start separately; benchmark with a **real plugin set**, not only an empty editor.
- Separate memory peak vs steady-state; simulate poor remote latency/bandwidth; per-feature degradation
  policy; track allocator/allocation-count regressions; export a per-device performance profile.

## 18. CI/CD Additions (`OPS` addendum)
(Base pipeline in [../operations/ci-cd-and-release.md](../operations/ci-cd-and-release.md).)
- Change-impact analysis to run relevant tests fast, with periodic full CI; a **merge queue**.
- Build the release artifact **once**; all channels reuse it; **rollback = re-publish a prior verified
  artifact**.
- Ship a plugin-API compatibility report as a release asset; verify **binary reproducibility** where
  possible; long **soak tests** on nightly; **fault-injection CI** (disk full, permission loss, process
  crash, packet loss, truncated journal); test-quarantine entries carry expiry + owner; observe CI's own
  health.

## 19. Contributor Sustainability (`CONTRIB`)
- A new contributor can build/test within 30 minutes (bootstrap).
- Enforce architecture-boundary violations via lint / dependency rules.
- Separate good first-contribution areas from core-critical areas; an ownership map by **area**, not code.
- Clear criteria for what changes require an RFC; review checklist includes performance/compatibility/
  recovery/observability.
- Separate generated vs hand-written code; provide test-fixture generators; track maintainer bus factor; no
  single-person tacit knowledge on key design decisions.

## 20. Product Scope & Strategy (`SCOPE`)
The largest risk. Non-goals to hold (canonical: [`spec/PROJECT.md` §Non-goals](../../spec/PROJECT.md) /
[`spec/PRD.yaml` `mvp.non_goals`](../../spec/PRD.yaml)):
- Don't complete Vim + Emacs + Native all in v1; don't build TUI + GUI + Web simultaneously.
- Don't platformize Editor + IDE + OS shell + notebook + analyzer at once.
- Don't build a Marketplace before there are users, or a Plugin SDK before real plugins validate the API.
- Don't over-distribute local runtime/client into a distributed system before remote is needed.
- Don't generalize every feature until simple file editing becomes complex.
- Don't defer the MVP forever in the name of "sustainability"; don't refuse to use the current language's
  strengths for the sake of hypothetical future porting; don't perfect the architecture without real user
  feedback.

---

## How to Use
Each domain maps 1:1 to an anti-pattern category in
[../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md) (same code). Requirements here are the
"do"; anti-patterns are the "don't." The design-concern checklist + doc template are in
[design-charter.md](design-charter.md); the lock-before-coding decisions are in
[`spec/DECISIONS.md`](../../spec/DECISIONS.md).
