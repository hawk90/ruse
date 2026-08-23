---
doc: rfc
project: ruse
title: "RFC-0017: Better-than-Vim interactive defaults live at the frontend/Workspace layer"
summary: >
  Where a Vim default persists only for backwards-compatibility ruse has no legacy of (no .vimrc ecosystem,
  no script corpus to protect), ruse may ship the better interactive default — but the change lives in the
  frontend / Workspace layer, NEVER in the engine (EditorState::new / View::fresh). The engine keeps the Vim
  factory value so the differential parity oracle, which drives EditorState directly, keeps measuring true
  Vim parity. First applied: editor.ignorecase + editor.smartcase ship ON via
  Workspace::set_default_search_case, which propagates to every buffer (not just the first). Records D-056.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/DECISIONS.md
  - ../../../spec/config-schema.yaml
---

<!-- code-blocks: illustrative — the Rust shown is NOT normative (D-038). These blocks fix the SEMANTIC
     decision (WHERE a better-than-vanilla default lives), not any literal signature or line number. -->

# RFC-0017: Better-than-Vim interactive defaults live at the frontend/Workspace layer

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-23
- **Decision link:** D-056 (proposed by this RFC; recorded on acceptance)
- **Builds on:** D-043 (parity is a machine-derived census; humans classify), D-049 (a profile is a configuration of decided machinery), F-009 (search/substitute), C-CONFIG.
- **Evidence:** the 2026-08-23 "better-than-vanilla" audit; `apps/tui/tests/parity_compare.rs` drives `EditorState::new` directly, so an engine-level default change would corrupt the oracle baseline.

## Summary

ruse tracks Vim/Neovim/Emacs for parity, but it is a NEW editor with no compatibility debt. Some vanilla
defaults exist only to protect that debt (scripts, `.vimrc`-free muscle memory, slow-terminal redraws). Where
a default's original rationale is obsolete for ruse, ruse ships the better interactive default — **at the
frontend / Workspace layer**, leaving the engine at the Vim factory value.

## Motivation / Problem

`editor.ignorecase` + `editor.smartcase` are the single most widely recommended vimrc pair, yet Vim/Neovim
default both OFF. The reason is backwards-compat: `/` and `:s` in existing scripts assume case-sensitive
matching, so upstream can never flip the global default. ruse has no such script corpus.

The naive fix — flip the default in `EditorState::new` / `View::fresh` — breaks the evidence that ruse's Vim
profile is faithful: the differential parity oracle (`parity_compare.rs`) drives `EditorState` directly and
compares against pinned Vim/Neovim. If the engine default diverges, the oracle measures ruse-vs-ruse, not
ruse-vs-Vim, destroying the parity signal.

## Guide-level explanation

Two layers, two defaults:

- **Engine** (`EditorState` / `View`): the provable Vim baseline. Its defaults equal the upstream factory
  values. The oracle drives this layer, so it stays honest.
- **Shipped profile** (frontend + `Workspace`): the defaults a human actually gets. The frontend installs
  the better defaults once at startup; the `Workspace` propagates them to every buffer it creates.

`config-schema.yaml`'s `default:` field records the SHIPPED default (what the user experiences). The engine
default is the oracle baseline and is documented as such in the setting's `desc`.

## Reference-level explanation

- **The Workspace seam.** `Workspace::set_default_search_case(ignore, smart)` stores the workspace default
  and applies it to every live view; each newly created view (`add_buffer` / split / reload) also picks it
  up. This is what makes the default hold across `:e`/`:split`/reload rather than only the first buffer — a
  per-buffer `:set` at startup would not. Illustrative:

  ```rust
  // frontend startup:
  let mut ws = Workspace::new(initial);
  ws.set_default_search_case(true, true); // ruse's shipped default; engine default stays (false, false)
  ```

- **The engine stays vanilla.** `EditorState::new` / `View::fresh` are untouched; `search_case` defaults to
  `(ignore=false, smart=false)`. The oracle, `EditorState`-based property tests, and `Workspace`-core unit
  tests all keep the Vim factory behavior.

- **Runtime override.** `:set ignorecase/ic`, `:set smartcase/scs` (and `no`-prefix) still work per view.

- **Eligibility rule (the gate on WHAT may diverge).** A default qualifies only if its vanilla rationale is
  obsolete FOR RUSE: terminal-era performance, or backwards-compat for a script/config ecosystem ruse does
  not have. A default whose rationale still applies — current muscle memory, live script compatibility,
  genuine correctness — does NOT qualify and stays at the Vim value. (E.g. `gdefault` stays OFF: it is a
  controversial option upstream keeps off on purpose, not an obsolete-rationale default.)

## Reference Invariants

- The engine default of any divergent setting equals the upstream factory value (oracle baseline preserved).
- A shipped-default divergence is applied at the frontend/Workspace layer and recorded with a Decision +
  config-schema `default:` + `desc` note.

## Failure modes & Recovery

- **A new buffer misses the default.** *Recovery:* every `Workspace` view-creation site applies the stored
  default; the `default_search_case_propagates_to_new_buffers` test guards this.
- **The oracle starts measuring ruse-vs-ruse.** *Recovery:* the engine default is never changed by this RFC;
  the oracle drives `EditorState`, which stays vanilla. Guarded by the pinned-fixture parity oracle staying
  green after the change.

## Alternatives considered

- **Flip the engine default.** Rejected: destroys the parity-oracle baseline.
- **Per-buffer `:set` at startup.** Rejected: only the first buffer gets it; new buffers reset to
  `View::fresh`.
- **Wait for a config-file loader.** Deferred, not blocking: when a loader lands, the shipped defaults move
  into the default profile it reads; until then the frontend installs them. This RFC's seam is unchanged by
  that migration.

## Migration

None. Behavior change only: interactive search becomes case-insensitive-with-smartcase by default; existing
`:set noic` / `:set noscs` (or a future config) opt out.
