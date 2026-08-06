---
doc: rfc
project: ruse
title: "RFC-0012: Collapse to a two-crate terminal modal editor; defer remote/plugin/render boundaries"
summary: >
  ruse locked an xi-editor-shaped architecture (core ↔ render-model ↔ client/remote boundary + thin
  frontend + versioned plugin protocol) as ~8 decisions before any of that code existed. This RFC reframes
  ruse as, first and only, a terminal-based modal text editor: collapse the 9-member workspace to two
  crates (editor-core + the ruse TUI binary), defer the remote/plugin/render boundaries behind explicit
  re-boundary triggers, freeze the governance rubric, and adopt command-level edit traces as a product
  pillar. The governance plane stays as the dogfooded methodology, not a co-equal shipped product.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/PROJECT.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/ARCHITECTURE.md
  - ../../../spec/architecture.yaml
  - ../../invariants/reference-invariants.md
  - ../../../spec/review-axes.yaml
---

# RFC-0012: Collapse to a two-crate terminal modal editor; defer remote/plugin/render boundaries

- **Status:** proposed
- **Author(s):** hawk
- **Created:** 2026-08-07
- **Decision link:** D-039

## Summary

ruse is **a terminal-based modal text editor** whose success is decided by four things — rope buffer,
tree-sitter, input latency, and single-binary distribution — all of which Rust wins. Yet the repo locked an
architecture shaped like **xi-editor** (a core/frontend boundary with an RPC seam, a client/remote boundary
made first-class from day one, and a versioned plugin protocol) as ~8 decisions **before that code
existed**. This RFC keeps Rust and reframes scope: collapse the 9-member workspace to **two crates**
(`editor-core`, pure and IO-free, + `ruse`, the TUI binary), **defer** the remote/plugin/render boundaries
behind explicit re-boundary triggers, **freeze** the 566-axis review rubric, and make **command-level edit
traces** a first-class product feature. Guiding rule: **over-invest in semantics (transaction / undo /
trace) — which cannot be fixed later — and under-invest in structure (crate boundaries, protocols) — which
is normal to fix later.**

## Motivation / Problem

**The planned architecture is xi-editor's, and xi-editor failed at exactly this shape.** Raph Levien's
retrospective is direct: a core/frontend boundary put RPC on the edit hot path; the frontend still needed
document state so the boundary leaked; every UI feature required a protocol extension, and dev velocity
died. ruse's `spec/ARCHITECTURE.md` (ARCH-LAYER-001, ARCH-FLOW-001), `INV-REMOTE-FIRST`, and D-011/D-004/
D-014 encode that same core ↔ render-model ↔ client/remote ↔ thin-frontend + versioned-protocol shape —
and `remote-first + thin frontend` early is precisely the path that killed xi.

**Boundaries drawn before a second consumer exists are unverified commitments.** Only `crates/core` is real
(~1,900 lines — the Document→Transaction→Undo→Snapshot + anchor slice). The other five crates are 15–18-line
stubs; `apps/tui` does not even depend on `ruse-core`. `render-model` and `plugin-protocol` were drawn
without ever meeting a real rendering or plugin requirement — they are almost certainly the wrong lines, and
a **wrong boundary is more expensive to remove than an absent one** is cheap to add.

