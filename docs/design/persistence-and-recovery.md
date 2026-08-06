---
doc: persistence-and-recovery
project: ruse
title: "ruse Persistence, Crash Recovery & the Undo Journal"
summary: >
  Resolves D-005. Defines the four distinct tracked states (document / saved / disk / journal position)
  and derives "dirty" from them; the append-only transaction journal record layout with checksum + schema
  version + inverse-edit + TransactionMetadata and truncated-journal recovery; atomic save (temp + rename +
  directory fsync, metadata/encoding/EOL preservation, POSIX vs Windows); three-way crash recovery that
  never auto-overwrites the original; external-change conflict detection; undo-grouping boundaries by
  TransactionOrigin with explicit break/join; the chronological index over the branching undo tree
  (g-/g+, :earlier/:later Nf); and the per-buffer-kind exception for ephemeral buffers.
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - architecture.md
  - stability-and-observability.md
  - design-requirements.md
  - ../parity/common.md
  - ../parity/vim.md
  - ../parity/workspace.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
---

# ruse Persistence, Crash Recovery & the Undo Journal

> This doc **resolves [D-005](../../spec/DECISIONS.md)** (was `open`): journal format details, incremental-
> journal thresholds for large files, and undo-grouping boundaries. It is the normative specification behind
> `PERSIST` ([design-requirements.md §3](../architecture/design-requirements.md)) and cross-platform save
> ([§7 / §13](../architecture/design-requirements.md)). It builds on the transaction/undo model of
> [architecture.md §3](../architecture/architecture.md), the preflight model of
> [stability-and-observability.md §13](stability-and-observability.md), and the parity obligations
> [COM-8/COM-9/COM-12/COM-13](../parity/common.md) and [VIM-UNDO-1 / VIM-STATE](../parity/vim.md). It does
> not re-derive those; it cites them by ID.

## Problem

The state at the *moment of a crash* matters more than the running state
([design-requirements.md §3](../architecture/design-requirements.md)). A naive editor conflates "the buffer changed",
"the buffer differs from disk", "the file on disk changed under us", and "how much of my work would survive
a `kill -9`" into a single dirty bool and a `write()` syscall. That conflation produces the classic failures:
recovery that recovers *corrupted* state, an autosave that silently clobbers an externally-edited file, a
"Saved" indicator that lied because the bytes were still in the OS page cache when power dropped, and an undo
history that is either per-keystroke noise or an unrecoverable single line.

D-005 fixed the *direction* (atomic replace; append-only checksummed journal; recovery never auto-overwrites)
but left three things open: the journal record format, the incremental-journal threshold for large files, and
undo-grouping boundaries. This doc closes all three and specifies the surrounding machinery concretely enough
to implement and to test with fault injection ([design-requirements.md §18](../architecture/design-requirements.md)).

## Goals

- Four **distinct** tracked states, with "dirty" *derived* from them — never a stored bool.
- An **append-only** transaction journal that survives truncation (torn last write) and replays to the last
  valid record; large files use an **incremental** journal, not snapshots.
- **Atomic save**: durable temp + rename, directory fsync, metadata/encoding/EOL preservation, per-platform;
  "Saved" means a *durable* write, not syscall success.
- **Three-way** crash recovery (in-memory / disk / recoverable changes) that **never** auto-overwrites the
  original; bounded retention with PII redaction.
- **External-change** detection with a first-class surfacing path.
- Resolve **undo grouping** by `TransactionOrigin`, with explicit break/join (`C-g u` / `:undojoin`).
- A **chronological index** over the branching undo tree powering Vim `g-`/`g+` and `:earlier/:later Nf`.
- Per-**buffer-kind** journal/undo behavior (INV-BUFFER-KIND): ephemeral buffers do not accumulate undo.

## Non-goals

- Multi-client concurrent editing of one document (D-012), remote offline-journal reconciliation (D-013) —
  the on-disk formats here are versioned so those can extend them additively (INV-ADDITIVE), but their
  conflict semantics are out of scope.
- Vimscript-level `viminfo`/`:mksession` breadth beyond what [VIM-STATE](../parity/vim.md) requires for the
  editing surface; workspace/session persistence format details (its own doc) beyond "it is versioned".
- The register/kill-ring persistence model (D-026) and positions-history persistence (D-027).
- Running Vim's persistent-undo *file format* byte-for-byte; we provide equivalent capability, not `undofile`
  binary compatibility.

## Terminology

Uses the glossary (Document / View / Buffer / Revision / Transaction / Origin — see
[spec/PROJECT.md](../../spec/PROJECT.md)). New local terms:

