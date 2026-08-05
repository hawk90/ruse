---
doc: delivery-and-dependencies
project: ruse
title: "ruse Delivery Tiers & Implementation-Source Policy"
summary: >
  Two independent classification axes that must NOT be collapsed into each other or into the architecture
  layers: the product/delivery axis (Base / Official Pack / Third-party) and the implementation-source axis
  (Own / Wrapped / Direct / External-tool). Defines the classification questions, dependency tiers + budget,
  feature-flag policy, own-vs-dependency criteria, and evolution. The per-capability and per-dependency DATA
  lives in spec/capabilities.yaml and spec/dependencies.yaml. awesome-neovim is used as a feature inventory
  and boundary-validation source, not copied as an architecture.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../../spec/capabilities.yaml
  - ../../spec/dependencies.yaml
  - architecture.md
  - remote-runtime.md
  - ../../spec/DECISIONS.md
---

# ruse Delivery Tiers & Implementation-Source Policy

Communities like `awesome-neovim` list LSP, completion, file-explorer, terminal, debug, test, git, remote,
UI, AI, database, games — all as *plugins*. That is a great **feature inventory and boundary-validation
checklist**, but it does **not** mean everything must be an external plugin in ruse. Classify features along
**two independent axes** (D-033), and do not use either as the code architecture (Kernel / Built-in Service /
Bundled Extension / External Plugin — that is `architecture.md`).

## Axis 1 — Product / delivery (Base / Official Pack / Third-party)

| Tier | Meaning |
| --- | --- |
| **Base** | Always in the default install; the product is responsible for it. Complete local+remote editing with zero plugins. |
| **Official Pack** | Officially maintained + compat-tested, activated on demand (per language/workspace/command); excluded in safe mode. (This is the "ad-hoc" idea — but named a **Pack**, not "ad hoc".) |
| **Third-party** | Independent ecosystem: provider/domain-variable, optional, alternative UX, higher-risk permissions. |

**Decompose per feature (engine / UI / adapter), don't drop a whole feature into one cell.** e.g. File Tree:
remote filesystem service + watcher + ignore model = **Base Service**; minimal tree UI = **Base bundled
extension**; Dired/Oil-style editing = **Official Pack**; an S3/database explorer = **Third-party**.

Classification questions (full data in [spec/capabilities.yaml](../../spec/capabilities.yaml)):
- **→ Base** if: users expect it without plugins · needs consistent local+remote meaning · is a shared
  engine/state/resource · sits near input/save/recovery/security invariants · its failure shakes whole-product
  trust · duplicating it wastes CPU/memory/processes.
- **→ Official Pack** if: a good official default is needed · not everyone uses it · activates per
  language/tool/workspace · worth compat-testing with core · too quality-critical to leave to third parties.
- **→ Third-party** if: provider/language/domain-variable · multiple competing impls are meaningful ·
  independent updates matter · needs high-risk permissions or external accounts · only a subset of users need it.

**Base always provides a decent minimum; alternative UX stays replaceable** (`replaceable: true`) so users can
swap file-explorer / statusline / completion-UI / finder / git-UI / motion / profile / theme without forking
the product.

## Axis 2 — Implementation source (Own / Wrapped / Direct / External-tool)

Where the code *comes from and how controlled it is* — orthogonal to Axis 1. A single Base capability can be
Own engine + Wrapped storage + External-tool search. (Data: [spec/dependencies.yaml](../../spec/dependencies.yaml).)

| Source | Use when | Rule |
| --- | --- | --- |
| **Own** | project-defined semantics/invariants; long-term contract; near data integrity; must survive a language port | We own the core **types and contracts** even if a crate backs the storage. |
| **Wrapped** | external crate behind a thin Adapter/Facade (**the default** for infra) | No external type in the public domain API; swappable; platform/error isolation. Don't wrap trivial deps. |
| **Direct** | de-facto standard leaf utility, no state/lifecycle ownership | Only if external types never reach the public domain API and there's no reason to swap. |
| **External-tool** | a mature independent tool ecosystem exists | Run as a **versioned process** under the supervisor (git CLI, ripgrep, LSP servers, gdb, formatters) — isolates crashes/lifecycle/licensing/binary-size. |

**Own** (never let a crate decide the vocabulary): Document/View · Transaction/Undo · Command · Workspace/
Resource identity · Remote protocol · Plugin capability model · Status/Health · Scheduler policy · Render
semantic model · input Profile semantics.

### Own-vs-dependency criteria
- **Own** when: project-unique semantics · we must own the long-term spec · touches core invariants/integrity
  · external types would spread through the architecture · impl can change but meaning must persist · needs
  precise error/recovery/observability control · the concept must survive a future language port.
