---
doc: rfc
project: ruse
title: "RFC-0008: Document Model & Coordinates"
summary: >
  The Document is the sole owner of text bytes + revision; it never knows about a View, Window, or
  File. This RFC locks that ownership split (INV-DOC-VIEW, D-003), typed coordinates that are never a
  bare usize and the layer responsible for converting between them (INV-POS-TYPED), anchors with
  affinity/gravity as the only long-lived positions (INV-ANCHOR, D-023), generation-checked typed
  handles instead of long-lived references (INV-HANDLE), the ResourceId/DocumentId/WorkspacePath
  identity split (rename/symlink/unsaved/remote/read-only/virtual), buffer KIND as the selector of the
  mutation contract (INV-BUFFER-KIND), encoding/line-endings kept off the document data model, and
  large-file/degraded mode as a distinct profile rather than a slow normal mode. It defers no new
  identity to other RFCs and delegates the transaction/undo contract itself to RFC-0007.
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - ../../architecture/architecture.md
  - ../../parity/common.md
  - ../../parity/workspace.md
  - ../../invariants/reference-invariants.md
  - ./RFC-0007-transaction-engine.md
  - ../../../spec/DECISIONS.md
---

# RFC-0008: Document Model & Coordinates

- **Status:** proposed
- **Author(s):** ruse core
- **Created:** 2026-08-05
- **Decision link:** D-003 (Document ≠ View), D-023 (anchor-based typed positions); relates to
  RFC-0007/D-001, INV-BUFFER-KIND

<!-- Hard-to-reverse: this fixes the shape of the thing every other subsystem points at — the
     document/coordinate/identity model, part of the first of the five axes (architecture.md §12.1).
     The transaction/undo *contract* over a Document is RFC-0007; this RFC is the *noun*. -->

## Summary

A **Document** owns exactly one thing: the sequence of text bytes and its monotonic **revision**. It
does **not** own a cursor, a selection, a viewport, a scroll position, a window, or a filename — those
belong to a **View**, and a Document never holds a reference to a View (**INV-DOC-VIEW**, **D-003**).
One buffer, many views. Every position into a Document is a **typed coordinate** — byte, char,
grapheme, UTF-16 column, or screen cell — never a bare `usize` (**INV-POS-TYPED**); conversion between
units is an explicit, owned responsibility, not an implicit cast. Positions that must **survive edits**
(cursors, marks, diagnostics, decorations) are **anchors** with affinity/gravity, never stored raw
offsets (**INV-ANCHOR**, **D-023**). Everything held across time — a Document, a View, a Snapshot — is
referred to by a **generation-checked typed handle**, never a long-lived reference (**INV-HANDLE**).
Document identity is split three ways — `DocumentId` (in-memory buffer), `ResourceId` (the thing it is
of, if any), `WorkspacePath` (typed location) — so rename, symlink, unsaved, remote, read-only, and
virtual documents all have coherent identity. A Document's **kind** selects its mutation contract
(**INV-BUFFER-KIND**). Encoding and line-endings are metadata *beside* the document, not woven into its
data. Large-file handling is a **distinct degraded profile**, not a slow path through the normal one.
This RFC records the model and its boundaries; it does not restate the transaction/undo contract
(RFC-0007).

## Motivation / Problem

Neovim entangles buffer text, window/cursor state, marks, undo, and file identity in one apparatus
(architecture.md §0.2), so "the same file in two splits," "a renamed-on-disk buffer," and "a
scratch/terminal buffer" are all special-cased. ruse's premise (architecture.md §3.1, DECISIONS
**D-003**) is that these are *different nouns with different owners*. Getting the noun right is the
precondition for five things that are otherwise perpetually broken:

1. **Splits, GUI, remote, multi-client.** The same buffer must open in N views with independent
   cursors/viewports (COM-3, D-012). Impossible if cursor state lives in the Document.
2. **Position stability.** "Does this diagnostic still point at the right character after I edited
   above it?" is answerable only if positions are anchors, not offsets (INV-ANCHOR, extmark parity).
3. **Correct text math.** LSP speaks UTF-16 columns, the rope stores bytes, the user sees graphemes,
   the terminal paints cells. Conflating any two is a silent corruption (INV-POS-TYPED, PERF-6).
4. **Coherent identity under change.** Rename, symlink, "save as," unsaved scratch, remote, and
   virtual (help/output) documents must not collapse `file == document` (INV-REMOTE-FIRST, D-011).