- **Journal** — the durable, append-only, per-document log of applied transactions used for crash recovery.
  Distinct from the in-memory **undo history** (the branching tree) and from **autosave**/**backup**.
- **Recovery record** — one journal frame (header + payload). **Journal position** — a byte offset / record
  sequence identifying how far the durable journal has advanced.
- **Durable** — data the OS has committed to stable storage (fsync returned), not merely `write()`-accepted.
- **Roles** (kept separate, per [design-requirements.md §3](../architecture/design-requirements.md)):
  *autosave* = periodic full-document safety copy; *journal* = incremental change log for replay;
  *backup* = pre-overwrite copy of prior file contents; *swap-equivalent* = the lock/ownership marker that
  detects a second editor / a prior crash.

## Invariants

Depends on and enforces (registry: [reference-invariants.md](../invariants/reference-invariants.md); not
redefined here):

- **INV-TXN** — every editable-Document mutation is a Transaction carrying `base_revision`; applying strictly
  increases the Document revision. The journal is the durable projection of this stream.
- **INV-UNDO** — every such Transaction is undoable, recorded by logical unit; the history has no orphaned
  parents. The undo tree and its chronological index are the in-memory projection.
- **INV-BUFFER-KIND** — ephemeral/append-only/streaming/terminal/large-file-degraded buffers are the explicit
  exception: bounded-or-absent undo, a non-transaction append path (§ *Per-buffer-kind behavior*).
- Also relied upon: **INV-ORIGIN** (grouping keys), **INV-ASYNC-ORDER** (single-writer journal), **INV-POS-TYPED**
  / **INV-ANCHOR** (edit coordinates), **INV-FAIL-BOUNDED** (recovery-snapshot on core-invariant failure),
  **INV-ERR-CLASS** (typed save/recovery errors), **INV-TRUST-1** (redaction of recovery data).

## Proposed design

### 1. Four distinct tracked states — "dirty" is derived

A Document tracks these quantities, never a `dirty: bool` — dirtiness is *derived*, and it is a
**node-identity** test, not a revision-magnitude one (a monotonic `Revision` cannot serve: undo/redo advance
the counter yet can return to identical bytes):

```rust
/// Strictly-increasing per Document (INV-TXN). Opaque, comparable, not a wall clock.
struct Revision(u64);

struct DocumentPersistence {
    /// (A) The current in-memory document revision. Advances on every applied Transaction.
    document_revision: Revision,

    /// (B) The revision whose bytes we last durably wrote to the file (§3 atomic save).
    ///     None = never saved (scratch / new file).
    saved_revision: Option<Revision>,

    /// (E) The undo-tree node (§7 `seq`) whose bytes are on disk — the identity behind `is_modified`.
    ///     `document_revision` cannot serve: an undo applies an inverse (a new, higher revision — INV-TXN),
    ///     so returning to identical bytes yields a different revision but the *same* node. None = never saved.
    saved_node: Option<MonotonicSeq>,

    /// (C) What we last observed on disk: identity of the file bytes we believe are there,
    ///     independent of our own writes (§5 external-change detection).
    disk_observed: DiskFingerprint,

    /// (D) How far the durable journal has advanced. Records for revisions
    ///     up to journal_position survive a crash; anything past it does not.
    journal_position: JournalPos,
}

struct DiskFingerprint {
    /// Cheap change hint: size + mtime (+ inode/dev on POSIX, file-id on Windows). Never trusted alone.
    stat: FileStat,
    /// Authoritative when the hint is ambiguous (mtime granularity, clock skew, touch-without-change).
    content_hash: Option<Hash>,   // BLAKE3 of on-disk bytes at last observation
    /// Whose write produced this fingerprint — ours (from atomic save) or an external observation.
    provenance: FingerprintOrigin, // OurSave { revision } | ExternalObserved | Initial
}
```

**Derivation.** Editor state is *computed*, not stored:

| Predicate | Definition |
| --- | --- |
| `is_modified` (dirty) | `saved_node != Some(current_undo_node.seq)` — the current history node differs from the one on disk (node identity, **not** revision magnitude; see edge case) |
| `is_durably_saved` | `saved_node == Some(current_undo_node.seq)` **and** `disk_observed.provenance == OurSave{ saved_revision }` |
| `has_unjournaled_work` | `journal_position` does not cover `document_revision` |
| `disk_changed_externally` | `disk_observed.provenance == ExternalObserved` **and** it differs from our last `OurSave` (§5) |
| `recoverable_after_crash` | records exist in the journal beyond `saved_revision` |

Why not a bool: a bool cannot answer "saved but stale on disk", "unsaved but fully journaled (safe to crash)",
or "saved to disk but the disk was since overwritten by another process". Each of those is a distinct UI and a
distinct preflight ([stability §13](stability-and-observability.md)) decision. The status bar renders a *view*
of these (INV-STATUS), e.g. `[+]` modified, `[✎ journaled]` unsaved-but-recoverable, `[⚠ disk changed]`.

Edge cases: **undo across the save point** — `Revision` is a strictly-monotonic counter (INV-TXN / RFC-0007
§2: every apply, *including an undo's inverse*, increases it), so it is **not** the dirty test. The saved
state is identified by the undo-tree **node** current when we wrote to disk (`saved_node`, a `MonotonicSeq`,
§7). After saving at node *P* then undoing, `document_revision` keeps climbing but the current node differs
from *P*, so `is_modified` is true (correct: buffer no longer matches disk); navigating back to *P* (redo)
makes it false again — same bytes, same node, higher revision. We compare **node identity, not revision
magnitude**. (Vim's `[+]` behaves the same via `undofile` seq.)

### 2. Append-only transaction journal

One journal file per editable Document, path derived deterministically and namespaced by workspace + a hash of
the absolute file path (so two files with the same basename don't collide), stored under the recovery dir
(§ *Recovery files*). Opened `O_APPEND` (POSIX) / `FILE_APPEND_DATA` (Windows); a single writer (the
deterministic core executor, INV-ASYNC-ORDER) — never concurrently appended.

**Record layout.** Every record is self-describing and independently checksummed so a torn tail is detectable:

```
┌──────────────── record header (fixed) ────────────────┐
│ magic:        u32   "RJ01" — quick resync / file-type  │
│ schema_ver:   u16   record schema version (INV-ADDITIVE)│
│ record_kind:  u8    Edit | Meta | Checkpoint | Marker  │
│ flags:        u8    e.g. COMPRESSED, GROUP_BOUNDARY     │
│ payload_len:  u32   length of payload bytes            │
│ header_crc:   u32   CRC32C of the header fields above  │
├──────────────── payload (variable) ───────────────────┤
│ payload bytes (see record_kind)                        │
├──────────────── record trailer ───────────────────────┤
│ payload_crc:  u32   CRC32C of payload                  │
│ end_magic:    u32   "rj01" — trailer sentinel          │
└────────────────────────────────────────────────────────┘
```

An **Edit** record's payload is the transaction as an *inverse-first* pair plus metadata:

```rust
struct EditRecord {
    meta: TransactionMetadata,
    /// The forward edits (to replay) AND the inverse edits (to undo), both stored.
    /// Storing the inverse makes recovery + undo reconstruction O(record), never a re-diff.
    forward: EditList,   // normalized, non-overlapping, ordered (architecture.md §3.3)
    inverse: EditList,   // the exact inverse to restore base_revision
}

// Canonical core fields are defined once in stability §5 (transaction_id, correlation_id, origin,
// command_id, base_revision, timestamp); the journal persists THOSE plus result_revision / seq /
// group_hint below. This struct is that canonical metadata extended for persistence, not a second def.
struct TransactionMetadata {
    id: TransactionId,             // = stability §5 transaction_id; process-unique, ordered by seq (ID §7)
    correlation: CorrelationId,    // = stability §5 correlation_id (user gesture / LSP request / AI proposal)
    command_id: CommandId,         // = stability §5 command_id — the semantic command (INV-CMD-SEMANTIC)
    origin: TransactionOrigin,     // UserInput | Macro | Plugin | Lsp | AiAgent | RemotePeer (INV-ORIGIN)
    base_revision: Revision,       // the revision this applied onto (INV-TXN)
    result_revision: Revision,     // = base_revision's successor; the revision after apply
    timestamp: WallClockUtc,       // DISPLAY ONLY (":earlier 5m", audit) — never used for ordering
    seq: MonotonicSeq,             // authoritative ordering + chronological index key
    group_hint: GroupBoundary,     // Continue | BreakBefore | JoinPrev (§6)
}
```

A **Checkpoint** record is a periodic full-document (or, for large files, a compacted incremental base) snapshot
that lets recovery skip replaying the entire history and lets the journal be truncated ahead of it. A **Meta**
record carries save-point advances (`saved_revision` moved to R), external-reload events, and encoding/EOL
facts (§3) so recovery can reconstruct not just text but *how to write it back*. A **Marker** record records
`journal_position` fsync barriers.

**Durability discipline.** Append record → (for user-visible "safe" points) `fdatasync` the journal → advance
`journal_position` (a Marker). Interactive edits append without a per-keystroke fsync (batched on an idle/timer
boundary and always before a risky op), so `has_unjournaled_work` can be briefly true; that is exactly the state
the derived flags expose.

**Recovery from a TRUNCATED journal.** A crash can tear the last append (partial header, partial payload, or a
header with a missing/short payload). Replay is defensive and *forward-only to the last provably-good record*:

```
open journal read-only
pos ← 0; last_good ← 0; state ← empty (or last Checkpoint)
loop:
    if remaining < header_size: break                      # torn header → stop
    read header; if header_crc mismatch: break             # corrupt header → stop
    if magic != "RJ01": break                              # desync → stop
    if schema_ver unknown-major: refuse record, halt safe  # INV-ADDITIVE: unknown MAJOR is not skippable
    if remaining < payload_len + trailer_size: break       # torn payload → stop
    read payload + trailer
    if payload_crc mismatch or end_magic wrong: break      # torn/corrupt → stop
    apply(record) to state                                 # replay forward edits / adopt checkpoint
    last_good ← pos_after_record
    pos ← last_good
# state now == the document at the last intact record; last_good == safe truncation point
```

Key properties: (a) we **stop at the first bad record and never skip past it** — a hole means "everything after
is untrustworthy", so we replay strictly the intact prefix; (b) `last_good` is the offset we truncate to before
resuming appends, so a torn tail is discarded, not left to corrupt future recovery; (c) because each record is
independently CRC'd and length-framed, a torn *tail* never poisons the intact prefix. This is the behavior
fault-injection CI asserts ("truncated journal", [design-requirements.md §18](../architecture/design-requirements.md)).

**Large-file / incremental mode (resolves the D-005 threshold).** Full-document snapshots are prohibitive for
large files ([COM-12](../parity/common.md)). A document enters **incremental journal mode** when it crosses a
size/line threshold (initial policy: file bytes > 32 MiB **or** > ~5 M lines **or** average line length beyond
the long-line guard — tuned on real workloads, see Open questions). In this mode:

- No periodic full **Checkpoint**; instead one *base* Checkpoint at open (or a reference to the on-disk file as
  the base) plus a pure stream of incremental **Edit** records. Recovery = base + replay deltas.
- Undo is **bounded** (a capped in-memory window, INV-BUFFER-KIND large-file/degraded clause); records older
  than the window are still in the journal for crash recovery but are not individually undoable in-session.
- Compaction rewrites the journal as `new base Checkpoint (= current text) + empty tail` via the same atomic
  temp+rename discipline as §3, never in place.

### 3. Atomic save

Save is **durable-or-nothing**. "Saved" in the UI reflects a durable write (fsync of data *and* the containing
directory), not a returned `write()`.

**POSIX algorithm:**

```
1. dir ← parent dir of target; open dir fd.
2. stat target → capture mode, uid/gid, ACL/xattr, and EOL/encoding decision (from Meta / open-time detection).
3. create temp in the SAME directory (same filesystem so rename is atomic): ".<name>.ruse-tmp-<rand>"
      O_WRONLY|O_CREAT|O_EXCL, mode 0600 initially.
4. write encoded bytes (apply fileencoding + fileformat/EOL + optional BOM — TEXT-19, VIM-STATE); handle
      no-trailing-newline ('binary'/'noeol') exactly.
5. fdatasync(temp)                          # data durable before it becomes the file
6. fchmod/fchown temp to captured mode/owner; copy xattr/ACL. (best-effort ownership; see platform notes)
7. rename(temp, target)                     # atomic replace on POSIX
8. fsync(dir fd)                            # make the rename itself durable
9. close dir fd.
10. On success: saved_revision ← document_revision; saved_node ← current_undo_node.seq; disk_observed ← OurSave{revision, new hash/stat};
      append a Meta(save-point) record; only THEN surface "Saved".
```

If any step fails the temp file is unlinked and the original is untouched (INV-FAIL-BOUNDED, INV-ERR-CLASS);
"Saved" is never shown. `saved_revision`/`disk_observed` advance **only after step 8**, so a crash between
`rename` and `fsync(dir)` cannot leave us believing we saved when the directory entry might not survive.

**Preservation:** permissions, ownership (best-effort), ACL/xattr, encoding (`fileencoding`), line endings
(`fileformat` unix/dos/mac), BOM (`bomb`), and trailing-newline policy are captured at open and re-applied on
write — kept separate from document data ([TEXT-19 / VIM-STATE](../parity/vim.md)).

**Per-platform notes:**

| Concern | POSIX | Windows |
| --- | --- | --- |
| Atomic replace | `rename()` replaces atomically | `ReplaceFileW` (preserves ACLs/attrs/streams, keeps object identity) or `MoveFileEx(MOVEFILE_REPLACE_EXISTING\|WRITE_THROUGH)` |
| Directory durability | `fsync(dir_fd)` after rename | no dir-fsync; `FlushFileBuffers` on the file; `WRITE_THROUGH` for the metadata op |
| Sharing/locks | advisory only | mandatory sharing — target may be locked; retry/backoff, surface a typed "file busy" error |
| Ownership | `fchown` (often needs privilege; best-effort, warn) | preserve owner/ACL via `ReplaceFileW`; SIDs not uid/gid |
| Symlinks | write through the link target, don't replace the symlink itself (unless configured) | reparse points; resolve before replace |
| Case / normalization | case-sensitive (usually) | case-insensitive; also macOS NFD normalization (XPLAT §13) |
| Special files (`/dev`, FIFO, network FS) | detect; temp+rename may be impossible → fall back to in-place careful write, warn, and never claim durability we don't have |

Cross-device / same-directory: the temp **must** be in the target's directory (a `/tmp` temp cannot be
`rename`d across filesystems atomically). If the directory isn't writable (read-only mount, permission), save
fails cleanly rather than falling back to a non-atomic path silently.

### 4. Three-way crash recovery

On opening a file that has a recovery journal whose `saved_revision` point is *behind* recoverable records (a
prior crash), recovery presents **three** materialized options and **never auto-applies** any of them
([design-requirements.md §3](../architecture/design-requirements.md), D-005):

```rust
enum RecoveryOutcome {
    /// (a) The current on-disk file as-is. Discards recovered changes (journal archived, not deleted).
    KeepDiskFile,
    /// (b) The recovered document = replay journal to last-good record (§2). Shown as a DIFF vs disk first.
    RecoverChanges { recovered: DocumentSnapshot, up_to: JournalPos, lost_tail: bool },
    /// (c) Keep editing the just-opened in-memory document; recovery data retained until explicitly resolved.
    KeepInMemory,
}
```

Hard rule: recovery **writes nothing to the original path** on its own. Recovery produces an in-memory document
and (optionally) a side file (`<name>.ruse-recovered`), and the user chooses; only an explicit save (§3) touches
the original. If replay hit a torn tail, `lost_tail = true` is surfaced ("recovered up to HH:MM:SS; N seconds of
work after that could not be recovered"). This is the loss-safe, preflight-first stance of
[stability §13](stability-and-observability.md): we do not "apply and hope to recover."

**Swap-equivalent / second-editor detection.** An ownership marker (a lock record with pid/host/start-time,
Vim-swap-analogous) distinguishes "another live ruse has this file open" (→ read-only or attach prompt) from "a
dead marker from a crash" (→ offer recovery). Stale markers are detected by liveness check, never blindly
honored.

**Retention + PII.** Recovery/journal files:
- live under a per-user recovery dir (workspace-scoped subdir), mode `0600` / owner-only ACL;
- have a **retention window** (default 14 days) and a **count cap** per file; a GC sweep removes journals whose
  save-point is fully durable and older than the window, and archives (not deletes) journals with unrecovered
  tails until the user resolves them;
- carry document *content*, so they are treated as sensitive (INV-TRUST-1): excluded from diagnostic bundles by
  default, redaction-preview before any export (D-017), never logged, and never synced off-box without consent.
  A path-redaction option stores only reversible-locally content for high-sensitivity workspaces (Open questions
  covers encryption-at-rest).

### 5. External-change conflict detection

Between our last `OurSave` and now, another process may change the file (git checkout, formatter, another
editor). We detect this by comparing `disk_observed` against our last known `OurSave` at the moments it matters,
using both the cheap hint and the authoritative hash:

- **Sources:** a filesystem watcher (treated as *best-effort* — missing/duplicate events are normal, XPLAT §13),
  plus an explicit re-stat on focus-gain, before-save, and before-reload. The watcher only *arms* a check; the
  stat+hash *decides* (mtime alone is untrusted: touch-without-change, mtime granularity, clock skew).
- **Decision:** if `stat` differs, hash the disk bytes; if `content_hash` differs from our last `OurSave` hash,
  set `disk_observed.provenance = ExternalObserved` → `disk_changed_externally` becomes true.

**Surfacing (INV-STATUS, typed event):**
- Buffer *not* modified locally (`!is_modified`): offer/auto-reload is safe *only* if user opted in; default is
  a non-blocking prompt ("file changed on disk — reload?").
- Buffer *is* modified locally **and** disk changed: this is a **conflict**. It surfaces as a typed
  `ExternalChangeConflict` error/event with a three-way choice (keep mine → will overwrite on next save; take
  theirs → reload, my changes go to a recovery side-file; diff/merge). A **save into this state is refused by
  preflight** ([stability §13](stability-and-observability.md) — "file changed externally"): we never silently
  overwrite an externally-changed file; the user must acknowledge. This mirrors Vim's `W12`/`FileChangedShell`.

### 6. Undo grouping boundaries (resolves the open D-005 part)

Undo is recorded by **logical unit, not per keystroke** (INV-UNDO, [architecture §3.3](../architecture/architecture.md)). The
grouping rule keys off `TransactionOrigin` (INV-ORIGIN) plus an idle/gesture boundary:

**Default grouping policy:**

| Situation | Boundary rule |
| --- | --- |
| One insert session (`i … <Esc>`) | **one** undo group — all keystrokes between entering and leaving Insert coalesce (Vim semantics). |
| A single operator (`dw`, `>}`, `ciw…<Esc>`) | one group (the whole change, including inserted text — [VIM-REPEAT-DOT](../parity/vim.md)). |
| Consecutive same-origin `UserInput` edits with no intervening cursor-gesture/idle break | may coalesce within the session; a **mode change, cursor jump, or idle timeout closes the group**. |
| Formatter / `Lsp` edits (format-on-save, code action, rename) | grouped **separately** by their `correlation` — one code action = one undo group, distinct from surrounding typing, even if applied mid-session. |
| `AiAgent` edits | always a **separate** group per proposal (INV-TRUST-1: reviewed before apply); never merged into user typing. |
| `Macro` playback | the whole macro invocation is **one** group (so a single `u` undoes one `@a`), matching Vim. |
| `Plugin` / `RemotePeer` | one group per `correlation` by default; a plugin may request finer/coarser via the transaction API but cannot merge across origins. |

**Rule:** two adjacent transactions merge into one undo group **iff** same `origin`, same `correlation`
(or both `UserInput` within one insert/gesture session), and no explicit break between them. Different origins
**never** silently merge — a formatter edit landing while you type does not swallow your keystrokes into its
group.

**Explicit break / join:**
- `group_hint = BreakBefore` — start a new group even within a session. Backs Insert-mode **`C-g u`**
  ([VIM-INS / VIM-UNDO](../parity/vim.md)): mid-insert undo-break so one `u` stops at the break, not the whole
  insert.
- `group_hint = JoinPrev` — merge this transaction into the previous group even across the normal boundary.
  Backs **`:undojoin`** (the next change joins the prior undo block; if the prior block was already undone,
  `:undojoin` is an error, matching Vim).
- Line-level **`U`** (VIM-UNDO-1) is modeled as a synthesized inverse over "all changes to the current line
  since the cursor last landed on it" — a derived group, not a tree node type; re-doable because it is itself an
  undoable transaction.

Grouping lives in the in-memory undo history; the journal stores every transaction individually with its
`group_hint`, so recovery reconstructs the same groups (grouping is metadata, replay is per-transaction).

### 7. Chronological index over the branching undo tree

The undo history is a **tree**: undoing then making a new change **branches** (no history is lost —
[COM-8](../parity/common.md), Vim `undo.txt`). Parent/child expresses *document lineage*. But Vim's `g-`/`g+`
and `:earlier/:later {N|Nf|5m}` traverse states in **the order they were created in wall-clock/sequence time** —
which zig-zags *across* branches and is **not** the same as parent/child. So we keep two structures over the
same nodes:

```rust
struct UndoNode {
    id: UndoNodeId,
    parent: Option<UndoNodeId>,      // lineage: the state this was derived from (undo goes here)
    children: Vec<UndoNodeId>,       // redo branches (newest child = default redo)
    seq: MonotonicSeq,               // creation order — the chronological key
    saved_here: bool,                // was saved_revision ever == this node (for '[+]'/':earlier {N}f')
    result_revision: Revision,
    // the transaction (forward+inverse) that produced this node from parent
}

struct UndoHistory {
    nodes: SlotMap<UndoNodeId, UndoNode>,
    current: UndoNodeId,             // where the document is now

    /// Chronological index: nodes in strictly increasing `seq`. A node is appended here ONCE,
    /// when created, and never reordered. This is a separate total order from the tree edges.
    chronological: Vec<UndoNodeId>,  // (or an order-statistics tree if we need O(log n) :earlier N)
}
```

**Navigation:**
- **Tree** (`u` / `C-r`): `u` = move `current` to `parent` (apply the node's *inverse*); `C-r` = move to the
  most-recent `child` (apply that child's *forward*). This is normal undo/redo — stays on lineage.
- **Chronological** (`g-` / `g+`): find `current` in `chronological` by its `seq`, then step to the previous /
  next entry regardless of tree branch. Moving between two chronological neighbors that are **not** in a
  parent/child relationship is realized as *walk up to their lowest common ancestor applying inverses, then walk
  down applying forwards* — a single logical "time step" the user sees as one `g-`. `g-`/`g+` therefore visit
  **every** state ever reached, in creation order, exactly as Vim does.
- **`:earlier`/`:later`**:
  - `Nf` (file-writes): step chronologically across nodes where `saved_here` toggled — i.e. `N` save-points
    back/forward. `saved_here` makes this O(scan) or O(log n) with the order-statistics variant.
  - `N` (count): `N` chronological steps.
  - `5m` / `10s` (time): binary-search `chronological` by each node's display `timestamp` (this is the one place
    wall-clock time is *consumed*, and it's display-sourced, tolerant of coarse granularity).

Because `chronological` is append-only and `seq` is monotonic, the temporal order is stable under new branches:
a new change appends a node with a larger `seq`; it never renumbers existing nodes, so `g-` history is immutable
history, matching Vim's guarantee that branching loses nothing.

Persistent-undo (Vim `undofile`): the journal's Edit records already carry `seq`, `base_revision`,
`result_revision`, `timestamp`, and `group_hint` — enough to reconstruct the *entire tree and its chronological
index* on reopen, giving cross-session undo without a separate `undofile` format. (Reconstruction is the same
forward replay as §2, then linking each node's parent by `base_revision`.)

### 8. Per-buffer-kind behavior (INV-BUFFER-KIND)

Buffer *kind* determines the mutation contract ([workspace.md](../parity/workspace.md)); journal and undo
differ accordingly:

| Kind | Journal | Undo history | Save/atomic §3 |
| --- | --- | --- | --- |
| **Editable Document** | full append-only journal (§2); incremental mode for large files | full branching tree + chronological index (§7) | yes |
| **Read-only Document** (git rev, LSP virtual doc) | none (immutable source) | none | n/a |
| **Generated Document** (help, command output) | none; regenerable from its source | none | n/a (regenerate, don't persist) |
| **Streaming Document** (logs, build output) | **no** transaction journal; an optional **bounded ring** append log for scrollback only; content is not "recoverable work" | none / bounded — appends are not transactions | n/a |
| **Interactive View** (git status, file tree, debugger) | none — edits are **domain CommandRequests** (V-14), not text transactions, each preflighted through its own service | the *domain* action is undoable via its service (e.g. `git reset`), not via text-undo | n/a |
| **Terminal / PTY** (WS-5) | none; scrollback is a bounded ring; input goes to the PTY, not a Document | none | n/a |
| **Large-file / degraded** | incremental journal (§2), **bounded** undo window | capped ring, older states recoverable-only, not undoable | yes (careful/streamed write; still atomic where possible) |

The append path for streaming/terminal buffers is explicitly **not** a full Transaction (INV-BUFFER-KIND): no
inverse-edit is generated, no undo node is created, and nothing is journaled for crash recovery (a terminal's
output is not the user's unsaved work). This is the concrete resolution of the INV-TXN↔INV-UNDO contradiction
flagged for F-011/WS-5/COM-12 (V-4): those buffers opt out of both, by kind, rather than forcing a fake
transaction or an unbounded undo log.

## Failure modes

- **Torn journal tail** (crash mid-append) → replay stops at last-good record, truncate to it, `lost_tail`
  surfaced (§2, §4). No corruption of the intact prefix.
- **Journal corruption in the middle** (bad sector) → replay halts at the corrupt record; everything after is
  treated as lost; the recovered prefix is offered as a side-file, never auto-written (§4).
- **Crash between `rename` and `fsync(dir)`** → on POSIX the directory entry may or may not survive; because
  `saved_revision` advances only after `fsync(dir)`, we still consider it unsaved and offer recovery — safe
  underclaim, never overclaim durability (§3).
- **Disk full / quota** during temp write or fsync → save fails cleanly, temp unlinked, original untouched,
  typed error; the in-memory doc and journal are intact (fault-injection target, §18).
- **External overwrite while modified** → conflict; save refused by preflight; three-way resolution (§5).
- **Second editor** on the same file → ownership marker → read-only/attach; a dead marker → recovery offer (§4).
- **Permission loss mid-session** (file becomes read-only) → detected at save preflight, surfaced, edits kept in
  journal so nothing is lost.
- **Core-invariant violation** → INV-FAIL-BOUNDED recovery snapshot (a Checkpoint + journal flush) then safe
  shutdown; reopen path uses §4.

## Recovery behavior

Summarized: on open, if a journal with unrecovered work exists → replay to last-good (§2) → present three-way
(§4), never auto-writing the original. On core-invariant failure → snapshot+flush then safe shutdown. On
external change → conflict surfacing (§5). Retention GC removes fully-durable, out-of-window journals and
archives ones with unresolved tails; recovery data is owner-only and redacted from diagnostics (§4).

## Security impact

Recovery/journal files contain full document content → sensitive by default (INV-TRUST-1): `0600`/owner-only,
excluded from diagnostic bundles, redaction-preview before export (D-017), never logged, not synced without
consent. External-change detection is a defense against silently overwriting another process's (or another
user's, on shared mounts) work. `AiAgent`/`Plugin`/`RemotePeer` origins are journaled with their `origin` +
`correlation` so a post-hoc audit ("what did the AI change") is exact (SEC-15). Ownership markers include
host/pid to prevent cross-user swap confusion.

## Performance impact

- Interactive edits append to an in-memory buffer + the journal without per-keystroke fsync; fsync is batched
  on idle/timer and forced before risky ops — keeps input off the durability critical path (PERFS, D-019).
- Storing both forward and inverse edits per record makes undo O(record) and recovery a single forward pass —
  no re-diffing.
- Chronological index is append-only `O(1)` insert; `:earlier N` is `O(N)` scan or `O(log n)` with an
  order-statistics tree (chosen if profiling shows large histories matter).
- Large files never snapshot the whole document (incremental journal + bounded undo), bounding memory/IO
  ([COM-12](../parity/common.md)).
- Save cost is dominated by the two fsyncs (data + dir); acceptable because "Saved" must mean durable.

## Compatibility impact

Delivers [COM-9](../parity/common.md) session recovery, the undo-tree half of [COM-8](../parity/common.md), and
[VIM-UNDO-1](../parity/vim.md) (`g-`/`g+`, `:earlier/:later Nf`, `U`, `:undojoin`, `C-g u`) onto ruse's
transaction/undo model. Encoding/EOL preservation satisfies [COM-13 / VIM-STATE](../parity/common.md). Formats
are versioned (`schema_ver`, INV-ADDITIVE) so future multi-client (D-012) / remote-offline (D-013) journaling
extends them without a breaking change. We do **not** claim Vim `undofile` binary compatibility (Non-goals).

## Observability

`:debug transactions` (already in [stability §14](stability-and-observability.md)) shows the live transaction
stream. Add derived-state visibility to the status view (INV-STATUS): modified / journaled-unsaved /
disk-changed. Journal health (position, last fsync age, truncation events) is a per-component Status with
*freshness*. Fault-injection CI (truncated journal, disk full, permission loss, external overwrite) asserts the
§2/§3/§5 behaviors ([design-requirements.md §18](../architecture/design-requirements.md)).

## Alternatives

- **Single dirty bool + mtime check** — rejected: cannot express saved-but-stale, unsaved-but-journaled,
  or externally-overwritten-while-modified; each drives different UI/preflight. §1 derives all four.
- **Periodic full-document autosave only (no journal)** — simple, but loses up-to-interval work, is prohibitive
  for large files, and can't reconstruct the undo tree cross-session. We keep autosave as a *complementary*
  safety copy but the journal is primary.
- **In-place careful write (write() over the file)** — rejected as the default: a crash mid-write corrupts the
  file. Used only as a last-resort fallback for special files where temp+rename is impossible, and only with an
  explicit "durability not guaranteed" warning.
- **Reuse Vim `undofile` format** — rejected: couples us to a foreign binary format for no user benefit; our
  journal already reconstructs the tree + chronological index.
- **One flat undo list (no tree)** — rejected: loses history on branch, fails [COM-8](../parity/common.md) /
  [VIM-UNDO-1](../parity/vim.md).
- **Derive chronological order by sorting on demand** — rejected vs an append-only index: sorting is redundant
  work and the monotonic `seq` gives a stable order for free.

## Rejected approaches

- **Auto-applying recovered changes to the original file** on open — directly violates D-005 ("recovery never
  auto-overwrites") and can recover *corrupted* state onto good disk data. Always three-way, always user-chosen.
- **Trusting mtime alone** for external-change detection — false positives (touch) and false negatives (coarse
  granularity, clock skew); we always confirm with a content hash.
- **fsync per keystroke** — correctness-neutral but destroys interactive latency; we batch and force before
  risky ops instead.
- **Skipping unknown-major journal records** — an unknown *major* schema means we cannot trust our
  interpretation; we halt safely rather than silently drop data (INV-ADDITIVE handles unknown *minor*/additive
  fields, not major).

