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

## D-004 — Plugins use WASM/process protocol, not a Rust dylib ABI · decided
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

## D-008 — Profile > plugin keymap priority · decided (principle) / open (exact tiers)
- **Decision (principle, locked):** profiles are isolated (never share a key space); user overrides beat
  plugin bindings; plugins cannot force global keys; real conflicts are detected statically.
- **Open (do not lock yet):** the exact 8-tier ordering (esp. plugin-explicit vs plugin-suggested) — those
  tiers cannot be validated until an input engine and real plugins exist. Locking them now would violate
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

## D-011 — Client ⁄ Workspace-runtime boundary is first-class · decided
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

## D-014 — Semantic View vs Render IR boundary · decided
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
