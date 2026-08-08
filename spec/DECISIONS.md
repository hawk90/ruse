---
doc: decisions
project: ruse
title: "ruse — Decisions (D-*)"
summary: >
  Registry of hard-to-reverse decisions, each with Decision · Reason · Re-evaluation condition and a
  stable D-* ID. Small refactors and implementation choices live in Git history, not here. Decisions are
  never deleted — a superseded one is kept and marked, replaced by a new ID.
audience: [maintainers, contributors, llm-agents]
status: canonical
related:
  - PROJECT.md
  - ARCHITECTURE.md
  - ../docs/rfc/README.md
---

# Decisions

> Only decisions that are **hard to reverse**. Small refactors/impl choices live in Git commits/PRs, not
> here. Each: Decision · Reason · Re-evaluation condition. Status: `decided` | `open`. Never delete a
> decision — supersede it with a new ID and mark the old `superseded`. IDs are stable (`D-xxx`).

## D-001 — Transaction is the only path to document change · decided
- **Decision:** Every mutation goes through a Transaction (base revision + origin); no direct edits.
- **Reason:** Undo, traceability, crash consistency, deterministic replay all depend on it.
- **Re-evaluate if:** a measured hot-path allocation cost proves prohibitive and can't be pooled.
- Refs: INV-TXN, ENG-TXN-001.

## D-002 — Core is a single-threaded deterministic executor · decided
- **Decision:** Preserve observable ordering with a deterministic executor; async producers defer to it.
- **Reason:** Neovim's re-entrancy pain; replay/testability; avoid stale-result races.
- **Re-evaluate if:** profiling shows the single loop is the dominant bottleneck for interactive latency.
- Refs: INV-ASYNC-ORDER, ARCH-EXEC-001.

## D-003 — Document ≠ View ownership · decided
- **Decision:** Document owns text/revision; View owns cursor/selection/viewport; one buffer, many views.
- **Reason:** Enables splits, GUI/remote frontends, and avoids state corruption.
- **Re-evaluate if:** never expected; would require a new invariant.
- Refs: INV-DOC-VIEW, ARCH-OWN-001.

## D-004 — Plugins use WASM/process protocol, not a Rust dylib ABI · decided (DEFERRED by D-039)
- **Decision:** Extension transport is a versioned protocol over WASM or an external process.
- **Reason:** Rust ABI is unstable; need crash isolation and a language-independent SDK.
- **Re-evaluate if:** a stable native-component ABI with isolation becomes established.
- Refs: INV-PROTOCOL-VERSIONED, ENG-PLUG-001.

## D-005 — Save & recovery journal + undo model · decided (design) / open (tuning)
- **Decision:** Distinct Document/Saved/Disk/Journal-position revisions (dirty derived, not a bool);
  append-only transaction journal (checksum + schema version + inverse-edit + metadata) with truncated-
  journal recovery; atomic save (temp+rename, dir fsync, permission/encoding/line-ending preserved); 3-way
  crash recovery (current/disk/recoverable) that never auto-overwrites the original; undo grouping by
  TransactionOrigin; a chronological index over the branching undo tree for `g-`/`g+`/`:earlier Nf`;
  ephemeral/streaming buffers exempt (INV-BUFFER-KIND). Full design:
  [persistence-and-recovery.md](../docs/design/persistence-and-recovery.md). Resolves V-4/V-5.
- **Open (tuning):** large-file incremental-journal thresholds; recovery retention window numbers.
- **Re-evaluate:** finalize numbers at F-008 implementation.
- Refs: [persistence-and-recovery.md](../docs/design/persistence-and-recovery.md), design-requirements.md §3,
  INV-TXN, INV-UNDO, INV-BUFFER-KIND.

## D-006 — Command IDs are stable, namespaced contracts · decided
- **Decision:** Namespaced IDs (`core.*`, `org.example.*`) with alias + deprecation windows on change.
- **Reason:** Configs, keymaps, macros, and other plugins depend on them.
- **Re-evaluate if:** never for meaning; only additively.
- Refs: INV-CMD-SEMANTIC, ENG-CMD-001.

## D-007 — Parity levels are graded and weighted · decided
- **Decision:** L1 feature / L2 interaction / L3 config-plugin-compat; per-feature Exact/Equivalent/Adapted/
  Unsupported/Intentionally-different; L3 (running Vimscript/Elisp) is a non-goal. Parity % weighted by usage.
- **Reason:** "hjkl works = Vim compatible" is meaningless; avoid shallow feature inflation.
- **Re-evaluate:** per-feature as parity CI matures.
- Refs: docs/parity/README.md, docs/architecture/design-requirements.md §2.

## D-008 — Profile > plugin keymap priority · decided (principle) / open (exact tiers) · NARROWED by D-046
- **Decision (principle, locked):** profiles are isolated (never share a key space); user overrides beat
  plugin bindings; plugins cannot force global keys; real conflicts are detected statically.
- **Open (do not lock yet):** the exact ordering of the **provenance** tiers (workspace → user →
  plugin-explicit → plugin-suggested → built-in; esp. the last two) — those cannot be validated until an
  input engine and real plugins exist. *(D-046 narrowed this: former tiers 1-3 were never a provenance
  question at all and are now the D-045 scope axis, decided on census evidence. Only tiers 4-8 remain here.)* Locking them now would violate
  D-010/APIX ("don't stabilize the unvalidated"). Treat the current ordering as provisional.
- **Reason:** predictable conflicts + user control are safe now; fine-grained tiers are not.
- **Re-evaluate:** finalize tiers once F-003 (input) and F-016 (plugins) exist.
- Refs: INV-PROFILE-ISOLATION, INV-PRIORITY, ENG-PROFILE-001. (V-12)

## D-009 — Built-in extension API before public plugin API; no WASM in MVP · decided
- **Decision:** MVP uses an internal extension API; the public WASM/process plugin host lands post-MVP once
  real built-ins (git, search) have validated the surface.
- **Reason:** Don't stabilize an unproven API; official features dogfood it first.
- **Re-evaluate:** promote APIs to Stable only after ≥2 independent users (ENG-PROTO-001).
- Refs: F-016, docs/architecture/design-requirements.md §24.

## D-010 — Stable-API promotion requires 2 independent users · decided
- **Decision:** Internal→Experimental→Preview→Stable ladder; ≥2 independent implementations before Stable.
- **Reason:** The biggest risk is stabilizing a wrong abstraction.
- **Re-evaluate if:** never expected.
- Refs: INV-PROMOTION, docs/protocols/versioning-and-evolution.md.