## Trade-offs

- Storing forward **and** inverse edits doubles per-record size but buys O(1) undo and single-pass recovery — a
  deliberate space-for-time and space-for-simplicity trade.
- Batched fsync trades a bounded window of `has_unjournaled_work` for interactive latency; the derived-state
  model makes that window *visible* rather than pretended-away.
- Two fsyncs per save (data + dir) make save slower than a bare `write()`, accepted because "Saved" must mean
  durable.
- Best-effort ownership preservation (POSIX often needs privilege) can leave uid/gid unchanged; we warn rather
  than fail the save.

## Migration strategy

Formats are versioned from v1 (`magic RJ01`, `schema_ver`). No prior format exists (pre-F-008), so v1 is
greenfield. Future changes: additive fields bump minor and are ignored by older readers per INV-ADDITIVE;
layout-breaking changes bump the record major and readers refuse unknown majors (halt-safe, §2). A journal
written by a newer ruse is read by an older ruse only up to the first record it doesn't understand, and it
never auto-overwrites — so cross-version open degrades to "recover as much as safely possible", never silent
loss.

## Test strategy

- **Property tests**: apply random transaction sequences → `document_revision` strictly increases (INV-TXN);
  `is_modified` derivation matches an oracle; undo/redo round-trips restore exact text (INV-UNDO); chronological
  traversal visits every node exactly once in `seq` order.
