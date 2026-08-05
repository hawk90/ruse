---
doc: parity-plugin-ecosystem
project: ruse
title: "Parity: Plugin Ecosystem Foundations"
summary: >
  Neovim-level ecosystem capability depends less on feature count than on foundations: stable namespaced
  command IDs, a versioned plugin API (WASM/process, not Rust ABI), manifests, capability/permission model,
  config schemas, per-profile recommended keymaps, semantic (surface-independent) UI, local/remote execution
  location, lockfiles, compatibility CI, deprecation policy, marketplace signing/verification levels.
  Plugins provide ONE semantic command; profiles wire it to keys.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - ../architecture/architecture.md
  - ../protocols/versioning-and-evolution.md
  - ../architecture/design-requirements.md
  - ../operations/ci-cd-and-release.md
---

# Parity: Plugin Ecosystem Foundations

To reach a Neovim-scale ecosystem "that breaks less than Neovim," the foundations matter more than feature
count. A plugin registers **one semantic command**; input profiles wire it to keys per profile — authors
barely think about Vim/Emacs differences.

```
org.example.git.stage
  Vim Style    → s  or  <leader>gs
  Emacs Style  → C-c g s
  Native Style → Space g s
  Palette      → Git: Stage Selection
```

| ID | Foundation | Target | Detail | Compat | Weight |
| --- | --- | --- | --- | --- | --- |
| ECO-1 | Stable namespaced command IDs | L1 | [architecture.md](../architecture/architecture.md) §2.2 | Adapted | high |
| ECO-2 | Versioned plugin API (WASM/process, not Rust ABI) | L1 | [architecture.md](../architecture/architecture.md) §4.3, D-004 | Intentionally-different | high |
| ECO-3 | Plugin manifest (features known before execution) | L1 | §4.4 | Adapted | med |
| ECO-4 | Capability / permission model (deny-by-default) | L1 | §4.4, §10 | Adapted | high |
| ECO-5 | Config schema per plugin | L1 | §4.5 | Adapted | med |
| ECO-6 | Command-palette auto-registration | L1 | §2 | Equivalent | med |
| ECO-7 | Per-profile recommended keymaps (suggested, not forced) | L1 | §1.4 | Adapted | med |
| ECO-8 | Semantic UI API (TUI/GUI/Web-independent) | L1 | render-and-frontends | Adapted | med |
| ECO-9 | Local/remote execution-location distinction | L1 | [remote.md](remote.md) | Adapted | med |
| ECO-10 | Lockfile + reproducible install | L1 | §4.6 | Equivalent | med |
| ECO-11 | Compatibility CI (old-SDK fixtures) | L1 | [ci-cd-and-release.md](../operations/ci-cd-and-release.md) §4 | Adapted | med |
| ECO-12 | Long deprecation policy (≥2 majors) | L1 | §11 | Adapted | low |
| ECO-13 | Marketplace signing + verification levels | future | design-requirements §9 | Adapted | low |
| ECO-14 | Stable/Experimental/Internal API layering + promotion ladder | L1 | versioning-and-evolution | Adapted | med |
| ECO-15 | Extension SDK conformance test kit | post-MVP | design-requirements §9 | Adapted | low |

## Governance (beyond the API)

A stable API does not imply a stable ecosystem (design-requirements §9). Targets:
- Verification levels: **Official · Verified · Community · Unreviewed · Deprecated · Quarantined**.
- Re-approval on permission change; malicious/abandoned-package policy; namespace ownership + transfer.
- Quality metrics: crash rate, activation latency, API compatibility.
- Composition rules where plugins overlap (diagnostics merge, one formatter, completion aggregate, keymap
  conflict resolution, decoration layer priority) — not a single priority number (design-requirements §21).
- Clean **deactivation**: commands, tasks, status, decorations, views, persistent data all cleaned up
  (design-requirements §22). Plugin state separates user-config / regenerable-cache / persistent / session
  (§23).

## Dogfooding path (official features)

- **MVP:** internal extension API (no public WASM host — D-009).
- **Before 1.0:** extract the common API that built-ins (git, search) actually needed.
- **After:** grow the share of official extensions built on the stable public API (design-requirements §24).

## Reference Invariants
INV-PLUGIN-NO-CORE, INV-PLUGIN-ISOLATED, INV-PROTOCOL-VERSIONED, INV-PROMOTION.
