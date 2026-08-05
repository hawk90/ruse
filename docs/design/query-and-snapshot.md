---
doc: query-and-snapshot
project: ruse
title: "ruse Query / Snapshot Layer (C-QUERY, the CQRS read side)"
summary: >
  The read side of ruse's Command/Query split: how a Query returns an immutable, revision-stamped
  Snapshot/DTO (never a live mutable object — INV-QUERY-SNAPSHOT), how a cheap-to-share Snapshot is
  produced by rope structural sharing rather than deep-copying the document, how every snapshot and
  async result carries a revision so stale results are dropped (INV-ASYNC-ORDER), how background
  consumers (parser/LSP/git) read snapshots not live buffers with coalescing and cancellation, and the
  bounded snapshot-scoped decoration-provider API that resolves the NVIM-EXT-7 clash (V-26).
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - render-and-frontends.md            # §2 Command/Query split, §3-5 render IR
  - editing-language.md                # house style; Range/Anchor consumers
  - ../architecture/architecture.md    # §3 revision/snapshot, §8 async/stale-drop, §9 perf
  - ../parity/neovim.md                # NVIM-EXT-7 decoration providers, NVIM-ASYNC fast-context
  - ../invariants/reference-invariants.md
  - ../../spec/PRD.yaml                # C-QUERY
  - ../../spec/DECISIONS.md            # D-012, D-018
---

# ruse Query / Snapshot Layer (C-QUERY)

`C-QUERY` is the **read side** of ruse's CQRS boundary. Command (mutation) is specified in
[render-and-frontends.md §2](render-and-frontends.md) and realized by `C-TRANSACTION` / `C-COMMAND`;
this doc specifies everything on the *read* half: the `Query` contract, the `Snapshot` value it returns,
the revision stamp that makes async safe, and the background-read + decoration surfaces built on top.

It does not re-specify the render IR ([render-and-frontends.md §3-5](render-and-frontends.md)), the
anchor model ([architecture.md §3.2](../architecture/architecture.md); INV-ANCHOR), or the scheduler
budgets ([DECISIONS D-018](../../spec/DECISIONS.md)); it consumes them and cites by ID.

## Problem

The core splits **mutation (Command)** from **read (Query)** at mutation/remote/plugin boundaries
([render-and-frontends.md §2](render-and-frontends.md)). The mutation half is well-specified. The read
half has to satisfy four pressures that pull against each other, and no existing doc pins how:

1. **No live-object leakage.** A query must never hand out a mutable, aliased handle into core state —
   a stale or aliased `&mut Document` (or an internal `Rope`) can corrupt core state and couples every
   reader to internals (INV-QUERY-SNAPSHOT, INV-PLUGIN-NO-CORE). Reads must return an *immutable
   snapshot / DTO*.

2. **No deep copies.** [architecture.md §9](../architecture/architecture.md) forbids implementing a
   snapshot as a deep copy and forbids cloning the whole document per command. A snapshot of a 200 MB
   file must be effectively free to take on every keystroke and every redraw.

3. **Async correctness.** Parser, LSP, and git run off the main loop and answer *later*. Between request
   and answer the document moves. Applying an answer computed against an old revision drifts decorations,
   mis-places diagnostics, and re-introduces exactly the Neovim fast-context hazard ruse designs against
   (NVIM-ASYNC). Every result must be revision-stamped and stale ones dropped (INV-ASYNC-ORDER).

4. **Bounded plugin reads during paint.** Neovim's decoration providers (NVIM-EXT-7) run a plugin
   callback *synchronously per visible line during redraw* — plugin code inside the paint pass. That
   directly violates INV-QUERY-SNAPSHOT / INV-PLUGIN-NO-CORE. V-26 requires a bounded, visible-range,
   snapshot-scoped provider that runs *outside* the paint critical section.

The job of `C-QUERY` is to make all four hold simultaneously, with concrete Rust types and O-bounds.

## Goals

- A `Query` contract: a read-only request that returns an immutable `Snapshot` or a plain DTO, never a
  live mutable core object (**Q-QUERY**; INV-QUERY-SNAPSHOT).
- A `DocumentSnapshot` value that is **cheap to clone and share** (O(1) rope structural share + `Arc`ed
  auxiliary indices), **revision-stamped**, and `Send + Sync` so it can cross to background workers
  (**Q-SNAP**).
- A revision stamp on **every** snapshot and **every** async result, with a single stale-drop rule
  (**Q-REVSTAMP** / **Q-STALE**; INV-ASYNC-ORDER).
- A named **query catalog** (visible lines, cursor/selection, diagnostics, render snapshot, available
  commands, symbols) that is the read surface the render layer, palette, and plugins consume (**Q-CATALOG**).
- Background consumers (parser / LSP / git) that read snapshots, with **coalescing** of duplicate
  requests and **cancellation** of superseded ones via the central scheduler (**Q-BG**; INV-SCHED-1).
- A bounded, visible-range, snapshot-scoped **decoration-provider** API that runs outside the paint
  critical section (**Q-DECO**; resolves V-26 / NVIM-EXT-7).