- **Differential (parity, TEST-2)**: `g-`/`g+`, `:earlier N/Nf/5m`, `:later`, `U`, `:undojoin`, `C-g u`,
  insert-session grouping, macro-as-one-group vs a Vim oracle ([VIM-UNDO-1](../parity/vim.md)).
- **Fault injection (§18)**: truncate the journal at every byte offset → replay always yields a valid prefix and
  a correct `lost_tail` flag; kill between `rename` and `fsync(dir)` → recovery offered, never a false "Saved";
  disk-full / permission-loss during save → original intact, typed error; external overwrite while modified →
  conflict + save refused by preflight.
- **Cross-platform**: encoding/EOL/BOM/no-trailing-newline round-trip; POSIX vs Windows atomic-replace and
  metadata preservation; macOS NFD normalization; symlink target preservation.
- **Buffer-kind**: streaming/terminal buffers create no undo nodes and no journal; interactive-view edits route
  to domain CommandRequests, not text transactions (INV-BUFFER-KIND, V-14).

## Open questions

- Exact large-file thresholds (32 MiB / 5 M lines / long-line guard) and whether they are user-tunable — must be
  validated on real workloads (mirrors D-018's "budgets need real load").
- fsync batching cadence (idle timeout + max-unjournaled-work bound) — tune against latency budgets (D-019).
- Encryption-at-rest for recovery files in high-sensitivity workspaces vs the default `0600` + redaction
  (relates to D-017).
- Whether `:earlier N` needs the O(log n) order-statistics tree or the O(N) scan suffices in practice.
- Interaction with remote runtime (D-013): does the journal live client-side, runtime-side, or both, and how
  does offline reconnect reconcile — deferred to F-017, the versioned format is designed to extend into it.
- Whether autosave (full-doc safety copy) is on by default or opt-in given the journal already covers recovery.

## Reference Invariants

This doc depends on / enforces (registry: [reference-invariants.md](../invariants/reference-invariants.md)):

- **INV-TXN** — every editable-Document mutation is a Transaction carrying `base_revision`; revision strictly
  increases. The journal is its durable projection (§1, §2).
- **INV-UNDO** — every such Transaction is undoable by logical unit; history has no orphaned parents. The
  branching tree + chronological index are its in-memory projection (§6, §7).
- **INV-BUFFER-KIND** — ephemeral/append-only/streaming/terminal/large-file-degraded buffers are the explicit
  exception: bounded-or-absent undo, non-transaction append, no crash journal (§8).

Also relied on: INV-ORIGIN, INV-ASYNC-ORDER, INV-POS-TYPED, INV-ANCHOR, INV-FAIL-BOUNDED, INV-ERR-CLASS,
INV-STATUS, INV-ADDITIVE, INV-TRUST-1.