- **Dependency** when: a well-solved general problem · standard/protocol is costly to implement · self-rolling
  is a **security risk** · complex cross-platform differences · not our differentiator · a well-tested impl
  exists · it hides behind an Adapter.
- **"It's small, so implement it" is a dangerous heuristic.** UTF-8 parsing, shell escaping, terminal escape
  parsing, glob matching, path normalization, atomic file replace, SSH, cryptographic hashing all look short
  but are edge-case/security minefields — prefer vetted crates. Conversely, Command registry, Transaction,
  plugin policy, scheduler policy, workspace identity have crates but rarely match our semantics — keep Own.

## Dependency tiers & review (D-034)

Not all crates are equal — tier drives review depth (data: `spec/dependencies.yaml`):

```
Tier 0 tooling-only (tests/benches/codegen)   Tier 1 leaf utility (no core state)
Tier 2 infrastructure (terminal/watcher/serde/PTY)   Tier 3 critical path (text storage/parser/scheduler/runtime)
Tier 4 trust boundary (SSH/crypto/plugin runtime/network)
```

Higher tier ⇒ review maintenance status · security history · transitive count · unsafe surface · platform
support · MSRV · license · binary size · startup/runtime cost · cancellation support · API stability · bus
factor · replacement availability.

### Dependency budget — by cost type, not crate count
compile-time · binary-size · startup · runtime allocation · background threads · transitive deps · native
system deps · unsafe surface · supply-chain risk. One native library can cost more than 20 small crates. **A
dep that spins its own thread pool or global runtime is flagged** — it can fight the scheduler policy.

### Feature-flag policy
Do **not** split every feature into a Cargo feature (dependency + CI combinatorial explosion). Cargo features
are for **big platform/distribution differences only** (`core`, `terminal`, `remote`; large opt-ins like
`language-services`, `debug`, `git-integration`). Per-user feature enablement is **runtime activation**
(`activation:` in capabilities.yaml), not a compile flag.

## Evolution — source is not permanent
`external crate (validate fast) → stabilize Adapter boundary → find perf/compat/maintenance evidence → switch
to Own or another dep`. And Own↔dep can reverse if maintenance cost is high. The goal is **not** to over-build
replaceable boundaries everywhere, but to **stop external types from spreading through the domain**.

## CI / PR governance
- On `Cargo.lock` change: generate a dependency report; a new dep requires a justification (see PR template
  below); forbid banned crates in `core`; detect duplicate-role crates; feature-combination check; license +
  security-advisory (`cargo-audit`) + unsafe/binary-size/compile-time deltas. Wire into
  [ci-cd-and-release.md](../operations/ci-cd-and-release.md) §11.
- PR note for a new dependency:
  ```
  New dependency: X   Purpose: …   Why not implement: …   Tier: N
  Exposure: wrapped, no public types   Runtime impact: no threads/global state
  Exit strategy: replace adapter implementation
  ```

## Combined example (both axes)

| Capability | Product | Impl source |
| --- | --- | --- |
| Document / Transaction / Command | Base kernel | **Own** |
| Rope storage · terminal backend · watcher · LSP codec | Base internal/service | **Wrapped** |
| File tree UI · search UI · LSP coordinator · debug model | Base / Official | **Own** |
| Search exec · rust-analyzer · gdb · git | Base/Pack service | **External-tool** (some wrapped) |
| Git UI · debug UI | Official Pack | **Own** |
| AI / GitHub / DB provider | Third-party | **External API/SDK** |

## Reference Invariants
- **INV-PLUGIN-NO-CORE / INV-CONTRACT-FIRST** — external types never define the public domain vocabulary;
  wrap them behind Adapters. **INV-CAP-DEGRADE** — missing external tools degrade, not fail.

## Alternatives / Rejected / Trade-offs
- **Rejected: use the awesome-neovim plugin taxonomy as the architecture** (everything a plugin). Loses the
  shared Base engines and consistent local+remote semantics.
- **Rejected: "Base feature ⇒ implement everything ourselves."** Reinventing crypto/unicode/escape-parsing is
  a security/quality risk.
- **Rejected: name the middle tier "ad hoc."** Sounds temporary; use **Official Pack / Feature Pack / Optional
  Built-in**.
- **Rejected: a Cargo feature per sub-feature.** Combinatorial CI/dependency blow-up; use runtime activation.
- **Trade-off:** two axes + inventories are more bookkeeping. Accepted: they prevent the wrong conclusions
  ("basic ⇒ all self-built", "plugin list ⇒ architecture") and are machine-checkable.
