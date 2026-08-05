---
doc: rfc
project: ruse
title: "RFC-R001: Rust Dynamic-Library Plugin ABI (Rejected)"
summary: >
  Rejected-decision record documenting why a native Rust dynamic-library (.so/.dylib/.dll) plugin ABI was NOT
  chosen as ruse's extension foundation: Rust has no stable ABI, so plugins couple to the exact compiler and
  crate versions of the host; there is no crash isolation (a plugin fault is a host fault); and it forces a
  Rust-only SDK. Replacement: a versioned, language-independent protocol over WASM or an external process
  (D-004, INV-PROTOCOL-VERSIONED). Records the single re-evaluation condition so it is never re-litigated.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../proposed/RFC-0003-plugin-api.md
  - ../../architecture/architecture.md
  - ../../invariants/reference-invariants.md
---

# RFC-R001: Rust Dynamic-Library Plugin ABI (Rejected)

- **Status:** rejected
- **Author(s):** hawking90a@gmail.com
- **Created:** 2026-08-05
- **Decision link:** D-004 (rejected alternative to the accepted WASM/process protocol)

> This is a **rejected-decision record**. It exists so the "just load a Rust `.so`" idea is not re-argued every
> six months. The accepted decision is D-004 / [RFC-0003](../proposed/RFC-0003-plugin-api.md); this document
> records *why the native-ABI alternative loses* and *the one condition* under which it could be reopened.

## Summary

Using Rust's native dynamic-library ABI (a `Plugin` trait behind `.so`/`.dylib`/`.dll`, loaded via
`dlopen`/`libloading`) as ruse's ecosystem foundation is **rejected**. It couples every plugin to the host's
exact compiler and crate versions, provides **no crash isolation**, and forces a **Rust-only** SDK — the
opposite of ruse's "reference design that outlives the language" goal. It is replaced by a **versioned,
language-independent protocol over WASM or an external process** (D-004, INV-PROTOCOL-VERSIONED, ENG-PLUG-001),
specified in [RFC-0003](../proposed/RFC-0003-plugin-api.md).

## Motivation / Problem

A native ABI is the tempting "obvious" choice: it is the fastest call path, needs no serialization, and lets a
plugin hold a `&mut EditorState`. Exactly that temptation is the Neovim-class trap
([architecture.md](../../architecture/architecture.md) §4.3): plugins that touch internals make internals
unchangeable, and the whole ecosystem becomes as fragile as the least-careful plugin. This record captures why
the shortcut is a dead end.

## Reference-level explanation (what was rejected)

The rejected shape, from [architecture.md](../../architecture/architecture.md) §4.1, §4.3:

```rust
// Rejected: plugin receives core state directly, linked as a native dynamic library.
pub trait Plugin {
    fn activate(&mut self, editor: &mut EditorState);
}
// plugin.so → Rust trait ABI → sensitive to compiler version and crate versions
```

### Why it was rejected

1. **Rust has no stable ABI.** `repr(Rust)` layout, trait-object vtables, and monomorphized generics are not
   guaranteed stable across compiler releases or even flag changes. A plugin built against one host build can
   be silently incompatible with the next — mismatches manifest as memory corruption, not a clean error.

2. **Compiler- and crate-version coupling.** A native plugin must be compiled with (effectively) the same
   `rustc` and the same versions of shared crates as the host. Every host bump forces a synchronized ecosystem
   rebuild — the "broke one day after an update" failure the lockfile is meant to prevent
   ([architecture.md](../../architecture/architecture.md) §4.6) — now unavoidable at the ABI level.