5. **Non-editable surfaces.** Terminals, logs, generated output, dired/Magit views, and large-file
   mode cannot be forced through the editable-document contract (INV-BUFFER-KIND, V-4/V-14).

Storing `usize` offsets, `Rc<RefCell<Document>>` references, or a `file` field on the Document quietly
breaks all five. This RFC fixes the shape so they cannot.

## Guide-level explanation

From a command or plugin author's view:

- You never hold a `Document`. You hold a **`DocumentId`** (a typed, generation-checked handle) and ask
  the workspace for a **`Snapshot`** at a revision to read (INV-QUERY-SNAPSHOT). Using a handle whose
  generation was freed is an *invariant violation*, not an error (INV-HANDLE) — it means you kept a
  handle past its lifetime.
- Your cursor, selection, and viewport are **View** state, addressed by a **`ViewId`**. Two views of
  one `DocumentId` scroll and select independently; editing through either advances the *one* shared
  revision (INV-DOC-VIEW).
- You never write `position: usize`. You write `BytePos`, `CharPos`, `GraphemePos`, `Utf16Col`, or
  `CellCol`, and you **ask the Document to convert** between them against a snapshot. There is no
  `as`-cast between coordinate spaces (INV-POS-TYPED).
- For anything you keep across edits — a mark, a decoration, a diagnostic range — you create an
  **`Anchor`** with an **affinity** (which side it clings to at an insertion boundary) and let the
  anchor store move it as edits land (INV-ANCHOR, D-023). You do **not** stash a `BytePos` and hope.
- The buffer you're looking at has a **kind** (editable / read-only / generated / streaming /
  interactive). The kind tells you what you may do to it: an editable file takes a Transaction; a
  terminal takes appends; a Git-status view takes *domain commands* (stage/unstage), not text edits
  (INV-BUFFER-KIND, workspace.md V-14).
- A document opened past the large-file threshold enters **degraded mode** — a *named profile* with
  syntax off, bounded undo, and streaming search — not the normal editor running slowly (COM-12).

## Reference-level explanation

Language-independent contract (Rust types are illustrative, per the invariants doc preamble).

### 1. Document ≠ View ≠ Window ≠ File (INV-DOC-VIEW, D-003)

Ownership is partitioned, one owner per fact:

| Fact | Owner | Never on |
| --- | --- | --- |
| Text bytes, revision, encoding/EOL metadata, kind | **Document** | View, Window |
| Cursor(s), selection(s), viewport, scroll, fold state, mode | **View** | Document |
| Layout, focus, split geometry, tab membership | **Window / Layout** | Document, View |
| On-disk/remote location, mtime, read-only-on-disk | **Resource** (§4) | Document data |