The repo already says this to itself: `spec/review-axes.yaml` **RA-RUSE-003** ("flag process that outweighs
implementation") and **RA-RUSE-004** ("the first vertical slice validates the architecture before breadth is
built"). This RFC is that unheeded axis, acted on.

## Guide-level explanation

- **Identity:** a terminal-based **modal** text editor (Vim/Neovim editing language; Emacs command model as
  the long horizon). No GUI, no remote, no plugin host in v0.
- **Two crates.** `editor-core` (pure, IO-free) holds the buffer, transaction/undo, anchors, the semantic
  command set, and the modal input engine. `ruse` (binary) is the crossterm TUI: it turns keys into
  commands, runs them against core, performs the IO core asks for, and draws.
- **Same process, function calls — no RPC.** The core↔frontend split stays only as *discipline* (core has no
  IO dependency), not as a process/protocol boundary. That preserves the option to split later without
  paying xi's tax now.
- **Traces are a product feature.** Every edit is a replayable, shareable **command-level trace**: save a
  session, replay it on another file, attach it to a PR, hand it to a reviewer. The same format is the test
  corpus. Helix and Zed do not do this; it turns spec-first from a dev methodology into something a user
  feels in the first ten minutes.

## Reference-level explanation

- **Pure plan / commit split.** `plan(&EditorState, Command) -> Plan` is a *pure decision* (no mutation, no
  IO). `commit(&mut EditorState, Plan) -> Vec<Effect>` applies it and returns `Effect`s
  (`WriteFile`/`ReadFile`/`Quit`/…). The core **emits** effects; it never performs IO. `editor-core`'s
  `Cargo.toml` stays dependency-free so the compiler — not a rule — enforces IO-free (no `std::fs`, no
  `tokio`). This is where most of the value a full Haskell rewrite would buy is captured in Rust.
- **Input engine as a pending-state enum.** `Normal | AwaitingMotion{count,op} | AwaitingTextObject{..} |
  Insert | Visual{..}` resolves keys into a semantic `Command` (INV-CMD-SEMANTIC). Exhaustive `match` +
  newtypes give "make illegal states unrepresentable" for the parts that matter.
- **Command-level trace format** (designed *with* the input engine, not bolted on later):
  `Trace { header: { doc_hash, format_version }, commands: [Command] }`, serialized to
  `testdata/traces/*.json`. Key events are stored as side information only — recording at the *command*
  level survives keymap changes. **Determinism is the contract:** same initial document + same trace →
  identical final state. This holds automatically because core is IO-free (every external input is an
  `Effect`). UX: `:trace start` / `:trace save`, `ruse --replay t.json file.rs`.

## Reference Invariants

**Kept (already proven by `crates/core`):** INV-TXN, INV-UNDO, INV-ANCHOR, INV-POS-TYPED, INV-CMD-SEMANTIC,
INV-DOC-VIEW, INV-QUERY-SNAPSHOT, INV-ORIGIN, INV-ERR-CLASS. `ARCH-LAYER-001` ("core depends on nothing
above it") is kept — it is now an *in-crate* discipline enforced by an empty dependency set, not a
cross-crate/RPC boundary.

**Deferred:** `INV-REMOTE-FIRST` is downgraded from an active invariant to a **deferred design commitment**
(see D-039). A future re-introduction is a new invariant, earned when the re-boundary trigger fires — not a
day-one constraint on the editor.

## Failure modes & Recovery

The dominant failure this RFC removes is *architectural*: paying xi's protocol/RPC tax on the edit hot path.
Collapsing to same-process calls eliminates it. Text-level failure modes (atomic transaction apply, undo
round-trip, anchor survival) are unchanged and already covered by the golden slice.

## Security impact

Reducing surface: no plugin host, no remote transport, no external process in v0 means no plugin-trust,
protocol-auth, or remote-skew attack surface to defend yet. Traces carry document content, so a saved trace
is treated as sensitive (owner-only) like a recovery file — noted for when persistence lands.

## Performance impact

The four axes that decide a daily-driver editor — rope, tree-sitter, input latency, single-binary — are all
Rust-favourable and unblocked by this RFC. Same-process command dispatch has no RPC latency. Perf choices
(rope, redraw diffing) are made *after* measurement (criterion + a big-file corpus), never by intuition.

## Compatibility & Migration

- **Cargo-clean collapse.** `members = ["crates/*","apps/*"]` is a glob and every `[dependencies]` is empty,
  so removing the five stub crates + `apps/{gui,remote-agent}` needs no Cargo edits.
- **Atomic spec-as-code triad.** `tools/rusekit/repo.py` `CRATES`, `spec/architecture.yaml` `crates:`, and
  `spec/dependencies.yaml` `allowed_layers` (plus `tools/tests/test_architecture.py`, `test_classify.py`)
  must change together to `{core}` or `ruse arch deps` fails. Done in the follow-up collapse PR.
- **Design intent is preserved, not deleted.** The deferred boundaries keep their design docs
  (`docs/design/{remote-runtime,render-and-frontends,view-window-workspace,delivery-and-dependencies}.md`,
  `spec/contracts/*`) as **design notes** — thinking retained, code removed.

## Alternatives

- **(A) Full rewrite to Haskell.** Rejected. The editor's value lives in rope + tree-sitter FFI + latency +
  single-binary — the exact cells where GHC is weakest (no ropey-class rope; tree-sitter C FFI + pinned
  memory + GC is GHC's most painful combination; residency unpredictability from thunks holding old document
  versions). Right only if the goal were an editing-semantics *research artifact*, which it is not.
- **(B) Rust product + Haskell reference model (differential).** Rejected. Differential testing shines when
  semantics are *fixed* and implementations are complex (SQLite, TLS, CPU emulators). ruse is the opposite:
  semantics still fluid, implementation simple. A second implementation doubles spec-change cost and, solo +
  AI, the model rots — a guilt asset, not a verification asset. What we buy cheaply instead: a
  language-neutral golden **trace corpus**, a few Rust proptests, and TLA+/Alloy for the *protocol only* if
  and when remote lands.
- **(C) Keep Rust, collapse, traces (this RFC).** Chosen.

## Rejected approaches

- **RPC / process split between core and frontend.** This is exactly where xi-editor died; kept only as an
  in-crate IO-free discipline that leaves the *option* to split later.
- **A second implementation in any language.** See Alternative B.
- **async/tokio in the core.** async is contagious; it would force the editing logic's tests onto a runtime.
  LSP and file-watching are isolated on threads + channels; `editor-core` stays sync.
- **Pre-designing the plugin API.** A plugin API is what you write *after* the internal API stabilizes; the
  reverse order bends the internal design to a protocol that has no consumer.

## Trade-offs

- Deferring the boundaries means some later rework when they return — accepted, because *adding* a boundary
  around working code is cheap and *removing* a wrong one is not.
- Keeping the governance plane (dogfooded) retains process weight the critique flags; mitigated by
  **freezing** it (no new rubric axes; the 566-axis rubric becomes a manual checklist, never a merge gate)
  and by making product velocity the priority.

## Re-evaluation conditions

Explicit **re-boundary triggers** — when one fires, that boundary is reintroduced as its own RFC/crate:

| Boundary (crate) | Reintroduce when |
| --- | --- |
| `render-model` / Render IR (D-014/D-015) | a GUI frontend is actually started |
| `plugin-protocol` (D-004/D-009) | the internal API is unchanged for 3 months **and** ≥2 concrete plugins are wanted |
| `remote-runtime` / client-remote (D-011/D-029/30/31, INV-REMOTE-FIRST) | ≥2 months of local dogfooding done |
| `workspace` (multi-project) | multi-project handling actually becomes painful |

## Open questions

- Exact `Command` enum granularity for the trace format (what counts as one replayable command vs a
  composite) — settled alongside the input engine in the editor-spine PR.
- Whether `editor-core` keeps a byte buffer through v0 or swaps to `ropey` immediately (leaning: byte buffer
  until a big-file benchmark justifies the rope).