- Position/UTF-16 conversion computed **on a snapshot via a precomputed index**, never by scanning the
  whole document per request (**Q-U16**; [architecture.md §9](../architecture/architecture.md)).

## Non-goals

- Mutation, transactions, undo — the Command side ([architecture.md §3.3](../architecture/architecture.md),
  INV-TXN/INV-UNDO). Queries never mutate.
- The anchor/gravity data structure itself (`C-ANCHOR`, INV-ANCHOR) — snapshots *carry* anchors; how
  anchors survive edits is [architecture.md §3.2](../architecture/architecture.md) and
  [positions-history.md](positions-history.md).
- The render IR node schema and backend lowering ([render-and-frontends.md §3-5](render-and-frontends.md)).
  A "render snapshot" query *produces input* to that pipeline; it does not define the nodes.
- Scheduler priority/deadline/budget **numbers** — open under [D-018](../../spec/DECISIONS.md); this doc
  fixes only the *ownership and coalescing/cancellation shape* the read side depends on.
- Multi-client sequencing (optimistic vs authoritative) — [D-012](../../spec/DECISIONS.md); snapshots are
  the mechanism that makes per-client-view reads coherent, but enabling multi-client is post-MVP.

## Terminology

Reference [spec/glossary.yaml](../../spec/glossary.yaml): **query** (read-only request returning a
snapshot/DTO, never a live mutable object), **snapshot** (immutable, cheap-to-share view of state at a
revision), **revision** (monotonically increasing document version stamp), **anchor** (position that
survives edits). New local terms:

- **Snapshot target** — what a query reads: a `DocumentSnapshot`, a `ViewSnapshot` (client/view-local
  cursor/viewport/selection), or a `WorkspaceSnapshot` (open-buffer list, diagnostics by namespace).
- **Stamped result** — an async payload wrapped with the `RequestId` and the `base_revision` it was
  computed against (`Stamped<T>`).
- **Visible range** — the inclusive line span currently laid out for a client-view; the *only* span a
  decoration provider may read (**Q-DECO**).
- **Query catalog** — the fixed, named set of read requests (**Q-CATALOG**); the read counterpart of the
  semantic Command list.

## Invariants

This doc **enforces**:

- **INV-QUERY-SNAPSHOT** — queries return immutable snapshots/DTOs, never live mutable core objects; any
  per-redraw decoration provider is bounded to a visible-range snapshot and runs outside the paint
  critical section. (This doc is the primary realization.)
- **INV-ASYNC-ORDER** — single-threaded deterministic executor; every async response carries request ID +
  revision; stale results dropped.

This doc **depends on**: INV-ANCHOR (snapshot positions are anchors), INV-POS-TYPED (typed byte/char/
grapheme/UTF-16 coordinates), INV-HANDLE (snapshot targets are typed handles with generation),
INV-SCHED-1 (background work is scheduler-owned, coalesced, cancellable), INV-PLUGIN-NO-CORE (plugins see
snapshots/handles only), INV-DOC-VIEW (view-local state is not in the Document), INV-RENDER-IR (the render
snapshot feeds lowering; it is not backend bytes).

## Proposed design

### 1. The Query contract (Q-QUERY)

A query is a **pure read** over a snapshot. It is a function of *(snapshot, params) → DTO*; it takes no
`&mut`, performs no I/O, and cannot observe a mutation in progress.

```rust
/// The output of any query is a value type: an immutable Snapshot, or an owned DTO.
/// It is `Send + 'static` so results may cross to background workers or remote clients.
pub trait Query {
    type Output: Send + 'static;
    /// Pure: reads the snapshot(s), allocates its own result, mutates nothing.
    fn run(&self, cx: &QueryCx<'_>) -> Self::Output;
    /// Which targets this query needs — lets the engine grab a consistent snapshot set once.
    fn targets(&self) -> QueryTargets;
}

pub struct QueryCx<'a> {
    pub doc: Option<&'a DocumentSnapshot>,   // present iff QueryTargets::DOCUMENT
    pub view: Option<&'a ViewSnapshot>,      // present iff QueryTargets::VIEW
    pub workspace: Option<&'a WorkspaceSnapshot>,
}
```

The engine, not the caller, decides how to obtain the snapshot. In-process readers on the main thread may
call `QueryEngine::run` directly (the strong CQRS form is applied only at **mutation / remote / plugin**
boundaries — [render-and-frontends.md §2](render-and-frontends.md); do not over-apply CQRS):

```rust
impl QueryEngine {
    /// Grabs a consistent snapshot set at the current revision, then runs the pure query.
    pub fn run<Q: Query>(&self, q: &Q) -> Q::Output {
        let cx = self.snapshot_set(q.targets());   // O(#targets), each snapshot O(1)
        q.run(&cx)
    }
}
```

Two properties make INV-QUERY-SNAPSHOT structural rather than a convention:

- **`Output` is a value type, not a borrow.** A query cannot return `&Document` or `&mut _`; the trait
  bound `Send + 'static` forbids it. A plugin/remote/render caller gets an owned DTO or a shareable
  `DocumentSnapshot` (itself immutable — see §2), never an aliased path back into core state.
