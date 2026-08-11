---
doc: testing-and-benchmarks
project: ruse
title: "ruse Testing & Benchmarking Strategy"
summary: >
  The strategy/reference for how ruse proves correctness and performance: a test taxonomy that fixes the
  FILE FORMATS for each layer (unit, property, differential parity, plugin-compat, protocol fixtures,
  platform matrix, TUI golden/visual-regression, fault-injection, end-to-end/dogfood, deterministic replay);
  the `tests/` directory layout and how `spec/PRD.yaml` `trace.verify` points into it so every feature is
  traceable to evidence; a p50/p95/p99 benchmarking methodology with per-stage latency budgets, a fixed
  baseline gating machine, and realistic-plugin-set measurement; and what the development model's "Proven"
  state requires as evidence per feature type. This doc defines formats + methodology + directory +
  traceability; the CI orchestration, gates, and budget numbers live in ci-cd-and-release.md.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ci-cd-and-release.md
  - development-model.md
  - ../anti-patterns/anti-patterns.md
  - ../design/render-and-frontends.md
  - ../design/stability-and-observability.md
  - ../parity/README.md
  - ../protocols/versioning-and-evolution.md
  - ../../spec/PRD.yaml
---

# ruse Testing & Benchmarking Strategy

> **One home per fact.** [ci-cd-and-release.md](ci-cd-and-release.md) owns *when* tests run (the CI/CD
> pipeline, the merge/release gates) and the *budget numbers* (§10). This doc owns *what the tests are* —
> the file formats, the directory, the traceability, and the benchmarking methodology. Where the two touch,
> this doc points at ci-cd rather than restating it. The development model
> ([development-model.md](development-model.md)) owns the readiness state machine ("Proven"); §5 here says
> which test kind is the evidence for which feature type.

Scope: this is a strategy/reference doc, not a tutorial. It fixes formats and methodology so that fixtures
written under `trace.verify` are uniform, machine-checkable, and don't rot into doc tables (guards
[anti-patterns](../anti-patterns/anti-patterns.md) OPS-8, TEST-2).

---

## v0 scope — measuring the two-crate editor ([D-042](../../spec/DECISIONS.md))

The full methodology (§3) is written for the whole platform (plugins, remote, render IR). This scopes it to
what the two-crate v0 ([D-039]) actually has, and — per RFC-0012's rule that perf choices are made *after*
measurement, never by intuition — fixes **what we measure first** before any optimization lands.

**The per-keystroke cost is O(n) in the buffer today, in four independent places.** Naming them (with code
homes) so a regression localizes to one, and so the storage question is not confused with the others:

| # | Hot spot | Where | Cost / key |
| --- | --- | --- | --- |
| 1 | `EditList::apply_to` builds a fresh `Vec` → `Arc::from` (a second copy), then `rebuild_index` | `crates/core/src/{edit,document}.rs` | O(n) |
| 2 | **full tree-sitter re-parse every frame** — the `Tree` is not kept | `apps/tui/src/highlight.rs` | O(n) parse + query |
| 3 | full per-byte color vec + walk from byte 0 (no viewport/scroll exists) | `apps/tui/src/main.rs` `render` | O(n) |
| 4 | line-start index rebuilt whole | `crates/core/src/document.rs` `rebuild_index` | O(n) |

**Hypothesis (to confirm with numbers, not assert):** at daily-driver sizes (hundreds–thousands of lines) a
full buffer copy is tens of µs — negligible — so the *felt* latency is dominated by **#2 (re-parse) and #3
(render)**, not #1 (buffer). A rope's `O(log n)` edit only pays off at MB scale. If the baseline confirms
this, the storage rewrite is **not** the first move.

**The v0 benchmark set** (criterion, `default-features = false`; percentiles per §3.1) isolates the two
pure, terminal-free costs whose comparison answers the hypothesis:

