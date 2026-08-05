---
doc: rfc
project: ruse
title: "RFC-0003: Plugin API & Lifecycle"
summary: >
  Defines ruse's extension surface as a versioned, language-independent protocol carried over WASM or an
  external process — never a Rust dynamic-library ABI. Specifies the stable/experimental/internal API
  layering and the ≥2-users promotion ladder, the plugin manifest + deny-by-default capability model,
  surface-independent semantic UI, crash/timeout isolation, versioned config schema, the reproducible
  lockfile, deterministic deactivation cleanup, and the client/workspace execution-location distinction.
  Cites D-004/D-009/D-010, ENG-PLUG-001, ENG-PROTO-001.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../../architecture/architecture.md
  - ../../protocols/versioning-and-evolution.md
  - ../../parity/plugin-ecosystem.md
  - ../../invariants/reference-invariants.md
  - ../rejected/RFC-R001-rust-dylib-plugin-abi.md
---

# RFC-0003: Plugin API & Lifecycle

- **Status:** proposed
- **Author(s):** hawking90a@gmail.com
- **Created:** 2026-08-05
- **Decision link:** D-004, D-009, D-010 (see also D-006 command IDs)

## Summary

The ruse extension surface is a **versioned, language-independent protocol** carried over **WASM or an
external process — never a Rust dynamic-library ABI** (D-004, ENG-PLUG-001). Plugins see only handles,
snapshots, commands, events, typed UI models, and capabilities; never internal core types. The surface is
layered **Stable / Experimental / Internal**, and nothing reaches Stable without **≥2 independent users**
plus a migration strategy (D-010, INV-PROMOTION). Plugins declare intent in a **manifest** with
deny-by-default **capabilities**, render through a **surface-independent semantic UI model**, run **isolated**
so a panic or timeout can never kill the editor, ship a **versioned config schema**, are pinned by a
per-workspace **lockfile**, and clean up **deterministically on deactivation**. This RFC does not restate the
wire-evolution rules — it consumes them from
[protocols/versioning-and-evolution.md](../../protocols/versioning-and-evolution.md).

## Motivation / Problem

Neovim's ecosystem is powerful but fragile: plugins reach into internal implementation details, load order
silently decides who wins, one plugin's error can wedge the session, and there is no static contract between
plugin and host (see [architecture.md](../../architecture/architecture.md) §4, §0.2). ruse's stated goal is "a
platform that grows an ecosystem yet **breaks less than Neovim** from day one"
([architecture.md](../../architecture/architecture.md) intro). That is a *foundations* problem, not a feature-count
problem — see the ECO-1…ECO-15 foundation table in
[parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md). This RFC locks the third of the five axes to
lock first ([architecture.md](../../architecture/architecture.md) §12.1): the **Plugin Stable API**. Because the
plugin boundary is hard to reverse once an ecosystem depends on it, it is an RFC-grade decision rather than a
PR.

## Guide-level explanation

From a plugin author's point of view:

1. **You write one semantic command, not per-profile code.** A plugin registers a single namespaced command
   such as `org.example.git.stage`; input profiles wire it to keys (Vim `<leader>gs`, Emacs `C-c g s`, Native
   `Space g s`, palette "Git: Stage Selection"). See [architecture.md](../../architecture/architecture.md) §2.1 and
   [parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md). The command ID is an ecosystem contract
   (D-006); you rename it only via alias + deprecation window.

2. **You ship a manifest.** It declares your commands, the API version range you require, the capabilities
   you need (filesystem/network/process — none granted by default), your config schema, and any views. Nothing
   about your plugin is discoverable only after it runs (ECO-3, INV-TRUST-1).

3. **You target WASM or a process, and you get a stable SDK.** You never link against ruse's Rust types.
   You speak a versioned protocol; the host negotiates a compatible version and hands you handles, not objects
   ([architecture.md](../../architecture/architecture.md) §4.1, §4.3). The same SDK works whether your plugin is
   WASM-hosted in-process or a separate process, and whether it runs on the local client or the remote
   workspace runtime.