- **`run` takes `&QueryCx`, never `&mut`.** There is no legal way for a query to enact a mutation; a
  change is expressible only as a `CommandRequest` on the write side.

### 2. The Snapshot type — cheap, revision-stamped, structurally shared (Q-SNAP)

A `DocumentSnapshot` is an immutable view of one document at one revision. It is **not** a deep copy: the
text is a persistent rope whose clone shares structure in O(1), and every auxiliary index is behind an
`Arc` that is rebuilt *incrementally on transaction commit*, not per snapshot.

```rust
#[derive(Clone)]           // Clone is O(1): one rope handle clone + a handful of Arc bumps
pub struct DocumentSnapshot {
    doc: DocumentId,               // typed handle w/ generation (INV-HANDLE)
    revision: Revision,            // the stamp (Q-REVSTAMP)
    text: Rope,                    // persistent B-tree rope: clone = O(1) structural share
    lines: Arc<LineIndex>,         // byte↔line + UTF-16/char prefix sums (Q-U16)
    anchors: Arc<AnchorIndex>,     // decoration/diagnostic/mark positions (INV-ANCHOR)
    diagnostics: Arc<DiagnosticSet>, // by namespace (NVIM-DIAG-1)
    syntax: Option<Arc<SyntaxTree>>, // last completed parse; may lag `revision` (see §5)
    kind: BufferKind,              // editable/read-only/generated/streaming (INV-BUFFER-KIND)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);      // monotonic; a transaction strictly increases it (INV-TXN)
```

**How a snapshot is produced without deep-copying.** The live document holds the *current* rope and the
current `Arc` indices. `Document::snapshot()` does no traversal:

```rust
impl Document {
    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            doc: self.id,
            revision: self.revision,
            text: self.text.clone(),          // O(1): shares the immutable rope root
            lines: Arc::clone(&self.lines),    // O(1)
            anchors: Arc::clone(&self.anchors),
            diagnostics: Arc::clone(&self.diagnostics),
            syntax: self.syntax.clone(),
            kind: self.kind,
        }
    }
}
```

Cost is a fixed number of atomic increments regardless of document size — snapshotting on every keystroke
and every redraw is free. The persistent rope gives **copy-on-write across revisions for free**: when a
transaction edits the document, only the O(log n) path of touched nodes is replaced; every outstanding
snapshot keeps pointing at the old root, so a background worker holding an old `DocumentSnapshot` sees a
stable, consistent document even as the foreground edits ([architecture.md §3.3](../architecture/architecture.md):
"Background parsers read a snapshot, not the live buffer").

**Auxiliary indices are rebuilt on commit, not on read.** `LineIndex` and `AnchorIndex` are recomputed
*incrementally* inside transaction apply (touching only the affected range), producing a fresh `Arc`. A
snapshot therefore never triggers an O(doc) build — it borrows the `Arc` that already exists. Anchor
update cost stays off O(anchors × edits) (INV-ANCHOR, PERF-6) because it is done once per transaction, not
once per snapshot or once per query.

**Immutability is enforced, not requested.** `DocumentSnapshot` exposes only `&self` readers
(`slice`, `line`, `char_len`, `byte_to_utf16`, `anchors_in`, …). It has no interior mutability and no
method returning `&mut` or the inner `Rope` by value. That is what makes it safe to publish across the
plugin and remote boundaries (INV-PLUGIN-NO-CORE): the worst a holder can do is read a slightly old
document.

`ViewSnapshot` is the same idea for **view-local** state (INV-DOC-VIEW) — cursor, selection set, viewport/
visible range, input mode, fold state — carried separately so the same `DocumentSnapshot` serves every
client-view (the mechanism that makes per-client-view reads coherent under [D-012](../../spec/DECISIONS.md)).

```rust
#[derive(Clone)]
pub struct ViewSnapshot {
    view: ViewId,
    doc: DocumentId,
    doc_revision: Revision,      // the document revision this view state is coherent with
    view_seq: u64,               // view-local monotonic seq (cursor/viewport moves w/o doc edits)
    cursor: Anchor,
    selections: Arc<[Selection]>,
    visible: VisibleRange,       // inclusive line span currently laid out
    mode: InputMode,
}
```

### 3. Revision stamping and stale-result dropping (Q-REVSTAMP / Q-STALE)

Every snapshot carries `revision`; every async result is wrapped as `Stamped<T>` carrying the
`RequestId` and the `base_revision` it was computed against. This is the single mechanism behind
INV-ASYNC-ORDER on the read side.

```rust
pub struct Stamped<T> {
    pub request: RequestId,
    pub base_revision: Revision,   // the doc revision the payload was computed against
    pub payload: T,
}

pub enum Freshness { Current, Stale, Rebasable }

impl Document {
    /// The one rule every async consumer applies before using a result.
    pub fn classify<T>(&self, r: &Stamped<T>, kind: ResultKind) -> Freshness {
        match kind {
            // Positions are anchor-based → the anchor index already carried them forward; usable.
            ResultKind::AnchorBound if r.base_revision <= self.revision => Freshness::Rebasable,
            // Offset/line-number-based payloads (raw LSP ranges, byte offsets) are only valid
            // at their base revision.
            _ if r.base_revision == self.revision => Freshness::Current,
            _ => Freshness::Stale,
        }
    }
}
```

