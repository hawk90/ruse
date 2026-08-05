---
doc: rfc
project: ruse
title: "RFC-0007: Transaction Engine"
summary: >
  The Transaction is the single, mandatory path to mutate an editable Document. It carries a
  base_revision and an origin, strictly increases the Document revision, is undoable by logical
  unit, and records the metadata that makes every change traceable. This RFC locks the
  transaction/undo contract (INV-TXN, INV-UNDO, INV-ORIGIN), the branching+chronological undo
  model that Vim g-/g+/:earlier requires, the ephemeral/append-only buffer exception
  (INV-BUFFER-KIND), and preflight (block-before-apply). It defers undo-grouping boundaries and
  the temporal-index format (D-005), and delegates the dot-repeat change-intent to the
  editing-language engine (D-025).
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - ../../architecture/architecture.md
  - ../../design/stability-and-observability.md
  - ../../invariants/reference-invariants.md
  - ../../parity/common.md
  - ../../parity/vim.md
  - ../../../spec/DECISIONS.md
---

# RFC-0007: Transaction Engine

- **Status:** proposed
- **Author(s):** ruse core
- **Created:** 2026-08-05
- **Decision link:** D-001 (transaction is the only path); relates to D-005, D-025

<!-- Hard-to-reverse: this defines the document/transaction boundary — the first of the five axes
     to lock (architecture.md §12.1). -->

## Summary

Every mutation of an **editable Document** goes through a **Transaction**: an atomic, undoable
change that names the revision it was built against (`base_revision`), declares who caused it
(`origin`), and, when applied, strictly increases the Document revision. No code inserts or deletes
text by any other path. Undo is recorded by **logical unit** (not per keystroke) over a **branching
history** that also supports **chronological** traversal for Vim's `g-`/`g+`/`:earlier`. Ephemeral
and append-only buffers (terminal, streaming logs, generated output, large-file/degraded mode) are
an explicit, bounded **exception** to the full-transaction path. Every transaction carries
traceability metadata, and every risky application is **preflighted** (checked before it applies,
never applied-then-recovered). This RFC does not duplicate the design docs it depends on; it
records the decision and its boundaries.

## Motivation / Problem

Neovim entangles editing state, undo tree, marks, and buffer storage (architecture.md §0.2), so
"who changed this and can it be undone cleanly?" has no single answer. ruse's premise
(architecture.md §3.3, DECISIONS **D-001**) is the opposite: **one mutation path**. Concretely, a
transaction engine is the precondition for five otherwise-independent guarantees:

1. **Undo/redo and the undo tree** — COM-7/COM-8, VIM-UNDO. Undo history is only consistent if all
   mutations flow through one recorder.
2. **Traceability** — "why was this character deleted?" (stability §5) is answerable only if each
   change carries `transaction_id`/`correlation_id`/`origin`.
3. **Crash consistency & recovery** — a recovery journal (D-005) and a safe snapshot on invariant
   failure (INV-FAIL-BOUNDED) both key off a monotonic revision.
4. **Deterministic replay / testing** — differential tests (architecture.md §0.3) replay ordered
   transactions; scattered edits cannot be replayed.
5. **Stale-result rejection** — async producers (LSP, AI, plugins) attach a `base_revision`, and
   results built against a superseded revision are dropped (INV-ASYNC-ORDER, §3.4).

Scattered `insert()`/`delete()` calls, or per-keystroke undo, quietly break all five. This RFC
locks the path so they cannot.

## Guide-level explanation

From a command or plugin author's view, you never mutate text directly. You **build** a
transaction against the revision you read, and **request** that it be applied:

- **Read** a `Snapshot` at some `Revision` (INV-QUERY-SNAPSHOT) — an immutable, cheap-to-share view.
- **Describe** the change as a set of edits (typed ranges + replacement text), not as imperative
  cursor pokes. Overlapping edits are normalized; application order is explicit (architecture.md
  §3.3).
- **Tag** it with an `origin` (UserInput | Macro | Plugin | Lsp | AiAgent | RemotePeer) — mandatory
  (INV-ORIGIN).