4. **You build UI once, semantically.** You emit a tree/list/table/decoration model; the host lowers it to
   TUI today and GUI/Web later. You never print escape sequences or touch cells
   ([architecture.md](../../architecture/architecture.md) §4.4, INV-RENDER-IR).

5. **Your failures are yours.** If your plugin panics, times out, or exceeds its resource budget, the host
   isolates and reports it; the editor keeps running (INV-PLUGIN-ISOLATED).

6. **Your users get reproducibility.** A workspace lockfile pins your version + API + checksum so an update
   never silently breaks a project ([architecture.md](../../architecture/architecture.md) §4.6).

**MVP note (D-009):** the public WASM/process host lands **post-MVP**. The MVP ships an *internal* extension
API that the built-ins (git, search) dogfood; the public surface is extracted only after real built-ins have
validated it (see [parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md) "Dogfooding path"). This RFC
defines the target contract so the internal API is shaped toward it from the start.

## Reference-level explanation

This section states the contract. Wire-evolution mechanics (version header, additive rules, unknown-handling,
deprecation windows, the promotion ladder) are defined once in
[protocols/versioning-and-evolution.md](../../protocols/versioning-and-evolution.md) and are **not** duplicated
here; the Plugin API is one of the protocols that document governs.

### 1. Transport: versioned protocol, never a Rust dylib ABI

The extension transport is a versioned protocol over **WASM or an external process** (D-004,
INV-PROTOCOL-VERSIONED). A Rust dynamic-library ABI as the ecosystem foundation is rejected outright — the
full reasoning lives in the rejected-decision record
[RFC-R001](../rejected/RFC-R001-rust-dylib-plugin-abi.md). The host and plugin negotiate a version at
activation using the shared protocol header (`major.minor` + capability set,
[versioning-and-evolution.md](../../protocols/versioning-and-evolution.md) §Version Header):

- **major** — incompatible; the host may refuse or run a compatibility shim.
- **minor** — additive; old plugins keep working (INV-ADDITIVE).
- **capabilities** — feature negotiation independent of the number.

The plugin declares its requirement as a range, e.g. `api_requirement = ">=1.4, <2.0"`
([architecture.md](../../architecture/architecture.md) §4.3). The host maintains previous-generation support and/or
a shim per the deprecation policy ([architecture.md](../../architecture/architecture.md) §11: ≥2 majors, ≥2-year LTS
window).

### 2. What a plugin can see (contract, not types)

A plugin sees only: command IDs, document handles, view handles, snapshot IDs, transaction *requests*, events,
typed UI models, and capabilities ([architecture.md](../../architecture/architecture.md) §4.1). It never receives
`EditorState`, `Rope`, slotmap entries, undo nodes, or renderer types (INV-PLUGIN-NO-CORE). Queries return
immutable snapshots/DTOs, never live core objects (INV-QUERY-SNAPSHOT). All document mutation is expressed as a
**transaction request** carrying a base revision; the host — not the plugin — applies it through the single
transaction path (INV-TXN, D-001), rejecting stale requests (INV-ASYNC-ORDER). Every mutation a plugin causes
carries origin `Plugin` (INV-ORIGIN).

### 3. API layering + promotion ladder

Three layers, per [architecture.md](../../architecture/architecture.md) §4.2:

| Layer | Audience | Guarantee |
| --- | --- | --- |
| **Stable** | most plugins | long-term compatibility |
| **Experimental** | early adopters | may change between releases (stated) |
| **Internal** | official core + bundled plugins only | not public to external plugins |

Every surface climbs the ladder `Internal → Experimental → Preview → Stable → Deprecated → Removed`
([versioning-and-evolution.md](../../protocols/versioning-and-evolution.md) §API Promotion Ladder). **Promotion
to Stable requires ≥2 independent implementations/plugins to have used it in Preview, plus a written migration
strategy** (D-010, INV-PROMOTION). Each API must be able to express failure, cancellation, and partial success
before it can stabilize. This is the direct antidote to the worst failure mode — stabilizing a wrong
abstraction (D-009, D-010).

### 4. Manifest + capability model