- A **Document does not know a View exists** — the dependency is one-way (architecture.md §3.1). A
  Document with a `cursor` field is the exact bug INV-DOC-VIEW forbids (anti-pattern #2, CORE-4/UI-6).
- **One buffer, many views** (COM-3): `View { id, document: DocumentId, cursors, selections,
  viewport, … }`. Cursor/viewport/mode are **client/view-local** so multi-client (D-012) is a later
  addition, not a rewrite.
- **Layout lifecycle ≠ view lifecycle ≠ document lifecycle**: closing a view must not keep its
  Document or tasks alive (workspace.md; design-requirements §11), and a Document may outlive every
  view (background buffer).

### 2. Typed coordinates and who converts (INV-POS-TYPED, TEXT-1/2)

A position is a *unit-tagged* offset. The distinct spaces:

| Coordinate | Unit | Primary consumer |
| --- | --- | --- |
| `BytePos` | UTF-8 byte offset | rope storage, on-disk, patches |
| `CharPos` | Unicode scalar (code point) | some regex/motion math |
| `GraphemePos` | extended grapheme cluster | user-perceived characters, motions |
| `Utf16Col` (line + column) | UTF-16 code unit | LSP/DAP wire protocol |
| `CellCol` (line + column) | terminal display cell | rendering, cursor placement, alignment |

Rules:

- **Coordinates are never a bare `usize`** (INV-POS-TYPED). A function taking `usize` for "a position"
  is anti-pattern #3 (TEXT-1/2).
- **Conversion is an explicit operation against a snapshot**, owned by the Document's text engine —
  there is no implicit `as`-cast between spaces, because the mapping is content-dependent (a grapheme
  is 1–N bytes; a cell is 0–2+ per grapheme via East-Asian-Width/emoji/combining marks — architecture.md
  §6.2). Bytes↔UTF-16 conversion must **respect rope chunk boundaries** and must not scan the whole
  document per LSP request (PERF-6, architecture.md §9).
- **Cursor position ≠ logical text position** (architecture.md §6.2): a `CellCol` is a rendering fact,
  a `GraphemePos` is a text fact; the render layer converts, the Document does not store cells.
- A raw offset value carries no unit and is meaningless across an edit — see §3.

### 3. Anchors: the only long-lived positions (INV-ANCHOR, D-023, TEXT-4/5/6)

A typed coordinate (§2) is valid **only against the snapshot it was taken from**. Anything retained
across edits is an **Anchor**:

An **Anchor** is a generation-checked handle into the anchor store, never a coordinate. Its canonical
value — an `offset`, a boundary **bias** (`Before | After`), and a span-delete **policy**
(`Clamp | Invalidate`) — is defined once in [anchor-store.md §1](../../design/anchor-store.md) (D-023);
this RFC does not restate the type (design-doc types are non-normative; the store owns it).

- **Long-lived positions are anchors, never raw offsets** (INV-ANCHOR, D-023): cursors, selections,
  marks, diagnostics, decorations, LSP ranges, fold points. Storing a `BytePos` on a diagnostic is
  anti-pattern #5 (TEXT-4).
- **Bias** (`Before | After`) decides behaviour at a boundary: text inserted *at* an anchor is either
  absorbed to its left (`Before`) or pushes the anchor right (`After`) — the semantics extmark stability
  and Vim/Emacs mark behaviour depend on (canonical: anchor-store.md §Bias, D-023).
- **The anchor store updates anchors as a transaction applies**, driven by the edit set — anchor
  update cost is **not** `O(anchors × edits)` (INV-ANCHOR, PERF-6). Anchors resolve **to** a typed
  coordinate on demand (against the current snapshot); they are not themselves a coordinate.
- The full positions-history model over the anchor store (jumplist / mark-ring / selection-sets) is
  **D-027, open** — out of scope here; this RFC only fixes that they are anchors.

### 4. Identity: DocumentId ≠ ResourceId ≠ WorkspacePath

A single "file handle" cannot express rename, symlink, unsaved, remote, or virtual. Split it:

- **`DocumentId`** — identity of the in-memory buffer + its revision stream. Stable for the buffer's
  lifetime regardless of what it is *of*. A scratch buffer has a `DocumentId` and no resource.
- **`ResourceId`** — identity of the backing thing (a file, a remote object, a Git blob, an LSP
  virtual document), if any. Survives a **rename** (the resource is the same, the path changed);
  **symlinks** resolve to one canonical `ResourceId` so a file opened via two paths is one document,
  not two divergent buffers.
- **`WorkspacePath`** — a *typed* location (**local path ≠ workspace path**, D-011, INV-REMOTE-FIRST),
  not a `String`; Windows/WSL/remote paths are not string-substituted (architecture.md §5.1).

This yields coherent behaviour for:

| Situation | Modeled as |
| --- | --- |
| Unsaved scratch | `DocumentId`, no `ResourceId` |
| Rename on disk | same `DocumentId` + `ResourceId`, new `WorkspacePath` |
| Symlink / same file via two paths | one `ResourceId`, one `DocumentId` |
| "Save as" | same `DocumentId`, new/copied `ResourceId` + `WorkspacePath` |
| Remote file | `ResourceId` on a remote `WorkspacePath`, negotiated runtime (D-011) |
| Read-only on disk | resource flag; distinct from *kind* (§5) — see below |
| Virtual (help, command output, Git revision) | `DocumentId`, synthetic/absent `ResourceId` |

Read-only-**on-disk** (a resource permission) is not the same as a read-only **kind** (a mutation
contract, §5): a writable file may back a read-only *view*, and a generated document has no writable
resource at all.

### 5. Buffer KIND selects the mutation contract (INV-BUFFER-KIND)

A Document's **kind** (workspace.md "virtual-document kinds") is what may be done to it:

| Kind | Example | Mutation contract |
| --- | --- | --- |
| **Editable** | source file | Full **Transaction** (RFC-0007 §2), full branching undo |
| **Read-only** | Git revision, LSP virtual doc | No mutation |
| **Generated** | help, search/diagnostics/command output | Rebuilt by its producer, not edited |
| **Streaming** | terminal/PTY, logs, build output | **Append** path (not a full transaction); bounded/absent undo |
| **Interactive** | dired/wdired, Git status, file tree, debugger | Buffer edits become **typed domain CommandRequests** (rename/stage/…), preflighted per service — **not** text transactions (workspace.md V-14) |

- Kind is a property of the Document; it is the selector INV-BUFFER-KIND names. The **editable**
  contract (RFC-0007) applies to editable Documents *only*.
- **Streaming** buffers must not allocate a transaction + inverse per line at thousands of lines/second
  (the V-4 contradiction; RFC-0007 §4). **Interactive** buffers translate edits to domain commands and
  re-render from new domain state (V-14), never mutating a text Document in place.
- This RFC owns *that kind exists and partitions the contract*; RFC-0007 owns the editable-path detail.

### 6. Encoding & line-endings are metadata, not data (TEXT-16/19, COM-13)

The Document's data model is **decoded text + a `revision`**. Encoding (`fileencoding`, BOM presence)
and line-ending style (`fileformat`: LF/CRLF/CR, mixed-EOL policy) are **separate metadata beside the
Document**, not interleaved into its byte model (COM-13; INV — TEXT-19). Consequences:

- Editing logic and coordinates (§2) operate on decoded text; they are unaffected by whether the file
  is UTF-8/UTF-16/Latin-1 or LF/CRLF.
- Detection order + BOM + `fileformat`/`fileencoding` semantics follow parity (common.md COM-13,
  vim.md VIM-STATE); a re-encode or EOL change is an explicit metadata operation, not a hidden rewrite
  of every position.
- Binary / invalid-UTF-8 content has a defined policy (architecture.md §3.2) and interacts with §7.

### 7. Large files are a distinct degraded profile, not a slow normal mode (COM-12)

A document past the large-file threshold enters an explicitly-named **degraded profile**
(architecture.md §3.2; COM-12): syntax highlighting off, **bounded undo** (INV-BUFFER-KIND),
streaming/incremental search, binary detection, and very-long-line handling. This is a *different
profile* the user (and code) can observe and reason about — **"not a slow version of normal mode"**
(common.md COM-12). It composes with kind (§5): a large streaming log is degraded *and* streaming.

## Reference Invariants

This RFC depends on and enforces these registry invariants (defined in
[reference-invariants.md](../../invariants/reference-invariants.md); not redefined here):

- **INV-DOC-VIEW** — Document ≠ View ≠ Window ≠ File; a Document never knows a View; view-local state
  is never stored in the Document; one Document, many Views. (§1)
- **INV-POS-TYPED** — Positions are typed by unit (byte / char / grapheme / UTF-16 column / cell);
  coordinates are never an untyped `usize`; conversion is explicit. (§2)
- **INV-ANCHOR** — Long-lived positions are anchors with affinity/gravity that survive edits, never
  raw offsets; anchor update cost is not `O(anchors × edits)`. (§3)
- **INV-HANDLE** — Long-lived references are generation-checked typed handles (`DocumentId`, `ViewId`,
  anchor ids, snapshot ids), never raw pointers/offsets; a freed-generation handle is an assert, not
  an error. (§1, §3, §4, Guide)
- **INV-BUFFER-KIND** — A buffer's kind (editable / read-only / generated / streaming / interactive)
  selects the mutation contract; non-editable kinds are an explicit exception to INV-TXN/INV-UNDO. (§5, §7)

Also relied upon (owned by other docs): **INV-TXN** / **INV-UNDO** (the editable-kind contract, owned
by [RFC-0007](./RFC-0007-transaction-engine.md), §5), **INV-QUERY-SNAPSHOT** (reads are immutable
snapshots, §2/§3, Guide), **INV-REMOTE-FIRST** (typed `WorkspacePath`, §4), **INV-NO-GLOBAL-STATE**
(no global `Document` behind `Arc<Mutex<_>>`, §1).

## Failure modes & Recovery

- **Stale coordinate used across an edit:** a typed coordinate resolved against an old snapshot is
  simply that snapshot's fact; code that must track across edits uses an anchor (§3). Misuse surfaces
  as a typed range error at preflight (RFC-0007 §5), never silent corruption.
- **Freed-generation handle** (`DocumentId`/`ViewId`/anchor id from a closed buffer): an **invariant
  violation** (INV-HANDLE), not an error — fail-fast to recovery snapshot + safe shutdown
  (INV-FAIL-BOUNDED), because it means a lifetime bug.
- **Identity ambiguity** (two paths to one resource, rename race): resolved to one canonical
  `ResourceId` (§4); a detected conflict (external rename/delete of an open resource) is a typed event
  handled by the save/reload flow (stability §13), not a crash.
- **Encoding/EOL detection failure or invalid bytes:** falls to the binary/invalid-UTF-8 policy and, if
  large, the degraded profile (§6/§7); the document opens read-only or binary-view rather than
  corrupting text.
- **Large-file threshold crossed:** entering degraded mode is an observable, reversible profile
  transition (§7), not an unbounded slowdown.

## Security impact

Identity separation (§4) is a trust boundary: a **remote** `WorkspacePath` and a **local** one are
different types (INV-REMOTE-FIRST), so remote content is never silently treated with local-client
authority (architecture.md §10). Buffer **kind** (§5) bounds what untrusted producers may do — a
plugin/AI-fed streaming or generated buffer cannot smuggle an editable-document transaction; interactive
write-back goes through preflighted, capability-gated domain commands (workspace.md V-14, INV-TRUST-1).
Read-only kind and read-only-on-disk are enforced separately so neither can be bypassed by confusing
one for the other.

## Performance impact

- **No whole-document clones**; reads are cheap immutable snapshots, not deep copies (architecture.md
  §9, INV-QUERY-SNAPSHOT). Coordinate conversion (§2) respects rope chunk boundaries and never
  UTF-16-scans the whole document per LSP request (PERF-6).
- **Anchor updates are batched by edit set**, not per-anchor-per-edit — off `O(anchors × edits)`
  (INV-ANCHOR, §3).
- **Kind exists partly for performance** (§5): streaming/terminal buffers bypass full-transaction
  allocation; the large-file profile (§7) trades features for bounded cost by design, not accident.
- Typed coordinates are zero-cost newtypes over integers — the safety is compile-time, not runtime.

## Compatibility & Migration

New subsystem; no migration. The **coordinate set**, **handle/anchor identity**, and the
`DocumentId`/`ResourceId`/`WorkspacePath` split are the substrate every later subsystem points at, so
they are locked here and evolve **additively** (INV-ADDITIVE): new coordinate spaces or kinds are added,
existing ones are not repurposed. The Document/View/anchor surface a plugin sees (handles, snapshots,
typed coordinates — architecture.md §4.1) is a Stable-track contract (D-010 promotion). On-disk
encoding/EOL metadata is versioned with the save format (D-005).

## Observability

A Document exposes structured identity in diagnostics: `DocumentId`, `ResourceId`, `WorkspacePath`,
`kind`, `revision`, `fileencoding`/`fileformat`, degraded-profile flag. `:debug` surfaces (stability
§14) can list open documents and their views, and resolve an anchor to its current typed coordinate for
"why is this decoration here?" Health of the document engine is a per-component state machine
(INV-STATUS).

## Alternatives

1. **Chosen: partitioned ownership + typed coordinates + anchors + split identity + kind.** Each fact
   has one owner and one type; every retained thing is a handle/anchor. Locks §1's five preconditions
   in the noun itself.
2. **One fat `Buffer` holding text + cursor + file + undo** (Vim/Neovim's shape). Rejected below — it
   is exactly the entanglement D-003 and INV-DOC-VIEW exist to prevent.
3. **Rope offsets everywhere, convert at the edges.** Cheaper to type today; but "the edges" are every
   LSP call, every motion, every decoration, and the conversions are content-dependent — pushing them
   to call sites is where silent UTF-16/byte/grapheme corruption breeds (§2). Typing the *space*, not
   just the integer, is the point.
4. **Piece-table / gap-buffer specifics in the model.** Deliberately out of scope: the model is
   coordinate + revision + anchors; the storage engine (rope today) is an implementation detail never
   exposed across layers (architecture.md §3.2, §4.1).

## Rejected approaches

*Recorded so they are not re-litigated (docs/README RFC process).*

- **Rejected: `usize` coordinates.** A bare integer position carries no unit, so byte/char/grapheme/
  UTF-16/cell get mixed and the compiler cannot catch it — the direct cause of off-by-column LSP bugs
  and mis-rendered wide characters. Coordinates are typed newtypes (INV-POS-TYPED, TEXT-1/2,
  anti-pattern #3). *This is a hard "no," not a preference.*
- **Rejected: raw-offset long-lived positions.** Storing a `BytePos`/`usize` on a cursor, mark, or
  diagnostic makes it wrong the instant text above it changes; there is no correct offset to store.
  Long-lived positions are anchors with affinity/gravity (INV-ANCHOR, D-023, TEXT-4, anti-pattern #5).
- **Rejected: `file == document` identity.** Collapsing the buffer, the backing resource, and the path
  into one identity cannot represent rename (path changes, resource same), symlink (two paths, one
  resource), unsaved scratch (no resource), "save as," remote (typed non-local path), or virtual
  (no resource) documents. Identity is split three ways (§4; D-011, INV-REMOTE-FIRST).
- **Rejected: Document knows about its View(s).** A `cursor`/`selection`/`viewport` field on the
  Document breaks splits, GUI/remote frontends, and multi-client, and corrupts state when two views
  edit (D-003, INV-DOC-VIEW, CORE-4/UI-6, anti-pattern #2). View-local state stays in the View, always.
- **Rejected: all buffers editable + transactional.** Forcing terminals, logs, generated output, and
  interactive (dired/Magit) views through the editable-document + full-undo contract is pure overhead
  for streaming buffers (per-line transaction+inverse at thousands of lines/second) and *wrong* for
  interactive buffers, where an edit means a domain command (stage/rename), not a text mutation. Kind
  selects the contract (INV-BUFFER-KIND, §5; V-4/V-14). Large-file is likewise a **distinct degraded
  profile**, not the normal path run slowly (COM-12) — "make normal mode faster for big files" is
  rejected because feature scope, not just speed, must change.

## Trade-offs

- **More types up front.** Five coordinate spaces, three identity types, handles, and anchors are more
  surface than "a buffer with an offset." Accepted: it is the substrate every later subsystem is
  correct through, and the cost is compile-time (Architecture > Code).
- **Explicit conversions.** Callers must convert coordinates deliberately rather than pass a naked
  integer — more ceremony, but it is exactly where corruption would otherwise hide (§2).
- **Anchor bookkeeping.** Maintaining and batch-updating an anchor store is more than storing offsets,
  but it is the irreducible cost of positions that survive edits (INV-ANCHOR) and is paid once in the
  core, not per feature.
- **Kind branching.** Distinguishing kinds means the mutation path is not uniform — but a uniform path
  is the thing that is wrong for four of the five kinds (§5).

## Re-evaluation conditions

- **Coordinate set:** revisit only additively if a new consumer needs a space not covered by
  byte/char/grapheme/UTF-16/cell.
- **Identity model:** revisit if a backing kind appears that fits neither `ResourceId`-backed nor
  resource-less (none currently anticipated; D-003 marked "never expected").
- **Kind set:** revisit if a new buffer kind fits neither the editable nor the append/read-only/
  generated/interactive contracts (shared re-eval hook with RFC-0007 §4).
- **Anchor semantics:** the affinity/gravity model is validated against extmark-stability and Vim/Emacs
  mark differential tests (D-023); revisit if a surface needs a semantics the two-axis model can't
  express.
- **Large-file thresholds/profile contents:** tuned on real workloads (COM-12; latency budgets D-019).

## Open questions

1. **Positions-history model (D-027, open, cross-RFC).** How marks / jumplist / mark-ring / selection-
   sets layer over the anchor store with per-surface membership/traversal policies. This RFC fixes only
   that they are anchors; the history model is D-027.
2. **Symlink / hardlink / case-insensitive-FS canonicalization rules.** The exact canonicalization that
   maps paths to one `ResourceId` (including remote and case-folding filesystems) needs a precise rule
   set before F-002/save land (relates to D-005, D-011).
3. **Encoding/EOL detection + re-encode boundaries.** Detection order, BOM handling, mixed-EOL policy,
   and when a re-encode is a metadata op vs a content change — finalized with the save format (D-005,
   COM-13).
4. **Large-file profile exact contents & thresholds (COM-12).** Which features degrade at which sizes,
   and the interaction with binary detection and very-long-line handling — tuned on real workloads
   before the relevant feature ships.
5. **Grapheme/segmentation library & versioning.** Grapheme and East-Asian-Width segmentation depend on
   a Unicode version; how a version bump interacts with persisted anchor positions is open.
