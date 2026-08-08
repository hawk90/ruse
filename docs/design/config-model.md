---
doc: config-model
project: ruse
title: "ruse Configuration Model (C-CONFIG)"
summary: >
  Closes the CFG gap: the layered configuration model for C-CONFIG. Defines the three scopes
  (user / workspace / machine-local) and their precedence, per-type merge rules
  (replace | append | set-union | deep-merge), source provenance and the `:inspect config` command,
  the security-locked settings a workspace can never override, a machine-managed schema that drives
  autocomplete / type-check / deprecation / migration, per-plugin config schemas, keymap layering,
  safe mode, and workspace-trust gating. The machine-managed schema lives in spec/config-schema.yaml.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md
  - ../architecture/design-requirements.md
  - delivery-and-dependencies.md
  - stability-and-observability.md
  - ../invariants/reference-invariants.md
  - ../../spec/config-schema.yaml
  - ../../spec/POLICY.yaml
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Configuration Model (C-CONFIG)

> This is the design for component **C-CONFIG** (`Config/keymap loading + merge`, kernel layer). It
> makes the `CFG` requirements in [design-requirements.md §10](../architecture/design-requirements.md#10-config-profile-feature-pack-cfg)
> concrete. State (per-setting types/defaults/scope/lock) lives in the machine-managed schema
> [`spec/config-schema.yaml`](../../spec/config-schema.yaml); this doc is the prose contract.

## Problem

ruse has no configuration model. `CFG` (design-requirements §10) requires separated user / workspace /
machine-local settings, per-type merge, source provenance, a `:inspect config` command, safe mode,
migration, and — the load-bearing security rule — **security-sensitive settings that a workspace cannot
override** ([architecture §10](../architecture/architecture.md#10-security-trust-model): "Project settings
do not arbitrarily override user settings"). Neovim's failure mode is the anti-model: config is arbitrary
Lua, so a value's origin is unknowable, one bad line aborts startup, a project `exrc` can silently change
security behavior, and there is no static type/deprecation/migration story. C-CONFIG must be the opposite:
a typed, layered, provenance-tracked model where **one bad key never prevents startup** and a workspace can
never touch a security boundary.

## Goals

- Three scopes with a fixed, documented **precedence** (CFG-LAYER-001).
- **Per-type merge** — replace | append | set-union | deep-merge — chosen by the schema, not the caller
  (CFG-MERGE-001).
- **Source provenance** for every effective value (and every collection element), surfaced by
  `:inspect config <key>` (CFG-PROV-001).
- **Security-locked** settings a workspace can never override — executables, trust, forwarding, unsigned
  plugins (CFG-LOCK-001); enforced, not documented.
- A **machine-managed schema** (types/defaults/enums/deprecation/version) that drives autocomplete,
  type-check, deprecation warnings, and migration.
- **Per-plugin config schema**: plugins declare their settings and get the same treatment.
- **Safe mode**: a bad config still boots (CFG-SAFE-001); one bad key degrades to a lower layer / default.
- **Migration** that separates auto-conversion from manual-warning (CFG-MIG-001).
- Workspace-trust gates workspace config: **untrusted workspace config is inert**.

## Non-goals

- Not the keymap **resolution** algorithm or the priority ABI itself — that is
  [architecture §1.2–§1.4](../architecture/architecture.md#1-input-philosophy-three-official-profiles)
  (INV-PRIORITY / INV-PROFILE-ISOLATION). This doc only covers how keymap *config layers* stack (§7).
- Not the plugin capability/permission grant flow (that is architecture §4.4 / §10); C-CONFIG only stores
  the declarative settings, never the trust decision.
- Not the config file **surface syntax**. The on-disk format (TOML/RON/…) is implementation-defined; this
  doc specifies the model, and the authored schema is YAML. The wire model is a typed key→value map.
- Not profiles-as-behavior-policy beyond the `input.profile` selector (bounded-scope profiles are CFG §10 /
  architecture §11.1, tracked separately).

## Terminology

Glossary terms (Document/View/Workspace/Client) are in [spec/glossary.yaml](../../spec/glossary.yaml). Local
terms:

- **Layer** — one physical source of settings (a schema-default set, or one file at a scope). Layers stack.
- **Scope** — the *class* of a layer: `machine` | `user` | `workspace`. A key's `scope` field is the
  broadest scope permitted to write it.
- **Effective value** — the single value a key resolves to after merge, with its provenance.
- **Security-locked** — a key a workspace layer may never contribute to (CFG-LOCK-001).
- **Inert layer** — a layer that is parsed and visible in `:inspect` but excluded from merge (e.g. an
  untrusted workspace, or a quarantined malformed file).

## Invariants

C-CONFIG depends on and enforces (registry: [reference-invariants.md](../invariants/reference-invariants.md)):

- **INV-TRUST-1** — no code runs before a workspace-trust decision; principals carry distinct trust; side
  effects are capability-gated. C-CONFIG is the enforcement point for the "workspace repo" principal:
  security-locked keys and untrusted-workspace inertness (CFG-LOCK-001, §5).
- **INV-FAIL-BOUNDED** — an external/lower-trust failure degrades, it does not abort. A bad key or malformed
  layer degrades to the next layer / default; safe mode is the escalation (CFG-SAFE-001, §8).
- **INV-PROTOCOL-VERSIONED** — config schema is versioned with deprecation windows; migration is part of the
  contract (§9).
- **INV-CONTRACT-FIRST / INV-ADDITIVE** — the schema is the contract; schema evolution is additive; readers
  handle unknown keys gracefully (unknown key ⇒ warn + ignore, never abort).
- **INV-PRIORITY / INV-PROFILE-ISOLATION** — keymap config layers feed the priority ABI; they do not merge
  across profiles (§7).
- **INV-STATUS / INV-ORIGIN** — the config layer publishes a health status; every effective value carries an
  origin (its provenance), mirroring the mutation-origin discipline (§10).

This doc introduces no `INV-*` IDs (minting is reserved to the registry). Two candidates —
`INV-CONFIG-LOCK` and `INV-CONFIG-PROVENANCE` — are proposed in Open questions.

## Proposed design

### CFG-LAYER-001 — Three scopes and precedence

Layers stack lowest→highest; for a `replace` key the **highest layer that is allowed to write it wins**.
Collection merges (§ CFG-MERGE-001) fold in the same order.

| # | Layer | Typical location | Trust principal | Writes keys with `scope` … |
|---|-------|------------------|-----------------|-----------------------------|
| 0 | **Schema defaults** | `spec/config-schema.yaml` (built in) | core | all (the `default` field) |
| 1 | **Machine-local** | system/admin (`/etc/ruse`, install dir) | core / admin | `machine`, `user`, `workspace` |
| 2 | **User** | per-user (`~/.config/ruse`) | user | `user`, `workspace` |
| 3 | **Workspace** | project (`.ruse/` in workspace root) | workspace repo | `workspace` **only**, and only when trusted |

Rules:

- A key's `scope` field is the **broadest** scope permitted to author it. A layer whose scope is *broader*
  than the key's `scope` is **inert for that key** (parsed, shown in `:inspect`, excluded from merge). So a
  `scope: user` key set in a workspace layer is dropped with a diagnostic; a `scope: machine` key set in a
  user layer is dropped.
- **Machine-local can pin.** A machine-local layer value for a `security_locked` or `scope: machine` key
  sits above user and workspace and cannot be overridden by them — this is the enterprise/admin lock. (For
  non-locked keys, layer 3 > 2 > 1 as usual: a project may set `editor.tab_width`, a user overrides the
  machine default.)
- **Precedence is per-key, provenance-tracked**, never "last file loaded wins" globally.

The layering is orthogonal to the two delivery/impl axes in
[delivery-and-dependencies.md](delivery-and-dependencies.md); config is the **runtime-activation** substrate
those docs point at — enabling a feature is a config value (`activation:` in
[capabilities.yaml](../../spec/capabilities.yaml)), **not a Cargo feature** (feature-flag policy, delivery
doc §Feature-flag policy).

### CFG-MERGE-001 — Per-type merge rules

The **schema** fixes each key's merge strategy; callers cannot choose. Four strategies:

| `merge` | Applies to | Fold behavior (low→high) |
|---------|-----------|---------------------------|
| **replace** | scalars (bool/int/string/enum) | higher layer's value wins outright |
| **append** | ordered lists | concatenate lower then higher, preserving order + duplicates (e.g. `files.exclude`) |
| **set-union** | unordered sets | union with de-dup; order canonicalized (e.g. trusted-host / allowed-executable lists) |
| **deep-merge** | maps | recurse per key; leaves resolve by their own strategy (default replace); missing keys inherit lower layer (e.g. `ui.*` overrides, per-plugin config maps, keymap tables' *values*) |

- Merge strategy is a **property of the key's type**, recorded in the schema, so behavior is identical
  across every layer and every consumer.
- A layer may **remove** a lower-layer contribution only through an explicit negation form
  (e.g. a `!pattern` prefix for `append`/`set-union` lists); silent shadowing of list elements is not
  supported — this keeps provenance well-defined.
- `deep-merge` never merges across a `security_locked` boundary: a locked leaf inside an otherwise
  workspace-writable map is still dropped from the workspace layer.

### CFG-PROV-001 — Source provenance + `:inspect config`

Every effective value carries **where it came from**. For scalars: the winning layer, file path, and
location. For collections: provenance is **per element / per map-key** (which layer contributed each list
item or map entry), because `append`/`set-union`/`deep-merge` blend layers.

`:inspect config <key>` prints the effective value and the **full layer stack**, including inert layers and
the reason they are inert:

```
:inspect config editor.tab_width
editor.tab_width = 2            (int, merge=replace, scope=workspace)
  effective  ← workspace   .ruse/config.toml:3
  overrides  user          ~/.config/ruse/config.toml:11   = 4
  overrides  machine       (unset)
  default    schema        = 4

:inspect config plugins.allow_unsigned
plugins.allow_unsigned = false  (bool, scope=machine, security_locked)
  effective  ← schema default = false
  IGNORED    workspace   .ruse/config.toml:8  = true   (security-locked; workspace cannot override)

:inspect config files.exclude
files.exclude = [".git", "node_modules", "target"]   (list, merge=append)
  ".git"          ← machine
  "node_modules"  ← user
  "target"        ← workspace   .ruse/config.toml:5
```

`:inspect config` with no key lists every non-default value and its origin; a `--json` form emits the same
provenance ledger for tooling. Provenance is retained in memory for the lifetime of the session so the
answer is always exact, not reconstructed.

### CFG-LOCK-001 — Security-locked settings a workspace cannot override

This is the security spine. A `security_locked: true` key (schema) may **never** receive a value from a
workspace layer, regardless of workspace trust. Enforcement is at load time, before merge:

1. Parse the workspace layer.
2. For each key it sets: if `security_locked` **or** the key's `scope` is narrower than `workspace`, **drop
   the entry** and emit a `config.locked_key_ignored` diagnostic naming the key and its source location.
3. Merge only the surviving entries.

Security-locked keys cover the three categories called out by `CFG`/architecture §10 — **executables**
(which binaries may run, formatter/tool paths), **trust** (unsigned-plugin allowance, remote trust
prompting), and **forwarding** (credential/port forwarding). In the schema these are, at minimum:
`plugins.allow_unsigned` (scope machine), `remote.trust_prompt` (scope user), plus the trust/forwarding
keys owned by C-REMOTE. A workspace-authored keymap may likewise never bind a security-gated command to a
key (§7). This directly discharges INV-TRUST-1 for the "workspace repo" principal and POLICY
[`ENG-TRUST-001`](../../spec/POLICY.yaml).

A plugin cannot escalate its own settings to security-locked: the host validates that a plugin schema only
marks keys `security_locked` within a bounded allowlist (network/credential/exec-touching), and never
un-locks a core-locked key.

### Deriving config keys from parity — depth-first, never bulk

Config keys are **concrete commitments** (a type, a default, a scope, a merge strategy, security semantics),
so they are derived from the parity catalog **depth-first**: a `:set`-style option becomes a `config-schema`
key **only once its behavior is analyzed and shipped/designed** — never bulk-materialized from the option
*prose* in the parity docs (`docs/parity/vim.md` lists e.g. `ignorecase smartcase incsearch hlsearch wrapscan
gdefault` as a checklist, not as typed keys). Materializing a typed key with a guessed default from an
un-analyzed option is exactly the "shaky derivation" the breadth→depth split guards against. The key arrives
**with its feature's design**, alongside the runtime that honors it. Example: `editor.scrolloff` exists
because the viewport behavior it configures shipped (its margin was a constant); the Vim search options do
**not** yet have keys because that search behavior is not built. So `config-schema.yaml` grows one analyzed
option at a time, and completeness-vs-parity is a *per-feature* obligation, not a one-time backfill.

### CFG schema — types/defaults/enums driving tooling

The machine-managed schema ([`spec/config-schema.yaml`](../../spec/config-schema.yaml)) is the single source
for every core setting. Each entry declares `type`, `default`, `scope`, `merge`, `security_locked`,
optional `enum`/`min`/`max`, and a one-line `desc`. From it C-CONFIG derives, with **no manual upkeep**:

- **Autocomplete** — key names + enum values + type hints in config files and the command line.
- **Type-check** — a value of the wrong type / out-of-range int / non-enum string is rejected *for that key*
  (falls back per §8), not accepted silently.
- **Deprecation** — a key marked `deprecated` (optionally `replaced_by`) warns and, where lossless, aliases
  to its replacement for a deprecation window (INV-ADDITIVE).
- **Migration** — the schema `version` drives §9.

The schema is authored (records-with-fields ⇒ YAML, per [D-021](../../spec/DECISIONS.md#d-021)); a generator
may emit a JSON copy for editor tooling but the YAML is canonical and hand-edited.

### CFG-PLUGIN — Per-plugin config schema

Following [architecture §4.5](../architecture/architecture.md#4-plugin-stable-api), a plugin declares its
settings in its manifest, using the **same field set** as the core schema:

```json
{ "configuration": {
    "plugin.org.example.git.sign_commits": { "type": "bool", "default": false, "scope": "workspace", "merge": "replace", "desc": "GPG-sign commits" },
    "plugin.org.example.git.diff_algorithm": { "type": "enum", "enum": ["myers","minimal","histogram"], "default": "histogram", "scope": "workspace", "merge": "replace", "desc": "Diff algorithm" }
} }
```

- Plugin keys are **namespaced** under `plugin.<plugin-id>.*` (mirrors the command-ID namespace ABI,
  architecture §2.2); a plugin may only declare keys under its own namespace.
- Plugin schemas register into the same layered model and get identical autocomplete / type-check /
  deprecation / migration and provenance.
- Plugin config schema is **versioned** with the plugin (INV-PROTOCOL-VERSIONED); a plugin migration table
  is scoped to its namespace.
- A plugin schema is only consulted while the plugin is activated; its keys are otherwise stored as
  opaque-but-preserved (unknown-key rule: warn-and-keep, never drop user data), so deactivating a plugin
  does not lose its config.

### Keymap config (§7)

Keymaps are configuration, but they are **not** scalar-merged. Keymap config layers **append** into the key
resolver, which then applies the two **priority axes** (architecture §1.4, D-046) and static conflict
detection (INV-PRIORITY). Note the collision of the word *layer*: a config layer is a source of settings
(default → user → workspace), while a **keymap layer** (D-045) is a scope in the resolver's stack. A config
layer contributes bindings *into* keymap layers at its own provenance rank; it is not itself one. Two consequences for the config model:

- Cross-profile keymaps never merge or conflict (INV-PROFILE-ISOLATION): a workspace Vim binding and a user
  Emacs binding are in different key spaces.
- A workspace keymap config layer enters at the **workspace provenance tier** (the old ABI tier 4) and only
  when the workspace is trusted; it can never register a global binding above the user tier, and can never
  bind a security-gated command (CFG-LOCK-001). Plugin-suggested bindings remain the lowest, opt-in tier.
  Provenance never lets a config layer reorder *keymap layers* — only bindings within one (D-046).

The keymap *values* (the map of `key → command + when`) deep-merge; the *resolution* is out of scope here.

## Failure modes

The governing rule: **one bad key must not prevent startup** (CFG-SAFE-001 / INV-FAIL-BOUNDED). Each failure
degrades locally.

| Failure | Handling |
|---------|----------|
| Unknown key | Warn (`config.unknown_key`), keep the value opaque (may belong to a not-yet-active plugin), do not abort. |
| Wrong type / out-of-range / non-enum | Reject **that key only**; fall back to the next lower layer's value, else the schema default; diagnostic `config.invalid_value`. |
| Malformed layer (parse error) | **Quarantine the whole layer** (mark inert), keep lower layers; diagnostic `config.layer_quarantined` with the parse location. |
| Workspace sets a security-locked / out-of-scope key | Drop the entry, `config.locked_key_ignored` (CFG-LOCK-001). |
| Untrusted workspace | Entire workspace layer inert until trust granted; `:inspect` shows it as `untrusted`. |
| Schema version older than file | Migrate (§9); unmigratable keys kept inert with a manual-warning. |
| Plugin schema conflict / bad plugin config | Isolated to that plugin's namespace; the plugin degrades, the editor does not (INV-PLUGIN-ISOLATED). |
| Repeated boot failure attributable to config | Auto-escalate to **safe mode** (below). |

**Safe mode** (CFG-SAFE-001) — two triggers: explicit `--safe-mode`, or automatic after a detected
config-attributable crash loop. In safe mode C-CONFIG ignores the user **and** workspace layers entirely and
boots on **schema defaults + machine-local only**, with plugins disabled (matching the "excluded in safe
mode" delivery contract). Safe mode is distinct from per-key degradation: a single bad key does **not** enter
safe mode — it just falls back. The status line shows `Config: safe-mode` (INV-STATUS) and `:inspect config`
still works so the user can find the offending key.

## Recovery behavior

- Per-key fallback and layer quarantine are automatic and non-destructive: C-CONFIG **never rewrites a user
  file to "fix" it**. Bad input is preserved on disk; the in-memory model degrades.
- `:inspect config` (and its `--json` form) is the recovery tool: it names the ignored/quarantined/migrated
  keys and their locations so the user can repair the source.
- Leaving safe mode is an explicit user action after repair; there is no silent auto-heal that could mask a
  persistent problem.
- Migration writes (§9) are opt-in and atomic (write-new-then-replace), never in place, and keep a backup of
  the pre-migration file.

## Security impact

- **INV-TRUST-1 is enforced here** for the workspace-repo principal: security-locked keys (executables,
  trust, forwarding, `plugins.allow_unsigned`, `remote.trust_prompt`) are never workspace-overridable, and an
  **untrusted workspace's config is fully inert** — opening a repo cannot change security behavior. This is
  the config half of architecture §10 ("project settings do not override user settings").
- Machine-local **lock** provides enterprise policy that user/workspace cannot loosen.
- Provenance makes every security-relevant value **auditable** — `:inspect config` shows exactly which layer
  set (or tried and failed to set) a trust/executable/forwarding key.
- Secret-bearing values are never logged and never printed in full by `:inspect` (redaction, per
  [stability-and-observability.md](stability-and-observability.md)); the schema marks such keys `secret`.
- A plugin cannot self-escalate a key to security-locked, nor un-lock a core key (§CFG-PLUGIN).

## Performance impact

- Config is loaded and merged **once at startup** (and on explicit reload), not per command; the effective
  map is an immutable snapshot behind a typed handle (INV-QUERY-SNAPSHOT), so reads are O(1) lookups with no
  locking.
- Provenance metadata is retained but bounded (one record per set key, per element for collections) — O(size
  of authored config), not O(document).
- Reload recomputes the merge and diffs the snapshot; consumers subscribe to change events rather than
  polling. No layer is re-parsed unless its file changed (watcher-driven, with a full-reparse fallback).

## Compatibility impact

- The schema `version` and INV-ADDITIVE govern evolution: adding a key is additive; changing a key's type,
  scope, lock, or removing it is a breaking change requiring a version bump + migration entry.
- `security_locked` and `scope` are part of the compatibility contract: **loosening** a lock (removing
  `security_locked`, widening `scope` to `workspace`) is a security-relevant change and is called out in the
  compatibility-impact report, not a silent minor.
- Plugin config schemas version independently with the plugin.

## Observability

- **Health status** — C-CONFIG publishes a per-component status (INV-STATUS): `ok` / `degraded (N keys
  ignored)` / `safe-mode`, with freshness (last reload time). The status bar renders it; it does not own it.
- **Diagnostics** — stable codes: `config.unknown_key`, `config.invalid_value`, `config.layer_quarantined`,
  `config.locked_key_ignored`, `config.migrated`, `config.deprecated_key`. Each carries the key, source
  location, and the layer, and is logged once at the config boundary (INV-ERR-CLASS).
- **`:inspect config`** is the primary observability surface (CFG-PROV-001), with a `--json` provenance
  ledger for tooling and support bundles (redacted for `secret` keys).
- **Origin discipline** — every effective value's provenance is the config analog of INV-ORIGIN: no value is
  ever "just there"; it always names its layer.

## Alternatives

- **VSCode-style Default < User < Workspace, no machine lock.** Rejected as insufficient: `CFG` requires a
  machine-local scope and enterprise lock, and requires the workspace to be *unable* to override security
  keys — a pure precedence stack cannot express "workspace may never win here." CFG-LOCK-001 adds the lock
  dimension on top of precedence.
- **Config as a script (Neovim/Emacs model).** Rejected: no static provenance, no type/scope/lock guarantees,
  one bad line aborts startup, and a project script can change security behavior — the exact failure modes
  §Problem exists to prevent.
- **Caller-chosen merge per read.** Rejected: merge must be a stable property of the key (the schema), or the
  same key merges differently in different consumers and provenance becomes meaningless.

## Rejected approaches

- **"Last file loaded wins" global precedence.** Rejected: destroys per-key provenance and makes lock
  enforcement position-dependent. Precedence is strictly per-key and layer-ranked.
- **Rewriting user config to migrate/repair it.** Rejected: destructive and surprising; C-CONFIG degrades
  in memory and leaves the file untouched (migration writes are opt-in, atomic, backed up).
- **A Cargo feature per configurable capability.** Rejected per the delivery-doc feature-flag policy
  (combinatorial CI/dependency blow-up); enablement is a **runtime** config value.
- **Letting plugins declare arbitrary security-locked keys.** Rejected: privilege-escalation vector; plugin
  locks are host-validated within a bounded allowlist and can never un-lock a core key.

## Migration strategy

CFG-MIG-001 (INV-PROTOCOL-VERSIONED): the schema carries `version`. On load, for a layer whose stored version
is older:

- **Auto-conversion** — deterministic, lossless changes (key rename, enum-value rename, widening an int
  range, splitting/merging a key with a total mapping) are applied from a **migration table** keyed by
  `from`→`to` version. The migrated values are used immediately; the on-disk file is only rewritten on the
  user's explicit save/opt-in (atomic, backed up).
- **Manual-warning** — ambiguous or lossy changes (a removed key with no equivalent, a semantic change, a
  now-`security_locked` key that a workspace previously set) are **not** auto-applied. The old value is kept
  inert and a `config.migrated` (manual) diagnostic points at a migration doc.
- **Deprecation window** — a `deprecated` key with `replaced_by` aliases to its replacement for a window and
  warns; after the window the alias is removed (breaking, version-gated).

Migration never blocks startup: an unmigratable key degrades to inert + warning (safe-mode-compatible).

## Test strategy

- **Precedence** — table-driven: for each `(scope, security_locked)` combination, assert the winning layer
  and that broader-than-`scope` layers are inert (CFG-LAYER-001).
- **Merge** — property tests per strategy: `replace` = top layer; `append` preserves order + duplicates;
  `set-union` de-dups and is order-independent; `deep-merge` recurses and inherits missing keys; negation
  removes exactly one contribution (CFG-MERGE-001).
- **Lock** — a workspace layer setting each `security_locked` key is dropped with `config.locked_key_ignored`
  and never reaches the effective map; an untrusted workspace layer is fully inert; loosening a lock trips
  the compatibility report (CFG-LOCK-001). Differential against INV-TRUST-1.
- **Provenance** — `:inspect config` output matches the constructed layer stack for scalars and per-element
  for collections (CFG-PROV-001).
- **Safe mode / failure** — fault injection (design-requirements §18): malformed layer, wrong-type key,
  unknown key, older version — assert startup **succeeds** and the specific key/layer degrades, and that a
  crash loop escalates to safe mode (CFG-SAFE-001).
- **Migration** — golden fixtures per `from`→`to`: auto-convert applies, manual-warning does not, deprecated
  alias resolves within the window (CFG-MIG-001).
- **Plugin schema** — namespace isolation, versioned migration, deactivation preserves config.

## Open questions

- **New invariants?** Should `INV-CONFIG-LOCK` (workspace can never contribute a security-locked key) and
  `INV-CONFIG-PROVENANCE` (every effective value names its layer) be added to the
  [registry](../invariants/reference-invariants.md)? They are currently expressed via INV-TRUST-1 /
  INV-ORIGIN; promoting them would make them directly test-addressable. (Requires a registry change +
  `spec validate`.)
- **Machine-local discovery** — exact search paths per platform (XPLAT §13) and whether a machine layer may
  itself be split (admin vs vendor). Deferred to the C-CONFIG implementation RFC.
- **Negation syntax** — the concrete `!pattern` / removal form for `append`/`set-union` lists.
- **Reload semantics for `security_locked` keys** — may a live reload change a locked value, or only at
  restart? Leaning restart-only for trust/executable keys.
- **Per-language / per-filetype sub-scopes** (VSCode `[rust]` blocks) — modeled as `deep-merge` maps now;
  whether they warrant a first-class sub-scope is open.

## Reference Invariants

- **INV-TRUST-1** — security-locked keys are never workspace-overridable; untrusted-workspace config is inert
  (CFG-LOCK-001, §5, Security impact).
- **INV-FAIL-BOUNDED** — one bad key/layer degrades to a lower layer/default; safe mode boots on defaults +
  machine-local (CFG-SAFE-001, Failure modes).
- **INV-PROTOCOL-VERSIONED / INV-ADDITIVE / INV-CONTRACT-FIRST** — the schema is the versioned contract;
  evolution is additive; migration is part of it (CFG-MIG-001, §9, Compatibility).
- **INV-PRIORITY / INV-PROFILE-ISOLATION** — keymap config layers feed the priority ABI and never merge
  across profiles (§7).
- **INV-STATUS / INV-ORIGIN / INV-ERR-CLASS / INV-QUERY-SNAPSHOT** — config health is a state machine; every
  effective value carries provenance; diagnostics are typed and logged once; the effective map is an
  immutable snapshot (Observability, Performance).