The manifest is the single source of a plugin's declared surface (ECO-3): identity + namespace, `api_requirement`,
commands, contributed views/keymap *suggestions* (per-profile, suggested not forced — ECO-7,
[architecture.md](../../architecture/architecture.md) §1.4), config schema, capabilities, and execution-location
preference (§8). Capabilities are **deny-by-default**: filesystem, network, and process access are not granted
on install (ECO-4, [architecture.md](../../architecture/architecture.md) §4.4, §10; INV-TRUST-1). Merely opening a
workspace executes no plugin code, and a permission change requires explicit re-approval — never silent
application ([architecture.md](../../architecture/architecture.md) §10, INV-TRUST-1). Plugin output is untrusted UI
input: escape-injection is filtered and plugin text is never treated as trusted markup
([architecture.md](../../architecture/architecture.md) §10).

### 5. Semantic (surface-independent) UI

A plugin expresses UI as a **semantic view model** — commands, decorations, tree/list/table views — with one
shared API across TUI/GUI/Web; the plugin API is **not** split per surface
([architecture.md](../../architecture/architecture.md) §4.4, §7; ECO-8). All output is a lowering of a single
semantic Render Tree; no plugin emits backend-specific bytes (INV-RENDER-IR, D-014). Per-redraw decoration
providers are bounded to a visible-range snapshot and run outside the paint critical section
(INV-QUERY-SNAPSHOT).

### 6. Isolation & lifecycle

Plugins load **isolated** so a panic never terminates the editor
([architecture.md](../../architecture/architecture.md) §4.4; INV-PLUGIN-ISOLATED, ENG-PLUG-001). Per-plugin
memory/CPU limits, timeouts, cancellation, and a shutdown model are defined; a plugin panic or timeout never
crosses the FFI/host boundary and its blast radius is bounded (INV-FAIL-BOUNDED). Failures surface as typed,
coded errors at the host boundary and are logged once (INV-ERR-CLASS, D-016). Re-entrant mutation in plugin
event callbacks is prevented, and one handler's failure does not abort the whole dispatch
([architecture.md](../../architecture/architecture.md) §8).

**Deactivation is deterministic cleanup.** On deactivate/unload the host reclaims everything the plugin
registered: commands, tasks, status entries, decorations, views, and persistent data
([parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md) "Clean deactivation"; design-requirements §22).
Plugin state is separated into user-config / regenerable-cache / persistent / session so cleanup and recovery
know what to keep (§23). Background work a plugin started is owned by the central scheduler and cancelled on
deactivation (INV-SCHED-1).

### 7. Config schema (versioned)

A plugin provides a **config schema** (JSON-schema-style: types, enums, defaults) so the editor can offer
autocompletion, type checking, doc generation, deprecated-option warnings, and migration
([architecture.md](../../architecture/architecture.md) §4.5, ECO-5). The config schema is a governed protocol and
evolves additively under the same rules
([versioning-and-evolution.md](../../protocols/versioning-and-evolution.md)); default-value changes are not
applied silently in a minor release.

### 8. Execution location (client / workspace)

Every command and plugin distinguishes execution location — local client vs remote workspace runtime (ECO-9,
[architecture.md](../../architecture/architecture.md) §2.3, §5). Plugins do not arbitrarily choose a remote
command's execution location; remote vs local extensions are distinguished by the host
([architecture.md](../../architecture/architecture.md) §5.1), the client/runtime boundary is a first-class type
distinction (INV-REMOTE-FIRST, D-011), and remote runtime code never runs with local-client permissions
([architecture.md](../../architecture/architecture.md) §10).

### 9. Reproducibility (lockfile)

A per-workspace lockfile pins each plugin's `id`, `version`, `api`, and `checksum`
([architecture.md](../../architecture/architecture.md) §4.6, ECO-10). Two modes: **Rolling** (auto-update within the
compatible range) and **Locked** (exact pins for company/server environments). Update signatures and
marketplace checksums are verified ([architecture.md](../../architecture/architecture.md) §10).

## Reference Invariants

This RFC depends on and enforces (all defined in
[invariants/reference-invariants.md](../../invariants/reference-invariants.md); this RFC mints none):