## D-011 — Client ⁄ Workspace-runtime boundary is first-class · decided (DEFERRED by D-039)
- **Decision:** Local path ≠ workspace path (typed); remote runtime negotiates version; boundary present
  from the start (even single-TUI).
- **Reason:** Retrofitting remote is the classic failure.
- **Re-evaluate if:** never expected.
- Refs: INV-REMOTE-FIRST, ARCH-LAYER-001.

## D-012 — Multiple clients per workspace · open
- **Direction:** Design so cursor/viewport/mode are client/view-local; decide optimistic vs authoritative
  sequencing before enabling.
- **Open:** whether v1.x enables multi-client at all.
- **Re-evaluate:** before F-017 hardening.

## D-013 — Offline / reconnect policy · open
- **Open:** on remote disconnect — continue editing, go read-only, or local journal + conflict resolution.
- **Re-evaluate:** before F-017.

## D-014 — Semantic View vs Render IR boundary · decided (DEFERRED by D-039)
- **Decision:** Plugins see the semantic view model; the low-level Render IR is backend-neutral (not the
  union of backends); backend-specific bits isolated in a capability namespace.
- **Reason:** Prevent the IR from becoming an unbounded legacy DOM.
- **Re-evaluate:** as GUI backend arrives (F-018).
- Refs: INV-RENDER-IR, ENG-RENDER-001.

## D-015 — Terminal capability fallback + pinned render profile · decided
- **Decision:** Active probe → confidence ledger + user override; render profile pinned per client-view;
  images degrade, never disappear.
- **Reason:** Screen stability; TERM alone is unreliable.
- **Re-evaluate:** on explicit renegotiation events (resize/reconnect/override).
- Refs: INV-CAP-DEGRADE, ENG-RENDER-001.

## D-016 — Status / Error / Panic are separate · decided
- **Decision:** Error = event (typed, coded); Status = per-component state machine; Panic = invariant
  violation captured only at boundaries.
- **Reason:** Debuggability; bounded blast radius.
- **Re-evaluate if:** never expected.
- Refs: INV-ERR-CLASS, ENG-ERR-001, ENG-OBS-001.

## D-017 — Log PII / redaction policy · open
- **Direction:** Designate never-log fields by default; diagnostic bundles redact by default with preview.
- **Open:** exact field list and retention windows.
- **Re-evaluate:** before any telemetry/diagnostic-export ships.

## D-018 — Background scheduler owns all background work · decided (principle) / open (budgets)
- **Decision (principle, locked):** all background work goes through one central scheduler; input + render
  always outrank background; duplicate per-document index/parse is coalesced and superseded work cancelled.
- **Open (do not lock yet):** per-task priority/deadline/cost/budget *specifics* — no MVP feature exercises
  the scheduler (first users are post-MVP F-011/F-014/F-015), and budgets must be tuned on real workloads.
- **Reason:** the ownership principle prevents the Neovim "plugins accumulate → slow" failure; the numbers
  can't be validated pre-workload.
- **Re-evaluate:** set budgets when F-011/F-014 provide real load.
- Refs: INV-SCHED-1, ENG-ASYNC-001. (V-12, V-16)

## D-019 — Performance latency budgets exist and gate CI · decided
- **Decision:** Per-stage p95/p99 budgets; PR trend + warn, main/nightly gate on a fixed machine.
- **Reason:** Prevent silent regressions.
- **Re-evaluate:** budgets revised with data.
- Refs: ENG-PERF-001, docs/operations/ci-cd-and-release.md §10.

## D-020 — 1.0 scope / non-goals · decided (revisable)
- **Decision:** Vim Style leads; TUI-first; no GUI/Web/Marketplace/WASM-host/collab in v1 (see PROJECT.md
  Non-goals).
- **Reason:** Accepting too much future at once blurs core semantics.
- **Re-evaluate:** after MVP ships and real user feedback exists.

## D-021 — Doc system + authoring format · decided
- **Decision:** `spec/` = state source of truth, `docs/` = prose reference; CONTEXT.md and `.context/*` are
  generated (hand-maintained until the generator exists).
- **Authoring format:** author everything human-side; machine artifacts are **generated**, not hand-kept.
  Test: a thing whose entries are **records with fields** (query/patch) is authored **YAML** (PRD, POLICY,
  context-profiles, glossary); a thing you **read as prose/checklist** is authored **Markdown**
  (anti-patterns, invariants, parity, architecture prose). No JSON is hand-authored — if a runtime needs
  JSON (e.g. `glossary.json`), the generator emits it from the YAML source.
- **Glossary:** canonical = `spec/glossary.yaml` (multi-language); the human table + `glossary.json` are
  generated.
- **Anti-patterns / invariants / parity stay Markdown**; their ID sets are machine-extractable so
  `spec validate` checks cross-references (POLICY `refs:`, invariant `Guards:`) resolve.
- **Reason:** one fact one home; comments/readability where humans author; unambiguous JSON only where a
  machine consumes — as a generated artifact.
- **Re-evaluate:** when `xtask` is built, flip generated files to generated-only.
- Refs: ENG-DOC-001, D-022.

## D-022 — Build the spec-context generator (`xtask`) · open
- **Direction:** A tool that (a) **generates** CONTEXT.md, `.context/*.md`, the human glossary table +
  `glossary.json`, and an `anti-patterns.index.*` (id→category→label) from the YAML/MD sources; and (b) runs
  `spec validate`: YAML syntax/schema, duplicate IDs, dangling `depends_on`, component layer violations,
  policy cycles, expired exceptions, illegal status transitions, and **cross-reference resolution** — every
  `INV-*`, `ENG-*`, `D-*`, `F-*`/`C-*`, anti-pattern `CATEGORY-n`, and parity ID referenced anywhere must
  resolve to a real definition (IDs extracted from the Markdown registries).
- **Open:** implementation once the repo exists.
- **Re-evaluate:** first coding milestone.

## D-023 — Positions are anchor-based · decided
- **Decision:** Long-lived positions (cursors, decorations, diagnostics, marks) are anchors with affinity/
  gravity that survive edits; never raw offsets. Coordinates are typed (byte/char/grapheme/cell).
- **Reason:** Foundational for F-002; extmark-stability parity; charter decision #04 had no D-entry (V-19).
- **Re-evaluate if:** never expected.
- Refs: INV-ANCHOR, INV-POS-TYPED, ENG-POS-001, F-002.

## D-024 — Remote protocol version-skew policy · open
- **Direction:** Client and runtime negotiate a compatible protocol version (not commit-pinned); bundle/
  offline-cache compatible runtime builds or negotiate down; additive evolution (INV-ADDITIVE).