Drop/rebase decision:

- **`Current`** — apply directly.
- **`Rebasable`** — the payload's positions are **anchors** (INV-ANCHOR), so the edits between
  `base_revision` and now have already moved them; map through the anchor index and apply. This is how a
  late diagnostic or decoration set still lands correctly rather than being thrown away on every
  keystroke.
- **`Stale`** — a *raw-offset* payload computed against a superseded revision (e.g. an LSP response using
  byte/line-col against text that has since changed and was not anchored). **Drop it**; the coalescer
  (§5) has already scheduled a re-request against the current revision.

The consumer never guesses: raw-offset async payloads are converted to anchors *at ingestion against
`base_revision`* whenever possible (using the snapshot for that revision if still alive), which upgrades
them from `Stale` to `Rebasable`. Payloads that cannot be anchored (e.g. a whole-document reformat) are
strictly `Current`-or-drop.

Ordering is enforced by the single-threaded deterministic executor ([architecture.md §8](../architecture/architecture.md)):
results are *ingested* on the main loop in arrival order, but **applied** only after the freshness check,
so a slow answer can never overwrite a newer one. Per-consumer, only the highest `base_revision` result
is retained (last-writer-by-revision, not last-writer-by-arrival).

### 4. The query catalog (Q-CATALOG)

The catalog is the fixed read surface — the read counterpart of the semantic Command list. Each entry is a
`Query` impl returning a value type. These are the six the render layer, palette, and plugins depend on;
the set is **additive** (INV-ADDITIVE).

| Query | Reads | Returns (DTO) | Notes |
| --- | --- | --- | --- |
| `get_visible_lines` | doc + view | `VisibleLines { revision, lines: Vec<LineDto> }` | Only the `visible` span; feeds the render snapshot. Never materializes off-screen lines. |
| `get_cursor_selection` | view | `CursorSelection { cursor: PositionSet, selections: Vec<RangeDto> }` | View-local (INV-DOC-VIEW); positions in typed units (INV-POS-TYPED). |
| `get_diagnostics` | doc | `Diagnostics { revision, by_namespace: Vec<(Namespace, Vec<DiagnosticDto>)> }` | Producer/consumer split (NVIM-DIAG-1); anchor-backed so they track edits. |
| `get_render_snapshot` | doc + view | `RenderSnapshot { revision, lines, decorations, gutter }` | The immutable input to render-IR lowering ([render-and-frontends.md §3](render-and-frontends.md)); includes resolved decorations from §6. |
| `get_available_commands` | workspace + view | `Vec<CommandDto>` | Context-filtered ([architecture.md §2.3](../architecture/architecture.md)); computed from a cached index, not re-indexed per call (PERF). |
| `get_symbols` | doc | `Symbols { revision, tree: Vec<SymbolDto> }` | From the last completed parse/LSP `documentSymbol`; `revision` may lag current (stale-marked, not blocking). |

Every DTO that carries positions carries them as typed coordinates (`Position { line, utf16_col, .. }`,
INV-POS-TYPED) and stamps its `revision`, so a consumer can display "diagnostics for rev N while you type
rev N+3" honestly instead of blocking or drifting.

`get_render_snapshot` is the hot path: it is called per redraw, reads only `view.visible`, and is O(visible
lines) — **not** O(document) (see §7).

### 5. Background consumers read snapshots, not live buffers (Q-BG)

Parser (`C-TS`), LSP host (`C-LSPHOST`), and git all run off the main loop. Each is handed a
`DocumentSnapshot` (a `Send` value) and answers with a `Stamped<T>`. They never touch the live document
(guards TEXT-16; [neovim.md NVIM-TS design note](../parity/neovim.md)).

```rust
pub struct ParseJob { snap: DocumentSnapshot }   // owns an immutable view at snap.revision

impl BackgroundJob for ParseJob {
    type Out = Stamped<SyntaxTree>;
    fn run(self, cancel: &CancelToken) -> Option<Self::Out> {
        let tree = incremental_parse(&self.snap.text, cancel)?;   // reads the frozen rope
        Some(Stamped { request: self.request_id, base_revision: self.snap.revision, payload: tree })
    }
}
```

**Coalescing and cancellation are the scheduler's job** (INV-SCHED-1; [D-018](../../spec/DECISIONS.md)),
consumed here as a contract. The read side requires exactly this shape:

- **Coalesce duplicates.** Background work is keyed by `(DocumentId, JobKind)`. If a parse for a document
  is already queued and a newer edit arrives, the queued request is **replaced** with one bound to the
  newer snapshot — "do not accumulate unbounded parse requests; apply only the latest"
  ([architecture.md §8](../architecture/architecture.md)). At most one pending + one in-flight job per
  key.
- **Cancel superseded.** An in-flight job for a superseded revision is signalled via its `CancelToken`;
  `incremental_parse` checks it at chunk boundaries and bails (`None`), so CPU is not spent finishing a
  parse whose result would be `Stale`. Cancellation is cooperative — the frozen rope means an
  un-cancelled job that *does* finish still produces a consistent (if old) tree that the freshness rule
  (§3) then classifies.