- **INV-PLUGIN-NO-CORE** — plugins never receive internal types; only handles/snapshots/commands/events/UI
  models/capabilities (§2).
- **INV-PLUGIN-ISOLATED** — a plugin panic/timeout never terminates the editor and never crosses the host
  boundary (§6).
- **INV-PROTOCOL-VERSIONED** — the surface is a versioned WASM/process protocol, never a Rust dylib ABI; API,
  command IDs, config schema, and profiles are all versioned (§1, §3, §7).
- **INV-PROMOTION** — no API reaches Stable without ≥2 independent users and a migration strategy (§3).
- **INV-CONTRACT-FIRST** — the contract is defined independently of Rust types; changing an internal type is
  not, by itself, an API change (§1, §2).
- **INV-ADDITIVE** — compatible evolution is additive; readers handle unknown variants/fields/capabilities
  gracefully; breaking changes require a major bump (§1, §7).

Supporting invariants relied on: INV-CMD-SEMANTIC, INV-QUERY-SNAPSHOT, INV-RENDER-IR, INV-TXN, INV-ORIGIN,
INV-TRUST-1, INV-REMOTE-FIRST, INV-FAIL-BOUNDED, INV-ERR-CLASS, INV-SCHED-1. Governing policies: **ENG-PLUG-001**
(isolation over a versioned protocol) and **ENG-PROTO-001** (additive, contract-first evolution).

## Failure modes & Recovery

- **Plugin panics / infinite loop / resource-budget breach** → host isolates, reports a typed error, and (for
  repeated failures) quarantines the plugin; editor stays up (INV-PLUGIN-ISOLATED, INV-FAIL-BOUNDED).
- **Version negotiation fails** → host refuses to activate or engages a compatibility shim; the plugin is
  marked incompatible in the UI, never silently half-loaded.
- **Stale transaction request** (base revision behind current) → rejected by the host, not applied
  (INV-ASYNC-ORDER, INV-TXN).
- **Deactivation mid-work** → scheduler cancels the plugin's background tasks; registered
  commands/views/decorations/persistent data are reclaimed (INV-SCHED-1; §6).
- **Corrupt/abandoned plugin** → governance quarantine + lockfile pinning let a workspace stay on a known-good
  version ([parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md) Governance; §9).

## Security impact

Deny-by-default capabilities, workspace trust (no execution before a trust decision), explicit re-approval on
permission change, untrusted plugin output, verified update signatures/checksums, and remote-vs-local
permission separation (§4, §8; [architecture.md](../../architecture/architecture.md) §10; INV-TRUST-1). AI-agent
plugins get no default full-filesystem access and their changes are reviewed before apply (INV-ORIGIN,
[architecture.md](../../architecture/architecture.md) §10).

## Performance impact

Isolation adds a marshalling boundary. Mitigations from [architecture.md](../../architecture/architecture.md) §9:
plugin IPC must not be chatty (no per-cell RPC); queries hand back bounded snapshots (INV-QUERY-SNAPSHOT); the
central scheduler coalesces duplicate per-document parse/index work and keeps input+render ahead of plugin
background work (INV-SCHED-1). WASM in-process avoids per-call process-boundary cost; process isolation is
reserved for plugins that need it. Activation latency is a tracked quality metric
([parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md) Governance).

## Compatibility & Migration

Evolution follows [versioning-and-evolution.md](../../protocols/versioning-and-evolution.md) verbatim:
additive minors, capability negotiation, alias + `deprecated_since`/`remove_after` on renames, migration tools
for large changes, ≥2-major deprecation windows ([architecture.md](../../architecture/architecture.md) §11, ECO-12).
A **plugin-compatibility CI** runs representative plugins (git, file tree, LSP, DAP, theme, remote provider,
media viewer) against old-SDK fixtures on every core-change PR (ECO-11,
[architecture.md](../../architecture/architecture.md) §11). MVP→post-MVP path per D-009: internal API first, extract
the validated public surface before/after 1.0.

## Observability

