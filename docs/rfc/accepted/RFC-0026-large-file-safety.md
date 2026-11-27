---
doc: rfc
project: ruse
title: "RFC-0026: Large-file safety: bounded undo, stop-at-first-match search, a highlight budget"
summary: >
  The degraded large-file profile (RFC-0008 §7, COM-12) ships only "no highlighting past 32 MiB" and
  `[large]`. Measurements on a 100 MB file and on 10-17 MB files show where ruse actually breaks: one big
  change keeps gigabytes of undo, every search collects every match in the buffer, and tree-sitter
  highlighting of a 10-17 MB file costs seconds per keystroke. This RFC bounds undo by levels and bytes for
  every buffer, makes searches stop at the first landing and honour the interrupt, puts a time budget and a
  column cap on highlighting, and asks the Emacs large-file question for the startup file too.
audience: [maintainers, contributors, llm-agents]
status: accepted
related:
  - ../../design/persistence-and-recovery.md
  - ../../parity/common.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/PRD.yaml
  - ../../../spec/config-schema.yaml
  - ../accepted/RFC-0007-transaction-engine.md
  - ../accepted/RFC-0008-document-model.md
  - ../accepted/RFC-0023-per-transaction-journal.md
---

# RFC-0026: Large-file safety: bounded undo, stop-at-first-match search, a highlight budget

- **Status:** proposed — D-067 is open; the maintainer decides
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-28
- **Decision link:** D-067

<!-- Hard-to-reverse: dropping undo states changes INV-UNDO's "no state is ever lost" for every buffer, and
     the undo tree's identity rules (seq, root) are what g-/:earlier/:undolist and a future persistent undo
     read. The search and highlight parts are reversible; they ride along because #1290 tracks them. -->

## Summary