- **Request** application. The engine **preflights** it (below), and either applies it — advancing
  the revision `N → N+1` and recording an undo entry — or refuses it with a typed error.

You do not choose whether the change is undoable; on an **editable** Document it always is
(INV-UNDO). You do choose **grouping**: a logical unit (e.g. one Insert-mode session, one `dw`, one
multi-cursor edit) is one undo step, regardless of how many keystrokes produced it. Undo groups are
by *logical unit*, not keystroke — this is the decision COM-8/VIM-UNDO-1 validate.

Undo is not a stack. A new change made *after* an undo **branches** the history rather than
discarding the redo tail (VIM-UNDO). Two traversals coexist over that tree:

- **Structural** undo/redo (`u`/`C-r`) walks parent/child edges.
- **Chronological** traversal (`g-`/`g+`, `:earlier`/`:later {N|5m|3f}`) walks states in the order
  they were *created*, across branches. This requires a **temporal index** over the tree — a
  time/sequence-ordered view of nodes — that the tree structure alone does not give (VIM-UNDO-1).

Not every buffer is editable. A terminal, a streaming log, generated output, or a file opened in
large-file/degraded mode is **append-only or ephemeral**: it grows through a lightweight append
path with a bounded (or absent) undo log — you do not get infinite undo of terminal scrollback. A
buffer's *kind* decides which contract applies (INV-BUFFER-KIND).

## Reference-level explanation

Language-independent contract (Rust types below are illustrative, per invariants doc preamble).

### 1. The Transaction and its metadata

A Transaction is atomic (all-or-nothing; no partial mutation on failure — architecture.md §2.3) and
carries the traceability record defined canonically in **stability §5** (not re-specified here):

```
TransactionMetadata {
  transaction_id,        // identity of this change
  correlation_id,        // the input/request that caused it (stability §5 chain)
  origin,                // UserInput | Macro | Plugin(id) | Lsp | AiAgent(id) | RemotePeer(client)
  command_id,            // the semantic command that produced it (INV-CMD-SEMANTIC)
  base_revision,         // the revision the edits were built against
  timestamp,             // injected, not sampled inside pure logic
}
```

The correlation chain `input(correlation_id) → command → transaction → revision → frame`
(stability §5) is what makes a change back-traceable. `origin` is the audit hook for
"AI/plugin changes are reviewed before apply" (architecture.md §10, INV-TRUST-1).

### 2. Application contract (INV-TXN)

On an **editable** Document, applying a transaction:

1. **requires** `metadata.base_revision == document.revision` (else preflight refuses — see §5);
2. **strictly increases** the revision (`assert_eq!(new, old + 1)` conceptually — a revision that
   did not increase is an *invariant violation*, not an error: stability §1);
3. **produces an inverse** (the undo edit) by consistent rules, recorded as an undo entry (§3);
4. is the **only** way text changes — direct scattered insert/delete is forbidden (INV-TXN;
   assert-guarded per stability §1).

Edits within a transaction are normalized: overlaps resolved, multiline edits handled, order made
explicit (architecture.md §3.3). Anchors (cursors, decorations, diagnostics) update via the anchor
store (INV-ANCHOR, D-023), not by re-scanning — anchor cost is not `O(anchors × edits)`.

### 3. Undo model (INV-UNDO)

- **Grouping:** by logical unit, not keystroke. The engine exposes group boundaries (open/extend/
  close) so an Insert session or a compound operator collapses to one step. *Exact boundary rules
  are open — see Open questions / D-005.* Vim's `:undojoin` and `i_CTRL-G_u` (VIM-UNDO) are surface
  controls that map onto explicit group boundaries.
- **Branching history:** the history is a **tree** of states. A change after an undo adds a child
  branch; the prior redo path is retained, never lost (COM-8, VIM-UNDO). History consistency is an
  invariant: a node whose parent does not exist is an assert, not an error (stability §1 lists
  "undo-tree node's parent does not exist" as an invariant violation).