- **Open:** the supported skew range and downgrade behavior (charter #13 had no D-entry — V-19).
- **Re-evaluate:** before F-017.
- Refs: INV-ADDITIVE, REM-VERSION-1.

## D-025 — Editing-language composition engine · decided (design) / open (details)
- **Decision:** A first-class core subsystem (`C-EDITLANG`) composes operator + count + (motion|text-object)
  into a typed `Range{kind, inclusivity}` with the exclusive→inclusive→linewise promotion, emits ONE
  Transaction, records a re-parameterizable **change-intent** for dot-repeat, and supports plugin-registrable
  operators (`g@`). `editor.delete_selection` is a *sibling* (selection→Range), not the generalization.
  Full design: [editing-language.md](../docs/design/editing-language.md). Resolves V-1/V-7/V-9.
- **Open (details):** blockwise operator semantics (ragged-right/virtual columns); `=`/`!` external-filter
  outcome; `ChangeIntent` serialization when a macro contains `.`.
- **Re-evaluate:** finalize details as F-003 reaches L2.
- Refs: [editing-language.md](../docs/design/editing-language.md), vim.md VIM-OP/VIM-MOT-PROMOTE/VIM-REPEAT-DOT,
  INV-CMD-SEMANTIC, INV-ANCHOR, INV-TXN.

## D-026 — Unified register / kill-ring model · decided (design)
- **Decision:** One shared store (`C-REGISTER`) reproduces BOTH surfaces exactly: typed slots (char/line/
  block governing paste geometry) + numbered shift-ring (`"1`–`"9`, `"0` yank-only, `"-` small-delete) +
  named/special slots; the Emacs kill ring is a **view/policy** over the same store with consecutive-kill
  **coalescing** and a transient post-yank **yank-pop** state; OS-clipboard + OSC-52 bridge. Full design +
  per-surface mapping tables + differential tests: [register-model.md](../docs/design/register-model.md).
  Resolves V-2.
- **Re-evaluate:** adjust as Vim/Emacs register parity (F-003/F-012 L2) is implemented.
- Refs: [register-model.md](../docs/design/register-model.md), vim.md VIM-REG, emacs.md EMACS-KILL, common.md COM-11.

## D-027 — Positions-history model (marks / jumplist / mark-ring / selections) · decided (design)
- **Decision:** One subsystem (`C-POSHIST`) over the D-023 anchor store reproduces every surface via
  **pluggable membership + traversal policies** across three containers (NamedMap / Ring / CursoredList) +
  the live selection Set: Vim jumplist (per-view cursored list; membership = `NavMeta.is_jump`, so `n` is a
  jump / `j` isn't) + `m{a-z}` buffer-local, `m{A-Z}` global-persistent, special marks; Vim changelist
  (`g;`/`g,`); Emacs per-buffer mark ring + global mark ring with pop-rotate; Helix/Kakoune selection sets.
  Point-rings and selection-sets coexist because **every entry is a `Selection`** (a set of anchor-based
  carets) and a bare cursor is a degenerate one-caret collapsed selection — so single→multi-selection
  (NAT-5) needs **no type rewrite** (design-requirements §4). Global marks/bookmarks persist as
  re-anchorable `Detached` coordinates (ties D-005). Full design:
  [positions-history.md](../docs/design/positions-history.md). Resolves V-6.
- **Open (details):** exact `is_jump`/`push-mark` command sets; jumplist end/count edge cases; changelist↔undo
  interaction; selection-history depth/bindings for Native — see doc Open questions.
- **Re-evaluate:** finalize details as Vim marks (F-003 L2) and Native multi-selection (NAT-5) are implemented.
- Refs: [positions-history.md](../docs/design/positions-history.md), vim.md VIM-MARK-1, emacs.md EMACS-REGION-2,
  native-style.md NAT-5, INV-ANCHOR. (V-6)

## D-028 — Vim regex dialect strategy · decided (design)
- **Decision:** Option (a). A `Regex` abstraction (`C-REGEX`) with a magic-aware Vim-dialect **front-end**
  that parses any surface pattern into a ruse-owned regex IR, and **pluggable engines**: a wrapped Rust
  `regex` **DefaultEngine** (linear, ReDoS-free) for internal/LSP/fast paths, and an **owned Vim-dialect
  engine** (Pike-VM/NFA + bounded-backtracking) for `\zs`/`\ze`, lookbehind `\@<=`/`\@<!`, backrefs, and
  magic levels `\v \V \m \M`. `'gdefault'` handled at the substitute-command layer; capability-first engine
  router; ReDoS step-budget → typed `BudgetExceeded` + scheduler cancellation. Full design:
  [vim-regex.md](../docs/design/vim-regex.md). Resolves V-8.
- **Open (details):** OQ-1..6 in the doc (perf tuning, untrusted-pattern policy).
- **Re-evaluate:** as F-009 Vim search reaches L2.
- Refs: [vim-regex.md](../docs/design/vim-regex.md), vim.md VIM-SEARCH-1, DEP-REGEX, F-009.

## D-029 — Remote connection is built-in, not a plugin · decided
- **Decision:** The SSH connector + agent bootstrap are **Built-in Services**; users run `editor ssh host`
  with no local Remote package. Host picker/status may be a bundled UI; special providers (AWS SSM, K8s,
  VPN) are third-party plugins.
- **Reason:** plugin-ization would hurt discoverability, couple connection to plugin-load success, break
  safe-mode/recovery, and make the client/runtime protocol hostage to a 3rd-party API.
- **Re-evaluate if:** never expected for the SSH path.
- Refs: [remote-runtime.md](../docs/design/remote-runtime.md), INV-REMOTE-FIRST.

## D-030 — Remote Agent = headless workspace runtime, auto-bootstrapped, no sudo · decided
- **Decision:** The Agent is a supervisor of workspace-local services (fs/watch, search, git, LSP, debug,
  PTY, port-forward, toolchain discovery) — not a file daemon. First connect auto-installs it: detect OS/
  arch → client uploads the version-matched bundle over SSH → checksum → atomic install under `$HOME` →
  run `--stdio`. No sudo; versioned side-by-side; never overwrite a running agent; offline-first (client
  ships bundles). Missing external tools degrade, they don't fail install.
- **Reason:** real remote dev needs toolchain-adjacent execution; offline-first avoids remote-download
  failures.
- **Re-evaluate:** add selective download + local cache after the offline-first path works.
- Refs: [remote-runtime.md](../docs/design/remote-runtime.md), INV-CAP-DEGRADE, INV-TRUST-1.

## D-031 — Remote transport: SSH stdio + multiplexed framed protocol first · decided (persistent socket later)
- **Decision:** Start with SSH stdio + a per-service multiplexed framed protocol (no listening ports).
  stdout is protocol-only (logs via messages/stderr). Evolve to a persistent socket later for fast
  reconnect / multi-client / task survival / session sharing.
- **Reason:** no firewall/port setup, auth delegated to SSH, clean lifecycle, simple to debug.
- **Re-evaluate:** when reconnect/multi-client/background-indexing demands it.
- Refs: [remote-runtime.md](../docs/design/remote-runtime.md), D-024.

## D-032 — Debug uses a location model; DAP backend first, GDB/MI later · open (backend detail)
- **Direction:** Model `DebugSession{ui_location, adapter_location, debugger_location, target_location,
  source_map, executable, symbols, transport}` — debugger and target locations are independent (remote
  process / container+gdbserver / board+OpenOCD). Common Debug Service over a **DAP** backend first; add a
  **GDB/MI** native backend later for embedded/FPGA/firmware. Language results normalize into a Language
  Service model (no raw-LSP passthrough to UI).
- **Open:** whether GDB/MI is pulled forward if the target audience is systems/firmware devs.
- **Re-evaluate:** before F-021 (debugger).
- Refs: [remote-runtime.md](../docs/design/remote-runtime.md) §Debug.

## D-033 — Two classification axes (delivery + implementation), distinct from architecture layers · decided
- **Decision:** Classify capabilities on two independent axes, neither equal to the code architecture:
  **product/delivery** = Base | Official Pack | Third-party; **implementation source** = Own | Wrapped |
  Direct | External-tool. Decompose each feature (engine/UI/adapter) rather than dropping it in one cell. The
  middle delivery tier is named **Official Pack** (not "ad hoc"). Data lives in
  [`spec/capabilities.yaml`](../spec/capabilities.yaml); framework in
  [delivery-and-dependencies.md](../docs/design/delivery-and-dependencies.md). awesome-neovim is a feature
  inventory, not an architecture to copy.
- **Reason:** avoids the wrong conclusions "plugin list = architecture" and "Base = build everything ourselves".
- **Re-evaluate:** as packs/ecosystem mature.
- Refs: [delivery-and-dependencies.md](../docs/design/delivery-and-dependencies.md), architecture.md ARCH-LAYER-001.

## D-034 — Dependency & implementation-source policy · decided
- **Decision:** We own domain **semantics/contracts** (Document/Command/Transaction/Remote/Plugin/Scheduler/
  Render/Profile), not all code. External crates are welcome but **wrapped behind an Adapter** by default so
  their types never reach the public domain API; trivial deps stay Direct; mature tool ecosystems run as
  **external processes**. Deps are tiered 0–4 (tooling→trust-boundary) with tier-scaled review; budget is by
  **cost type** (compile/binary/startup/alloc/threads/transitive/native/unsafe/supply-chain), not crate count.
  Cargo features are for big platform/distribution splits only — per-user enablement is **runtime activation**.
  Never hand-roll crypto/unicode/escape-parsing/SSH. Source is not permanent (crate→Own or reverse) as long as
  external types don't spread. Data: [`spec/dependencies.yaml`](../spec/dependencies.yaml).
- **Reason:** "small ⇒ self-implement" and "crate exists ⇒ import" are both wrong; control the meaning + failure
  boundaries, use vetted crates for solved problems.
- **Re-evaluate:** per dependency at review; promote CI dependency-gate (Cargo.lock report, banned-in-core,
  license, advisory, unsafe/size deltas).
- Refs: [delivery-and-dependencies.md](../docs/design/delivery-and-dependencies.md), INV-CONTRACT-FIRST, INV-PLUGIN-NO-CORE.

## D-035 — Documentation strategy: repo docs over Wiki; Discussions for Q&A · decided
- **Decision:** Authoritative docs live **in the repository** (`spec/` state + `docs/` prose) and pass through
  `spec-validate` / PR review / branch protection. **GitHub Discussions** hosts questions, ideas, and early
  design chatter (promoted to Issue/RFC once scoped). **GitHub Wiki is NOT used as an authoritative source** —
  it is a separate `.wiki.git` repo outside our validation, so it would become a third doc layer that drifts
  from `spec/`/`docs/` and that `spec-validate` can't check (violates "one fact, one home"). If a Wiki is ever
  added, it is **community-maintained + non-normative** (recipes / terminal-compat reports / platform quirks),
  each page banner-marked "non-normative; authoritative spec is in spec/ & docs/". Long-term, publish `docs/`
  via mdBook/GitHub Pages so the site *is* the repo docs (no separate authoritative copy).
- **Reason:** avoid a third drifting doc layer; keep all normative docs inside the enforced validation loop.
- **Re-evaluate:** only if real users repeatedly need a community-writable, non-normative space.
- Refs: [../docs/contributing/README.md](../docs/contributing/README.md), ENG-DOC-001, D-021.

## D-036 — Disambiguate the "layer" axes (build_stage / architecture_tier) · decided
- **Decision:** Rename the two spec fields that literally spell "layer": PRD component `layer` →
  `build_stage`, capabilities `architecture_layer` → `architecture_tier`. Values unchanged; `runtime` and
  `trust` already unambiguous; `product_layer` and `dependencies.yaml` `allowed_layers` left for a later pass.
- **Reason:** "layer" named four independent axes (tier / build-stage / runtime-location / trust-domain); the
  shared key invited invalid cross-axis conclusions by humans and LLMs — costlier now that the project is
  multi-contributor.
- **Re-evaluate if:** a rename of `product_layer`/`allowed_layers` proves worth the churn.
- Refs: RFC-0011, [../docs/rfc/proposed/RFC-0011-layer-axis-terminology.md](../docs/rfc/proposed/RFC-0011-layer-axis-terminology.md), ENG-DOC-001.

## D-037 — Delivery phases as a spec-owned axis synced to GitHub · decided
- **Decision:** A repo's delivery milestones live in `phases.yaml` — an ordered phase ladder that REFINES the
  coarse PRD F-* `stage` (mvp/post-mvp/future), validated in spec-validate (partition + no cross-stage). GitHub
  Milestones are a one-way generated MIRROR via `ruse phase sync` (idempotent, dry-run by default), never the
  gate of record. Phase ids are slugs, not `P0/P1` (that vocabulary is methodology-rollout priority in the
  governance model). Intra-phase implementation order is derived from `depends_on`, not stored.
- **Reason:** the "when does it ship" axis existed only as prose (roadmap.md) and drifted from capabilities; a
  structured, validated source with generated mirrors removes the drift and is a portable governance methodology.
- **Re-evaluate if:** phases need per-feature (not per-phase) granularity, or a non-GitHub tracker is adopted.
- Refs: `phases.yaml`, [../docs/operations/governance-model.md](../docs/operations/governance-model.md), [../docs/parity/roadmap.md](../docs/parity/roadmap.md), D-021, D-022.

## D-038 — Design-doc code is non-normative; types live in code / spec/contracts · decided
- **Decision:** A design doc specifies the CONTRACT — invariants (INV-*), field semantics, algorithms,
  edge-case rules — not the concrete type. The authoritative home for a concrete type is **code** (internal
  in-memory types) or **`spec/contracts/`** (cross-boundary formats/protocols). A design doc that shows code
  marks it illustrative with a `code-blocks: illustrative` banner pointing at the real source; no fact is
  hand-synced across a doc and code. Enforced by `ruse gov design_code` (warn-only now — the doc↔code
  consistency check that extracts a block and diffs it against the real type succeeds it once the reference
  implementation exists).
- **Reason:** hand-written structs in prose are a drift liability — when the code or the design changes,
  nobody chases the copies (the exact "when do you update them all?" trap). SSOT + generated/checked
  derivations (D-021/D-022) applied to design-doc code.
- **Re-evaluate if:** a generator makes doc type-blocks derivable FROM code (rustdoc extraction) — then they
  may return as generated, never hand-written.
- Refs: [../docs/operations/governance-model.md](../docs/operations/governance-model.md), `tools/rusekit/gov/design_code.py`, D-021, D-022.

## D-039 — Collapse to a two-crate terminal modal editor; defer remote/plugin/render boundaries · decided
- **Decision:** ruse is, first and only, a **terminal-based modal text editor**. The workspace collapses to
  two crates — `editor-core` (pure, IO-free) and `ruse` (the crossterm TUI binary) — via same-process
  function calls, **no RPC/process split**. The remote/plugin/render boundaries are **deferred** (not
  deleted): their stub crates are removed and their design docs kept as notes, reintroduced only when an
  explicit re-boundary trigger fires (RFC-0012 §Re-evaluation). `spec/review-axes.yaml` is **frozen** at 566
  axes — a manual checklist, never a merge gate — until a product need reopens it. Command-level edit
  **traces** (record/replay/share) are a first-class product feature, designed with the input engine.
  Governance stays as the *dogfooded methodology*, secondary to the editor.
- **Deferred by this decision (paused; triggers in RFC-0012):** INV-REMOTE-FIRST (downgraded from an active
  invariant to a deferred commitment), D-011/D-029/D-030/D-031 (remote), D-004/D-009 (plugin protocol),
  D-014/D-015 (render IR). They remain recorded — the thinking is retained — but do not constrain v0.
- **Reason:** the planned architecture was xi-editor's (core/frontend RPC seam + remote-first + versioned
  protocol), and xi-editor failed at exactly that shape (Raph Levien's retrospective). Boundaries drawn
  before a second consumer are unverified commitments; a wrong boundary costs more to remove than an absent
  one costs to add. The repo's own RA-RUSE-003/004 already say this. Over-invest in semantics
  (transaction/undo/trace) which cannot be fixed later; under-invest in structure (crate boundaries,
  protocols) which is normal to fix later.
- **Re-evaluate if:** a re-boundary trigger fires (RFC-0012 §Re-evaluation) — then that boundary returns as
  its own RFC + crate.
- Refs: [../docs/rfc/proposed/RFC-0012-collapse-to-two-crate-editor.md](../docs/rfc/proposed/RFC-0012-collapse-to-two-crate-editor.md), RA-RUSE-003, RA-RUSE-004, D-004, D-011, D-014.

## D-040 — v0 stability/observability scope: assert-vs-error + panic-recovery + tracing; supervisor/health deferred · decided
- **Decision:** For the two-crate v0 editor ([D-039]), the [stability-and-observability](../docs/design/stability-and-observability.md)
  contract is scoped (see its "v0 scope" section). **Live:** the three failure classes / assert-vs-error
  discipline (`debug_assert!` for internal invariants; typed `Result` errors for expected failures);
  loss-safe preflight (`Document::apply` is atomic — no partial state); external-failure graceful degrade
  (tree-sitter/IO drop feature level, never crash); a panic hook that saves `<file>.ruse-recovered` then
  unwinds (NO `catch_unwind`-swallow, NO blanket `panic=abort`); and structured `tracing` logging
  (frontend, `RUSE_LOG`) kept SEPARATE from the replay `Trace`. **Deferred to the LSP/async slice:** the
  `ErrorCode` ecosystem API, the service supervisor, the multi-component Health Registry (INV-STATUS state
  machine + `SystemHealth`), the diagnostic bundle, and distributed transaction-ID trace propagation —
  they need the failure-prone components v0 lacks.
- **Reason:** the stability doc assumes the full architecture (plugins/LSP/remote/supervisor); building it
  all now is the "pattern names first is over-design" the doc itself warns against, and pre-speccing
  unbuilt boundaries (RFC-0012). Scope to what the 2-crate editor actually has so v0 code is built against
  the real contract — heeding anti-patterns STAB-1/2/5/6 and TRACE-1.
- **Re-evaluate if:** the LSP/async or plugin slice lands — then the supervisor, health registry, and
  `ErrorCode` API are re-scoped live with their boundary.
- Refs: [../docs/design/stability-and-observability.md](../docs/design/stability-and-observability.md), RFC-0012, D-039, INV-ERR-CLASS, INV-FAIL-BOUNDED, INV-STATUS.

## D-041 — The assert/error/log discipline is a merge gate, not a review convention · decided
- **Decision:** The [stability](../docs/design/stability-and-observability.md) "v0 decision table" (which of
  `debug_assert!`/`expect` · typed `Result` · `tracing` · `Trace` applies to a given situation) is enforced
  mechanically at merge. **One rule → one mechanism → the most accurate one** (no rule enforced twice):
  every AST-expressible rule is clippy's, and the checker owns ONLY what clippy structurally cannot see.
  **(1) clippy** (the required `rust` check), configured once via crate-root `#![deny]` + `clippy.toml`:
  `print_stdout`/`print_stderr` (diagnostics go through `tracing`; headless CLI carries a commented
  `allow`), `clippy::unwrap_used` (a non-test `.unwrap()` — `allow-unwrap-in-tests` exempts tests), and
  `disallowed-methods = [catch_unwind]` (STAB-6). **(2) the `ruse gov rust_discipline` checker** — the two
  clippy cannot express: `Result<_, String>` (§2, a specific generic argument) and `panic = "abort"` in a
  manifest (STAB-5, a Cargo profile, not Rust).
- **Reason:** the project dogfoods its governance; a rule that lives only in a design doc rots into a
  review-time habit. The code already satisfied both layers at adoption (0 non-test `unwrap`, 7 justified
  `expect`, 0 banned patterns), so the gate carries no annotation debt — it only catches future drift. The
  set is deliberately narrow (no blanket `unwrap`/`panic` ban) to avoid the process-over-implementation
  anti-pattern (RA-RUSE-003) — it targets only the failure modes §1/§2/§8 name.
- **Re-evaluate if:** a sanctioned `catch_unwind` boundary lands (LSP/async slice) — extend the allowlist;
  or clippy gains a native lint that subsumes a checker rule.
- Refs: [../docs/design/stability-and-observability.md](../docs/design/stability-and-observability.md), D-040, RFC-0012, INV-ERR-CLASS, STAB-2, STAB-5, STAB-6, TRACE-1, RA-RUSE-003.

## D-042 — Perf is measure-first: benchmark, then optimize #2/#3 before storage; keep core dep-free · decided
- **Decision:** The per-keystroke cost is O(n) in four independent places (see the [testing-and-benchmarks]
  (../docs/operations/testing-and-benchmarks.md) "v0 scope"): #1 buffer copy+reindex, #2 full tree-sitter
  re-parse per frame, #3 full render (no viewport), #4 line-index rebuild. A criterion baseline
  (`edit_apply` for #1, `highlight_parse` for #2) is captured BEFORE optimizing. Order: **(A) highlight
  caching** (recompute spans only on revision change — no tradeoff, no benchmark needed) and **(B) viewport
  render + scroll** (a missing feature / correctness gap, not an optimization) land first; **(C) incremental
  tree-sitter** and **(D) incremental line index** are gated on the baseline showing #2/#4 dominate. The text
  store stays flat `Arc<[u8]>`; a rope/gap-buffer swap is deferred until a benchmark shows buffer mutation
  actually dominates (MB files), and then goes behind a `TextStorage` trait that seals coordinates to **byte
  offsets**, with the concrete impl **injected from the frontend** so `editor-core` stays dependency-free.
- **Reason:** ropey is not in the build (core has no `[dependencies]`), so "keep ropey" was never the
  question — and at daily-driver sizes a buffer copy is tens of µs while a full re-parse/redraw dominates, so
  a storage rewrite is the wrong first move (RFC-0012: decide perf *after* measurement; anti-patterns PERFS).
  Measure-first applies to *tradeoff* choices (C/D/storage); a pure-win cache (A) and a missing feature (B)
  do not wait on numbers. Sealing coordinates in `TextStorage` keeps a future rope's char/line model from
  leaking into Edit/anchor/undo semantics — the part [D-039] says to churn carefully.
- **Re-evaluate if:** the baseline contradicts the hypothesis (buffer #1 dominates at daily-driver sizes), or
  files routinely reach MB scale — then storage moves up and `TextStorage` + a measured rope-vs-gap-buffer
  choice lands.
- Refs: [../docs/operations/testing-and-benchmarks.md](../docs/operations/testing-and-benchmarks.md), [../docs/design/render-and-frontends.md](../docs/design/render-and-frontends.md), RFC-0012, D-039, DEP-ROPE.

## D-043 — Parity is a machine-derived census of pinned upstreams; humans classify, never enumerate · decided
- **Decision:** `docs/parity/*.md` loses source-of-truth status. The source becomes
  `spec/parity/inventory/<editor>/*.yaml`, generated by `tools/parity/extract_*.py` from upstreams pinned by
  **peeled commit SHA** in [spec/parity/upstreams.yaml](parity/upstreams.yaml) and materialised into a
  gitignored cache (upstream files are never vendored: ruse is MIT, Vim ships under the Vim license, Emacs
  under GPL-3.0 — we extract facts, not files). Three rules bind it: **(1) discovery strict / classification
  lazy** — every item in the declared census scope must be enumerated at the pin (blocking), while
  `status: unclassified` is a legitimate resting state; **(2) classification is locked at SURFACE
  granularity** — a surface is opened whole or not at all; **(3) source of record is per item type, not
  uniform** — options come from runtime introspection, ex-commands from the static table, per-mode keys from
  `runtime/doc/index.txt`, and behaviour only from execution. `verified` is reserved for an item a
  differential fixture proved against the pinned oracle; corroboration between two documents is
  `attestation`, not verification. Enforced by `ruse gov parity_discovery`; counts and their **method** are
  recorded in [spec/parity/coverage.yaml](parity/coverage.yaml).
- **Reason:** every prior governance gate validated the spec's *internal* consistency — parity_coverage
  (parity → PRD), capability_coverage (ruse-native surface), design_backing (depth) — so nothing could see
  outside `spec/`. A hand-authored catalog of 211 IDs reported 100% coverage while omitting an entire
  dimension of the Vim input model: modes were tabulated as *transitions* (enter/exit keys) rather than as
  the eight disjoint keymap namespaces `runtime/doc/map.txt` declares, so the unmatched-key policy, state
  ownership and return semantics had nowhere to live and `C-INPUT` grew a three-axis state machine with
  Insert as a special case. Because ruse's constitution puts spec above implementation, a wrong parity item
  does not stay a documentation bug — it hardens into a wrong requirement. The first Neovim census found
  **1,788 items at v0.12.4** against those 211, which is the size of the gap the old gate could not express.
  Per-item-type sourcing is empirical, not stylistic: `nvim_get_commands({builtin:true})` returns 1 item
  (documented but unimplemented), while options agree exactly between runtime and static table.
- **Re-evaluate if:** the oracle layer proves unworkable — three harnesses were prototyped and three corrupted their own first
  observation (`vim -es` reports the harness's Ex mode from `mode()`; `emacs --batch` hangs forever on
  `read-from-minibuffer`; `execute-kbd-macro` under `--batch` silently empties the buffer on a plain `M-d`),
  so `oracle_selftest` gates the fixture corpus and `undo` is comparable vim↔nvim only. (The Emacs
  *denominator*, listed here as blocking when this decision was taken, was resolved by **D-044**; the Emacs
  *oracle* remains blocked and still needs a pty-hosted `emacs -nw` — the two were separate problems and
  only the second was ever about batch mode.)
- Refs: [parity/upstreams.yaml](parity/upstreams.yaml), [parity/coverage.yaml](parity/coverage.yaml), [../docs/parity/README.md](../docs/parity/README.md), D-007, D-033, D-044, F-003, C-INPUT, VIM-MODE-6.

## D-044 — The Emacs census baseline is the scope list itself; its commands are derived, not enumerated · decided
- **Decision:** the Emacs denominator that D-043 left blocking is resolved as follows. The baseline is
  **`emacs -Q --batch` plus `require` of exactly the libraries in `census_scope.include`** — the load set IS
  the scope list, so it is deterministic and reviewable, and widening it means editing a list that already
  requires a per-path reason. The **command** surface has *no independent denominator*: it is DERIVED from
  `key_binding` (a command reachable from no bound key in an in-scope keymap is not a parity surface), and
  every item carries `derived_from: key_binding` to keep that visible. Emacs surfaces are `R`-primary
  (`keymap`, `key_binding`, `command`, `option`, `hook`) with one `D`-primary exception, `keymap_tier`,
  because no runtime call returns the active-keymap precedence as an ordered list. Since an `R`-primary
  census has no tree to diff against, each generated document declares `derived_from: runtime-binary` +
  `binary_version`, and `ruse gov parity_discovery` FAILS when that version does not match the pin's
  `version_label`; the build is not byte-identical to the pinned commit and says so
  (`binary_identity: unverified-build`). Pseudo-events (`<menu-bar>`, `<tool-bar>`, mouse, `<remap>`) are
  enumerated, never dropped — discovery is strict — but carry `event_class` so a family claiming *keyboard*
  parity selects `key` alone. Every binding also carries `namespace_group` ∈ {core, minibuffer, major-mode}.
- **Reason:** D-043 recorded three candidate baselines differing by multiples (`emacs -Q` + mapatoms = 3,011
  commands; `(interactive` across the tree = 12,087; defcustom = 9,371) and concluded no number was
  defensible. Both halves were the wrong question. The three candidates were three guesses at "how much
  Emacs is Emacs" — a question `census_scope.include` already answers — and "how many commands exist" is
  unanswerable *because* a command nobody can reach is not a surface. Asking instead which commands a key
  reaches yields one number, 803. The check that this cut is honest rather than convenient is that every
  Emacs surface then lands beside its Neovim counterpart instead of dwarfing it: core keyboard namespaces
  1,106 vs `mode_key` 708, minibuffer 233 vs `cmdline` 59, commands 803 vs `ex_command` 557, options 434 vs
  374, hooks 106 vs `event` 141. Editor semantics is about the same size in both editors, which the 12k
  figure made impossible to see. Two guards earned their place immediately: the load-failure check caught
  that `indent.el`/`paragraphs.el` carry no `provide` form and cannot be required (a silent short
  denominator), and the basename-set scope replaced a regex because elisp treats `(` and `|` as literals, so
  a PCRE-shaped pattern would have censused zero and looked successful.
- **Consequence:** the census produced a structural finding, not just counts. **613 keyboard bindings live
  in major-mode maps and have no Vim counterpart at all** — Vim selects a namespace by editor STATE, Emacs
  by what the BUFFER is. That is recorded as CONCEPT-KEYMAP-DISPATCH, and unlike the other entries in
  `concepts/irreconcilable.yaml` it resolves toward **absorption**: the layered model strictly contains the
  disjoint one, so a router built layered yields Vim's eight namespaces as the depth-1 case.
- **Re-evaluate if:** ruse targets Emacs *applications* rather than Emacs editor semantics (Org, Magit,
  Gnus), at which point the scope exclusions — not the baseline — are what must change, and the question
  becomes substrate adequacy rather than parity (HOLE-SUBSTRATE-UNTESTED); or the pinned tree is fetched and
  `S`-corroboration contradicts an `R`-derived surface.
- Refs: [parity/upstreams.yaml](parity/upstreams.yaml), [parity/families.yaml](parity/families.yaml), [parity/concepts/irreconcilable.yaml](parity/concepts/irreconcilable.yaml), D-043, F-016, C-INPUT.

## D-045 — Keymap resolution is one ordered layer stack; Vim's eight namespaces are its depth-1 case · decided
- **Decision:** `C-INPUT` resolves a key against an ORDERED STACK OF KEYMAP LAYERS, consulted highest-rank
  first until one binds. Each layer carries its own `unmatched_key` policy, its own owned state, and a rank;
  a layer may declare `sealed`, meaning resolution stops there instead of falling through. The three input
  profiles are then configurations of one mechanism, not three mechanisms:
  **Vim Style** installs its eight namespaces as eight `sealed` layers of which exactly one is active,
  selected by editor state — a depth-1 stack, which is what makes VS-OBL-1..4 remain literally true.
  **Emacs Style** installs the nine-tier stack (`overriding-terminal-local-map` → … → `global-map`)
  unsealed, selected by what the buffer is. **Derived maps** (one mode = another plus a diff) are a depth-2
  stack `[override, base]`, which may be collapsed at build time as an optimisation but is not a separate
  feature. `config-schema` `keymap.<namespace>` ×8 stay exactly as they are: they are the Vim profile's layer
  overlays, and an Emacs profile adds its own keys rather than renaming these.
- **Reason:** the layered model strictly CONTAINS the other two, so building it costs one implementation and
  building the disjoint one costs two. That is not an aesthetic claim — it is what three censuses show.
  Neovim's `map.txt` declares eight disjoint namespaces (`nvim.mapmode.*`, 8 items). Emacs's precedence stack
  is nine tiers (`emacs.keymaptier.*`), and **613 of its 1,952 keyboard bindings live in major-mode maps** —
  a tier Vim's model has no seat for at all, because Vim selects a namespace by editor STATE while Emacs
  selects one by what the BUFFER is. Helix arrived independently at a third arrangement: `Select` is
  `normal.clone()` + `merge_nodes(overrides)`, 301 inherited bindings under a 33-binding diff. Read
  carelessly that is a third dispatch model; read correctly the merge is BUILD-TIME, so at dispatch each
  Helix mode is one flat map — Vim's depth-1 case again, and in layer terms simply a depth-2 stack collapsed
  early. A disjoint router must either duplicate those 301 bindings or grow a bespoke inheritance feature;
  a layered router expresses it for free. No oracle is required to take this decision: all three facts are
  structural, attested by the census at each pin, and none of them is a behavioural claim.
- **Consequence:** F-003 is restated as the layer router with the Vim profile as its first configuration.
  `crates/core/src/editor.rs` `Mode { Normal, Insert, Visual { line } }` is the shape this replaces — three
  variants for eight namespaces is why `apps/tui/src/input.rs` handles Insert as an `if mode == Mode::Insert`
  early return ahead of a single `Feed::Ignored` fallthrough, i.e. one `closed/ignore` policy standing in for
  five `open` ones. The cost of adopting the layer model rises sharply once F-003 ships a disjoint router,
  which is why the decision is taken now rather than when the Emacs profile is scheduled (F-016, ecosystem).
- **Re-evaluate if:** a profile needs resolution that is not a total order over layers — the known candidate
  is Vim's Lang-Arg, which TRANSLATES a key and re-dispatches rather than binding it (CONCEPT-LANG-ARG,
  still `pending`); if re-dispatch cannot be expressed as a layer that rewrites and yields, the stack needs a
  second mechanism and this decision is incomplete rather than wrong.
- Refs: [parity/concepts/irreconcilable.yaml](parity/concepts/irreconcilable.yaml), [parity/contracts/keymap-layers.yaml](parity/contracts/keymap-layers.yaml), [parity/contracts/vim-style.yaml](parity/contracts/vim-style.yaml), D-043, D-044, F-003, F-016, C-INPUT, C-EDITLANG.

## D-046 — Keymap priority is TWO axes (scope, provenance), not one eight-tier list · decided
- **Decision:** the "priority ABI" of [architecture §1.4](../docs/architecture/architecture.md) is split into
  two independent axes, and `C-INPUT` resolves them in this order:
  1. **Scope** — *which keymaps are consulted at all, and in what order.* This is D-045's ordered layer
     stack, and it absorbs ABI tiers **1–3** (temporary state → active widget/view → buffer-local mode)
     as three ranks of one stack. Old tier 3's V-28 "ordered sub-list, not flat" stops being a special
     case: sub-list members are simply more layers.
  2. **Provenance** — *when two sources bind the same key **within one layer**, whose binding wins.*
     This is ABI tiers **4–8** (workspace → user → plugin-explicit → plugin-suggested → built-in), and it
     stays exactly as D-008 left it: principle locked, exact ordering open.
  A binding therefore carries a `(layer, provenance)` pair. Resolution walks layers by rank (D-045); within
  the layer that binds, provenance decides the winner; `unmatched_key` and `sealed` are properties of the
  layer, never of a provenance tier.
- **Reason:** the one-list form makes the two questions look like one, and that is why the *whole* of D-008
  has been open since it was written. Tiers 1–3 are now attested by three censuses — Neovim's eight
  namespaces, Emacs's nine buffer-selected tiers, Helix's derived mode — and need no plugin to validate.
  Tiers 4–8 are about who *registered* a binding, a question no upstream census can answer and which
  genuinely waits on F-016. Splitting the axes lets the evidence-backed half close now instead of being
  held open by the half that cannot be validated yet. It also removes a live hazard: two models of one
  mechanism existed in the spec — `keymap-layers.yaml` (rank/sealed/unmatched_key) and the priority ABI
  (eight tiers) — neither citing the other, so F-003 would have implemented whichever its author read first.
- **Consequence:** INV-PRIORITY is restated over the two axes rather than the flat tier list; D-008 is
  narrowed to provenance only and remains `open` there. `keymap-layers.yaml` gains the provenance field as
  a declared NON-goal of the layer primitive, so the contract is explicit about the axis it does not own.
  Static conflict detection (INV-PROFILE-ISOLATION) is unchanged and now has a sharper definition of
  "same priority": same layer **and** same provenance tier.
- **Re-evaluate if:** a real profile needs provenance to reorder *layers* rather than bindings within one —
  e.g. a plugin that must install a whole layer above the user's. That would make the axes interdependent
  and this split would need a joining rule rather than two independent orders.
- Refs: D-008 (narrowed), D-045, INV-PRIORITY, INV-PROFILE-ISOLATION,
  [parity/contracts/keymap-layers.yaml](parity/contracts/keymap-layers.yaml),
  [../docs/design/input-engine.md](../docs/design/input-engine.md), F-003, F-016, C-INPUT.

## D-047 — The kernel editing primitive is (selection-set, operation); Vim is its resolve-first case · decided
- **Decision:** the kernel editing-language primitive (`C-EDITLANG`) is a pair **(selection-set, operation)**, not an operator-with-pending-motion state machine. Operator-pending is the window between naming an `operation` and the motion that RESOLVES its selection — a deferred, possibly-invisible selection build, not a flag on one shared state machine (evil-mode ships exactly this). The two grammars are two policies over the one primitive: the **Vim profile** resolves the selection from the motion, applies the operation, and drops the selection; the **selection-first profile** establishes the selection first and PERSISTS it across the operation. `CONCEPT-OP-PENDING` is therefore `unified` on this primitive and is no longer a concept-level blocker.
- **Reason:** F-003 ships in the MVP and `input.rs` must rewire operator-pending; without the primitive fixed, op-pending is re-baked as a flag on the one shared state machine — the exact defect the census (D-043) was built to expose. The primitive is decidable now because it is a DESIGN choice that strictly contains both grammars, and its Vim column is verifiable against the usable nvim oracle.
- **Evidence asymmetry (explicit, not laundered):** the Vim column of the observables OBS-BARE-MOTION and OBS-SELECTION-PERSISTENCE is observed against nvim. The selection-first column is NOT — Helix is `role: reference` (`prd: []`, no compatibility promise), and the observables' own `needs_oracle` fields record that a keymap census cannot encode what a motion does to a live selection. That runtime observation stays open as `HOLE-SELECTION-BEHAVIOUR`; no fixture claims the selection-first column verified. D-047 commits ruse's primitive plus the Vim default on observed evidence and records the selection-first side as documented-only reference.
- **Consequence:** unblocks `C-EDITLANG`, `C-INPUT` (operator-pending) and `F-003` — `input.rs` may rewire operator-pending as a deferred-selection construct rather than a flag. `OBS-DOT-REPEAT-UNIT` is split to `D-EDITLANG-DOT-REPEAT` (still open): the primitive does not require settling repeat/macro semantics. `FAM-EDIT-SELECTION` verification and the Helix oracle remain open (`HOLE-SELECTION-BEHAVIOUR`).
- **Re-evaluate if:** a built Helix oracle observes selection-first behavior the `(selection, operation)` primitive cannot express without a phantom stage — i.e. the primitive turns out NOT to contain both grammars.
- Refs: D-043, D-045, D-046, [parity/concepts/irreconcilable.yaml](parity/concepts/irreconcilable.yaml), [parity/contracts/vim-style.yaml](parity/contracts/vim-style.yaml), C-EDITLANG, C-INPUT, F-003, OBS-BARE-MOTION, OBS-SELECTION-PERSISTENCE.