Per-plugin health is a per-component state machine feeding the aggregate Health Registry (INV-STATUS, D-016);
crash rate, activation latency, and API-compatibility are governance metrics
([parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md)). Plugin-caused mutations are attributable via
origin `Plugin` (INV-ORIGIN). Errors are typed, coded, and logged once at the host boundary (INV-ERR-CLASS).

## Alternatives

1. **Rust dynamic-library ABI** — rejected; see [RFC-R001](../rejected/RFC-R001-rust-dylib-plugin-abi.md) and
   §Rejected approaches.
2. **Embedded script runtime (Lua/Vimscript), Neovim-style** — high L3 compat but reintroduces
   internal-detail coupling and a second-language maintenance burden; L3 (running Vimscript/Elisp) is an
   explicit non-goal (D-007). WASM lets authors use any language compiling to it, keeping the SDK
   language-independent.
3. **The Rust `serde` struct *is* the contract** — rejected in
   [versioning-and-evolution.md](../../protocols/versioning-and-evolution.md); ties the wire format to Rust
   internals and violates INV-CONTRACT-FIRST.
4. **Public WASM host in the MVP** — rejected as premature; stabilizing an unproven surface is the top risk.
   Internal-API-first per D-009.
5. **"We will never remove an API"** — rejected; freezes bad abstractions and bloats the core path. Bounded
   deprecation windows instead ([versioning-and-evolution.md](../../protocols/versioning-and-evolution.md)).
6. **Single global priority number for plugin composition** — rejected; overlap needs composition rules
   (diagnostics merge, one formatter, completion aggregate, decoration layers), not one integer
   ([parity/plugin-ecosystem.md](../../parity/plugin-ecosystem.md); design-requirements §21).

## Rejected approaches

**A Rust dynamic-library (`.so`/`.dylib`/`.dll`) plugin ABI** is rejected and recorded so it is not
re-litigated. In short: Rust has no stable ABI, so plugins would be coupled to the exact compiler and crate
versions the host was built with; there is no crash isolation (a plugin fault is a host fault); and it forces a
Rust-only SDK. Full record with the re-evaluation condition:
[RFC-R001 — Rust dylib plugin ABI](../rejected/RFC-R001-rust-dylib-plugin-abi.md) (D-004).

Also rejected inline (details above): serde-struct-as-contract, never-remove APIs, MVP WASM host, and a single
priority number.

## Trade-offs

- **Upfront overhead vs. long-term stability.** A schema + header + promotion process + isolation boundary is
  real cost; accepted as the mechanism that lets ruse "grow the ecosystem yet break less than Neovim"
  ([versioning-and-evolution.md](../../protocols/versioning-and-evolution.md) Trade-offs).
- **Isolation marshalling cost vs. safety.** A boundary costs cycles; bought back with coarse IPC, snapshots,
  and scheduler coalescing (§Performance).
- **Slower stabilization (≥2 users) vs. avoiding a wrong Stable abstraction.** Deliberately slow; the wrong
  frozen abstraction is the more expensive mistake (D-010).
- **Language-independent protocol vs. deep L3 script compat.** ruse gives up running Vimscript/Elisp (D-007)
  to keep the SDK any-language and the boundary clean.

## Re-evaluation conditions

- The **transport** decision re-opens only under RFC-R001's condition: a stable native-component ABI *with*
  crash isolation becomes established (D-004). Absent that, do not re-litigate.
- **API-layer specifics** and composition rules firm up as real plugins (F-016) and the WASM host land
  post-MVP (D-009); Stable promotions gate on ENG-PROTO-001 / INV-PROMOTION.
- **Priority/composition tiers** for keymaps remain provisional until an input engine (F-003) and real plugins
  exist (D-008); this RFC does not lock them.

## Open questions

- Exact resource-budget defaults (memory/CPU/timeout) per plugin — tune on real workloads once F-016 exists
  (cf. D-018 scheduler budgets still open).
- WASM component-model vs. a custom framed protocol as the concrete in-process transport.
- Marketplace signing/verification-level mechanics (ECO-13) and SDK conformance test kit (ECO-15) — post-MVP.
- Precise composition semantics where plugins overlap (diagnostics/formatter/completion/decoration) —
  design-requirements §21.