- **Two traversals:**
  - *Structural* — undo/redo along tree edges (`u`, `C-r`; line-level `U` re-doable, VIM-UNDO-1).
  - *Chronological* — `g-`/`g+`, `:earlier`/`:later` walk nodes in creation order across branches,
    including by wall-clock (`5m`) and by file-write count (`3f`). This needs a **temporal index**
    over the tree (a sequence/time-ordered node map). The *format* of that index is open (D-005).
- Both surfaces (Vim undo-tree; Emacs undo-as-undoable + undo-tree package) map onto this one model
  (COM-8). ruse does not build two engines.

### 4. Buffer-kind exception (INV-BUFFER-KIND)

The full-transaction path is the contract for **editable** Documents only. Buffer *kind*
(editable / read-only / generated / streaming / interactive — see parity/workspace.md) selects the
mutation contract:

| Kind | Mutation path | Undo |
| --- | --- | --- |
| Editable | Full Transaction (§2) | Full branching history (§3) |
| Read-only | No mutation | — |
| Generated (search/diagnostics results) | Rebuild, not edit | — |
| Streaming (terminal, log, PTY output) | Append path (not a full transaction) | Bounded or absent |
| Interactive / large-file / degraded | Append or bounded-edit | Bounded undo (COM-12) |

This resolves the INV-TXN ↔ INV-UNDO tension for F-011 / WS-5 / COM-12 (V-4): a terminal buffer
producing thousands of lines per second must **not** allocate a full transaction + inverse per line.
Large files use bounded undo (parity/common.md COM-12: "not a slow normal mode" — a distinct
degraded profile). The exception is explicit and kind-scoped, never an ad-hoc bypass of INV-TXN.

### 5. Preflight — block before apply (stability §13)

Risky applications are **checked before they apply**, not applied-then-recovered. A transaction
that fails any precondition is **refused** with a typed error (INV-ERR-CLASS) and a diagnostic
(stability §11), leaving no partial state. Preflight checks (stability §13):

- **base_revision matches** the current Document revision (else: concurrent modification — the
  producer must rebase and retry);
- **ranges valid** within the document;
- **capability permitted** for the origin (plugin/AI capability-gated, INV-TRUST-1);
- **undo record producible** (an inverse can be formed);
- for save/AI/remote: external-change, conflict, and binary/large-file checks (stability §13).

This upholds INV-FAIL-BOUNDED and INV-TXN jointly: preflight is how "transaction is the only path"
stays *loss-safe* rather than "apply and hope to recover."

### 6. Change-intent for dot-repeat (belongs with D-025)

Vim's `.` repeats the last text-changing command including inserted text, and `g@`/`operatorfunc`
is dot-repeatable (VIM-REPEAT-DOT). A **raw transaction is not a change-intent**: replaying the
inverse edits of the last `cw hello` at a new location is *not* what `.` does — `.` re-parameterizes
the *intent* (operator + object + count + inserted text) against the new cursor context, honoring a
new count (VIM-CNT-INS). Dot-repeat must be distinguished from transaction replay (anti-pattern
VIM-11).

Therefore the **change-intent record** (operator / object / count / inserted-text, re-parameterizable)
is owned by the **editing-language composition engine** (`C-EDITLANG`, **D-025**), *not* by the
transaction engine. The transaction engine's contribution is only that each intent execution
produces exactly one grouped, origin-tagged transaction so the intent and its resulting revision
are correlated for traceability and replay. This RFC records the boundary; the intent IR itself is
specified by D-025 and is out of scope here.

## Reference Invariants

This RFC depends on and enforces these registry invariants (defined in
[reference-invariants.md](../../invariants/reference-invariants.md); not redefined here):

- **INV-TXN** — Every mutation of an editable Document goes through a Transaction carrying
  `base_revision`; applying it strictly increases the revision; scattered direct insert/delete is
  forbidden. (§1, §2, §5)
- **INV-UNDO** — Every transaction on an editable Document is undoable; undo is by logical unit (not
  per keystroke); the history is itself consistent (no orphaned parents). (§3)