3. **No crash isolation.** In-process native code shares the address space: a plugin `panic` across an FFI
   boundary is undefined behavior, and a segfault or infinite loop takes the whole editor down. This directly
   violates **INV-PLUGIN-ISOLATED** ("a plugin panic/timeout never terminates the editor and never crosses an
   FFI/host boundary") and ENG-PLUG-001.

4. **Internal types leak by construction.** Passing `&mut EditorState` (or `Rope`, slotmap entries, undo
   nodes, renderer types) hands plugins internal structures, freezing them as public contract and violating
   **INV-PLUGIN-NO-CORE**. Changing internals would break the ecosystem — the exact coupling ruse exists to
   avoid.

5. **Rust-only SDK.** A native ABI locks plugin authors into Rust, contradicting the project's
   language-independent, contract-first stance (INV-CONTRACT-FIRST) and its "design outlives the language"
   goal ([docs/README.md](../../README.md) Philosophy).

6. **Security surface.** Native code runs with full host privileges, defeating the deny-by-default capability
   model ([architecture.md](../../architecture/architecture.md) §4.4, §10; INV-TRUST-1).

## The replacement

A **versioned protocol over WASM or an external process** ([architecture.md](../../architecture/architecture.md)
§4.3; [RFC-0003](../proposed/RFC-0003-plugin-api.md)):

- **Stable across host builds** — the contract is a language-independent protocol with a `major.minor` +
  capability header, not a Rust vtable (INV-CONTRACT-FIRST, INV-ADDITIVE).
- **Crash-isolated** — WASM sandboxes memory and traps cleanly; a process boundary contains segfaults; either
  way a fault cannot cross into the host (INV-PLUGIN-ISOLATED).
- **Language-independent SDK** — any language targeting WASM (or the process protocol) can author plugins.
- **Capability-gated** — the sandbox is deny-by-default; filesystem/network/process access is granted only via
  declared capabilities (INV-TRUST-1).
- **Handles, not types** — plugins get handles/snapshots/commands/events/UI models, never internals
  (INV-PLUGIN-NO-CORE).

## Reference Invariants

This rejection is required by, and re-affirms:

- **INV-PROTOCOL-VERSIONED** — the extension surface is a versioned protocol (WASM/external process), **never a
  Rust dynamic-library ABI**.
- **INV-PLUGIN-ISOLATED** — a plugin panic/timeout never terminates the editor and never crosses an FFI/host
  boundary.
- **INV-PLUGIN-NO-CORE** — plugins never receive internal core types.

Supporting: INV-CONTRACT-FIRST, INV-ADDITIVE, INV-TRUST-1. Governing policy: **ENG-PLUG-001** (isolation over a
versioned protocol — `exception.allowed: false`). All defined in
[invariants/reference-invariants.md](../../invariants/reference-invariants.md).

## Alternatives

- **Accepted:** WASM / external-process versioned protocol — D-004,
  [RFC-0003](../proposed/RFC-0003-plugin-api.md). See its Alternatives section for embedded-script, MVP-host,
  and composition trade-offs.
- **C ABI with a hand-frozen `extern "C"` boundary** — a stable-ish ABI is possible, but still no crash
  isolation, still in-process native privileges, still a C-shaped SDK; it solves only problem (1) of six.
  Rejected for the same isolation/security reasons.

## Trade-offs

Rejecting the native ABI gives up the fastest possible call path and zero-copy access to core state. Accepted:
the cost is bought back with coarse IPC, bounded snapshots, and scheduler coalescing
([architecture.md](../../architecture/architecture.md) §9), and the alternative's "speed" is worthless if one plugin
can crash the editor or block every host upgrade.

## Re-evaluation conditions

Reopen **only if a stable native-component ABI *with* crash isolation becomes established** — i.e. a widely
supported, versioned native component model that guarantees ABI stability across compiler/crate versions *and*
sandboxes faults so a plugin cannot take down the host (D-004 re-evaluation clause). Until such a mechanism
exists and is proven, a Rust dynamic-library plugin ABI stays rejected and is not re-litigated. A faster WASM
runtime or a chattier-IPC complaint is **not** a re-evaluation trigger — those are addressed within RFC-0003,
not by abandoning isolation.

## Open questions

None for this record — it is closed by D-004. Concrete transport details (WASM component model vs. framed
process protocol, resource budgets) are open questions of the accepted design,
[RFC-0003](../proposed/RFC-0003-plugin-api.md).
