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
  - ../architecture/architecture.md
  - stability-and-observability.md
  - ../architecture/design-requirements.md
  - ../parity/common.md
  - ../parity/vim.md
  - ../parity/workspace.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->
<!-- design-code-ack: UndoNode — design ahead of code: the first vertical slice stores forward/inverse/origin per undo node (in-memory undo/redo) and tracks the saved point via Document.saved_node, with the node id being its slot index; the persistent-undo `id`/`saved_here` fields land with persistent undo (#639). -->

# ruse Persistence, Crash Recovery & the Undo Journal

> **Shipped (RFC-0023 slice 2, D-064).** The journal is the per-transaction `RJ01` stream of §2
> (`apps/tui/src/persist/rj01.rs`) in `$XDG_STATE_HOME/ruse/journal/`, written by
> `apps/tui/src/persist/journal.rs` after 4 s idle, 200 changes or before a risky operation; a crash path appends
> a Checkpoint of the exact text. A legacy `<file>.ruse-journal` (`RJS0` whole-buffer snapshots) is read once and
> archived. Undo is in-memory (`crates/core/src/undo.rs`). Still the target: the three-way recovery of §4, the
> `InUse` owner check, archive retention and the status flags (RFC-0023 slices 3-4); the status of each save and
> recovery surface is in [PRD.yaml](../../spec/PRD.yaml) F-008.
>
> **Journal lifecycle** (#862, #863, #869, #873, #1191). A journal exists only while it holds work that differs from
> disk. It is deleted by a durable save; by the recovery prompt's discard; when the buffer becomes clean without a
> save (undo back to the saved text, `:e!`); by `:bd[!]`; and on every normal exit (`:q`, `:q!`, `:qa!`, `:cq`,
> `ZQ`), as nvim deletes its swap file on these. It is kept by a fatal signal or a panic (which write it), by the
> prompt's quit choice, and when it could not be read (never offered, so never deleted). A recovered buffer starts
> as the disk text with the recovery applied as one edit: it is modified, `u` returns to disk, and a saved trace
> starts from the disk text. Only a regular file is ever opened as a journal (a FIFO or device is reported, not
> read). A record over 4 GiB is refused with an error, not truncated into an unreadable one.

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

> **Decided details (D-064, [RFC-0023](../rfc/accepted/RFC-0023-per-transaction-journal.md)):** the core change
> log the journal drains, the state-dir location and naming, the sync policy, the 32 MiB `DiskBase` mode, the
> legacy `RJS0` migration, and SHA-256 (not BLAKE3) for content hashes. Where this section and the RFC differ, the
> RFC wins.

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

**Swap files, auto-save files and locks (v0.1.9, #619).** The RJ01 journal is ruse's swap file (Vim) and
auto-save file (Emacs); the options that steer those map onto it:

| Option | Mapping |
| --- | --- |
| Vim `'swapfile'` (per buffer; `:set` also sets the default for buffers opened later), Emacs `auto-save-default` | off: the buffer keeps no journal, and an existing one is discarded |
| Vim `'updatecount'` | the change count that syncs the journal (default 200, the old `SYNC_EVERY`); 0 keeps no journal, as nvim keeps no swap file |
| Vim `'directory'` | read-only: `:set dir?` shows the journal directory with `//`; another value is "option not supported", since a moved journal would hide unsaved work from the next session's recovery |
| Emacs `auto-save-file-name-transforms` | divergence: the journal's name and place are fixed (`<name>.<path hash>.rj`), for the same reason |
| Emacs `lock-file-name-transforms` (and lock files) | divergence: no `.#name` lock file; a live journal's owner (pid + start time) is the lock, and a second session gets the in-use prompt (E325's choices, #1208) |
| Emacs `auto-save-visited-mode` / `auto-save-visited-interval` | built: after the interval idle, every modified buffer visiting a writable file is saved to it, once per idle period; `auto-save-visited-predicate` (a Lisp function) is not offered |

The idle sync time stays 4 s (`'updatetime'` is not an option yet).

### 3. Atomic save

Save is **durable-or-nothing**. "Saved" in the UI reflects a durable write (fsync of data *and* the containing
directory), not a returned `write()`.

**POSIX algorithm:**

```
1. dir ← parent dir of target (`.` for a bare relative name).
2. stat target → refuse a non-regular file (FIFO, socket, device, directory); refuse a file the user cannot
      write with E505 unless forced (`:w!`); a file with more than one hard link is written IN PLACE (below).
3. create temp in the SAME directory (same filesystem so rename is atomic): ".<name>.ruse-tmp-<rand>"
      (<name> shortened to keep the whole within NAME_MAX), O_WRONLY|O_CREAT|O_EXCL, mode 0600 initially,
      a fresh <rand> on EEXIST; a user's file of any name is never removed. EACCES/EROFS on an existing
      target → IN PLACE.
4. write encoded bytes (apply fileencoding + fileformat/EOL + optional BOM — TEXT-19, VIM-STATE); handle
      no-trailing-newline ('binary'/'noeol') exactly.
5. fchown temp to the captured uid/gid when they differ (EPERM → IN PLACE); fchmod to the captured mode;
      copy every xattr (on Linux this includes the POSIX ACL; a value that cannot be set → IN PLACE).
6. fsync(temp)                              # data and metadata durable before it becomes the file
7. rename(temp, target)                     # atomic replace on POSIX
8. open + fsync(dir)                        # make the rename itself durable; a failure here is E667,
      except EINVAL/ENOTSUP/EOPNOTSUPP/EISDIR (the filesystem does not support it), which saves with
      the note "directory sync not supported here; rename durability not proven"
9. close dir fd.
10. On success: saved_revision ← document_revision; saved_node ← current_undo_node.seq; disk_observed ← OurSave{revision, new hash/stat};
      append a Meta(save-point) record; only THEN surface "Saved".
```

If any step before 7 fails the temp file is unlinked and the original is untouched (INV-FAIL-BOUNDED,
INV-ERR-CLASS); "Saved" is never shown. `saved_revision`/`disk_observed` advance **only after step 8**, so a
crash between `rename` and `fsync(dir)` cannot leave us believing we saved when the directory entry might not
survive: a failed step 8 reports `E667` ("written but not proven durable"), the buffer stays modified and its
journal stays. A filesystem that does not support a directory fsync at all (`EINVAL`, `ENOTSUP`/`EOPNOTSUPP`,
`EISDIR` from opening or fsyncing the directory; some network and FUSE filesystems) is not a failure, as in
SQLite and PostgreSQL: otherwise `:w` could never succeed there although the file's bytes are fsynced. The
save then counts and the buffer is marked saved, but the report says `written (directory sync not supported
here; rename durability not proven)` and makes no durability claim for the rename. A real I/O error (`EIO`,
`ENOSPC`, `EROFS`, `EACCES`, ...) stays `E667`.

**In place** (Neovim's `'backupcopy'` `auto`, Emacs's `backup-by-copying-when-linked`): when a rename would
change the file's identity — other hard links would keep the old bytes, the owner/group or an xattr/ACL
cannot be reproduced on a new inode — or the directory is not writable, the existing file is opened without
truncation, overwritten, truncated to the new length and fsynced. This keeps links, owner, ACLs and xattrs
and is durable, but not atomic: an error after the first byte is reported as `E514` ("may be partly
written"), never as "written", and the buffer stays modified. A crash during the write can leave a partial
file; the recovery journal (when its directory is writable) is one safety net and the `'writebackup'` copy
(below) the other. With `:w!`, a file the owner has made read-only gets owner write permission for the
duration of an in-place write, then its mode back.

**Write policy and backups (#619; `persist/backup.rs`, `app/write_opts.rs`).** One engine serves both
editors' option families; each profile reads its own editor's options with that editor's defaults (Vim and
Native: nvim v0.12.6; Emacs: Emacs 31.1, config keys `emacs.*`):

| Concern | Vim profile | Emacs profile |
| --- | --- | --- |
| Method | `'backupcopy'`: `auto` (above), `yes` in place, `no` rename even over hard links; `breakhardlink` / `breaksymlink` | `file-precious-flag` = `no`; `break-hardlink-on-save` = `breakhardlink`; precious without `file-preserve-symlinks-on-save` = `breaksymlink`; the save that makes the backup copies (in place) or renames by `backup-by-copying*` |
| When a backup is made | every write with `'backup'`; a write backup for an in-place write with `'writebackup'` (removed after it, kept and named when the write fails) | the first save of a visit that finds the file on disk (`make-backup-files`; `buffer-backed-up`); a re-read (`:e!`) or a new name starts a new visit |
| Name and place | `<name><'backupext'>` in the first `'backupdir'` entry that works (`.`, `./sub`, a `//` entry names it after the whole path with `%`); a missing directory is created 0700 | `<name>~`, or `<name>.~N~` under `version-control`; beside the file or in the `backup-directory-alist` directory (absolute: whole path with `!`) |
| Exempt files | `'backupskip'` patterns (default: the temp directories) | under `temporary-file-directory` / `small-temporary-file-directory` |
| Old versions | `'patchmode'`: `<name><pm>` beside the file, once (empty for a new file) | numbered versions pruned to `kept-old-versions` + `kept-new-versions` when `delete-old-versions` is `t` |

A rename-path backup is a hard link to the file being replaced (exactly the old file), else a copy; an
in-place write's backup is always a copy. A backup name never follows a symlink or replaces anything but a
regular file. A backup that cannot be made stops the write (`E510`, or `E509` when copying) unless `:w!`
forces it (event `save.backup_failed`). A write by rename needs no write backup: the original stays whole
until the rename, so the temp file is the write backup. `'write'` off makes every write `E142`;
`'writeany'` drops `E13` (not `E505`/`E45`/the changed-on-disk refusal, as nvim); `'nofsync'` skips steps 6
and 8, so "written" no longer claims durability (the user's explicit choice). Divergences: Emacs's
`make-backup-file-name-function` needs Lisp (the naming keys cover its default function); `delete-old-versions`
`nil` and `require-final-newline` `ask` would prompt inside a save, so ruse keeps the versions (the report
counts them) and adds no newline (`[noeol]` shows it); `visit` / `visit-save` add nothing at visit time
(ruse never edits a buffer on open). There is no `M-x set-variable`: the Emacs variables are config keys
(`:config-reload` applies a change).

Known gap: macOS ACLs are not visible as xattrs, so the rename path does not carry them over.
`com.apple.provenance` is attached by the kernel to every file a process creates, so it is neither copied
nor a reason to write in place.

**Preservation:** permissions, ownership (best-effort), ACL/xattr, encoding (`fileencoding`), line endings
(`fileformat` unix/dos/mac), BOM (`bomb`), a DOS file's final CTRL-Z (`endoffile`) and trailing-newline policy
(`endofline`, `fixeol`, Emacs `require-final-newline`) are captured at open and re-applied on write — kept
separate from document data ([TEXT-19 / VIM-STATE](../parity/vim.md)). `:set eol` / `eof` / `bomb` change them
per buffer (a BOM change modifies the buffer, as nvim).

**Per-platform notes:**

| Concern | POSIX | Windows |
| --- | --- | --- |
| Atomic replace | `rename()` replaces atomically | `ReplaceFileW` (preserves ACLs/attrs/streams, keeps object identity) or `MoveFileEx(MOVEFILE_REPLACE_EXISTING\|WRITE_THROUGH)` |
| Directory durability | `fsync(dir_fd)` after rename | no dir-fsync; `FlushFileBuffers` on the file; `WRITE_THROUGH` for the metadata op |
| Sharing/locks | advisory only | mandatory sharing — target may be locked; retry/backoff, surface a typed "file busy" error |
| Ownership | `fchown` (often needs privilege; best-effort, warn) | preserve owner/ACL via `ReplaceFileW`; SIDs not uid/gid |
| Symlinks | write through the link target, don't replace the symlink itself (unless configured) | reparse points; resolve before replace |
| Case / normalization | case-sensitive (usually) | case-insensitive; also macOS NFD normalization (XPLAT §13) |
| Special files (`/dev`, FIFO, socket) | detect and refuse (`is not a regular file`, #907): never replaced by a regular file, never written into | n/a |

Cross-device / same-directory: the temp **must** be in the target's directory (a `/tmp` temp cannot be
`rename`d across filesystems atomically). If the directory isn't writable (permission), an existing writable
file is written in place as above (as Neovim does), and a new file fails cleanly; on a read-only mount both
fail cleanly.

**Line endings, `'binary'` and large files (v0.1.9, #619; `persist/encoding.rs`).** `'fileformat'` is per
buffer (`unix`, `dos`, `mac`; `:set ff=` modifies the buffer and converts the file on the next write, whose
report says `[dos]` / `[mac]` as nvim's `msg_add_fileformat`). `'fileformats'` decides a read as nvim v0.12.6:
`dos` when listed and every line break is CRLF, else `unix` when listed and the file has a LF, else `mac` when
listed and it has CR but no LF, else the first listed; one name forces it, an empty list takes the global
`'fileformat'` (also the format of a buffer without a file). `++ff=unix|dos|mac` forces one read or write.
`'binary'` (`:set bin`, `++bin`, `ruse -b`) reads and writes the bytes unconverted (no line endings, BOM or
CTRL-Z, the final newline only as `'endofline'` says, whatever `'fixeol'`), and zeroes `'textwidth'` /
`'wrapmargin'` and `'expandtab'` until it goes off. The Emacs profile asks before visiting a file over
`large-file-warning-threshold` (`File f is large (10 MiB), really open? ([Y]es, [N]o, [L]iterally, [?])`,
`l` reads it with `'binary'`) and warns past `out-of-memory-warning-percentage` of the free RAM and swap.

A file read past the large-file threshold (32 MiB, `rj01::LARGE_FILE`, the incremental-journal one) enters
the **degraded profile** (RFC-0008 §7): no syntax highlighting, and `[large]` on the status line. Still to
come for the profile: bounded undo, streaming search and the long-line guard (the core has no undo-window
bound yet). The journal's format byte carries neither `mac` nor `'binary'`: a recovered buffer of such a file
comes back unix (with #1286's `'endoffile'`). The startup file is not asked about with the Emacs
large-file question (the question needs the session, which starts after the file is read).

### 4. Three-way crash recovery

> **Decided details (D-064, [RFC-0023](../rfc/accepted/RFC-0023-per-transaction-journal.md) §6-§9):** the prompt
> keys, the live-owner case, when a journal is deleted versus archived, retention numbers, and the status flags.
>
> **As built (v0.1.8, RFC-0023 slice 3, #933):** `persist::Recovery` is `None` / `Available` / `InUse` /
> `Unreplayable` and one prompt (`apps/tui/src/ui/recovery.rs`) serves the startup file and `:e`. Where the build
> differs from the text below: (c) `KeepInMemory` (`e`) opens the disk file and moves the journal to the archive,
> as `k` does; the two differ only in wording, and retention (#934) treats their archives alike. The diff is a full-screen
> read-only view inside the prompt, not an editor buffer (the startup prompt runs before the editor exists).
> There is no `<name>.ruse-recovered` side file. The ownership marker is the RJ01 Header's owner record; a live
> owner is `kill(pid, 0)` plus a matching start time read from the system (`/proc/<pid>/stat`, `proc_pidinfo`),
> and an unreadable start time counts as a crash. "Read-only" is the buffer's `'readonly'` option (#1216): `:w`,
> `:wa` and `ZZ` refuse with E45 unless forced, and the first change warns W10. A session that edits anyway journals
> to `<name>.<pid>.rj` beside the owner's and only ever deletes a journal whose Header names itself. The same
> "beside, never over" rule holds for a journal from a newer major version and for one this process cannot read.

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

  > **As built (#934, RFC-0023 §9, `apps/tui/src/persist/retention.rs`):** a journal with unrecovered work is
  > never swept while live: it stays in `journal/` until the prompt resolves it, and `e`/`k` move it to
  > `journal/archive/`. The sweep covers only that archive: on startup, at most once a day (the time in
  > `journal/archive-sweep`), it removes archives older than 14 days, counted from the archiving (the move sets
  > the file's modification time), and then keeps the newest 20 per document path (`<base>.<path hash>`; a
  > second session's `<name>.<pid>.rj` counts with the owner's; unnamed buffers count as one path; a legacy
  > sidecar by its basename; a name shortened to fit `NAME_MAX` by its shortened stem). Archives are named
  > `<name>@<unix ms>`, and only archiving writes the `@`, so the sweep owns exactly those names (and the
  > `<journal name>.<unix ms>` names of the first build) and no archive escapes it. It removes only regular
  > files, follows no symlink, and does not sweep a symlinked archive directory. The numbers are fixed (no config key).
  > `:checkhealth` shows the archive's file count, size and last sweep.
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

> **As built (v0.1.8, #938, #1219, #1220):** the check runs after a shell command, on `:checktime`, before a save,
> on the 4000 ms idle timer and on focus gain. There is no filesystem watcher. Focus gain needs focus reports
> (DECSET 1004): the terminal guard asks for them when the F-010 capability ledger has them (an xterm-like
> `$TERM` hint or a DECRQM `CSI ? 1004 $ p` reply; a "not recognised" reply or `RUSE_NO_FOCUS=1` turns them
> off) and turns them off on every exit, the signal watchdog's included. nvim sets 1004 on every terminal; a
> terminal that ignores the mode ignores the set. Inside tmux, focus reports reach ruse only with
> `set -g focus-events on` in `tmux.conf`; without it a change is noticed by the other triggers.
>
> **`'autoread'` (v0.1.9, #619):** on by default, as nvim's. An unmodified buffer whose file changed is re-read
> at the check, silently and as ONE undoable change (`Workspace::reload_undoable`; nvim keeps the reload in
> undo while the file is under `'undoreload'`), with the file's new line endings and BOM. A modified buffer
> keeps its text and gets W12; with `:set noar` an unmodified one gets W11 as before. A trace cannot record a
> re-read, so a session that autoread refuses to save its trace, as after `:e!`.

### 5a. Buffer state and leaving changed buffers (v0.1.9, #619)

`'modifiable'` is buffer state on the `Document`; the editor's two entry points refuse a text change while it
is off (`apply_command` for a planned edit or an undo step, `EditorState::apply_ex` for a resolved Ex edit)
with nvim's `E21`, before any register, mark or mode change, so the Transaction layer never sees the edit. A
reload is not such a command. `'modified'` reads the undo-saved state, `:set nomod` marks the buffer saved
and `:set mod` marks it unsaved (`Document::mark_unsaved`, which `:set bomb` also uses).

Leaving a changed buffer (`app/abandon.rs`) follows nvim's `check_changed` / `can_abandon` /
`dialog_changed`: with `'hidden'` (on, as nvim) a buffer no window shows stays loaded with its changes; off,
`:e {other}`, `:enew`, `:b`/`:bn`/`:bp` and `:q`/`:close` of a window are `E37` when no other window shows the
buffer, and `:only` over another window's changes is `E445`. `'autowrite'` writes instead for the `:b`
family and writes every changed buffer before `:!`, `:r !` and a filter; `'autowriteall'` adds `:e`,
`:enew`, `:q` and `:qa`; neither writes a `'readonly'` buffer or one without a file. `'confirm'` (or
`:confirm` on one command) asks with nvim's dialog on the status line (`[Y]es, (N)o, (C)ancel`, plus
`Save (A)ll` / `(D)iscard All` when several buffers changed), then runs the command again, so `:qa` asks
buffer by buffer. Divergences: the dialog is one status line (nvim's console dialog is two); `(N)o` marks the
buffer unchanged and keeps it loaded (nvim may unload it, losing the text it kept); `(Y)es` on a buffer
without a file is `E141` (nvim names it `Untitled` and writes that). The window keys run as the Ex commands
they stand for (`CTRL-W q` / `CTRL-W CTRL-Q` = `:quit`, `CTRL-W c` = `:close`; `ZZ` / `ZQ` already were `:x` /
`:q!`), so the gate covers them. `:set (no)ma` and `:set (no)mod` are recorded in the replay trace
(`ex_modifiable`, `ex_modified`, RFC-0020), so a session with an `E21` refusal saves a trace that replays.

**The Emacs profile (v0.1.9, #619; `app/session/emacs_files.rs`).** Emacs 31.1's questions, in the echo
area, from the `emacs.*` config keys:

- `C-x C-c` is `save-buffers-kill-terminal`: `save-some-buffers` over every modified buffer (PRED `t`, so a
  non-file buffer `buffer-offer-save` offers too), then `Modified buffers exist; exit anyway?` when one is
  left, `Active processes exist; kill them and exit anyway?` for a running terminal
  (`confirm-kill-processes`), and `Really exit Emacs?` (`confirm-kill-emacs`, `yes-or-no-p` or `y-or-n-p`)
  unless it already asked. The palette's Quit runs the same.
- `C-x s` is `save-some-buffers` with `save-some-buffers-default-predicate` (`save-some-buffers-root`: the
  buffers under the nearest `.git` directory above the working one). Its question is
  `Save file /abs/f.txt? (y, n, !, ., q, M-~, C-r, C-f, d or C-h) `: `y`/`SPC` save, `n`/`DEL` skip, `!`
  saves the rest, `.` saves this one and stops, `q`/`RET` stop, `M-~` skips and marks it unmodified.
- `M-x revert-buffer` asks `Revert buffer from file X?` (`Discard edits and reread from X?` when modified)
  unless `revert-without-query` matches an unmodified buffer; `C-x x g` (`revert-buffer-quick`) reverts an
  unmodified buffer at once (`Reverted buffer`) and asks with `y-or-n-p` under
  `revert-buffer-quick-short-answers`. A revert is one undoable change, as `Workspace::reload_undoable`.
- `C-x C-f` reads `Find file: ` from the working directory. Visiting a file whose buffer is stale asks
  `File f changed on disk.  Reread from disk?` (`Discard your edits?` when modified) under
  `query-about-changed-file`, says `... C-x x g to load new contents` without it, and reverts silently
  where `revert-without-query` matches an unmodified buffer.
- `C-x C-q` is `read-only-mode`, on `'modifiable'`: a change says `Buffer is read-only: #<buffer f.txt>`.
  With `view-read-only` it enters View mode: `SPC`/`DEL`/`S-SPC` page, `d`/`u` half a page, `RET` a line,
  `<`/`>` the ends, `q` leaves it (writable again when View mode made it read-only), `e` leaves it
  read-only, `E` makes it writable.
- `yes-or-no-p` takes a typed word and `RET` (`Please answer yes or no.` otherwise), `y-or-n-p` one key;
  `C-g` quits any question (`Quit`). A question is drawn on one row, its start cut when too wide, never
  behind a hit-enter prompt.
- Emacs announces nothing when a file changes under a buffer until it is visited or edited, and has no
  auto-revert by default: in the Emacs profile the idle W11/W12/E211 messages are off and `'autoread'` is
  off.

Divergences: `C-r`, `C-f`, `d` and `C-h` in save-some-buffers (view, diff, help) are not offered (the key
keeps the question); the page keys of View mode move the cursor a page rather than scrolling the window
(the view follows); buffers are offered in buffer-number order, not Emacs's most-recently-selected order;
`save-some-buffers-default-predicate` takes `nil` or `save-some-buffers-root`, no Lisp function.

### 6. Undo grouping boundaries (resolves the open D-005 part)

Undo is recorded by **logical unit, not per keystroke** (INV-UNDO, [architecture §3.3](../architecture/architecture.md)). The
grouping rule keys off `TransactionOrigin` (INV-ORIGIN) plus an idle/gesture boundary:

**Default grouping policy:**

| Situation | Boundary rule |
| --- | --- |
| One insert session (`i … <Esc>`) | **one** undo group — all keystrokes between entering and leaving Insert coalesce (Vim semantics). |
| A single operator (`dw`, `>}`, `ciw…<Esc>`) | one group (the whole change, including inserted text — [VIM-REPEAT-DOT](../parity/vim.md)). |
| Consecutive same-origin `UserInput` edits with no intervening cursor-gesture/idle break | may coalesce within the session; a **mode change, cursor jump, or idle timeout closes the group**. Under the Vim profile every TYPED command outside text entry (and every `i_CTRL-O` command, and the text typed after one) closes it first (Vim's `u_sync`, `Document::sync_undo`), so `xx` is two groups; a save closes it too. |
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
- Line-level **`U`** (VIM-UNDO-1, #605) keeps, per Document, a copy of the last changed line as it was
  before its run of changes (nvim's `u_saveline`; `crates/core/src/line_undo.rs`). `U` swaps that copy in as
  an ordinary transaction in its own group, so `u` undoes it and a second `U` redoes it. It is not a tree
  node type, and undo/redo leave the copy alone; its line number follows lines added or deleted above it.

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

The target format is versioned from v1 (`magic RJ01`, `schema_ver`). The shipped v0 frames have no header. How a v1 reader treats a headerless v0 journal is not decided; #932 should
settle it. Later changes: additive fields bump minor and are ignored by older readers per INV-ADDITIVE;
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