- **Input and render outrank background** (INV-SCHED-1): taking a snapshot for a foreground redraw never
  waits on a parse.

Result application: a completed `Stamped<SyntaxTree>` is ingested on the main loop; if `Current`/
`Rebasable` it becomes the document's `syntax` `Arc` (visible to the *next* snapshot); if `Stale` it is
dropped and the coalescer already holds a fresh request. The document's `syntax` field is therefore
allowed to lag `revision` — highlighting shows the last good parse and never blocks typing.

Debounce (watcher/edit storms) is applied at the scheduler edge, not per consumer, so a burst of
keystrokes yields one coalesced parse, not one per key.

### 6. The bounded snapshot-scoped decoration provider (Q-DECO) — resolves V-26 / NVIM-EXT-7

Neovim's `set_decoration_provider` runs a plugin callback synchronously **per visible line during
redraw** (NVIM-EXT-7). ruse cannot: plugin code inside the paint pass violates INV-QUERY-SNAPSHOT and
INV-PLUGIN-NO-CORE. The V-26 resolution is a provider that is **bounded, visible-range only,
snapshot-scoped, and runs outside the paint critical section**.

```rust
pub trait DecorationProvider: Send {
    /// Pure over the snapshot; returns ephemeral marks for the visible range only.
    fn decorate(&self, cx: &DecorationCx<'_>) -> DecorationBatch;
    fn namespace(&self) -> Namespace;    // isolates producers (NVIM-EXT-1)
}

pub struct DecorationCx<'a> {
    pub snapshot: &'a DocumentSnapshot,  // immutable; the ONLY document access a provider gets
    pub view: &'a ViewSnapshot,
    pub visible: VisibleRange,           // provider may read ONLY these lines
    pub revision: Revision,              // == snapshot.revision; stamped onto the batch
    pub deadline: Instant,               // budget; overrun ⇒ batch dropped, provider skipped this frame
}

/// Ephemeral: valid for exactly `revision`; never stored in the document.
pub struct DecorationBatch {
    pub revision: Revision,
    pub namespace: Namespace,
    pub marks: Vec<EphemeralMark>,       // hl range / virt_text / virt_lines / sign (NVIM-EXT-2..5)
}
```

Where it runs relative to paint:

```
edit ─▶ commit(rev N) ─▶ [snapshot rev N] ─▶ decoration phase (providers.decorate)  ← OUTSIDE paint
                                                     │  producers run here, on a snapshot,
                                                     │  visible-range only, under deadline
                                                     ▼
                                          get_render_snapshot(rev N)  ─▶  render-IR lowering ─▶ PAINT
                                                                              (no plugin code here)
```

Contract, and how each clause upholds an invariant:

- **Outside the paint critical section.** Providers run in the *decoration phase* that builds the input to
  `get_render_snapshot`, before lowering. Paint consumes an already-materialized `RenderSnapshot` and
  calls no provider (INV-QUERY-SNAPSHOT: "runs outside the paint critical section"; INV-RENDER-IR: paint
  lowers IR, it does not run plugins).
- **Visible-range only.** `decorate` may read only `cx.visible`; the provider is called once per frame
  with the whole visible span, **not once per line**, killing NVIM-EXT-7's per-line callback cost. Result
  is O(visible), not O(document).
- **Snapshot-scoped.** The provider sees a `&DocumentSnapshot`, never the live buffer — so it cannot
  mutate, cannot observe a half-applied edit, and cannot corrupt core state (INV-PLUGIN-NO-CORE).
- **Ephemeral + revision-stamped.** A `DecorationBatch` is stamped `revision` and is *not* persisted; a
  batch computed against a superseded revision is dropped by the §3 rule (unlike persistent extmarks,
  which are anchor-backed and *do* survive edits — the two are distinct surfaces).
- **Deadline-bounded.** A provider that blows `deadline` is skipped for that frame and its prior batch is
  reused or dropped; one slow plugin cannot stall the frame (INV-PLUGIN-ISOLATED). Providers are ordered
  by `namespace` priority for deterministic composition.

Persistent decorations (the extmark model, NVIM-EXT-1..6) remain anchor-backed in `AnchorIndex` and are
read via `get_render_snapshot` directly; `Q-DECO` is only the **per-redraw, computed-fresh** surface. Both
land in the same `RenderSnapshot.decorations`, merged by namespace priority.

### 7. Position / UTF-16 conversion on snapshots, not whole-doc-per-request (Q-U16)

LSP speaks UTF-16 code units; ruse stores UTF-8; the core also needs byte/char/grapheme
(INV-POS-TYPED). The anti-pattern to avoid is "run UTF-16 conversion over the whole document on every
LSP request" ([architecture.md §9](../architecture/architecture.md)). The fix lives in the snapshot:
`LineIndex` carries **prefix sums** so a conversion is O(log n) locate + O(line) within a line, never
O(doc).