- **INV-BUFFER-KIND** — Ephemeral/append-only buffers (terminal/PTY, streaming logs, generated
  output, large-file/degraded) are an explicit exception with bounded/absent undo and an append
  path that is not a full transaction; buffer kind selects the contract. (§4)
- **INV-ORIGIN** — Every mutation has an explicit origin (UserInput | Macro | Plugin | Lsp | AiAgent
  | RemotePeer). (§1)

Also relied upon (owned by other docs): **INV-ASYNC-ORDER** (stale results dropped by revision,
§2/§5), **INV-QUERY-SNAPSHOT** (reads are snapshots, Guide), **INV-ANCHOR** (anchor update cost,
§2), **INV-CMD-SEMANTIC** (`command_id` in metadata, §1), **INV-ERR-CLASS** / **INV-FAIL-BOUNDED**
(preflight refusal is a typed error, §5), **INV-TRUST-1** (capability-gated origins, §5).

## Failure modes & Recovery

- **base_revision mismatch (concurrent modification):** preflight refuses with a typed error
  (DOC-004 class, stability §2.1); the producer rebases against the current snapshot and retries.
  No partial apply.
- **Stale async result:** an LSP/AI/plugin transaction built against a superseded revision is
  dropped, not applied (INV-ASYNC-ORDER); the producer is notified to recompute.
- **Undo-history corruption** (orphaned parent, revision failed to increase, delete range outside
  document): these are **invariant violations**, not errors (stability §1). Fail-fast: stop editing
  → save a recovery file/journal (D-005) → diagnostic snapshot → safe shutdown (INV-FAIL-BOUNDED,
  stability §6).
- **Streaming buffer overflow:** bounded undo/append log is trimmed by policy (INV-BUFFER-KIND);
  data loss here is expected and non-corrupting (it is not editable history).

## Security impact

`origin` is the enforcement point for the trust model: plugin/AI/remote transactions are
capability-gated at preflight (INV-TRUST-1), and AI-origin transactions are held for review before
apply (architecture.md §10, SEC-15). Because every mutation is attributable, a wrong or malicious
change is traceable to its exact source (stability §5). The engine never grants an origin more reach
than its principal's trust level.

## Performance impact

- Do not clone the whole document per command; snapshots are cheap immutable views, not deep copies
  (architecture.md §9). Inverse-edit generation and anchor updates must stay off `O(anchors × edits)`
  and respect rope chunk boundaries (PERF-6, INV-ANCHOR).
- The buffer-kind exception exists **for** performance: high-throughput streaming/terminal buffers
  bypass full-transaction allocation (§4).
- **D-001 re-evaluation hook:** if a measured hot-path allocation cost of the transaction path
  proves prohibitive and cannot be pooled, D-001 is revisited (see Re-evaluation). Latency budgets
  gate this (D-019).

## Compatibility & Migration

New subsystem; no migration. The **recovery-journal on-disk format** and **temporal-index format**
are forward-looking commitments deferred to D-005 — they must be versioned (checksum + schema
version, D-005 direction) so future format changes are additive (INV-ADDITIVE). Command authors
target the transaction-request API, which is a Stable-track surface (subject to D-010 promotion).

## Observability

Every transaction emits structured fields (stability §4): `transaction_id`, `correlation_id`,
`command_id`, `origin`, `base_revision`, resulting revision. `:debug transactions` (stability §14)
lists recent transactions and lets a change be back-traced through the §5 correlation chain. Status
of the document engine is a per-component state machine (INV-STATUS); undo-history health is part of
it.

## Alternatives

1. **Chosen: single mandatory transaction path (D-001)** with a branching history + temporal index,
   and a kind-scoped append exception. Locks undo/trace/recovery/replay in one mechanism.
2. **Operational-transform / CRDT core from day one.** Powerful for multi-writer, but heavy and
   premature: multi-client is D-012 *open*, v1 is single-writer (D-020). A base_revision + rebase-on-
   mismatch model covers the single-writer case without CRDT cost; CRDTs remain a later option if
   D-012 enables collaborative editing.