- `crates/core/benches/edit_apply.rs` — a single mid-buffer insert on N = {100, 1k, 10k, 100k} lines (#1).
- `apps/tui/benches/highlight_parse.rs` — one full `spans()` re-parse on the same N (#2).

`render` (#3) is terminal-coupled; its pure span-flatten cost is folded into a later viewport slice. The
end-to-end input→render budget (§3.2, target < 16 ms) is the sum a human feels.

**Optimization order (measure-first applies to *tradeoff* choices, not to every step):**
1. **(A) highlight caching** — recompute `spans()` only when the revision changed; a no-op frame becomes
   free. No tradeoff (pure removal of wasted work) — justified without a benchmark.
2. **(B) viewport render + scroll** — a **missing feature / correctness gap** (a file taller than the screen
   cannot be edited today), not an optimization. Needed for dogfood regardless of numbers.
3. **(C) incremental tree-sitter** (keep the `Tree`, `tree.edit()` + re-parse) and **(D) incremental line
   index** — real tradeoffs; **gated on the baseline** showing #2/#4 dominate.

**Storage is deferred behind a seam, not chosen now.** `Arc<[u8]>` (flat bytes) stays until a benchmark
shows buffer mutation actually dominates (MB files). When it does, the swap goes behind a `TextStorage`
trait that seals the coordinate model to **byte offsets** (Edit/EditList/anchor/undo/snapshot already speak
bytes; a rope's char/line coordinates must never leak past the trait). The concrete rope/gap-buffer impl is
**injected from the frontend** so `editor-core` stays dependency-free (the compiler-enforced IO-free/dep-free
invariant — `crates/core/Cargo.toml` has no `[dependencies]`); the trait alone lives in core. Rope vs an own
gap buffer (amortized `O(1)` for the cursor-local edits a modal editor makes) is itself a measured choice at
that point — neither is in the build today.

### v0 baseline

Captured on adoption so later slices prove they helped and cannot silently regress (absolute numbers are one
dev machine's — a relative baseline, not a budget; the budget numbers live in ci-cd §10). Reproduce with
`cargo bench`.

| N (lines) | #1 apply (p50) | #2 re-parse (p50) | ratio #2/#1 |
| --- | --- | --- | --- |
| 100 | 0.59 µs | 794 µs | ~1350× |
| 1 000 | 18.8 µs | 7.98 ms | ~425× |
| 10 000 | 25.3 µs | 82 ms | ~3200× |
| 100 000 | 1.13 ms | 841 ms | ~740× |

**The hypothesis holds decisively:** the full re-parse (#2) is **~400–3200× the buffer cost (#1)**. At 10 000
lines a single keystroke pays ~82 ms of re-parse *per frame* (≈12 fps, plainly laggy) against ~25 µs of
buffer copy. So a rope would shave ~µs off a cost that is already three orders of magnitude below the one a
human feels: **storage is confirmed the wrong first move.** The wins are **(A) caching** (skip the re-parse
on frames where nothing changed) and **(C) incremental tree-sitter** (re-parse only the edited region) —
exactly where the next slices go.

#### Win (C) landed — incremental parse + viewport-bounded query (F-015 #3)

`apps/tui/src/highlight.rs` was rewritten off `tree_sitter_highlight` (which re-parses internally each
call and hides the `Tree`) onto raw `tree_sitter`: it keeps the previous `Tree`, computes the `InputEdit`
from a prefix/suffix diff of old vs new bytes, and reparses with `parser.parse(src, Some(&old_tree))`. It
also bounds the highlights query to the **visible byte range** (`QueryCursor::set_byte_range`) — the real
lever, since the parse is cheap but walking the whole document's captures is O(buffer). Per-keystroke cost
(`highlight_incremental`, 1-byte edit against a primed tree, ~50-line viewport):

| N (lines) | full re-parse | incremental + viewport | speed-up |
| --- | --- | --- | --- |
| 1 000 | ~7.5 ms | **~1.2 ms** | ~6× |
| 10 000 | ~77 ms | **~10 ms** | ~7.5× |

At daily-driver sizes (≤1k lines) the per-keystroke syntax cost is now well within a frame. Correctness is
pinned by `incremental_matches_full_parse_across_edits` (incremental spans must equal a full parse across
an edit sequence). **Remaining lever:** the `InputEdit` is derived by an O(buffer) byte-diff, which is the
residual cost at 10k+ lines; threading the actual edit deltas from the core `commit` (INV-TXN) would make
it O(edit). Injections / folds / indent / multi-language (F-015 #4) are still unbuilt, so F-015 stays
`planned` — only the parse path (#3) is done.

#### hlsearch spans cached (F-009) — measured, not assumed

The hlsearch/incsearch highlight ran a full-buffer Vim regex (compile + `find_all`) every frame while a
search was active. `CachedSearch` keys it on `(revision, viewport, pattern)` (`search_hl` bench):

| N (lines) | `full_buffer` (old, per frame) | `cached_hit` (idle frame) | `cached_miss` (scroll/edit) |
| --- | --- | --- | --- |
| 1 000 | 37 µs | 4.3 ns | 2.1 µs |
| 10 000 | 351 µs | 4.3 ns | 2.4 µs |
| 100 000 | 3.35 ms | — | 2.3 µs |

**Honest read:** the regex is far cheaper than a tree-sitter parse (37 µs vs 7.5 ms at 1k), so at
daily-driver sizes the old cost was minor (~0.2 % of a frame) — the "same severe class as the highlighter"
framing was an over-claim corrected by measuring. The real wins are (1) **idle frames** — cursor motion /
mode switches now cost 4.3 ns instead of a full regex, and (2) **large files** — the viewport-bounded miss
is ~2.3 µs *regardless of buffer size* vs the old O(n) 3.35 ms at 100k lines. Measure before claiming.

#### Every other perf claim, re-measured (`line_math`, `input_dispatch`, `edit_apply`)

Auditing my own claims with benches rather than analogy:

- **Refactor perf-neutrality (`edit_apply`, post-decomposition):** 581 ns / 10.1 µs / 25.7 µs at
  100 / 1k / 10k lines — unchanged from before `plan()`/`commit` were decomposed and before the DRY
  line-math extraction. The refactors are perf-neutral (Rust inlines the extracted free fns/methods).
  `edit_apply` also covers the `anchor_index` rebuild, so that eager-rebuild is measured here too.
- **Input dispatch (`input_dispatch`):** a 9-key Normal sequence is ~1.34 µs (~150 ns/key). feed_impl's
  decomposition is negligible, and dispatch is human-rate (once per keystroke, never per frame) anyway.
- **Line math (`line_math`) — a claim that was only PARTLY right.** `line_of`/`line_start` (what `row_col`
  runs, once per pane per frame) and `nth_line_start` (the viewport range):

  | N (lines) | `line_of` (cursor at end) | `nth_line_start` (last line) |
  | --- | --- | --- |
  | 1 000 | 6.6 µs | — |
  | 10 000 | 61 µs | — |
  | 100 000 | 623 µs | 2.13 ms |

  "O(pos), fine at daily sizes" is TRUE at ≤10k lines (≤61 µs) but FALSE at 100k: `row_col` is ~0.6–1.2 ms
  and the viewport's `nth_line_start` is ~2.1 ms — and the F-015 viewport work calls `nth_line_start`
  **twice per pane per frame** (vis-start + vis-end), so a 100k-line buffer scrolled near the bottom pays
  several ms/frame just to compute the viewport range. Irony noted: the syntax query is viewport-bounded,
  but the viewport COMPUTATION is O(n). This is exactly D-042's deferred **(D) incremental line index** —
  now measurement-justified for large-file support, still correctly deferred if the target stays
  daily-driver sizes (≤10k = fine). The line index is the next real perf lever, not storage.

---

## 1. Test taxonomy — the layers and their formats

The layer *scopes* and *run-on* schedule are defined in [ci-cd-and-release.md §2](ci-cd-and-release.md)
(L1 Unit, L2 Property, L3 Compatibility, L4 Platform, L5 End-to-end). This section adds the missing half:
the **on-disk format** each kind takes, so a fixture author has one shape to follow.

| Kind | ci-cd layer | Lives in | Format |
| --- | --- | --- | --- |
| Unit | L1 | `tests/unit/`, in-crate `#[test]` | Rust test functions; table-driven where inputs enumerate |
| Property | L2 | `tests/property/` | `proptest`/`quickcheck` generators + shrink; invariants below |
| Differential parity | L3 | `tests/parity/<profile>/*.yaml` | Declarative YAML fixture (§1.3), optional stock-editor oracle |
| Plugin-compat | L3 | `tests/plugin-compat/<api>-<fixture>/` | WASM built with a **previous** SDK + expectation manifest |
| Protocol fixtures | L3 | `tests/protocol-fixtures/<version>/`, `.../malformed/` | Versioned golden payloads + malformed/unknown-variant cases |
| Platform matrix | L4 | `tests/terminal-matrix/`, `tests/remote-scenarios/` | Scenario scripts run under each PTY/host driver |
| TUI golden / visual-regression | L4 (subset L3) | `tests/parity/*` render assertions + `tests/terminal-matrix/golden/` | Cell-grid snapshot (§1.7) |
| Fault-injection | nightly | `tests/fault-injection/` | Scenario + injected fault + expected graceful outcome |
| End-to-end / dogfood | L5 | `tests/e2e/`, dogfood corpus | Launch→edit→save→restart→recover scripts |
| Deterministic replay | cross-cutting | `tests/replay/` (+ saved fuzz failures) | Semantic-event log → expected final document state |

### 1.1 Unit (L1)

Smallest deterministic checks on a single contract: Document, Transaction, Anchor, key parser, command
resolver, register/kill-ring, capability ledger. Prefer **table-driven** functions so a new case is a data
row, not a new function. Unit tests assert on **stable identifiers** — error *codes*
([stability-and-observability.md §2.1](../design/stability-and-observability.md)), command IDs, revisions —
never on whole human-facing strings (which are not contracts).

### 1.2 Property (L2)

Generative tests that assert an **invariant** holds over a generated space, with shrinking to a minimal
counterexample. The four load-bearing invariants (from [ci-cd-and-release.md §2](ci-cd-and-release.md),
guarding [anti-patterns](../anti-patterns/anti-patterns.md) TEST-5/6/7):

- **Undo round-trip** — for any edit sequence, `apply` then `undo` restores the exact prior document
  (bytes + revision relationship). Anchors valid before are valid after. (`tests/property/undo-roundtrip`.)
- **Anchor-transform invariants** — for any edit and any anchor set, transformed anchors respect affinity
  and never point outside the document; cost is not O(anchors × edits). (`tests/property/anchor-stability`.)
- **Transaction atomicity** — a transaction applies fully or not at all; a failed transaction leaves no
  partial mutation and does not advance the revision (guards CMD-16, upholds INV-TXN).
- **Serialization round-trip** — `decode(encode(x)) == x` for every serializable contract (document,
  journal record, protocol message, config). Feeds §1.6 versioning.

Each failing case **must be reduced to a saved deterministic fixture** (§1.10), never left as a seed.

#### v0 status — SHIPPED (`crates/core/tests/properties.rs`)

The property layer is **live** for the v0 semantic core (the first mechanized test-quality layer beyond
example tests). Seven invariants run under `proptest` (dev-dependency only; the core crate stays dep-free):

- **UTF-8 / cursor-boundary safety** — no command sequence, from any buffer, ever produces invalid UTF-8 or
  leaves the cursor off a char boundary / out of bounds. (The headline totality property.)
- **Undo/redo round-trip** — full undo restores the initial buffer, full redo the edited buffer, regardless
  of undo-group coalescing.
- **Trace replay determinism** — recording a command sequence and replaying it on the same initial document
  reproduces the exact final state, and survives a serialize→parse round-trip (the RFC-0012 trace pillar).
- **Edit inverse identity** — `inverse(apply(buf))  == buf` for any in-range edit list (the undo substrate).
- **EditList canonical form** — a constructed list is sorted and pairwise disjoint (touching allowed).
- **Transaction atomicity** — a rejected transaction (stale base revision / out-of-range edit) leaves bytes
  **and** revision exactly unchanged; a successful one strictly advances the revision (INV-TXN / F-001).
- **Anchor-transform in-bounds** — after any in-range edit, every still-live anchor resolves inside the
  document, never past the end (INV-ANCHOR totality).

**This layer paid for itself immediately** — on first run it caught two latent core panics that every example
test had missed:

1. A stale Visual-mode selection anchor (a raw byte offset) sliced out of bounds when an edit shrank the
   buffer under it → `EditorState::commit` now clamps the anchor each step (totality; the edit-tracking
   anchor position is deferred to D-027).
2. `EditList::inverse` panicked on *touching* forward edits — the inverse touches too, but `from_sorted`
   demanded a strict gap, and a pure-delete-then-touch collapsed two inverse edits onto one position →
   `inverse` now merges same-position inverse edits and `from_sorted` allows touching (matching `new`);
   `new` also drops no-op edits so they can't seed the collision.

Deferred (still target-state above): anchor **affinity/span-delete** semantics as properties (bias direction,
`Invalidate` flag correctness — only in-bounds totality is covered now), the O(A+E) cost bound as a
performance property, and the differential-parity / fault-injection / replay-corpus / fuzzing layers.

### 1.3 Differential parity — `tests/parity/<profile>/*.yaml`

Vim/Emacs/Native behavior is encoded as **executable fixtures**, not doc tables
([ci-cd-and-release.md §3](ci-cd-and-release.md); guards OPS-8, TEST-2/3; [parity/README.md](../parity/README.md)
weights parity % by usage). This doc fixes the full fixture schema — a superset of the §3 example, adding the
assertion fields the Definition of Done requires (cursor/register/mode/undo/error-timing):

```yaml
name: vim-delete-inner-word
profile: vim                 # vim | emacs | native
parity: [VIM-OP-1, VIM-TOBJ-1]   # parity IDs this fixture is evidence for (back-link to PRD trace.parity)
tags: [operator, text-object]

initial:
  text: "hello world"
  cursor: 1                  # coordinate model per docs/design/positions-history.md
  mode: normal
  registers: {}              # vim registers; emacs fixtures use `kill_ring: [...]`
  selection: null

input:
  profile: vim
  keys: ["d", "i", "w"]      # semantic key sequence, not raw bytes

expected:
  text: " world"
  cursor: 0
  mode: normal
  registers: { '"': "hello", '-': "hello" }   # OR kill_ring: ["hello"] for emacs
  selection: null
  undo:
    groups: 1                # one undo group produced by the whole d-i-w (guards TEXT-12/13)
  error: null               # OR error: { code: "...", timing: after-motion }

oracle:                     # OPTIONAL — differential check against a stock editor
  tool: vim                 # vim | nvim | emacs (version pinned in the runner)
  compare: [text, cursor, registers]   # which fields must match the oracle; others are ruse-defined
```

Semantics of the assertion fields (a fixture asserts **all present** fields, not just final text):

- **text / cursor / mode** — final document text, cursor position, and input mode.
- **registers / kill-ring** — vim register contents by name, or the emacs kill-ring stack; asserting the
  *unnamed*, *yank*, and numbered registers separately is how register semantics are pinned (guards VIM-9).
- **selection** — selection *shape* (characterwise/linewise/blockwise, anchor+head), not just a range.
- **undo.groups** — how many undo groups the sequence produced; this is the contract that a compound edit is
  one undo step, distinct from the transaction count (guards TEXT-12/13; F-005 `undo-grouping`).
- **error / error.timing** — whether an error is raised and **when** in the sequence (e.g. a motion that
  fails mid-operator). Error *timing* is a behavioral contract, not an afterthought.

**Oracle-based differential testing** (`oracle:` block): where ruse claims stock-editor parity and no
hand-authored `expected` is worth maintaining, drive a pinned stock `vim`/`nvim`/`emacs` in a harness on the
same `initial` + `keys` and assert the listed fields match. Use it to *generate* and *police* expectations,
not for ruse-specific behavior (Native profile, ruse extensions) — those keep an explicit `expected`
(guards PAR-6, SPEC-6: do not substitute an external project as the spec).

#### v0 status — SHIPPED (nvim differential oracle, `tools/parity/oracle.py` + `tests/parity/vim/fixtures/corpus.yaml`)

The Neovim half is live and is the primary way the Vim input engine is now built and policed. Notes that
generalize to any oracle:

- **Oracle = pinned binary, black-box.** `tools/parity/oracle.py` runs the pinned `nvim` (`--headless -u NONE
  -i NONE`); the local `nvim` must equal the census pin (v0.12.4) and its version is recorded in every run.
  **Non-corruption is the load-bearing property** (three prior harnesses corrupted their own first
  observation): feed keys with `nvim_feedkeys(...,'x')` then read state via the API AFTER — never through a
  call that itself mutates. `oracle.py --selftest` (identity + determinism + hand-verified known ops) GATES
  the corpus; `gov parity_oracle` runs it when a pinned nvim is present and otherwise verifies corpus
  integrity only (nvim-optional, so CI without nvim stays green).
- **Every `expect` is ORACLE-CAPTURED, never hand-written.** For v0 the corpus uses a compact JSON-in-YAML
  form (`{name, lines, keys, expect:{text,cursor,registers…}}`) rather than the full schema above.
- **Config-dependent ops** (indent `>>`, etc.) carry a per-fixture `setup` ex-command so nvim runs under
  ruse's defaults (e.g. `set shiftwidth=4 expandtab`) — cross-default comparison is invalid; match config or
  the fixture lies.
- **A divergence is a FINDING, not a test failure.** `apps/tui/tests/parity_compare.rs` replays each fixture
  through ruse and reports verified-vs-divergent; it stays green so ruse can be early. Divergences are the
  work-list.

**Fixture taxonomy — ATOMIC then COMPOSITE (do both):**

- **Atomic** — one operation (`dw`, `di(`, `3x`). Pins per-op correctness; found e.g. the `di(` forward-scan
  and count-not-applied bugs.
- **Composite** — a 6–20 keystroke *editing session* (`ciwX<Esc>w.`, `"ayiw$"ap`, `vjdp`). These test
  **cross-command state** — dot-repeat chains, named-register reuse, count accumulation, mode transitions —
  which atomic fixtures structurally cannot reach. **Composite fixtures find INTERACTION bugs**: they
  surfaced that dot-repeat (`.`) was entirely unimplemented, that named registers (`"a`) mis-parse, and a
  charwise-multiline paste cursor bug — none visible to single-op tests. **A corpus of only atomic fixtures
  gives false confidence; composite coverage is required, not optional.**

### 1.4 Plugin-compat — `tests/plugin-compat/`

Ecosystem-stability gate ([ci-cd-and-release.md §4](ci-cd-and-release.md); guards ECO-9, TEST-13/14, and
Critical-15 #15). Each fixture is a directory holding a **WASM plugin built with a previous SDK** plus an
expectation manifest:

```
tests/plugin-compat/
├── api-v1-minimal/         plugin.wasm (built w/ SDK v1.0)  +  expect.yaml
├── api-v1-git-view/
├── api-v1-language-service/
├── api-v1-remote-provider/
└── api-v1-media-view/
```

`expect.yaml` asserts the cross-version contract: command registration succeeds, declared events are
received, transaction requests are honored, capability negotiation resolves. On every core PR the host is
built with the **current** SDK and each old-SDK plugin is loaded: **if an existing fixture breaks, merge is
blocked.** New API surface may be added additively; existing fixtures may not regress. Old-SDK artifacts are
committed as opaque binaries (or built from a pinned SDK tag) so "previous" is reproducible.

### 1.5 Protocol fixtures — `tests/protocol-fixtures/`

The remote, plugin, render-IR, diagnostic, and command-descriptor schemas are long-lived contracts
([ci-cd-and-release.md §5](ci-cd-and-release.md), [versioning-and-evolution.md](../protocols/versioning-and-evolution.md)).
Three fixture families:

```
tests/protocol-fixtures/
├── v1.0/          golden payloads (canonical serialized messages for each schema)
├── v1.1/
└── malformed/     truncated / type-mismatched / out-of-range payloads
```

Required assertions (the additive-evolution rules):

- **Backward read** — the current decoder reads every `v1.0/` golden payload.
- **Unknown enum variant** — an unrecognized variant is tolerated (ignored/deferred), not fatal.
- **Missing optional field** — decodes with the documented default.
- **Ignored new capability** — an old peer ignores an added capability without error.
- **Malformed** — every `malformed/` payload yields a *typed* protocol error (code), never a panic or
  silent accept.

Golden payloads are **frozen once published** — a changed byte is a breaking-change signal.

### 1.6 Platform matrix — `tests/terminal-matrix/`, `tests/remote-scenarios/`

L4, run on main/nightly (WSL/tmux/SSH are not per-PR — hard to reproduce on hosted runners,
[ci-cd-and-release.md §1](ci-cd-and-release.md)). A scenario is written once and executed under each driver:

- **Terminal drivers** — Unix PTY, Windows ConPTY, tmux (passthrough), WSL, SSH.
- Each driver runs the same scripted session and asserts input parsing (partial escape sequences, bracketed
  paste, query-response vs user input; guards TERMIN-14/15/16), capability probing, and — where visual — a
  cell-grid golden (§1.7).
- **Remote scenarios** (`tests/remote-scenarios/`) — first-connect bootstrap, reconnect/resume, SSH-drop
  recovery, path translation (guards TEST-9/10/11, REMOTE-8/9).

### 1.7 TUI golden / visual-regression

The render path is a **deterministic transformation pipeline** (
[render-and-frontends.md §5](../design/render-and-frontends.md)); golden tests snapshot the **end of the
pipeline**, and because the pipeline is dumpable per stage, a diff localizes to a stage rather than "the
terminal is weird":

```
input state → Semantic Render Tree (paint IR) → virtual terminal → cell grid → golden snapshot
              (render-and-frontends §3)          (headless backend)  (chars+width+style)
```

- The **virtual terminal** is a headless backend that lowers the Render Tree exactly as a real terminal
  would but writes to an in-memory cell grid instead of a device — so snapshots are stable and diffable.
- A **cell-grid snapshot** records each cell's grapheme, computed **display width**, and style — not a raw
  escape-sequence dump (which would couple the test to the lowering, guarding TERMOUT-2, RENDER-2).
- **Mandatory case classes** (guards TERMOUT-3/4/5): wide (CJK / East-Asian-Width), emoji (incl. ZWJ
  sequences and variation selectors), combining marks (width 0), narrow/ambiguous-width, and **resize**
  (assert no stale layout mid-resize, guards TERMOUT-13).
- Snapshots are captured per render **profile** (compatibility vs enhanced,
  [render-and-frontends.md §4](../design/render-and-frontends.md)); a plugin's `ImageNode` lowers to a real
  image under `enhanced` and to a Unicode/placeholder under `compatibility` — both are golden-asserted, which
  is how the "degrade, never disappear" invariant (INV-CAP-DEGRADE) is tested.

### 1.8 Fault-injection — `tests/fault-injection/`

Inject a failure and assert **graceful, bounded** behavior ([ci-cd-and-release.md §12](ci-cd-and-release.md);
validates [stability-and-observability.md §6-7,§13](../design/stability-and-observability.md); guards
TEST-8, PERSIST-4). Fault set: **disk full, permission loss, process crash, packet loss, truncated
journal.** A fixture pairs a scenario with an injected fault and the expected outcome expressed as a *typed
error code + recovery action + no data loss* — e.g. a truncated recovery journal replays the valid prefix
and stops, never replaying corrupt tail (F-008 `truncated-journal`; guards PERSIST-4). The assertion is on
the recovery *contract* (state machine transition + error code), not on log text.

### 1.9 End-to-end / dogfood (L5)

Full lifecycle: **launch → edit → save → restart → recover** ([ci-cd-and-release.md §2](ci-cd-and-release.md)).
Plus a **dogfood corpus** — real editing sessions of the ruse repo itself, replayed nightly as a soak test.
E2E complements, never replaces, the lower layers (guards TEST-1: not "only UI tests").

#### v0 status — SHIPPED (system level: the binary as a process)

Two layers now drive the whole vertical, not just in-library units:

- **`apps/tui/tests/system_replay.rs`** spawns the **real compiled `ruse` binary** (`CARGO_BIN_EXE_ruse`)
  through its headless `--replay` entry (F-022) and asserts on **stdout + exit code** — argument parsing,
  trace/file IO, the core replay pipeline, and every FAILURE path (missing args, unreadable inputs,
  malformed trace, and a base-hash mismatch that must be *rejected*, not misapplied). This is the only
  test that exercises the finished binary as a process.
- **`apps/tui/tests/lang_arg_e2e.rs`** drives the **real input engine + core together** exactly as
  `main.rs::run` composes them (keystroke → Lang-Arg translate → `Feed` → `Command` → edit), covering the
  F-027 vertical the inline `feed` unit tests cannot reach (translated edits on a real document,
  dot-repeat replaying the translated char, and the adversarial "and to nothing else" boundary at the
  document level).
- **`apps/tui/tests/session_fuzz.rs`** is the **full-stack keystroke fuzzer**: random Vim keystroke
  sessions (up to 140 keys) driven through the real engine with the mode following the REAL editor state,
  applied to the core. It asserts the load-bearing kernel invariants after EVERY key (valid UTF-8, cursor
  on a char boundary) and that a full undo of any session restores the initial buffer; a separate
  monotonic-alphabet variant (no in-session `u`/CTRL-R) asserts the full undo↔redo round trip. This is the
  first test to fuzz the COMPOSED stack over long sessions — where mode × operator × register × dot ×
  undo-grouping interactions live. (It already surfaced one finding — see below.)
- **`apps/tui/tests/scenarios.rs`** is a curated set of realistic multi-step sessions with exact
  end-states — dot-repeat × count, named-register yank/paste, visual + operator, search → change → `n` →
  `.`, substitute + undo grouping, blockwise-insert replication + undo, and multi-window buffer sharing —
  each a readable regression guard for a known-tricky cross-feature interaction. All pass, i.e. the
  compound behaviours match Vim, not just the single ops the oracle corpus covers.

**Fuzzer finding (FIXED):** the session fuzzer showed that `i`…`CTRL-O u` (undo via the Insert one-shot)
dropped to **Normal** mode instead of returning to Insert — the undo/redo command plans hardcoded
`mode: Mode::Normal`, clobbering the KL-OBL-5 one-shot resume. Fixed by making undo/redo PRESERVE Insert
(they restore document state, not mode; the engine only reaches them from Normal or from Insert via
`i_CTRL-O`, so preserving Insert is correct and Normal is unchanged). Regression tests:
`crates/core/src/editor.rs` `undo_via_insert_one_shot_returns_to_insert` + `undo_from_normal_stays_in_normal`.
This is the loop working as intended — the fuzzer found a real parity edge and it was closed.

**Honest gap (still MANUAL):** the *live-terminal* path — the raw-key event loop, raw-mode, and the render
diff painting to an actual tty — has **no automated coverage**. `--replay` bypasses the input engine
(it replays `Command`s), and the e2e test drives the engine below the terminal, so the crossterm read
loop + screen flush in `main.rs::run` is verified only by hand. A PTY-driven harness (§1.7 TUI golden)
is the eventual home for it; it is deliberately not built yet (a flaky screen-scraping harness would cost
more than the thin glue it covers). Recorded here so the boundary is visible, not assumed.

### 1.10 Deterministic replay — `tests/replay/`

The replay log is a **semantic-event log** (commands + origins), *not* the raw keyboard byte stream and *not*
the debug log ([stability-and-observability.md §5](../design/stability-and-observability.md); guards DET-1,
DET-2). Replaying the same log against the same starting state must reproduce the **identical final document
state** (revision + bytes) — determinism is a contract (results must not depend on async completion order,
DET-3; timestamps are injected, not sampled).

- **Fuzz failures are saved as replay fixtures.** Every property/fuzz counterexample (§1.2) is reduced and
  committed here as a permanent regression, so a fix is proven by a replay, not by a raised `sleep()`
  (guards DET-4, TEST-20).
- The replay corpus is also the **real command-sequence replay corpus** the DoD requires (§5; guards
  TEST-21).

#### v0 status — SHIPPED (determinism at the process boundary)

`apps/tui/tests/system_replay.rs` asserts the DET-1 contract on the real binary: the **same trace against
the same base reproduces identical final bytes** across runs, and a trace whose recorded `doc_hash` does
not match the file is **rejected** (`TraceError::HashMismatch`) rather than run against the wrong base —
the failure mode this contract exists to forbid. Fuzz-counterexample-as-fixture (DET-4) remains future.

---

## 2. `tests/` directory layout & traceability

`tests/` does not exist yet (the tree below is the target); `trace.verify` in `spec/PRD.yaml` already points
into it, so the paths are a contract before the files exist.

```
tests/
├── unit/                 L1 — per-contract deterministic checks
├── property/             L2 — proptest invariants (§1.2)
├── integration/          L3/L4 — cross-component scenarios
├── parity/               L3 — differential fixtures
│   ├── vim/              operator-motion.yaml, text-objects.yaml, registers.yaml, undo-tree.yaml,
│   │                     search-substitute.yaml, ...
│   ├── emacs/            kill-ring.yaml, prefix-keys.yaml, universal-arg.yaml
│   ├── native/           modal-text.yaml, leader-discovery.yaml
│   └── expected/         oracle-captured / shared expectations
├── plugin-compat/        L3 — old-SDK WASM fixtures (§1.4)
├── protocol-fixtures/    L3 — v1.0/ v1.1/ malformed/ (§1.5)
├── terminal-matrix/      L4 — PTY/ConPTY/tmux/WSL/SSH + golden/ (§1.6, §1.7)
├── remote-scenarios/     L4 — connect/resume/disconnect (§1.6)
├── fault-injection/      nightly — disk-full/perm-loss/crash/packet-loss/truncated-journal (§1.8)
├── replay/               deterministic replay corpus + saved fuzz failures (§1.10)
├── e2e/                  L5 — lifecycle + dogfood (§1.9)
└── benches/              performance benchmarks (§3)   # may live at repo root as benches/
```

### Traceability — every `mvp`/`must` feature → fixtures

The chain is `F-* → trace.parity (parity IDs) → trace.design (design doc) → trace.verify (fixtures here)`
([development-model.md](development-model.md) "traceability chain"; enforced by `spec-validate`'s DoD rule —
every `stage: mvp` / `priority: must` feature must resolve `trace.design` and carry non-empty `acceptance`).
Below, each feature's `trace.verify` paths and the dominant test *kind* that is its evidence. **A feature is
not Proven until these fixtures exist and pass** (§5).

| Feature | Stage/Prio | `trace.verify` → | Evidence kind |
| --- | --- | --- | --- |
| F-001 Transactional editing | mvp/must | `property/undo-roundtrip`, `unit/transaction-revision` | Property + Unit |
| F-002 Document & coordinate model | mvp/must | `property/anchor-stability`, `unit/coordinate-widths` | Property + Unit |
| F-003 Vim input profile | mvp/must | `parity/vim/{operator-motion,text-objects,registers}.yaml` | Differential parity |
| F-004 Semantic command engine + palette | mvp/must | `unit/command-registry`, `integration/palette-context` | Unit + Integration |
| F-005 Undo/redo grouping | mvp/must | `parity/vim/undo-tree.yaml`, `unit/undo-grouping` | Parity + Unit |
| F-006 TUI rendering (ANSI/Unicode) | mvp/must | `unit/render-diff`, `integration/render-profile-pin` | Unit + TUI golden |
| F-007 Buffers/views/windows/splits | mvp/must | `integration/split-independent-cursors`, `unit/view-local-state` | Integration + Unit |
| F-008 File open/save/crash recovery | mvp/must | `fault-injection/truncated-journal`, `unit/save-atomic` | Fault-injection + Unit |
| F-009 Search & substitute | mvp/should | `parity/vim/search-substitute.yaml` | Differential parity |
| F-010 Terminal capability detection | mvp/must | `unit/capability-ledger`, `integration/da1-probe` | Unit + Platform |
| F-011 PTY-backed terminal buffer | post-mvp/should | `integration/{pty-unix,conpty-windows}` | Platform matrix |
| F-012 Emacs input profile | post-mvp/should | `parity/emacs/{kill-ring,prefix-keys,universal-arg}.yaml` | Differential parity |
| F-013 Native input profile | post-mvp/could | `parity/native/{modal-text,leader-discovery}.yaml` | Differential parity (explicit expected) |
| F-014 Built-in LSP (local+remote) | post-mvp/should | `integration/lsp-remote`, `unit/language-service-model` | Integration |
| F-015 Tree-sitter highlighting | post-mvp/should | `integration/treesitter-highlight`, `property/incremental-reparse` | Integration + Property |
| F-016 Plugin protocol + host | post-mvp/must | `integration/plugin-panic-isolation`, `integration/plugin-sdk-conformance` | Integration + Plugin-compat |
| F-017 Remote client/runtime (SSH) | post-mvp/should | `remote-scenarios/{first-connect-bootstrap,reconnect-resume}` | Remote scenarios (fault-injection) |
| F-018 Debugger (DAP + location) | future/could | `debug/{dap-backend,location-model}` | Integration |
| F-019 GUI frontend | future/could | `integration/gui-render-parity`, `unit/render-ir-shared` | TUI/render golden (cross-frontend) |
| F-020 Extension marketplace | future/could | `integration/{lockfile-reproducible,compat-ci-old-sdk}` | Plugin-compat + reproducibility |
| F-021 AI agent integration | future/could | `integration/ai-proposal-review`, `unit/execute-vs-propose` | Integration (preflight) |

> `spec-validate` should grow to check that each `trace.verify` path **resolves to a real fixture** once
> `tests/` exists (today it checks `trace.design` resolves). That closes the loop: a `must` feature with a
> dangling `verify` path fails CI — the machine-checkable form of "no evidence, not Proven."

---

## 3. Benchmarking methodology

Performance is a **merge gate**, not a vibe ([ci-cd-and-release.md §10](ci-cd-and-release.md), which owns the
budget *numbers* and the gate wiring). This section owns the *methodology* — how to measure so the numbers
mean something. It is the "don't" mirror of [anti-patterns](../anti-patterns/anti-patterns.md) PERFS.

### 3.1 Percentiles, not averages

Report and gate **p50 / p95 / p99** for every latency benchmark; **never a single average** (guards PERFS-3).
A mean hides the tail that users actually feel. The gate is on p95/p99 as defined in ci-cd §10.

### 3.2 Per-stage latency budgets

Latency is attributed to the transformation pipeline stages
([render-and-frontends.md §5](../design/render-and-frontends.md)), so a regression localizes to a stage
rather than "the editor got slow":

```
input → command → transaction → render
 (parse)  (resolve)  (apply)      (lower + diff + emit)
```

Each stage carries its own budget; the **end-to-end input-to-render** budget is the sum the user feels.
The **canonical budget numbers live in [ci-cd-and-release.md §10](ci-cd-and-release.md)** (e.g.
input-to-render p99, startup p95 vs baseline, idle CPU). Any per-stage split introduced here is
**provisional** until promoted into ci-cd §10 as the single home:

| Stage | Metric | Budget |
| --- | --- | --- |
| input → command | key/escape parse + resolve, p99 | *provisional — set in ci-cd §10* |
| command → transaction | apply + inverse + anchor transform, p99 | *provisional* |
| transaction → render | render-tree build + cell diff, p99 | *provisional* |
| **input → render (end-to-end)** | **p99** | **see ci-cd §10 (target < 16 ms)** |

### 3.3 What to measure (the benchmark set)

Covers the ci-cd §10 set — cold startup, empty-buffer latency, 10 MB file open, insert latency, scroll frame
time, plugin activation, remote round-trip, memory after 100 files — with these methodology rules:

- **Cold vs warm start** — measure both; report separately (guards PERFS-1: judging speed by startup alone,
  and PERFS-2: hiding cost in lazy loading — so measure **first-use latency**, not just startup).
- **Benchmark with a REAL plugin set, not an empty editor.** The Neovim failure mode is "fast empty, slow
  once plugins accumulate" ([ci-cd-and-release.md §10](ci-cd-and-release.md)). A representative plugin set
  (the plugin-compat fixtures, §1.4) is loaded for the gating benchmarks. An empty-editor number is a
  best-case reference, not the gate.
- **Memory: peak vs steady-state** — report both. Peak catches allocation spikes; steady-state catches leaks
  (memory after 100 files opened/closed should return near baseline).
- **Large-file + 10 MB open** — Rope/render performance is measured on large fixtures, never only small ones
  (guards PERFS-5, TEXT-7, PERF-4).
- **Allocation / allocation-count regression** — track allocation *count* and bytes, not just wall time; an
  allocation-count jump is an early regression signal even when latency looks flat.
- **Startup regression** — a dedicated benchmark (guards TEST-18); gated per ci-cd §10 (`startup p95 >
  baseline + N%`).

### 3.4 The fixed baseline machine

Benchmarks are noisy, so the **gate runs on a fixed baseline machine**, and the split matches
[ci-cd-and-release.md §10](ci-cd-and-release.md):

- **PR = trend comparison + warning** (hosted runners are too noisy to gate).
- **main / nightly = gate on the fixed baseline machine.**
- The baseline is **never updated from a developer's personal machine** (guards OPS-5). A baseline change is
  a reviewed commit with its own justification.

### 3.5 Keep observability under optimization

Optimization must not delete error context, IDs, or debug surfaces to win a benchmark (guards PERFS-4,
PERFS-6). Debug builds and release builds both retain enough context for incident analysis; the
transformation pipeline stays dumpable (§3.2).

---

## 4. Test hygiene rules

- **No sleep-based flaky fixes.** A flaky async test is fixed by making the wait deterministic (await the
  event / advance a controlled clock / drive the scheduler), never by adding or raising `sleep()` (guards
  TEST-20, DET-4, OPS-1). Retry-only is also banned as a "fix."
- **Assert on codes and IDs, not sentences** — error codes, command IDs, revisions, status enum states
  ([stability-and-observability.md §2.1,§11.1](../design/stability-and-observability.md)).
- **Quarantine carries an expiry + owner** ([ci-cd-and-release.md §13](ci-cd-and-release.md)); a muted test
  is a tracked debt, not a silent skip.
- **Golden/protocol payloads are frozen once published** — a changed byte is a signal, reviewed as a
  breaking change, not silently re-recorded.

---

## 5. What "Proven" requires

The development model ([development-model.md](development-model.md)) treats support/readiness as an
**evidence-based state** — "support is a **test result**, not a doc claim"
([ci-cd-and-release.md §9](ci-cd-and-release.md)). The Definition of Done requires *"Tests pass: differential
parity + property + unit"* plus a resolving `trace` and an adversarial review. This section maps **feature
type → the test kind that is its evidence**. A feature reaches "Proven" only when the mapped evidence exists
under `trace.verify` and passes.

| Feature type | Primary evidence (must exist to be Proven) | Supporting |
| --- | --- | --- |
| Editing core (Document/Transaction/Anchor/Undo) | **Property** — undo round-trip, anchor invariants, transaction atomicity, serialization round-trip | Unit |
| Input profile behavior (Vim/Emacs/Native) | **Differential parity** fixtures asserting text+cursor+mode+registers/kill-ring+selection+undo+error-timing | Oracle diff (Vim/Emacs), unit |
| Command/palette | **Unit** (registry, resolution) + **integration** (context filtering) | Parity where key-driven |
| Rendering / frontend | **TUI golden** (cell grid, per profile, wide/emoji/combining/resize) | Unit render-diff, property |
| Persistence / recovery | **Fault-injection** (truncated journal, disk full, crash) | Unit save-atomic |
| Terminal / platform | **Platform matrix** (PTY/ConPTY/tmux/WSL/SSH) + capability-ledger unit | Golden |
| Plugin / ecosystem | **Plugin-compat** (old-SDK WASM) + **protocol fixtures** | Integration isolation |
| Remote | **Remote scenarios** (connect/resume/disconnect) — a fault-injection family | Protocol fixtures |
| Anything with no explicit test yet | **Oracle-based differential testing** against a pinned stock editor (§1.3) where a stock analogue exists | — |

Two rules the model insists on:

1. **No sleep-based flaky fixes** count as evidence — a "passing" test stabilized with `sleep()` is not
   proof (§4; DET-4).
2. **A real command-sequence replay corpus** (§1.10, `tests/replay/`) is required evidence for the editing
   core, and fuzz counterexamples become permanent replay fixtures — proof is a reproducible replay, not a
   patched symptom (guards TEST-21, DET-1/2).

Where a feature claims stock-editor parity but has no hand-authored fixture, **oracle-based differential
testing is the evidence** (§1.3) — but only for behavior that genuinely mirrors the stock tool; ruse-specific
behavior always keeps an explicit `expected` (guards SPEC-6, PAR-6).

---

## Open questions

1. **Oracle pinning & availability.** Which exact `vim`/`nvim`/`emacs` versions are the oracle, how are they
   pinned in CI, and how do we handle oracle behavior that is itself a bug we intentionally diverge from?
   (A per-fixture `oracle.compare` allowlist is the current escape hatch — is that enough?)
2. **`spec-validate` `trace.verify` resolution.** When `tests/` lands, should `spec-validate` hard-fail on a
   dangling `verify` path for `mvp`/`must` features, and warn for others — or gate all? (§2.)
3. **Golden snapshot format & churn.** Cell-grid text vs a structured (grapheme,width,style) record — which
   minimizes noisy diffs while still catching width/combining bugs? How are intentional golden updates
   reviewed so they aren't rubber-stamped (guards TERMOUT-2)?
4. **Baseline machine identity.** What is the physical/virtual gating machine, who owns it, and what is the
   promotion procedure for a new baseline number into ci-cd §10?
5. **Per-stage budget numbers.** The §3.2 per-stage split is provisional — what are the actual input→command,
   command→transaction, transaction→render p99 budgets, and do they belong in ci-cd §10 or a shared budget
   table referenced by both?
6. **Old-SDK artifact reproducibility.** Are plugin-compat WASM fixtures committed as opaque binaries, or
   rebuilt from a pinned SDK tag in CI? Trade-off: repo weight/LFS vs build-time reproducibility.
7. **Corpus storage.** The dogfood + large-file + replay corpora can grow large; do they live in-repo, in
   Git LFS, or a separate corpus repo (per [render-and-frontends.md §7](../design/render-and-frontends.md)
   "keep large corpora in a separate repo or Git LFS")?
8. **Cross-frontend golden parity (F-019).** When a GUI backend arrives, is the Render Tree the shared
   golden (assert both TUI and GUI lower the *same* IR identically), or does each frontend keep its own
   snapshot? (`unit/render-ir-shared` implies the former.)