```rust
pub struct LineIndex {
    line_starts: Vec<ByteOffset>,     // byte offset of each line start (binary search: O(log n))
    utf16_prefix: Vec<u32>,           // cumulative UTF-16 units up to each line start
    // per-line residual conversion walks only that line's bytes, respecting rope chunk boundaries
}

impl DocumentSnapshot {
    pub fn utf16_to_byte(&self, p: Utf16Position) -> ByteOffset {
        let line_start = self.lines.line_starts[p.line as usize];   // O(1)
        // walk only within the target line, O(line length), on the frozen rope chunk
        self.text.utf16_col_to_byte(line_start, p.col)
    }
    pub fn byte_to_utf16(&self, b: ByteOffset) -> Utf16Position { /* symmetric, O(log n)+O(line) */ }
}
```

Because the index is part of the snapshot, a batch of LSP conversions for one response all run against
**one consistent revision** and share the prefix-sum arrays (built once at commit, §2). The conversion is
per-request-region, not per-request-document. Rope chunk boundaries are respected — the per-line walk
iterates chunks, never assembling the line into a `String` (PERF; guards "respect rope chunk boundaries").

## Failure modes

- **Live-object leakage (the invariant this doc exists to prevent).** Structurally impossible: `Query::Output:
  Send + 'static` forbids returning a borrow; `DocumentSnapshot` exposes no `&mut`/inner-`Rope`. A reviewer
  need only check that no query returns `&Document`/`Rope`.
- **Stale async result applied.** Prevented by Q-STALE: raw-offset payloads at a superseded revision are
  dropped; anchor-bound payloads are rebased. A missed stamp is an assertion (impossible state), not a
  silent apply (INV-ERR-CLASS).
- **Snapshot pins memory.** A background job holding an old `DocumentSnapshot` keeps the old rope root
  (and its `Arc` indices) alive; a runaway job on a huge file retains that version. Bounded by scheduler
  cancellation (§5) and a cap on in-flight jobs per key; snapshots are dropped as soon as a job finishes
  or is cancelled. Worst case is *one* extra document version resident per in-flight job, not unbounded
  history.
- **Provider overrun / panic.** `Q-DECO` deadline skips a slow provider for the frame; INV-PLUGIN-ISOLATED
  contains a panic to the provider, never the frame. The frame still paints (with the previous or no batch
  for that namespace).
- **`syntax`/symbols lag the current revision.** By design: highlighting/symbols show the last completed
  parse, stamped with its revision; consumers render it as "revision N of M" rather than blocking. Never a
  failure, always a labeled staleness.
- **Snapshot of a streaming/append buffer** (`BufferKind::Streaming`, INV-BUFFER-KIND): revision still
  increases per append; a snapshot is a point-in-time cut of the bounded window, not the full history.

## Recovery behavior

- Read paths are **pure and non-mutating**, so a query failing (e.g. an out-of-range visible span after a
  concurrent resize) recomputes on the next frame from a fresh snapshot; there is no state to roll back
  (contrast the write side's transaction rollback).
- On a core-invariant panic elsewhere, `INV-FAIL-BOUNDED` triggers a recovery snapshot + safe shutdown;
  the query layer's snapshots are exactly the cheap, immutable material a recovery dump can serialize
  without racing the live document.
- A dropped `Stale` result is *recovered* by the coalescer's already-pending fresh request (§5) — the
  system self-heals to the current revision without operator action.

## Security impact

- Snapshots and DTOs are the boundary that keeps plugins/remote from ever holding a mutable core handle
  (INV-PLUGIN-NO-CORE, INV-TRUST-1). A third-party decoration provider gets a read-only view of the
  *visible range only* — it cannot exfiltrate off-screen document content per frame, and cannot mutate.
- `WorkspaceSnapshot` for remote clients is a DTO; no path/handle in it dereferences into local core state
  (INV-REMOTE-FIRST: local path ≠ workspace path).
- Untrusted async results (LSP/remote) are revision-checked and anchor-normalized before touching state;
  a malicious or buggy server cannot place a decoration at an arbitrary live offset — it flows through the
  same freshness + anchor path (INV-ORIGIN records the origin).

## Performance impact

The read side is on the keystroke and redraw hot paths; the O-bounds are the point.

- **Snapshot = O(1).** One rope-handle clone + a few `Arc` bumps, independent of document size
  ([architecture.md §9](../architecture/architecture.md): no deep-copy snapshots). Safe to take per
  keystroke and per redraw.
- **No O(doc) per keystroke.** Auxiliary indices rebuild incrementally at commit over the touched range
  only; anchors update once per transaction, off O(anchors × edits) (INV-ANCHOR, PERF-6). Parsing is
  incremental and coalesced, never full-parse-per-keystroke (PERF-3).
- **`get_render_snapshot` = O(visible lines).** Reads `view.visible` only; off-screen lines are never
  materialized (guards "do not treat the visible region and the whole document the same").
- **`Q-DECO` = O(visible) per frame**, one provider call for the whole span — eliminates NVIM-EXT-7's
  per-line callback cost.
- **`Q-U16` = O(log n) + O(line)** via prefix sums; never a whole-document UTF-16 scan per LSP request
  (PERF). A batch of conversions shares one snapshot's arrays.
- **`get_available_commands`** reads a cached, incrementally-maintained index; never a full palette
  re-index per call (PERF).
- **No live-object leakage** ⇒ no defensive deep copies elsewhere: consumers share one immutable rope root
  instead of each cloning text.

Budgets are enforced by [D-019](../../spec/DECISIONS.md) per-stage p95/p99; the snapshot/query stages are
tracked there.

## Compatibility impact

- The query catalog (§4) is **additive** (INV-ADDITIVE): new queries and new DTO fields may be added;
  readers ignore unknown fields. Removing/renaming a query follows the command-ID deprecation window
  ([architecture.md §2.2](../architecture/architecture.md)).
- `DocumentSnapshot`/`ViewSnapshot`/`DecorationBatch` are **internal** value types on the plugin ABI
  boundary; plugins receive their **DTO projections** over the versioned protocol (INV-CONTRACT-FIRST,
  INV-PROTOCOL-VERSIONED), so changing an internal field is not an API change.
- `Q-DECO` deliberately diverges from NVIM-EXT-7's signature (batch-per-visible-range vs callback-per-
  line). Parity target is the *capability* (per-redraw computed decorations), not the wire shape
  ([neovim.md](../parity/neovim.md) parity philosophy); L2.

