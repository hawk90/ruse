---
doc: rfc
project: ruse
title: "RFC-0023: Per-transaction recovery journal (RJ01), state-dir location and three-way recovery"
summary: >
  The shipped recovery journal (`RJS0`) stores whole-buffer snapshots next to the file, has no schema version
  or metadata, and recovery is a two-way "recover or discard" prompt (#932, #933, #938). The design doc
  (persistence-and-recovery.md §2, §4) already specifies a versioned per-transaction format and a three-way
  recovery, but leaves open how the core exposes transactions, where journals live, how often they are
  synced, how the shipped format migrates, and what the prompt and status show. This RFC decides those.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../design/persistence-and-recovery.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/contracts/persistence-format.yaml
  - ../../invariants/reference-invariants.md
  - ./RFC-0007-transaction-engine.md
---

# RFC-0023: Per-transaction recovery journal (RJ01), state-dir location and three-way recovery

- **Status:** proposed
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-05
- **Decision link:** D-064 (refines D-005)

<!-- Hard-to-reverse: an on-disk format and its location. Once users have RJ01 journals, every later version must read them. -->

## Summary

The journal becomes a stream of versioned, individually checksummed `RJ01` records, one per applied change,
using the layout in [persistence-and-recovery.md §2](../../design/persistence-and-recovery.md). The core
gains a small drainable log of applied changes; the frontend writes it. Journals move from
`<file>.ruse-journal` into the per-user state directory, as Neovim's swap files do, and carry an owner
record so a live second editor is told apart from a crash. Appends are buffered and synced on idle, on a
change count, and before risky operations. Recovery offers recover, diff, edit-the-disk-file and discard;
discard archives instead of deleting. The status line shows only the states that need action. The shipped
`RJS0` format stays readable.

## Motivation / Problem

- **#932.** `apps/tui/src/persist/journal.rs` frames whole-buffer snapshots (`[RJS0][len][crc][payload]`)
  with no schema version, no metadata and no inverse edits. `spec/contracts/persistence-format.yaml` and
  D-005 promise all three. A snapshot is written every N changes, so a hard kill loses up to N changes, and
  each frame costs the size of the whole buffer.
- **#933.** Recovery has two outcomes (`persist::Recovery::{None, Available}`). F-008 acceptance 3 and
  PERSIST-005 require three: the disk file, the recoverable changes (shown as a diff first), and the current
  in-memory document. Discard deletes the journal, so a wrong key loses the work for good.
- **#938.** The status line shows only `[+]`. A failed journal append is reported once (E303-style) and then
  invisible; a file changed on disk is invisible until the save.
- **Location.** A sidecar `<file>.ruse-journal` is written into the project tree. It shows up in
  `git status`, fails in a read-only directory, and is world-readable by default in a shared directory,
  although it holds document content (INV-TRUST-1). The design doc already says "under the recovery dir".

The design doc fixes the record layout and the recovery outcomes. It does not fix the core API, the sync
policy, the location rule, the migration, or the user-visible prompt and status. Those are hard to change once
journals are on users' disks, so they need a decision now.

## Guide-level explanation

You edit `notes.md` and the machine loses power. Next time you run `ruse notes.md`:

```
Recovery journal for notes.md (ruse pid 4182 on laptop, last change 2026-11-05 14:03:12)
  37 changes after the last save.
  note: the file on disk is NEWER (modified 14:10); recovering replaces it in the buffer
[r] recover   [d] show diff   [e] edit the disk file   [k] discard (archived)   [q] quit
```

- `r` opens the recovered text as an unsaved change on top of the disk file: `[+]` is set and `u` walks
  back toward the disk text.
- `d` shows a unified diff between the disk file and the recovered text in a read-only scratch view, then
  asks again.
- `e` opens the disk file and keeps the old journal (archived) until you deal with it later.
- `k` opens the disk file and archives the journal. Archives expire after 14 days.
- `q` (or `Esc`) quits without opening anything, and the journal stays where it is.

No other key does anything. If the journal's owner is a ruse process that is still running on this host, the
prompt instead says the file is in use and offers `[o]` open read-only, `[e]` edit anyway, `[q]` quit, as
Neovim's E325 does.

## Reference-level explanation

### 1. Core: a drainable change log

`Document` keeps an ordered log of applied changes since a mark. Each entry has:

- `base_revision`, `result_revision`, `origin`, `group_hint`;
- `forward: EditList`, and `inverse: EditList` (already computed for the undo tree);
- `kind`: `Fresh { node, parent }` for a change that adds an undo node (typing, operators, `U`), or
  `Navigate { to_node }` for a move through existing nodes (`u`, `C-r`, `g-`, `g+`, `:earlier`), with nodes
  named by their `MonotonicSeq`.

`Document::drain_changes()` returns and clears the entries. The log is bounded: when it would exceed a cap
(default 4096 entries or 8 MiB of edit payload), it drops its entries and sets an `overflowed` flag, and the
next drain tells the writer to write a Checkpoint instead. The core does no I/O and never depends on the
journal; frontends that do not journal never drain, and the cap keeps memory bounded (INV-FAIL-BOUNDED). The
types follow RFC-0022: positions in `EditList` are already `BytePos`.

As built (slice 1): recording is **off by default** and a frontend turns it on per document
(`Document::record_changes`), because always-on recording measurably slowed small edits (it clones both edit
lists per change) and would hold up to the cap per document that nobody drains. `drain_changes` returns
`ChangeDrain::Changes(entries)` or `ChangeDrain::Checkpoint { reason, text, revision, node }`, where `text` is
the current buffer and `reason` is `NotRecording`, `Started` (recording was just turned on) or `Overflow`; after
an overflow the log records nothing until the drain, since the Checkpoint covers it. Every revision bump is one
entry: an undo step that crosses several nodes is one `Navigate` entry per node, and an empty transaction (a
revision bump with no undo node, #765) is a `Navigate` to the current node with empty edit lists.

### 2. Records

The layout is the one in [§2 of the design doc](../../design/persistence-and-recovery.md):
`magic "RJ01"`, `schema_ver: u16`, `record_kind: u8`, `flags: u8`, `payload_len: u32`, `header_crc: u32`,
payload, `payload_crc: u32`, `end_magic "rj01"`. The CRC is CRC32C (the design doc's choice), not the
CRC-32/IEEE the snapshot journal uses. A CRC is not cryptography, so a table implementation in `persist` is
fine, as today. Content hashes are SHA-256 from the `sha2` crate already in the tree (DEP-CRYPTO-HASH); the
design doc's BLAKE3 would add a crate for no gain at these sizes.

`schema_ver` is `major << 8 | minor` and starts at `1.0`. A newer minor version only appends payload fields,
which a reader skips using `payload_len`; a newer major version stops replay at that record, as the design
doc says.

Payloads are a small hand-written little-endian encoding (lengths as `u32` or `u64`, as named below); no
serialization crate is added. Kinds:

- **Header** (always the first record): schema version, owner (`pid`, host name, process start time), the
  canonical absolute path of the document, its `FileFormat` (EOL, BOM, encoding), and the base of the stream:
  either `DiskBase { len: u64, sha256: [u8; 32] }` or nothing, when the next record is a Checkpoint.
- **Checkpoint**: the full buffer text (`u64` length) plus the current undo node. It is the replay base.
- **Edit**: one drained change: `TransactionMetadata` as in the design doc (seq, revisions, origin,
  group_hint, wall-clock timestamp for display only) and `forward` + `inverse` edit lists. The inverse makes
  later persistent undo possible without a re-diff; it is stored now so the format does not change for it.
- **Meta**: a save-point (`saved_node`, disk `len` + `sha256` after the save), a `FileFormat` change, or a
  rename.
- **Marker**: written after each fsync; replay reports the last Marker's time as "recovered up to".

Payload lengths above `u32::MAX` are refused at write time (the record is not written and the append fails
loudly). A Checkpoint for a document that large is not taken; see §5.

### 3. Location, naming and permissions

Journals live in `$XDG_STATE_HOME/ruse/journal/` (default `~/.local/state/ruse/journal/`; on macOS the same
XDG path, as Neovim uses). The directory is created `0700`, files `0600`. A file's journal is named
`<basename>.<16 hex of sha256(canonical absolute path)>.rj`, so two `notes.md` in different trees do not
collide and a human can still tell what it is. A pathless buffer uses `unnamed.<pid>.<start time>.rj`.
Archives go to `journal/archive/` with the same name plus a timestamp. As built (slice 2): the basename is
shortened at a character boundary so the name, plus the archive's `.<unix ms>` suffix, fits `NAME_MAX` (#1196);
the start time in the unnamed name is the process's own startup clock in Unix milliseconds, fixed when the
session starts (#1192); a pathless start offers the newest `unnamed.*.rj` whose pid is no longer running, and
recovering it renames it to this process's unnamed journal. A legacy sidecar holding exactly the disk text is
archived without a prompt, so it stops sitting in the project tree.

This follows Neovim (swap in `stdpath("state")/swap//`, with the path encoded in the name) rather than Vim's
`directory=.,...` or Emacs's `#file#` next to the file. Writing next to the file pollutes the project tree,
fails in read-only directories and inherits the directory's permissions.

When opening a file, ruse also looks for a legacy `<file>.ruse-journal`. If it holds an `RJS0` frame, its
last valid snapshot is offered in the same prompt (the only possible "diff" is the whole-buffer one). On any
choice except `q` the legacy file is moved into the archive. RJS0 is never written again.

### 4. Write and sync policy

- After each command, the session drains the change log and appends the records to an in-memory buffer.
- The buffer is written and `fdatasync`ed when any of these comes first:
  - the editor has been idle for 4000 ms (Vim's `updatetime` default);
  - 200 changes have accumulated (Vim's `updatecount` default);
  - before a risky operation: `:!`, a shell or job start, suspend (`C-z`), a plugin or LSP edit batch, and
    the crash paths (panic hook, fatal signal), which write a Checkpoint of the exact current text.
- Each sync appends a Marker.
- A failed write or sync keeps the buffered records, sets the journal's health to failed, and retries on the
  next trigger. It never drops a record silently.
- The first journaled change of a file writes the Header and a Checkpoint (or a `DiskBase`, see §5). A save
  writes a Meta save-point. When the buffer is saved and clean, the journal is deleted.
- **Compaction.** When the Edit payload since the last Checkpoint exceeds both 1 MiB and the size of the
  Checkpoint, the journal is rewritten as Header + Checkpoint of the current text, by temp file, fsync and
  rename in the journal directory. The current undo history is not carried into the compacted journal.

Today's `Throttle` (a snapshot every N changes) and `MAX_FRAMES` go away.

As built (slice 2): the writer (`apps/tui/src/persist/journal.rs` `Writer`) starts a journal at the first
change after the buffer was last clean, as Header + Checkpoint of the text that change applied to, so the Edit
records chain from the disk text. A Checkpoint drain (recording just turned on with the buffer modified, an
overflow, `:e!` followed by an edit), a journal that is not the length the writer left it (deleted, truncated,
written by another process), and compaction all rewrite the journal whole as Header + Checkpoint of the current
text by temp, fsync and rename; nothing is ever appended behind bytes this writer did not write. The crash path
(`journal::rescue`) appends a Checkpoint and a Marker after the last intact record, cutting a torn tail first,
or writes a fresh Header + Checkpoint when no intact Header is there. A failed write keeps the records and
retries on the next trigger; past 8 MiB buffered they are replaced by a Checkpoint of the current text, so a
journal that keeps failing holds bounded memory. Risky operations are `:!`, `:{range}!`, `:r !`, `:w !`,
`:terminal` and an LSP edit batch (format edits, a rename, a code action's edit, a server's
`workspace/applyEdit`; #1202); ruse has no `C-z` suspend and no plugin edit batch yet. The save-point Meta record is in the format
and the reader but is not written in this slice: a save leaves the buffer clean, which deletes the journal (§6),
so a save point would only matter for a save followed by an edit within one command. A rename (`:saveas`,
`:file`) moves the journal to the new name and appends a Meta rename; a file-format change appends a Meta
format record.

### 5. Large files

A document whose text is larger than 32 MiB (D-005's open threshold, now fixed) gets a `DiskBase` instead of
a Checkpoint: its length and SHA-256 hash. As built (slice 2), the length and hash are of the disk text as the
buffer holds it (decoded with the Header's `FileFormat`, the text the deltas apply to), so the writer hashes
the text it already has instead of re-reading the file, and a replay checks the very bytes it applies to. When
the change log overflows on such a document the writer still writes a Checkpoint of the current text (one
large record) rather than lose the changes. Replay first checks the disk file against that hash. If it matches,
the deltas are applied to it. If it does not, recovery cannot rebuild the text. It says so ("the file changed
on disk since the journal began; the 37 changes cannot be replayed"), archives the journal and opens the
disk file. Compaction is skipped for these documents; a save starts a new journal with a new `DiskBase`.
As built (#1203), the hash is computed on a thread of its own, started when the writer first sees the clean
text (at open, after a save), so the first keystroke does not wait for it (about 90 ms for 33 MiB on an
arm64 laptop, now a few microseconds). The first change's records buffer behind a held Header; the sync
writes the Header and its `DiskBase` first, so no Edit record reaches disk before it. The idle and change-count
triggers wait for the hash; a risky operation's sync waits for it to finish.

### 6. When the journal is deleted

- After a durable save that leaves the buffer clean (`saved_node == current`), as today.
- When a buffer is closed or ruse quits with the buffer clean, including after `u` back to the save point.
  This replaces the current rule that leaves a stale journal (#862).
- After `:q!`, `:qa!`, `:bd!` and `:e!`, which discard changes on purpose. This is what Neovim does with its
  swap file. The discard is final: a `:q!` is not a crash.
- Never on a crash path, and never because of a key that was not one of the prompt's choices.

### 7. Recovery

`persist::Recovery` becomes:

- `None`;
- `Available { recovered, base, changes, last_change, up_to, lost_tail, stale, owner }`, which the prompt
  in the guide-level section renders;
- `InUse { owner }`, when the Header's owner is a live ruse process on this host (`kill(pid, 0)` succeeds and
  the process start time matches);
- `Unreplayable { reason }`, as in §5 or when the stream is from an unknown major version.

As built (#1210), `InUse` also carries the best journal a second session that is gone left beside the live
one's (`<name>.<pid>.rj`), and the note of one that cannot be offered. The in-use prompt names that work and
adds `[r] recover its changes`, which asks the recovery prompt above for that journal; whatever is opened
then edits beside the live session, as `e` does.

The outcomes map to the design doc's `RecoveryOutcome`: `r` is `RecoverChanges`, `k` is `KeepDiskFile`, and
`e` is `KeepInMemory`. A recovered buffer is opened with the disk text as its undo root and the recovered
text applied as one change, so `[+]` is set, `:q` gives E37, and `u` goes back to the disk text (#863). The
trace baseline is the disk text. The diff view is a line-based unified diff (Myers) in a read-only scratch
buffer, computed in `persist`; no crate is added. Recovery never writes the original path; only `:w` does.

As built (#1191, #1194; `persist::recovery_commands`): the change is the trace's existing records, so the
trace format is unchanged. A UTF-8 span is one `ex_edits`. Otherwise the bytes go in as lines: `ex_filter`
over every line, or `ex_read 0` when the disk text is empty. Both keep the buffer's final newline, so when
the recovered text's differs, a one-character `ex_edits` follows. Two records are wrapped in an
`ex_begin macro` / `ex_end` group, which replays as one undo step, as it runs live.

### 8. Status

The status line shows a flag only when something needs attention:

- `[+]` modified, as now;
- `[journal!]` when the last journal write or sync failed. The first failure also gives a one-time message
  with the error and the journal path, like Vim's E297 ("Write error in swap file");
- `[disk changed]` when the file on disk differs from the last thing ruse read or saved. It is checked by
  stat on focus gain, before a save and on the 4000 ms idle timer, and confirmed by hash; mtime alone is
  never trusted.

"Journaled but unsaved" is not shown. It is the normal state of every modified buffer, so a flag for it would
be noise. `:checkhealth` (F-030) reports each buffer's journal path, size, last sync age and failures.

As built (slice 4, first part, #938): `[journal!]` follows `[+]` (`f.txt [+][journal!]`) while the focused
modified buffer's last journal write failed, and goes when a write succeeds or the buffer is clean. The first
failure of a failing run keeps #907's E303 text and adds the journal path after the error; the next failures
are quiet. `:checkhealth` lists every check (one `OK`/`WARN`/`n/a` line each) with the summary on the status
line; its journal part names the journal directory (WARN without a state directory) and, per file buffer,
the size, last sync age, waiting changes and failed writes, then the path (WARN while the last write failed).

As built (slice 4, second part, #938): `[disk changed]` follows `[+]` and `[journal!]`. `persist::disk` keeps,
per canonical path, what this process last read from the file or saved to it: its stat (size, modification
time, inode, device) and a content hash (SipHash of the bytes, a change detector, not a security boundary).
Every read of a buffer's file (startup, `:e`, `:e!`) and every save record it. A check stats the file and
hashes it only when the stat moved, so a `touch` is not a change and an mtime alone is never trusted; a file
that is unreadable differs. Checks run on the 4000 ms idle timer, after a shell command (`:!` and the
other risky operations, as nvim checks after a shell command), on `:checktime`, on focus gain (ruse does not
yet ask the terminal for focus reports: DECSET 1004 is not in the F-010 probe), and before every save. A
newly changed file gives nvim's W11 (buffer unmodified) or W12 (modified) on the status line, once per
change; no reload is offered. A save without `!` into a changed file is refused with nvim's warning ("The
file has been changed since reading it!!!") plus "(add ! to write anyway)", where nvim asks y/n (design doc
§5: never silently overwrite an externally changed file); `:w!` writes, and the save records the new text.
A file that is gone (deleted, renamed away, a dangling link) is not "changed" (#1221): it gives nvim's
`E211: File "<name>" no longer available` once, modified buffer or not, with no `[disk changed]` flag, and a
save writes it again without `!`. A file that comes back is compared by content again (nvim forgets a deleted
file's timestamp and stays silent; ruse warns if the returned file differs). `:w >> %` (an append to the
buffer's own file, under any spelling) records the appended file as ruse's own write, as nvim does, unless
the file had already changed under ruse before the append; an append to another buffer's file is a change
for that buffer.

### 9. Retention

On startup, at most once a day, ruse removes archives older than 14 days and keeps at most 20 archives per
document path. It never removes a live journal. These numbers resolve D-005's "Open (tuning)" retention item.

## Slices

1. Core change log and `drain_changes` with bound and overflow (no format change yet).
2. RJ01 writer and reader, state-dir location, sync policy, legacy RJS0 read path, compaction, large-file
   `DiskBase`. Replaces `Throttle`. Closes #932.
3. The recovery prompt, `InUse`, diff view, archive, retention. Closes #933.
4. Status flags and the `:checkhealth` section. Closes #938.

## Reference Invariants

- **INV-ADDITIVE:** `schema_ver` gates every change; minor versions only add fields.
- **INV-TXN / INV-ORIGIN:** every journaled change carries its revisions and origin.
- **INV-UNDO:** inverse edits are stored; the recovered buffer has a real undo path to the disk text.
- **INV-BUFFER-KIND:** only editable documents are journaled. Scratch, generated, streaming and terminal
  buffers are not.
- **INV-TRUST-1:** journals are owner-only and live outside the project tree; they are never logged and are
  excluded from diagnostic bundles.
- **INV-FAIL-BOUNDED:** the core change log is capped; a journal failure never blocks editing.
- **PERSIST-003, PERSIST-005, PERSIST-007, PERSIST-009** move from partial or todo to done as the slices land.

## Failure modes & Recovery

- **Torn tail:** replay stops at the first bad record (header CRC, payload CRC, end magic or short read),
  reports `lost_tail` and the last Marker's time, and truncates there before the next append.
- **Corruption in the middle:** the same; everything after the bad record is treated as lost.
- **Unknown major version:** `Unreplayable`; the journal is kept, not archived, so a newer ruse can read it.
- **State dir not writable:** journaling is off for the session, `[journal!]` is shown, and editing goes on.
- **Owner check wrong** (pid reuse): the start time must match too. If it cannot be read, the prompt treats
  the journal as a crash and offers recovery rather than claiming the file is in use.
- **Disk changed under a `DiskBase`:** recovery refuses to replay deltas onto other text (§5).

## Security impact

Journals hold document content. Moving them into a `0700` per-user directory stops them from being written
into shared project directories with the directory's permissions, and from being committed by accident. The
owner record stores a pid, host name and start time; it holds no user content. Replay checks every length
against the remaining file size before allocating, so a crafted journal cannot make ruse allocate without
bound.

## Performance impact

- One record per change instead of one whole-buffer snapshot every N changes: a typing session writes
  bytes proportional to what was typed, not to the file size.
- The syncs are batched, so typing never waits on an fsync.
- Replay is a single forward pass with no re-diff. The contract budget of 500 ms (p95) for the recovery
  scan gets a bench in slice 2.

## Compatibility & Migration

- RJS0 sidecar journals are read once and archived (§3). No RJS0 is written after slice 2.
- The trace format and the save path are unchanged.
- `spec/contracts/persistence-format.yaml` moves from `declared` to `implemented` in slice 2, with
  `tests/contract/journal-roundtrip` and `journal-torn-write` as real tests.

## Alternatives

1. **Keep snapshots and add a version header.** Smallest change, but a hard kill still loses up to N
   changes, large files still cost their whole size per frame, and the inverse edits D-005 promises never
   exist.
2. **Keep journals next to the file** (Vim's default `directory=.`, Emacs's `#file#`). It works on shared
   machines with care, but it pollutes the tree, fails in read-only directories and leaks content through
   directory permissions. Neovim moved its swap to the state dir for the same reasons.
3. **Sync every change.** Simplest durability story, but it puts an fsync on every keystroke. Vim and Emacs
   both batch (`updatecount`/`updatetime`, `auto-save-interval`/`auto-save-timeout`).
4. **Journal from the core directly.** Puts file I/O into `crates/core`, which the architecture keeps
   I/O-free.

## Rejected approaches

- **Silent restore (VS Code "hot exit").** It suits a GUI that owns the session, but here it would apply
  recovered text without asking, against D-005's "never auto-apply".
- **Deleting on discard.** It is the current behaviour and is how work gets lost to one wrong key.

## Trade-offs

- A per-transaction format is more code than snapshots: an encoder, a decoder and a change log in the core.
- Journals in the state dir are harder to find by hand. The prompt and `:checkhealth` show the path.
- Compaction drops the undo history before the Checkpoint, so persistent undo (a later feature) only
  reaches back to the last compaction unless it changes the compaction rule. That is a later decision.

## Re-evaluation conditions

- Persistent undo (Vim `undofile`) is designed: revisit compaction so the tree survives.
- Multi-client editing (D-012) or remote journals (D-013): the Header gains a peer identity, as a minor
  version.
- Measured sync cost or loss window disagrees with the 4000 ms / 200-change defaults.

## Open questions

None for the maintainer. The numbers (4000 ms, 200 changes, 32 MiB, 1 MiB compaction, 14 days, 20 archives)
are defaults and can be tuned without a format change. Exposing 4000 ms and 200 as the `updatetime` and
`updatecount` options is a later, additive config change (`spec/config-schema.yaml`).