1. **Undo is bounded for every editable buffer, by levels and by bytes.** `'undolevels'` (default 1000, as
   nvim) caps the number of undo groups; a byte budget (default 256 MiB, ruse's own) caps the edit payload
   the tree holds. Past either, the oldest groups are dropped by **re-rooting** the tree: the oldest kept state
   becomes the root. The current group is never dropped, so the newest change is always undoable. `seq`
   numbers never change. Crash recovery is not affected: the RJ01 journal replays forward edits and never
   reads the undo tree.
2. **A search stops at its first landing.** `/`, `?`, `n`, `N`, `*`, `#`, `gn` scan from the cursor line
   outwards instead of collecting every match of the buffer first, and poll the existing interrupt checkpoint
   ([`interrupt::breakcheck`](../../../crates/core/src/interrupt.rs)) between lines.
3. **Highlighting has a budget, not only a size cutoff.** A tree-sitter parse or query that runs past
   `'redrawtime'` (2000 ms, as nvim) turns highlighting off for that buffer with a message; on a line longer
   than `'synmaxcol'` (3000, as nvim) the columns past it are not highlighted, and the query is bounded to the
   visible part of the line instead of the whole line.
4. **The Emacs profile asks the large-file question for the file given on the command line**, as Emacs does.
   The Vim profile keeps asking nothing, as nvim.

Not in this RFC: the Document text is a flat `Arc<Vec<u8>>`, so every edit copies the whole buffer (measured:
about 50-80 ms per keystroke on 100 MB). That is the largest gap to nvim on a big file, but fixing it is a
rope or piece table, which is its own design-first decision (see Re-evaluation conditions).

## Motivation / Problem

#1289 shipped the degraded profile in part (`mark_large`, `apps/tui/src/app/dispatch/open.rs`): a file over
32 MiB (`rj01::LARGE_FILE`) gets no highlighting and `[large]`. The design doc promised more
([persistence-and-recovery §3 and §8](../../design/persistence-and-recovery.md), RFC-0008 §7): "bounded undo
(a capped in-memory window)", "streaming search", "a long-line guard". #1290 tracks the rest. Before
designing, I measured what actually hurts.

### Measurements (2026-11-28, macOS arm64, 12 cores)

Method: each editor ran in a 100×30 pseudo-terminal driven by a script
(`drive.py`, in the PR's scratch directory, not committed). A step is the keys plus an unknown Ex command (in Emacs, `M-: (message ...)`)
(`:Zq<N>k`) whose echo marks that the keys before it were processed; the time is from sending the keys to
seeing that echo. A no-op step (`G` on the screen already there) measures 52-59 ms, the driver's overhead;
the values below have **55 ms subtracted** and are rounded. One run each, so differences under about 50 ms
are noise. RSS is the editor process tree's resident size after the step (`ps`). ruse is the release build of
main at 6b94f2ba; nvim is v0.12.6 `--clean`; Emacs is 31.1 `-Q` (`-nw` in the terminal, `--batch` for the
variables).

**A 100 MB file** (1.41 M lines of 80 bytes, words only, `NEEDLE` on the last line):

| step | ruse | nvim `--clean` | Emacs `-Q -nw` |
|---|---|---|---|
| open, first screen | 0.21 s, 318 MB | 0.32 s, 133 MB | asks first (below); after `y` 0.26 s, 125 MB |
| `x` / `C-d` (one edit) | 80 ms, 419 MB | ~0 | ~2 ms (from the ten below) |
| `u` / `C-/` | 230 ms, 519 MB | ~0 | not measured (the step errored at end of buffer) |
| ten `x` / `C-d` | 490 ms (≈49 ms each) | ~0 | ~20 ms |
| `/NEEDLE` / `C-s NEEDLE` from line 1 (match on the last line) | 135 ms | 120 ms | 180 ms |
| `n` / `C-s C-s` (wraps to the same match) | 135 ms | 60 ms | 180 ms |
| `gg/alpha` (a match on line 1) | 420 ms, +54 MB | ~0 | ~0 |
| `:%s/e/E/g` / `M-x replace-string` (10.1 M substitutions) | 3.0 s, **3537 MB** | 5.8 s, 875 MB | 48-93 s (two runs), 171 MB |
| `u` / `C-/` after it | 3.5 s, 2901 MB | 2.7 s, 876 MB | **not undoable**: `*Warnings*` (`undo discard-info`), the text keeps the `E`s |
| `C-r` | 2.4 s | 2.7 s | — |

Emacs facts on the same file. `-nw` asks `File big100.txt is large (100 MiB), really open? (yes, no,
literally, ?)` for the file on the command line (seen 2.3 s after start, before the file is read: RSS 25 MB).
In the session `undo-outer-limit` is 24000000, `undo-limit` 160000, `undo-strong-limit` 240000,
`long-line-threshold` 50000. In `--batch` `undo-outer-limit` reads `nil`: there the same replace (12.8 s, a
Lisp loop) kept all 20.3 M undo entries at a 1.38 GB peak.

**Long lines** (one line of 10 MB; the same words as one minified JSON array of 13.9 MB; and that array
pretty-printed, 17.3 MB in 1.3 M lines):

| step | ruse `.txt` | nvim `.txt` | ruse `.json` one line | nvim `.json` one line | ruse `.json` pretty | nvim `.json` pretty |
|---|---|---|---|---|---|---|
| open | 0.06 s, 36 MB | 0.32 s, 110 MB | **10.0 s, 986 MB** | 1.1 s, 169 MB | 3.9 s, 737 MB | 0.31 s, 48 MB |
| `$` | 70 ms | 150 ms | 180 ms | 790 ms | — | — |
| `x` | 280 ms | 125 ms | **7.2 s** | 170 ms | 4.5 s (first), 1.6 GB | ~0 |
| `u` | 225 ms | 170 ms | **6.9 s** | 870 ms | 1.7 s | ~0 |
| `/NEEDLE` (end of the line) | 85 ms | 145 ms | 440 ms | 190 ms | — | — |
| `0ihello<Esc>` | 590 ms | 210 ms | **39.9 s** | 2.05 s | — | — |
| `100\|iabcde<Esc>` | 275 ms | 100 ms | **43.5 s** | 720 ms | — | — |

What the numbers say:

- **Undo memory scales with the size of a change, not of the file.** ruse's undo nodes hold edit lists
  (`UndoNode::forward`/`inverse`, `crates/core/src/undo.rs`), not text snapshots, so ten thousand small edits
  to a 100 MB file cost kilobytes. The one case that explodes is one change with many edits: `:%s` with 10.1 M
  substitutions is one `EditList` of 10.1 M `Edit { pos, del, ins: Vec<u8> }` in each direction (40 bytes plus
  a heap block per edit), and peaks at 3.5 GB while it is built and journalled. nvim keeps the changed lines
  (875 MB). A count bound does not help here (it is one level); only a byte bound does, and only for the
  states before it. A large file makes this likelier but is not its cause, so the bound belongs to every
  buffer, not to the degraded profile.
- **Search on 100 MB is slower than nvim but not broken**, except that every search collects every match of
  the buffer first (`editor::search::search_spans` → `Regex::search_all`): `/alpha` with a match on the
  first line takes 420 ms instead of nothing, and keeps a `Vec` of every match. It also cannot be
  interrupted: the search loop has no `breakcheck`, while `:s` and `:g` do.
- **A long plain line is fine.** Display layout already skips to the visible columns
  (`ui/line_layout.rs`, `Walk::new` / `ascii_skip`); ruse is within 2-3× of nvim on a 10 MB line and needs no
  render cap.
- **Highlighting is the cliff, and it is not about 32 MiB.** A 13.9 MB one-line JSON is under the large-file
  threshold, so it is highlighted: 10 s to open, 7 s per keystroke, 1.5 GB. The pretty-printed 17 MB JSON is
  still 4.5 s for the first edit. The tree-sitter query is bounded to the visible byte range, but that range is
  whole lines (`CachedHighlight::spans`, `highlight/cache.rs`), so a one-line file queries all 13.9 MB on every
  frame. nvim's regex syntax stops at `'synmaxcol'` 3000 and gives up past `'redrawtime'`.
- **Every edit is O(file).** `Document::apply_inner` builds a new `Vec` for the whole text
  (`txn.edits.apply_to(&self.text)`), so `x` on 100 MB costs 50-80 ms and RSS holds about three copies
  (318 → 519 MB after one edit). That is a rope decision, not this RFC's.

## Guide-level explanation

- Editing as usual: nothing changes until the history passes 1000 undo steps or 256 MiB of edits. Then the
  oldest states go, quietly, as in nvim. `u` at the oldest kept state says `Already at oldest change`.
- One huge change (`:%s` over a 100 MB file) stays undoable: the newest change is never dropped. What it
  pushes out is the older history, which the user sees as `u` stopping sooner.
- `:set ul=-1` turns undo off for the buffer, as in nvim; `:set ul=10` keeps eleven steps, as nvim keeps
  `'undolevels'` + 1 (measured, below).
- `/foo` on a 1 GB file lands as soon as it finds the next `foo`; `CTRL-C` (`C-g` in the Emacs profile) stops
  a search that is still scanning, and the cursor stays where it was.
- Opening a minified 20 MB JSON shows it unhighlighted after at most about 2 s, with
  `'redrawtime' exceeded, syntax highlighting disabled`; `CTRL-L` tries again. On a long line, text past
  column 3000 is plain.
- In the Emacs profile, `ruse big.log` asks the large-file question (the one `C-x C-f` asks today) before
  reading it, as `emacs big.log` does.

## Reference-level explanation

### 1. The undo bound (core, `crates/core/src/undo.rs`)

**Limits.** `UndoHistory` gets `UndoLimits { levels: Option<u32>, bytes: Option<usize> }`, set by the
frontend from options (the core reads no config). `levels: None` is `'undolevels'` -1 (no undo at all);
`Some(n)` keeps at most `n + 1` groups, which is what nvim v0.12.6 keeps (`u_savecommon` frees headers while
`b_u_numhead > 'undolevels'` before it adds the new one; measured: with `ul=3`, five `x` leave four undoable
steps). `bytes` is the payload budget.

**Cost model.** A node costs `Σ(size_of::<Edit>() + ins.len())` over its forward and its inverse list. This
is the change log's `payload_bytes` with the real struct size in place of its 16-byte guess; the slice makes
both use the same function. The history keeps a running total, so the check is O(1) per change.

**When.** After `record` appends a node (and after `retract`), never during `u`/`C-r`/`g-`/`g+`:
navigation creates no nodes, so it cannot cross a limit.

**What is dropped: whole groups, oldest first, by re-rooting.** Repeat while over a limit:

1. Let `c` be the root's child on the path from the root to `current`. If `c`'s group is not the current
   group, the group's chain from `c` becomes history: its last node becomes the new root, the old root and
   every other subtree hanging off the old root (the branches that left the history before `c`) are freed.
   This is nvim's `u_freeheader`, which frees the oldest header and its alternate branches.
2. Otherwise (`current` is in the root's child group, e.g. right after undoing everything and making a new
   change) free the root's other child subtree whose newest `seq` is oldest.
3. Stop when nothing else can be freed: only the current group's chain remains.

The new root keeps its `seq`, its `result_revision` and its group's `uh_cursor`; it carries no edits (an
undo past it is not possible). The `mark_undo` entries of freed groups are removed.

**Identity.** `MonotonicSeq` is never renumbered, so `g-`/`g+` order, the journal's node numbers and the
`saved_node` check are unchanged for the kept nodes. `UndoNodeId` is an index into `nodes`; the slice either
compacts the vector and remaps ids (amortised: compact when half the slots are free) or stores
`Option<UndoNode>`. It is private to `undo.rs` and `document.rs`, so either is local.

**What the user sees** (nvim v0.12.6 behaviour, measured with `ul=3` and five `x`):

| command | after states were dropped |
|---|---|
| `u` past the oldest kept state | `Already at oldest change` |
| `g-`, `:earlier {N}`, `:earlier {N}m`, `:earlier {N}f` | stop at the oldest kept state (the root) |
| `:undo 0` | goes to the oldest kept state |
| `:undo {N}` for a dropped `N` | `E830: Undo number N not found` |
| `:undolist` | the kept leaves only; numbers unchanged (`5 4 …` with `seq_last` 5 after one drop) |
| `'modified'` | stays set if the saved state was dropped; no state can match it again |

`:undolist`, `:undo N` and `:earlier`/`:later` are not built yet in ruse (only `u`, `C-r`, `g-`, `g+`, `U`);
the table is the rule their slice follows. `U` (`line_undo.rs`) is separate state and is not affected.

**Crash recovery and the journal.** Unchanged. RJ01 replay (`apps/tui/src/persist/rj01.rs::replay`) applies
each Edit record's `forward` list in order, Fresh and Navigate alike; it never consults the undo tree, and
D-064 recovers a buffer as the disk text plus one change, not as a tree. A Fresh record may now name a parent
`seq` that the in-memory tree has dropped; nothing reads that today. Persistent undo (RFC-0023 Re-evaluation)
would rebuild the tree from these records and must apply the same limits, re-rooting where a parent is gone.

**Determinism.** The bound depends only on option values and edit payloads, never on a clock or on free
memory, so a `ruse --replay` trace (F-022) reproduces the same tree. That rules out Emacs's model of trimming
at garbage collection.

**Emacs profile.** Emacs bounds undo at GC by `undo-limit` (160000), `undo-strong-limit` (240000) and
`undo-outer-limit` (24000000; a single command past it loses its undo with a warning). The Emacs profile uses
the same two ruse limits instead and does not offer the three variables: their GC timing is not deterministic,
and `undo-outer-limit` throws away the newest change, which rule 3 above exists to keep. This is a documented
divergence (emacs.md). Measured: Emacs 31.1 `-nw` discarded the 10.1 M-edit replace's undo and popped up
`*Warnings*` (`undo discard-info`), so `C-/` could not take it back; ruse would keep it.

### 2. Search stops at the first landing (core, `crates/core/src/editor/search.rs`)

Today `search_fwd`/`search_bwd`, the counted walk and `gn` all call `search_spans`, which runs
`Regex::search_all` over the whole buffer and returns every match. The slice adds a step primitive:

- `next_landing(b, re, from, wrap)`: scan lines from the line holding `from` to the end, then (with
  `'wrapscan'`) from the start to that line, and return the first landing whose `landing_key` is `>= from`.
  Each line's scan is the same per-line loop as `scan_lines` (`pattern/mod.rs`), which restarts at every
  line start and carries nothing across lines but where the next line begins, so starting at any line start
  yields exactly the matches the whole-buffer scan yields from that line on. Same match set, same landings.
- `prev_landing(b, re, before, wrap)`: scan backwards in chunks of whole lines (for example 1 MiB), each chunk
  scanned forwards, keeping the last landing `< before`.
- The haystack is made per chunk (`Hay::new` validates UTF-8 and escapes stray bytes), not once for 100 MB.
- `breakcheck()` is called between lines (it polls the terminal at most every 20 ms). On an interrupt the scan
  returns `Interrupted`; the command fails without moving the cursor. `UNVERIFIED`: nvim's exact message for
  an interrupted `/` (likely `Interrupted` / `E486`-less); the slice captures it from nvim before choosing.
- A count (`3n`, `99999999/foo`) keeps `walk_count`, now stepping with `next_landing`; the cycle detection
  already caps it at one pass over the distinct landings.

Incsearch and hlsearch already search only the visible range (`highlight/search.rs`), so they change only
through the long-line part (§3). `:s` and `:g` already call `breakcheck`.

Rejected: a background search thread and a match index (see Rejected approaches).

### 3. The highlight budget and the long-line cap (frontend, `apps/tui/src/highlight/`)

- **Parse and query budget.** `CachedHighlight` parses with `Parser::parse_with_options` and a
  `ParseOptions::progress_callback`, and runs its query with a `QueryCursorOptions::progress_callback`
  (both exist in tree-sitter 0.27.2, `binding_rust/lib.rs`). Both stop at a deadline of `'redrawtime'`
  (default 2000 ms) from the start of the frame's highlight work. Past it the buffer's highlighter is
  switched off, the status line shows nvim's message (`'redrawtime' exceeded, syntax highlighting disabled`)
  and `CTRL-L` switches it back on for one more try, as nvim does for regex syntax ("When over the limit
  syntax highlighting is disabled until CTRL-L is used", `:help 'redrawtime'`). The budget also bounds
  hlsearch/incsearch span computation for a frame ("no further matches will be highlighted").
- **Column cap.** On a line longer than `'synmaxcol'` bytes (default 3000; 0 = no cap), highlight spans and
  the query range stop at that column, as nvim's syntax does ("the text after this column is not
  highlighted"). Rendering a long line itself needs no cap (measured above).
- **Bounded query range.** The visible range handed to the query is the visible byte range of the window,
  not whole lines, for a line longer than `'synmaxcol'`. This is what removes the per-frame cost of a one-line
  file; the parse itself stays incremental.
- The 32 MiB degraded profile keeps highlighting off as today; the budget covers the files under it.

Where the time goes (a throwaway probe over `CachedHighlight` on the 13.9 MB one-line JSON, release build,
deleted before commit):

| work | time | spans |
|---|---|---|
| first parse + query over the whole line | 7.6 s | 1.69 M |
| query only, whole line (tree cached) | 11.7 s | 1.69 M |
| query only, first 3000 bytes | 2.8 ms | 367 |
| one-byte edit: diff + incremental reparse + 3000-byte query | 1.9 s | 367 |

So the query range is the cliff, and bounding it removes almost all of it. What is left is about 1.9 s per edit
for the diff and the incremental reparse of one flat array node with 1.7 M children. That is under nvim's
2000 ms `'redrawtime'`, so a parity budget alone would leave this file at about 2 s per keystroke. Hence the
second rule: **an edit whose reparse takes more than 250 ms, three edits in a row, also switches the buffer's
highlighter off** with the same message (ruse's own; see the maintainer choices in D-067). The column cap
applies to tree-sitter spans, whose nodes can start before column 3000 and end after it; the slice clips
such spans at the cap.

### 4. The Emacs large-file question for the startup file (frontend)

Emacs asks in `find-file-noselect` (`abort-if-file-too-large`, `files.el`) for every visit, the command
line's included (measured above with `-nw`). ruse reads the startup file before the session exists (`app/session.rs`, `attach_file` then
`mark_large`), so the question cannot be asked. The slice stats the file first (size only, no read), and in
the Emacs profile with a size over `large-file-warning-threshold` starts the session with the buffer named
but unread and the question pending; `y` reads it, `l` reads it literally (`'binary'`), `n` answers
`Aborted` and leaves the buffer empty and unnamed (`UNVERIFIED`: what Emacs shows after `n` at startup — the
`*scratch*` buffer is the likely answer; captured before the slice). The question's text stays the one `C-x C-f` uses; the measured `-nw` rendering is
`(yes, no, literally, ?)` where ruse prints `([Y]es, [N]o, [L]iterally, [?])`, which the slice checks against
a PTY capture. The Vim and Native profiles ask nothing:
nvim has no such question, and the `[large]` profile is the warning.

### 5. Config, PRD and capabilities the slices change

| key (spec/config-schema.yaml) | type, default | Vim name | slice |
|---|---|---|---|
| `editor.undolevels` | int, 1000, min -1 | `'undolevels'`/`'ul'` (global-local) | 1 |
| `editor.undo_max_bytes` | int (bytes), 268435456, 0 = no byte bound | ruse only | 1 |
| `render.synmaxcol` | int, 3000, min 0 | `'synmaxcol'`/`'smc'` (buffer) | 3 |
| `render.redrawtime` | int (ms), 2000, min 1 | `'redrawtime'`/`'rdt'` (global) | 3 |

No key for the 32 MiB threshold: the design doc leaves "user-tunable?" open, and nothing measured asks for it.
No Emacs undo variables (§1). `emacs.large_file_warning_threshold` exists already.

PRD: F-005 gains an acceptance line for the bound (states past it are dropped by re-rooting, the newest change
is kept); F-009 gains "a search stops at its first landing and is interruptible"; F-015 gains the
`'redrawtime'`/`'synmaxcol'` budget; F-008's large-file comment drops "bounded undo / streaming search still to
come". `spec/capabilities.yaml` does not change: the work sits in CAP-EDIT-CORE and CAP-SYNTAX.
persistence-and-recovery §3/§8: the large-file row's "capped ring" becomes "the same undo bound as every
buffer"; common.md COM-12 says the same.

## Reference Invariants

- **INV-UNDO** is amended: every transaction on an editable Document is undoable **while it is within the
  undo bound**; the newest change always is. "No state is ever lost" (undo.rs module docs, COM-8) becomes "no
  state within the bound is lost; branching loses nothing". The tree stays consistent: one root, parents
  exist, `seq` increases with creation (the `assert_invariants` walker keeps every check; "the root is node
  0" becomes "the root is the one node without a parent").
- **INV-BUFFER-KIND**: the large-file/degraded clause no longer has its own undo window; it keeps the
  incremental journal and highlighting off.
- **INV-TXN**: unchanged; dropping undo states is not a transaction and does not move the revision.
- **INV-FAIL-BOUNDED**: the reason for the byte bound; the change log already follows it.
- **INV-NO-GLOBAL-STATE**: the limits are per history, set by the frontend; the interrupt latch stays per
  thread.

## Failure modes & Recovery

- A dropped state cannot be reached by undo; the file on disk and the journal are unaffected. A user who
  needs the old text has the disk file (if saved) or the journal's recovery.
- A single change bigger than RAM is still an out-of-memory risk while it is built; the bound limits what is
  *kept*, not the transient peak. The 3.5 GB peak for 10 M edits is mostly the transient (the edit lists, their
  clone for the change log, the anchor and line-map passes).
- An interrupted search leaves the cursor and the last-search register as before the search; the pattern
  still goes into the search history.
- A highlighter switched off by the budget stays off for that buffer until `CTRL-L`; no other buffer is
  affected.

## Security impact

None new. The byte bound narrows a memory-exhaustion path (a hostile file plus a `:%s`); the interrupt makes a
slow search stoppable.

## Performance impact

- Undo: O(1) bookkeeping per change, O(freed nodes) when dropping, amortised compaction.
- Search: proportional to the distance to the next landing instead of the buffer size. On the 100 MB file
  `gg/alpha` goes from 420 ms to a single-line scan. A search with no match still scans everything, now
  interruptible.
- Highlight: a hard ceiling of `'redrawtime'` per frame on parse plus query.
- Every slice adds a bench row (`crates/core/benches/` for search; `apps/tui/benches/highlight_parse.rs` gains
  a one-line-JSON case) so the numbers above are re-measured in CI's bench job rather than by hand.
- Memory: the bounded query keeps a few hundred spans instead of 1.69 M. How the 986 MB RSS of the 13.9 MB
  JSON splits between the tree, the spans and the copies of the text is not measured.

## Compatibility & Migration

No persistent format changes: RJ01 is untouched (schema 1.1 stays), no trace format change. Behaviour change:
undo history is no longer unbounded (today it is); 1000 levels is nvim's default, so Vim users see what they
expect. The config keys are new.

## Observability

`:set ul?`, `:set smc?`, `:set rdt?` show the values. When the byte budget drops states, the log (`RUSE_LOG`)
records the freed groups and bytes at debug level; the user sees no message, as in nvim. The highlight budget
shows nvim's message.

## Alternatives

- **Levels only** (nvim's model). Does nothing for the measured problem, which is one level of 1 GB.
- **Bytes only** (Emacs's model). Loses Vim's observable `'undolevels'`, which scripts and users set.
- **A smaller window only in the degraded profile** (the design doc's wording). Measured: undo size follows
  the change, not the file; a 2 MB file with repeated `:%s` grows the same way.
- **Drop the oldest node rather than the oldest group.** Splits an insert session, so `u` would stop halfway
  through a logical change (INV-UNDO grouping).
- **Lower the large-file threshold** (say to 8 MiB) instead of a budget. A 9 MB minified JSON would still
  stall; a 40 MB log with no grammar would lose nothing it has. The budget measures the real cost.

## Rejected approaches

- **Discard a change bigger than the budget** (Emacs `undo-outer-limit`). The memory was already spent to
  build the change; throwing away the only way back from a mistaken `:%s` is the worst trade.
- **Trim at memory pressure or on a timer.** Not deterministic; breaks `--replay` (F-022).
- **A background search thread or a prebuilt match index.** The editor runs commands on one thread and the
  interrupt checkpoint already exists; an index must be rebuilt on every edit of a 100 MB buffer.
- **A render cap for long lines.** Measured unnecessary: layout already skips to the visible columns.
- **so-long-mode-style automatic mode switching.** Emacs ships it off (`global-so-long-mode` not enabled under
  `-Q`); the highlight budget covers its case without changing modes.

## Trade-offs

- Users of very long undo histories (ruse is unbounded today) lose states past 1000 steps. It is nvim's
  default and `:set ul=100000` restores it.
- The 256 MiB default is a judgement: it keeps about one full rewrite of a 100 MB file (forward plus inverse)
  and hundreds of rewrites of a normal file. nvim has no byte bound; Emacs keeps far less (240 kB at GC).
- Re-rooting frees sibling branches together with the oldest step, as nvim does; a `g-` user loses old
  branches a little earlier than strictly necessary.

## Re-evaluation conditions

- **Rope / piece table:** reopen when a large-file workload is a stated goal, or when the per-edit copy on a
  file the user edits passes a frame budget (measured here: 50-80 ms at 100 MB, so about 16 ms at 20 MB).
  It needs its own RFC (D-003/RFC-0008 boundary); `spec/dependencies.yaml` already reserves DEP-ROPE for it. Until then the degraded profile is the answer.
- **Compact edit lists:** if a `:%s` over a large file is a common workflow, store an `EditList` as one byte
  arena plus `(pos, del, len)` triples (about 24 bytes per edit instead of 40 plus a heap block).
- Persistent undo (RFC-0023): its rebuild must apply these limits.
- Users ask to tune the 32 MiB threshold, or the budget fires on normal files.

## Open questions

- The default byte budget (256 MiB recommended; 0 = off is the alternative).
- `'undolevels'` 0 (Vi-compatible: `u` undoes itself) — implement in slice 1 or leave as "not supported"?
  nvim v0.12.6 measured: with `ul=0`, `x` then `u` then `u` toggles. Recommendation: implement it, it is a
  small rule on top of keeping one group.
- Whether the Emacs profile should offer `undo-outer-limit` at all (recommendation: no, §1).
- The ruse-only reparse rule (three edits in a row over 250 ms switch highlighting off). Without it the
  one-line 13.9 MB JSON stays at about 1.9 s per keystroke after the query bound. Recommendation: adopt it,
  with fixed numbers and no config key until someone asks.
- Unmeasured: ruse at 1 GB; Linux; how ruse's RSS of about 3× the file splits (the text, its snapshot copy,
  the line index).

## Slice plan

- **slice 1 (#1290)**: core undo bound (`UndoLimits`, re-rooting, cost model shared with the change log,
  `assert_invariants` update, property test: random edits/undos under random limits keep the tree valid and
  the text equal to a replay of the kept history), `editor.undolevels` / `editor.undo_max_bytes`, `:set ul`;
  nvim oracle fixtures for `u` past the bound and `ul=-1`. PRD F-005, config-schema, persistence §3/§8,
  INV-UNDO wording.
- **slice 2 (#1290)**: stop-at-first-landing search with `breakcheck`, and the nvim interrupt message
  captured first; bench row on a large buffer. PRD F-009.
- **slice 3 (#1290)**: the highlight budget (`render.redrawtime`) and the column cap (`render.synmaxcol`),
  bench row for the one-line JSON; measure parse versus query first. PRD F-015.
- **slice 4 (#1290)**: the Emacs large-file question for the startup file; PTY test.
- Separately, if the maintainer agrees: an issue for the rope decision with the numbers above.