3. **Undo as a flat stack + separate "undo-tree plugin"** (Emacs's shape). Rejected as the core
   model because Vim `g-`/`g+`/`:earlier` are *core* L2 obligations (VIM-UNDO-1), not an add-on; a
   flat stack cannot represent branch-preserving history without bolting a tree on afterward.

## Rejected approaches

*Recorded so they are not re-litigated (docs/README RFC process).*

- **Rejected: direct `insert`/`delete` scattered through the code.** Fast locally, but makes undo,
  traceability, crash consistency, and replay impossible to guarantee — no single recorder exists.
  This is the exact failure INV-TXN and D-001 exist to prevent (architecture.md §3.3). Guarded by
  assert (stability §1).
- **Rejected: per-keystroke undo.** Every keypress as an undo step makes `u` unusable (one keypress
  undone at a time through an Insert session) and bloats history. Undo is by **logical unit**
  (INV-UNDO, COM-8). Keystroke granularity also breaks dot-repeat's "full change including inserted
  text" (VIM-REPEAT-DOT).
- **Rejected: an undo-tree node per keystroke.** Combines both failures above — an enormous tree of
  trivial nodes, with a chronological index dominated by noise. Nodes are logical units; the temporal
  index orders *those*, not keystrokes.
- **Rejected: all buffers transactional, including streaming.** Allocating a full transaction +
  inverse per line for a terminal/log emitting thousands of lines/second is pure overhead for data
  that is never edited or undone. Hence the explicit INV-BUFFER-KIND exception (§4). Forcing
  streaming into INV-TXN is the contradiction V-4 flagged.

## Trade-offs

- **Upfront plumbing.** Snapshots, metadata, preflight, inverse-edit generation, and the temporal
  index cost engineering and bytes before any feature "needs" them — accepted, because this is the
  substrate the whole platform is debuggable and recoverable through (Architecture > Code;
  stability trade-offs).
- **Deliberate fail-fast.** Treating undo-history corruption as a crash (not a swallowed error)
  means intentionally stopping a running editor — mitigated by recovery-file + diagnostic snapshot
  before shutdown (INV-FAIL-BOUNDED).
- **Two-traversal complexity.** Maintaining both structural edges and a chronological index over
  the same tree is more than a stack — but it is the irreducible cost of Vim parity (VIM-UNDO-1) and
  is paid once in the core, not per surface.
- **Boundary discipline.** Keeping dot-repeat's change-intent *out* of the transaction engine
  (D-025) avoids conflating replay with re-parameterization (VIM-11), at the cost of a cross-engine
  contract that both RFC-0007 and D-025 must keep aligned.

## Re-evaluation conditions

- **D-001:** revisit if a measured hot-path allocation cost of the transaction path is prohibitive
  and cannot be pooled (DECISIONS D-001).
- **CRDT/OT core:** reopen if D-012 (multiple clients per workspace) is enabled and single-writer
  rebase proves insufficient.
- **Buffer-kind set:** revisit if a new buffer kind appears that fits neither the editable nor the
  append/bounded contract.
- **Temporal-index / journal format:** finalized under D-005 before F-008 implementation.

## Open questions

1. **Undo-grouping boundaries (D-005, open).** The exact rules that open/extend/close a logical undo
   group — Insert-session boundaries, operator compounds, multi-cursor edits, `:undojoin`/`C-g u`
   mapping, time-based coalescing. Deferred to D-005 ("undo-grouping boundaries") and validated
   against VIM-UNDO-1.
2. **Chronological temporal-index format (D-005, open).** The concrete representation of the
   sequence/time-ordered index over the history tree that backs `g-`/`g+` and `:earlier {N|5m|3f}`,
   including how it is persisted alongside the recovery journal (D-005) and whether persistent-undo
   (`undofile`) reuses it.
3. **Change-intent IR (D-025, open, cross-RFC).** The re-parameterizable intent record for dot-repeat
   and `g@` lives in the editing-language engine (D-025); RFC-0007 only guarantees one grouped,
   origin-tagged transaction per intent execution. The intent↔transaction contract must stay aligned
   as D-025 closes.
