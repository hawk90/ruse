---
doc: anchor-store
project: ruse
title: "ruse — Anchor Store (the long-lived-position primitive)"
summary: >
  The design of the anchor store (C-ANCHOR, D-023): the single primitive every long-lived position rests on
  — cursors, selections, marks, diagnostics, decorations, fold ranges. Specifies the boundary-offset model,
  the Before/After bias (extmark/Vim/Emacs gravity) with a full truth table, the exact transaction update
  rule including the span-delete collapse and the Clamp/Invalidate policy, the batched-update algorithm that
  meets the "not O(anchors × edits)" cost invariant, resolution to typed positions against a revision, and
  the detach/re-anchor contract for persistence and reload. This elaborates the Anchor sketch in RFC-0008 §3
  into an implementable contract; it introduces no new decision (D-023 already decided the direction).
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - ../rfc/proposed/RFC-0008-document-model.md
  - ../rfc/proposed/RFC-0007-transaction-engine.md
  - positions-history.md
  - editing-language.md
  - query-and-snapshot.md
  - ../parity/vim.md
  - ../parity/neovim.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
---

# Anchor Store

<!-- Design elaboration of D-023 / INV-ANCHOR. State lives in spec/ YAML; this is the contract prose.
     Stable {#ids} are given only to boundaries other docs/tests cite. -->

## Problem

Everything the editor holds across time — a cursor, a selection, a `'a` mark, an LSP diagnostic, a
decoration, a fold range — is a position that must **survive edits made elsewhere**. Storing a raw offset
and fixing it up per edit is O(anchors × edits) and gets the boundary cases wrong (does a highlight grow
when you type at its end? does a mark inside a deleted span vanish or clamp?). RFC-0008 §3 fixes the
*direction* — long-lived positions are **anchors** with affinity/gravity (INV-ANCHOR, D-023) — but leaves
the bias rules, the span-delete behavior, and the update algorithm as a struct sketch. Five subsystems
block on those details: transaction apply (RFC-0007), positions-history (C-POSHIST), decorations,
diagnostics, and every View's cursor/selection. This doc specifies them.

## Goals

- **G1** One authoritative per-document **anchor store**; no subsystem keeps its own offset bookkeeping.
- **G2** A **total, deterministic** update rule: for any anchor and any transaction, exactly one result
  position, boundary cases included.
- **G3** Bias that reproduces **extmark / Vim / Emacs** edge behavior (parity: NVIM-EXT-1/6, cursor/mark
  feel), expressed as a small truth table, not prose.
- **G4** Update cost **not O(anchors × edits)** (INV-ANCHOR, PERF-6): a transaction of `E` edits over a
  store of `A` anchors updates in `O(A + E)` after sorting.
- **G5** Anchors resolve to **typed** positions (byte/char/grapheme/screen) against a specific revision
  (INV-POS-TYPED), never a bare `usize`.
- **G6** A **detach → re-anchor** contract so positions persist across buffer unload and file reload
  (feeds positions-history `Detached` / global marks / bookmarks).

## Non-goals

- The navigation-history model over the store (jumplist / mark-ring / selection-sets) — that is
  [positions-history.md](positions-history.md) (C-POSHIST); this doc is the primitive **below** it.
- The on-disk session file format — owned by [persistence-and-recovery.md](persistence-and-recovery.md).
- Screen-cell layout / wrapping — a **resolution target** here, specified in
  [render-and-frontends.md](render-and-frontends.md).
- The fuzzy content-match re-anchor heuristic on reload — interface here; algorithm is an Open Question.

## Terminology

Reuses the [glossary](../../spec/glossary.yaml): **Anchor**, **Revision**, **Snapshot**, **Handle**. New
local terms:

- **Offset** — a position **between** two code units (a gap), `0..=N` over a document of `N` units in the
  document's canonical unit (bytes, RFC-0008). Half-open: `0` is before the first unit, `N` after the last.
- **Bias** — which side an anchor clings to at a boundary: `Before | After` (§[Bias](#anchor-bias)).
- **Edit** — one normalized replacement `(pos, del, ins)`: delete `del` units at `pos`, insert `ins` units.
  A Transaction (INV-TXN) is a set of **disjoint, position-sorted** edits.
- **Invalidation policy** — per-anchor `Clamp | Invalidate`, chosen when an anchor's span is deleted
  (§[Update rule](#anchor-update)).

## Invariants

Depends on (does not restate): **INV-ANCHOR** (long-lived positions are anchors with affinity/gravity; cost
not O(anchors × edits)), **INV-POS-TYPED** (typed coordinates), **INV-HANDLE** (generation-checked handles),
**INV-TXN** (mutation via Transaction), **INV-QUERY-SNAPSHOT** (reads are snapshots). Guards it enables:
TEXT-4/5/6, PERF-6.

## Proposed design

### 1. The anchor value {#anchor-value}

```
AnchorId   = { slot: u32, gen: u32 }        // INV-HANDLE: freed-slot reuse ⇒ gen mismatch ⇒ assert
Anchor     = { offset: Offset,              // canonical-unit gap position
               bias:   Before | After,      // boundary behavior (§Bias)
               policy: Clamp | Invalidate }  // span-delete behavior (§Update rule)
```

An anchor is **not** a coordinate you read directly; it is a handle into the store. You `resolve(id, rev)`
to obtain a typed position valid for revision `rev` (§[Resolution](#anchor-resolve)). Holding an
`AnchorId` across a freed anchor is an invariant violation (assert, INV-HANDLE), never a silent wrong
answer.

### 2. Bias: boundary behavior {#anchor-bias}

An anchor sits in a gap. The only ambiguous edit is one whose **insertion point coincides with the anchor's
offset**: does inserted text land to the anchor's right (anchor stays) or left (anchor advances)? `bias`
decides:

| bias | at an insertion `pos == offset` | clings to | extmark | typical use |
| --- | --- | --- | --- | --- |
| `Before` | inserted text goes **right**; `offset` unchanged | preceding text (left) | `right_gravity=false` | a range's **end** that should not grow; region right edge |
| `After`  | inserted text goes **left**; `offset += ins` | following text (right) | `right_gravity=true` (default) | a plain cursor/mark; a range's **start** |

**Ranges.** A region is `(start: Anchor, end: Anchor)` with **independent** bias, giving all four edge
behaviors. The common "grow with text typed inside, fixed at both outer edges" decoration is
`start.bias = After`, `end.bias = Before`.
A "grow at every edge including the boundaries" selection is `start=Before`, `end=After` (Neovim
`right_gravity=false` + `end_right_gravity=true`).

### 3. The transaction update rule {#anchor-update}

For a **single** edit `(pos, del, ins)`, let `Δ = ins − del` and the affected closed interval be
`[pos, pos+del]`. For an anchor at `offset = a` with the given `bias` and `policy`, the new offset `a'` is
the delete-then-insert composition — the deleted span collapses to `pos`, then the insertion is placed by
bias — which is how nvim moves an extmark under `nvim_buf_set_text` (verified against nvim v0.12.6, #941):

```
if a < pos:                      a' = a                      # wholly before the edit
elif a > pos + del:              a' = a + Δ                  # wholly after: shift by the delta
else:                            # a ∈ [pos, pos+del] — the affected interval
    a' = pos        if bias==Before else pos + ins           # collapse, then take the bias
    if pos < a < pos+del and policy == Invalidate:
        mark a invalidated (still resolvable, flagged)       # strictly inside a deleted span
```

Notes and consequences:

- **Pure insertion** (`del == 0`): the interval is the single point `a == pos`; only the bias branch runs
  (`Before → pos`, `After → pos + ins`). This is the whole of §[Bias](#anchor-bias).
- **Pure deletion** (`ins == 0`): every anchor in the span lands on `pos`, whatever its bias.
- The mapping is **monotonic within each bias** (`a1 ≤ a2 ⇒ a1' ≤ a2'` for two anchors of one bias), and
  at one offset `Before' ≤ After'`. So a `(Before, Before)`, `(After, After)` or `(Before, After)` range never
  inverts. An `(After, Before)` range ("fixed at its outer edges") whose whole extent a replacement covers
  can cross (`start → pos+ins`, `end → pos`), as an nvim extmark range does; its reader clamps it empty.
  The earlier rule here collapsed inside anchors to `pos` but sent an `After` anchor at `pos` to `pos+ins`,
  which inverted two `After` anchors (#941). The store asserts its order in debug builds.

#### Span delete & the Clamp/Invalidate policy {#anchor-span}

When `pos < a < pos+del` the two units the anchor's gap sat between are **both** gone — the anchor
**collapses into the replacement**, at its start (`Before`) or its end (`After`), matching extmark behavior.
`Clamp` keeps it live there (a cursor's caret lands where the text was removed); `Invalidate` additionally
flags it so a decoration/diagnostic owner can drop it. The choice is per-anchor `policy`.

#### 3.1 Line anchors {#anchor-line}

A **line** anchor (`Track::Line(gravity)`) is how a Vim mark follows an edit: by line, keeping its column,
as nvim's `mark_adjust` / `mark_col_adjust` move marks (#686). A position is `(line, column)`:

- an edit inside its line — or one that starts in its line and splits it, or joins the next line onto it —
  leaves the column alone, clamped onto the line (`x` before a mark does not move it; nvim keeps the
  column);
- whole lines inserted or deleted before it shift its line; a replaced block of whole lines (`:{range}!`,
  `:sort`) keeps it on the line of the same index while one is left;
- an edit that runs into its line from above (a join, a charwise delete across lines) moves it with the
  text after the edit, or to the edit's end if it sat in the removed head;
- when its own line is deleted, `gravity` decides: `Drop` removes it (`a`–`z`, `` `^ ``: `E20` after —
  the anchor stays resolvable at the edit, flagged `invalidated`), `Keep` moves it to the line that took
  the deleted line's place with its column (`A`–`Z`, jumplist, change list, `<`/`>`; nvim's
  `ONE_ADJUST_NODEL`), `Cursor` to column 0 of the line above (another window's cursor).

A whole-line edit is one whose span starts and ends at line starts and inserts whole lines (`dd`, `O`,
`:put`), or one that starts at a line's `\n` and ends at one or the buffer end and inserts newline-led lines
(`dd` of the unterminated last line, `o`); with no other clue the first reading wins (`o` and `O` on an
empty line are the same bytes). Known differences from nvim: a charwise delete across lines leaves a mark
on the last line on the text after the edit (nvim adds the kept head's length to the mark's old column), a
`J` from an empty line drops that line's marks, and `:m` keeps marks on their line numbers (nvim moves them
with the lines). The rule is `LineMap` in `anchor.rs`; it reads the text before and after the change.

### 4. Batched update & cost {#anchor-cost}

A Transaction applies its **whole disjoint edit set at once**, not edit-by-edit, and an anchor reached by
several edits — a delete ending where an insert begins, or an `After` anchor carried to the end of one
replacement where the next edit starts — goes through each in turn in the frame the edits before it leave,
so a batch gives exactly the answer of applying its edits one by one (#758). INV-ANCHOR / PERF-6 (**not**
O(anchors × edits), and since #1122 not O(anchors) per edit either):

```
apply(txn, store):
  if anchors were inserted/removed since the last apply: sort the store once     # O(A log A), amortized
  regions = each edit's lines with one line either side (a byte-only apply: its closed span),
            merged where they meet, each with its summed Δ
  for region in regions, right to left:
     find the anchors in the region by binary search over the offset order       # O(log² A)
     map each from its old offset by §3 (byte) or §3.1 (line); re-sort them in place
     add the region's Δ to every later rank in a Fenwick tree of pending shifts  # O(log A)
```

- **Cost:** `O(R log² A + touched)` per transaction for `R` regions, plus `O(log E)` per touched anchor;
  anchors outside every region are never visited (their pending shift is read on `resolve`, `O(log A)`).
  An insert or remove only marks the order stale, so a batch of creations (`:g`'s line anchors) is sorted
  once, at the next edit.
- The frozen snapshot index (§5) is built on the first snapshot after a change, not on every change.
- A View's marks are not in the store: the View hands them to the Document for each transaction
  (`Document::apply_tracked`), and a View at rest pins them (`Document::pin_tracked`); both are mapped by
  the same rules at `O(log E)` each, with no ordering to keep.
- Measured by the `anchor_apply` bench (`crates/core/benches/anchor_apply.rs`).

### 5. Resolution to typed positions {#anchor-resolve}

`resolve(id)` on the store returns the anchor's current position, and `DocumentSnapshot::resolve(id)` returns it
**as of the snapshot's revision** through a frozen `AnchorIndex` (INV-QUERY-SNAPSHOT), so a snapshot resolution
never sees a half-applied edit. The canonical unit is bytes; conversions to
char / grapheme / UTF-16 column / screen cell go through the document's coordinate layer (INV-POS-TYPED,
[render-and-frontends.md](render-and-frontends.md)) — the anchor store never stores a screen coordinate
(it changes on resize with no edit). The API takes no revision: to read an older revision, the caller holds a
snapshot of it. An anchor not live at that revision resolves to `None`.

### 6. Detach & re-anchor {#anchor-detach}

When a buffer unloads or a file is reloaded from disk, live anchors **detach** to a re-anchorable coordinate
(feeds positions-history `Detached`, global marks, bookmarks — [positions-history.md](positions-history.md)):

- **Detach:** `AnchorId → DetachedPos { line, col, context_hash }` where `context_hash` fingerprints a small
  window around the position. Deterministic, cheap, order-independent.
- **Re-anchor on load:** exact `(line, col)` if the surrounding window still hashes equal; else a bounded
  search re-anchors to the nearest matching window; else the anchor resolves to the clamped `(line, col)`
  and is flagged `reanchor_approx`. The **exact fuzzy-match algorithm is an Open Question**; the *interface*
  and the deterministic exact/clamped fallbacks are fixed here.

## Failure modes

- **Stale `AnchorId`** (freed generation): assert (INV-HANDLE) — an impossible state, not a runtime error.
- **Span-deleted anchor:** never silently wrong — `Clamp` → live at `pos`; `Invalidate` → flagged for the
  owner to drop. No dangling offsets.
- **Range inversion:** prevented within each bias by the monotonic update (§3); a debug assert on the store's
  order catches any regression. An `(After, Before)` range a replacement covers can cross, as in nvim (§3).
- **Resolve an anchor not live at the snapshot's revision:** `None`.

## Recovery behavior

Anchors are **derived** state, rebuildable from their owners (a diagnostic set, a decoration provider, the
View's cursor). The recovery journal (D-005) persists **owners + their DetachedPos**, not live store slots;
on restart the store is reconstructed and re-anchored (§6). A crash mid-transaction leaves the store at the
last committed revision (INV-TXN atomicity) — anchors are never observed half-updated.

## Security impact

None direct. `context_hash` must not leak document content across a trust boundary — it is a local
fingerprint, never sent to a plugin/remote peer as recoverable text (ENG-TRUST-001).

## Performance impact

Governed by PERF-6 / G4: `O(A + E)` per transaction, `O(log A)` per resolve/insert/remove, `O(A)` memory.
The store is the hot path of every keystroke (cursor + visible decorations update per edit); the batched
sweep, not per-anchor fix-up, is what makes large decoration/diagnostic sets viable. Concrete p95/p99
budgets are set with the scheduler budgets (D-018/D-019), not fixed here.

## Compatibility impact

Internal kernel contract (C-ANCHOR, build_stage kernel); no wire format. Plugins/remote peers see resolved
**typed positions** over the versioned protocol, never `AnchorId`s or store internals (INV-PLUGIN-NO-CORE,
protocol contract). Bias/policy are exposed to decoration APIs as the extmark-style flags in §2.

## Observability

Each anchor carries its owner tag (correlation, ENG-OBS-001). The store exposes counts by owner and a debug
resolve-trace (which edit moved an anchor, and how) so an "off-by-one after edit" bug is inspected, not
guessed (mirrors the offline `render-diff`/inspector tooling).

## Alternatives

- **Marker/piece-table markers** (Xi rope "spans"): equivalent cost; folded into G4's "rope leaf metadata"
  structure option.
- **Operational-transform positions:** needed only for concurrent multi-writer editing (D-012, deferred);
  the single-writer transaction model here is simpler and sufficient until then.

## Rejected approaches

- **Raw offsets fixed per edit** — O(anchors × edits), violates INV-ANCHOR; and gets boundary cases wrong.
- **Five independent stores** (one per surface: cursors, marks, diagnostics, …) — duplicated lifecycle and
  five subtly different bias rules; rejected for the single store (G1, mirrors positions-history G1).
- **Line/column as the stored form** — breaks on multi-byte/grapheme edits and is O(n) to re-derive; bytes
  are canonical, typed units are a resolution.

## Test strategy

This subsystem is where the missing differential corpus starts (`tests/property/anchor-*`,
`tests/parity/extmark-*`):

- **Bias truth table** (§2): one case per (bias × edit-at-boundary) cell; exact expected offset.
- **Span-delete** (§[span](#anchor-span)): inside / left-edge / right-edge × `Clamp`/`Invalidate`.
- **Property — round-trip:** for a random anchor set and a random sequence of transactions, resolving after
  applying-then-undoing every transaction restores the original offset of each anchor no span delete touched
  (INV-UNDO round-trip). An anchor inside a deleted span collapsed, so it does not recover its offset (§3).
- **Property — monotonicity & no-inversion:** ordering preserved under any edit set.
- **Property — batch ≡ sequential:** applying a disjoint edit set as one batch equals applying its edits
  left-to-right individually (validates §4).
- **Parity — extmark:** replay a Neovim extmark corpus (`right_gravity` / `end_right_gravity` cases,
  NVIM-EXT-1/6) and diff resolved positions.
- **Cost:** assert update time scales `O(A + E)`, not `O(A × E)`, on a large-anchor benchmark.

## Open questions

- **OQ-1** The fuzzy re-anchor algorithm on reload (§6) — window size, hash, search bound, and the
  precedence of exact vs approx. Interface fixed; heuristic deferred.
- **OQ-2** Whether `Invalidate` anchors are swept eagerly on the transaction or lazily on next resolve
  (memory vs latency); leans lazy.
- **OQ-3** Cross-anchor **constraints** (e.g., a fold whose end must stay ≥ start+1) — expressed as an owner
  post-pass or as a store-level constraint; likely owner post-pass to keep the store rule total and simple.
