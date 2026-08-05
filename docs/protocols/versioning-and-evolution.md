---
doc: versioning-and-evolution
project: ruse
title: "ruse Protocol Versioning & Additive Evolution"
summary: >
  The language-independent contract policy shared by every ruse protocol/schema: plugin API, remote
  protocol, render protocol, command descriptor, configuration schema, diagnostic bundle. Defines the
  version header, additive-evolution rules, unknown-handling, deprecation windows, and the API promotion
  ladder (Internal→Experimental→Preview→Stable→Deprecated→Removed). Contracts are defined before types:
  "Rust type changed ⇒ API changed" is explicitly rejected.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md
  - ../design/render-and-frontends.md
  - ../invariants/reference-invariants.md
  - ../anti-patterns/anti-patterns.md
---

# ruse Protocol Versioning & Additive Evolution

Every long-lived contract in ruse follows one policy. A schema is **language-independent and defined
before any Rust type** — the contract is primary; the implementation proves it. (Reference-implementation
philosophy, see [../README.md](../README.md).)

## Protocols Governed

| Protocol / schema | Boundary |
| --- | --- |
| Plugin API | host ↔ plugin |
| Remote protocol | client ↔ workspace runtime |
| Render protocol | core ↔ frontend (the Render Tree, see [render-and-frontends.md](../design/render-and-frontends.md)) |
| Command descriptor | command registry ↔ callers |
| Configuration schema | user/workspace config ↔ editor & plugins |
| Diagnostic bundle | editor ↔ support tooling |

## Version Header

Each protocol carries an explicit version + capability set (schema `schemaVersion` for tree formats):

```rust
pub struct ProtocolHeader {
    pub major: u16,
    pub minor: u16,
    pub capabilities: CapabilitySet,
}
```

- **major**: incompatible change.
- **minor**: backward-compatible additions.
- **capabilities**: feature negotiation independent of version numbers.

## Additive-Evolution Rules

| Change | Allowed? |
| --- | --- |
| Add a field | ✅ allowed |
| Add an enum variant | ✅ allowed — **requires a defined unknown-variant handling rule** on readers |
| Add an optional capability | ✅ allowed — old readers ignore it |
| Change an optional field to required | ❌ forbidden |
| Change the meaning of an existing command ID | ❌ forbidden |
| Delete a field | ❌ only in a **major** version |
| Change wire meaning of an existing field | ❌ forbidden (add a new field instead) |

Readers must implement: **unknown-variant handling** (never panic/reject on a new variant), **missing
optional field** defaults, and **ignore unknown capability**. These three are the compatibility-test
matrix (see [../operations/ci-cd-and-release.md](../operations/ci-cd-and-release.md) §Protocol-Compat).

## API Promotion Ladder

Do not stabilize a bad API quickly — the most dangerous failure mode is **stabilizing a wrong
abstraction**. Every API surface moves through:

```
Internal → Experimental → Preview → Stable → Deprecated → Removed
```

Promotion gates:
- **To Stable:** at least **two independent implementations/plugins** must have used it in Preview.
- Maintain an **API surface budget**; distinguish **convenience** APIs from **primitive** APIs.
- Every API must be able to express **failure, cancellation, and partial success**.
- Each new API ships with its versioning + migration strategy written at the same time.
- Deprecated APIs are kept for a defined window (≥ 2 majors; see [../architecture/architecture.md](../architecture/architecture.md) §11) and then Removed — they do not live forever complicating the core path.

## Deprecation & Migration

- Renames ship an **alias + `deprecated_since` / `remove_after`** (command IDs: [../architecture/architecture.md](../architecture/architecture.md) §2.2).
- Config key changes provide automatic migration where safe and a manual warning where not.
- Large API changes ship with a migration tool; never silently break configs/keymaps/macros.
- Default-value changes are **not** applied silently in a minor release.

## Reference Invariants (this doc)

- **INV-CONTRACT-FIRST** — Contracts are defined independently of implementation types; changing an
  internal Rust type is not, by itself, an API change. (Guards SPEC anti-patterns, ECO-1/17.)
- **INV-ADDITIVE** — Compatible evolution is additive; readers handle unknown variants/fields/capabilities
  gracefully; breaking changes require a major bump. (Guards ECO-2/3, APIX.)
- **INV-PROMOTION** — No API reaches Stable without ≥2 independent users and a migration strategy.
  (Guards APIX "stabilize on first request".)

## Alternatives / Rejected Ideas / Trade-offs

- **Rejected: Rust `serde` struct == the contract.** Ties the wire format to Rust internals; refactors
  break the ecosystem. → explicit schema + header.
- **Rejected: "we will never remove an API".** Freezes bad abstractions forever and bloats the core path.
  → promotion ladder + bounded deprecation window.
- **Rejected: bump major for every change.** Ecosystem churn. → additive minor + capability negotiation.
- **Trade-off:** schema + header + promotion process is upfront overhead. Accepted: it is the mechanism
  that lets the platform "grow the ecosystem yet break less than Neovim."