## Observability

- Snapshots are the natural unit for the `:debug document` surface ([render-and-frontends.md §5](render-and-frontends.md)):
  dumping revision + ranges is dumping a snapshot.
- The stale-drop path (§3) emits a counter per consumer (`stale_dropped`, `rebased`, `current`) so
  "diagnostics feel laggy" localizes to *drops* vs *slow producer* vs *coalescing*.
- Per-frame decoration timing per namespace (which provider is near its deadline) is exposed for
  `:debug render-tree`.
- `INV-STATUS`: C-QUERY reports a per-component health state (e.g. `Degraded` when parse/LSP lag exceeds a
  threshold) into the Health Registry; the status bar only renders it.

## Alternatives

- **Direct in-process reads with no snapshot (getters on `&Document`).** Fine — and *allowed* — for
  main-thread readers that don't cross a boundary ([render-and-frontends.md §2](render-and-frontends.md):
  do not over-apply CQRS). We keep direct getters for those; snapshots are mandatory only where a value
  must **outlive the current tick** or **cross to a worker / plugin / remote**. This doc specifies the
  latter without forcing the former.
- **Immutable persistent map for *all* state (à la a global immutable store).** Clean, but makes
  view-local and workspace state pay rope-style structural-sharing overhead they don't need, and blurs
  INV-DOC-VIEW. Rejected in favor of separate `Document/View/Workspace` snapshots.
- **MVCC with explicit version GC.** The rope already gives MVCC for free (old roots stay alive while a
  snapshot holds them; dropped when the last snapshot drops). An explicit version table would duplicate
  what `Arc` refcounts already do. Kept the implicit form; the only added machinery is the in-flight-job
  cap (§Failure modes).

## Rejected approaches

- **Rejected: snapshot = deep copy of the document.** Directly banned ([architecture.md §9](../architecture/architecture.md));
  O(doc) per snapshot destroys the keystroke budget. → O(1) structural share (§2).
- **Rejected: queries return `&Document` / the inner `Rope` / any `&mut`.** A stale or aliased mutable
  handle can corrupt core state and couples every reader to internals (the exact hazard INV-QUERY-SNAPSHOT
  names). → value-typed `Output` (§1).
- **Rejected: apply async results whenever they arrive.** Drifts decorations/diagnostics and re-creates
  the Neovim fast-context bug (NVIM-ASYNC). → revision stamp + freshness rule (§3), single-threaded
  deterministic ingestion (INV-ASYNC-ORDER).
- **Rejected: NVIM-EXT-7 verbatim (plugin callback per visible line during redraw).** Puts plugin code in
  the paint critical section, violating INV-QUERY-SNAPSHOT / INV-PLUGIN-NO-CORE (V-26). → bounded,
  visible-range, snapshot-scoped, deadline-bounded provider run *before* lowering (§6).
- **Rejected: convert the whole document to UTF-16 per LSP request.** O(doc) per request (PERF). →
  prefix-sum `LineIndex` on the snapshot, O(log n)+O(line) (§7).
- **Rejected: background workers read the live buffer under a lock.** Blocks the main loop / risks reading
  a half-applied edit. → hand each worker an immutable `DocumentSnapshot` (§5; TEXT-16).

## Migration strategy

Greenfield component (`C-QUERY`, `status: planned`); no legacy read API to migrate. Sequencing:

1. Land `DocumentSnapshot` + `Revision` + `LineIndex`/`AnchorIndex` `Arc` rebuild-on-commit alongside
   `C-DOCUMENT`/`C-TRANSACTION` (they are the substrate for both `Q-SNAP` and `Q-U16`).
2. Land the `Query` trait + `QueryEngine` and the six catalog entries; wire `C-RENDER` to
   `get_render_snapshot` (C-RENDER `depends_on: [C-QUERY]` in [PRD.yaml](../../spec/PRD.yaml)).
3. Land `Stamped<T>` + freshness rule; wire the first background consumer (`C-TS` tree-sitter) through the
   scheduler with coalescing/cancellation.
4. Land `Q-DECO` when the plugin host (`C-PLUGIN`) needs computed decorations — post-MVP, gated with the
   scheduler budgets ([D-018](../../spec/DECISIONS.md)).

No feature flag needed: the read side has no user-visible behavior of its own; it is exercised through
render, palette, and diagnostics.

## Test strategy

- **Property (INV-QUERY-SNAPSHOT):** for a random edit sequence, a `DocumentSnapshot` taken at rev N reads
  identically before and after later edits — outstanding snapshots are immutable. A compile-fail test
  asserts no query signature returns a borrow or `&mut`.
- **Property (INV-ASYNC-ORDER, Q-STALE):** inject out-of-order `Stamped<T>` results under random edit
  interleavings; assert applied state always reflects the highest `base_revision`, raw-offset stales are
  dropped, and anchor-bound results are rebased to correct positions.
- **Property (INV-ANCHOR × snapshot):** diagnostics/decorations placed at rev N and edited to rev N+k land
  at the anchor-correct positions in `get_render_snapshot(N+k)`.
- **Perf (D-019):** `snapshot()` is O(1) — timing flat across 1 KB … 200 MB documents; `get_render_snapshot`
  and `Q-DECO` scale with visible lines, not document size; `Q-U16` batch does not scale with document
  size. Regressions gate on main/nightly.
- **Concurrency:** background parse holds an old snapshot while foreground applies 10k edits; assert the
  parse output is internally consistent with its `base_revision` and is classified correctly on ingest.
- **Q-DECO bound:** a provider that reads outside `visible` or overruns `deadline` is rejected/skipped;
  paint proceeds; a panicking provider does not abort the frame (INV-PLUGIN-ISOLATED).

## Open questions

- **OQ-1** — Exact scheduler budgets/deadlines for the decoration phase and background reads: open under
  [D-018](../../spec/DECISIONS.md) until real F-011/F-014/F-015 workloads exist. This doc fixes the shape
  (coalesce/cancel/deadline), not the numbers.
- **OQ-2** — Whether `ViewSnapshot.view_seq` needs to participate in stale-drop for *view-only* async
  consumers (e.g. a background lens that depends on cursor), or whether `doc_revision` alone suffices.
  Ties into multi-client sequencing ([D-012](../../spec/DECISIONS.md)).
- **OQ-3** — Rebase policy for *partially* anchorable async payloads (some ranges anchor, some don't, e.g.
  a code-action edit spanning a deleted region): drop whole vs apply anchorable subset.
- **OQ-4** — Snapshot memory ceiling: the max number of distinct live document versions (in-flight jobs)
  before back-pressure kicks in on a very large file; needs a workload-driven cap.
- **OQ-5** — Whether `get_symbols`/`syntax` staleness should be surfaced to the user as an explicit
  indicator or remain silent-until-threshold (interacts with INV-STATUS thresholds).

## Reference Invariants (this doc)

Enforces:

- **INV-QUERY-SNAPSHOT** — queries return immutable snapshots/DTOs, never live mutable core objects; the
  per-redraw decoration provider (`Q-DECO`) is bounded to a visible-range snapshot and runs outside the
  paint critical section. *Realized by* §1 (value-typed `Query::Output`), §2 (immutable
  `DocumentSnapshot`), §6 (decoration phase before lowering).
- **INV-ASYNC-ORDER** — single-threaded deterministic ingestion; every snapshot and async result carries a
  revision; stale results dropped. *Realized by* §2 (`Revision` on every snapshot), §3 (`Stamped<T>` +
  freshness rule), §5 (ordered ingestion).

Depends on / reaffirms: **INV-ANCHOR** (snapshot positions are anchors; rebasable results), **INV-POS-TYPED**
(typed coordinates in every DTO; `Q-U16`), **INV-HANDLE** (typed snapshot targets), **INV-SCHED-1**
(coalescing + cancellation of background reads), **INV-PLUGIN-NO-CORE** (plugins see snapshots/DTOs only),
**INV-DOC-VIEW** (`ViewSnapshot` separate from `DocumentSnapshot`), **INV-RENDER-IR** (the render snapshot
feeds lowering; it is not backend bytes), **INV-BUFFER-KIND** (streaming/append buffers snapshot a bounded
window), **INV-ADDITIVE** (the query catalog evolves additively).

## Trade-offs

- **Snapshots everywhere at boundaries add a value type and an `Arc`-rebuild-on-commit discipline.**
  Accepted: it is the only structure that makes INV-QUERY-SNAPSHOT and INV-ASYNC-ORDER *structural* rather
  than conventional, and it is O(1) to take — the cost is at commit (paid once per transaction), not at
  read (paid per keystroke/redraw).
- **`Q-DECO` diverges from NVIM-EXT-7's ergonomics** (batch-per-range, not callback-per-line). Accepted:
  it is the price of keeping plugin code out of the paint pass while still delivering per-redraw
  decorations (V-26).
- **Two decoration surfaces** (persistent anchor-backed extmarks + ephemeral `Q-DECO` batches). Accepted:
  they have genuinely different lifetimes (survive-edits vs valid-for-one-revision); collapsing them would
  force one to pay the other's cost. They merge only at `RenderSnapshot.decorations`.
