---
doc: rfc
project: ruse
title: "RFC-0018: Multi-buffer arena — workspace-global state + buffer lifecycle"
summary: >
  The buffer arena already exists (F-007 `Workspace`), but the state Vim/Emacs treat as editor-GLOBAL
  (registers, uppercase/global marks, the cross-buffer jumplist, the alternate file) still lives per-View
  and is cloned on `:split`. This RFC promotes that state to the Workspace and adds buffer lifecycle
  (dispositions + kinds), which is what actually unblocks `mA`-`mZ`/global marks, buffer/file pickers, an
  editable `q:`, the `"#` register, and the "other buffers" completion source.
audience: [maintainers, contributors, llm-agents]
status: accepted
related:
  - ../../../spec/PRD.yaml
  - ../../../spec/DECISIONS.md
  - ../../../spec/capabilities.yaml
  - ../../design/buffer-arena-and-global-state.md
  - ../../design/view-window-workspace.md
  - ../../design/positions-history.md
  - ../../design/register-model.md
  - RFC-0002-workspace-architecture.md
  - RFC-0008-document-model.md
---

# RFC-0018: Multi-buffer arena — workspace-global state + buffer lifecycle

- **Status:** accepted (2026-11-03; slices 2 and part of 3 implemented, see [Acceptance](#acceptance-2026-11-03))
- **Author(s):** Claude Opus 4.8 (design agent), for maintainer review
- **Created:** 2026-08-24
- **Accepted:** 2026-11-03 — the maintainer decided to follow this design as written (#508)
- **Decision link:** D-057

<!-- Hard-to-reverse: this moves ownership of core editor state (registers, marks, jumplist) out of the
     per-View EditorState into the Workspace, changing the core data model and the swap-trick pipeline's
     inputs. That is an architecture-kind change (spec/change-kinds.yaml). -->

## Acceptance (2026-11-03)

The maintainer accepted this RFC as written on 2026-11-03 (#508): the Workspace owns editor-global state, cross-buffer
positions are anchors, and buffers gain a disposition and a kind. The body below is the design as proposed on
2026-08-24; its `file:line` citations describe the code of that date. This section records what has shipped since,
what remains, the answers to the open questions, and one rule the proposal left out. D-057 carries the same record.

### Shipped

| Slice | What landed | PR |
|---|---|---|
| 2 (registers) | The `Workspace` owns one `RegisterStore`. The swap-trick lends it into the swapped-out `View` (`Workspace::lend`/`reclaim`) at every `from_parts`/`into_parts` site; a View at rest holds an empty store, and `:split` no longer clones registers. Narrower than R1: `from_parts`/`into_parts` keep their signatures. `"#` reads the alternate buffer's file name. | #1114 (closed #944, #1022) |
| 1 (part: lifecycle) | `:close` on the last window of a buffer retires only the View; the buffer stays loaded, hidden and listed, and `:q` refuses with E162 while a hidden buffer is modified. There is no `Disposition` enum yet: "hidden" is a buffer with no View. | #1044 |
| 3 (part: uppercase marks) | `mA`-`mZ` are settable and usable, including `'A` in Ex ranges, but each stays with the View of its own buffer. A jump to an uppercase mark set in another buffer is not wired. | #1103 |
| 3 (part: anchoring) | Lowercase and uppercase marks, the special marks, the jumplist and the change list follow edits by nvim's `mark_adjust` rule (`Track::Line` in `crates/core/src/anchor.rs`). The focused View hands its positions to the Document for every transaction, undo and redo; `Workspace::lend` pins the other Views of the same Document, so an edit in one split carries the other split's marks. These positions are carried through the transaction, not stored in the `AnchorStore`. | #1124 |

### Remaining

- **Cross-buffer global marks (F-032):** `` `A `` from another buffer focuses the mark's buffer and places the
  cursor; a Workspace mark table of `(DocumentId, Anchor)` entries replaces the per-View `A`-`Z` slots.
- **Lowercase marks per buffer, not per View** (#636): today two Views of one buffer keep separate `a`-`z` marks.
- **Jumplist entries `(DocumentId, Anchor)`** so `CTRL-O`/`CTRL-I` cross buffers (OQ-4).
- **`Disposition` and `BufferKind`** as explicit per-buffer state, with `:ls!` and `'hidden'` (`editor.hidden`).
- **The retirement Effect** (R2): retirement returns or emits the retired `DocumentId`s so the frontend prunes its
  per-buffer maps.
- **Editable `q:`/`q/`/`q?`** on a `Scratch` buffer (slice 4). Today `apps/tui/src/input/cmdwin.rs` is a read-only overlay.
- **Other-buffers completion:** `Workspace::keyword_completion` still reads only the focused buffer.

### Open questions, answered

1. **Lowercase marks are anchors.** Done in #1124 with line-gravity tracking (`Drop` for `a`-`z`, so a deleted
   line's mark gives E20, as nvim). Moving them from the View to the buffer is #636.
2. **Registers are workspace-global.** Done in #1114: one store per workspace, which is Vim's model. The Emacs profile
   may layer buffer scoping as policy over the same store (D-026).
3. **`q:` reuses the driven executor.** `<CR>` in the editable window runs its line through the same path as
   `:normal` and `:g … normal`: the session key path (`Session::nested_key`/`normal_pass`, #1096). No second
   re-entry model. Recursion guard: opening the command-line window from inside it is refused with nvim's
   `E1292: Command-line window is already open`, and nested `:normal` keeps its `MAX_NORMAL_DEPTH` limit (E192).
4. **Jumplist: per View, entries `(DocumentId, Anchor)`.** Each window keeps its own jumplist and an entry may name
   another buffer. Vim and Helix both use this model. A single workspace jumplist is rejected.
5. **Generational handles are a separate issue:** #973 (generation-checked `DocumentId`/`ViewId`/`WindowId`). This
   RFC keeps the `None`-hole model and does not depend on it.
6. **Buffer picker in scope, file picker out.** The buffer picker is this RFC's consumer (today an overlay,
   `apps/tui/src/ui/buffer_picker.rs`; later a `Scratch`/`Unlisted` view). The file picker (CAP-FILE-PICKER, today
   the overlay `apps/tui/src/ui/file_picker.rs`) is a later consumer of CAP-FS/CAP-SEARCH and is not part of this RFC.

### Added rule: retirement degrades cross-buffer positions instead of dropping them

The proposal said a retired buffer's global marks and jumplist entries are pruned. That loses `` `A `` and
`CTRL-O` targets whenever a buffer is wiped, which Vim does not do. The accepted rule:

- When a **named** buffer (one with a file path) is retired, each global mark and cross-buffer jumplist entry
  that points into it degrades from `(DocumentId, Anchor)` to `(path, line, col)`: the anchor is resolved once,
  at retirement, and stored with the file name. This is Vim's file mark (`fmark` with `fname`).
- When a buffer is opened for that path again, every degraded entry for the path re-attaches: `(line, col)` is
  clamped into the new text and becomes an anchor in the new `Document`.
- Jumping to a degraded entry opens the file (as `` `A `` does in Vim for a mark in an unloaded file), then
  re-attaches as above.
- An **unnamed** buffer has no path to degrade to, so its entries are dropped at retirement.

Paths are matched the way `:e` finds an open buffer (`buffer_for_path` in `apps/tui/src/app/dispatch.rs`, which
compares canonicalised paths); the path lookup is the frontend's job, since the core does no IO (D-039). The degraded form is
in-memory only; persisting it is ShaDa's job (see Re-evaluation conditions).

## Summary

ruse already has a real multi-buffer arena: `crates/core/src/workspace.rs`'s `Workspace` owns many
`Document`s and `View`s in arenas keyed by `DocumentId`/`ViewId`, with a listed `buffer_order`, an
alternate `alt` buffer, and working `:e`/`:enew`/`:ls`/`:b`/`:bnext`/`:bprevious`/`:bdelete`/`:split`.
What is missing is the **cross-buffer layer** the still-blocked features consume. Two things:

1. **Editor-global state lives per-View.** The register store, the named marks, and the jumplist are
   fields on `View` (`crates/core/src/editor/mod.rs:167,233,255`). A `:split` *clones* the `View`
   (`workspace.rs:802-816`), so registers and marks copied at split time then diverge — the doc comment
   there admits this is a deferred Vim-parity gap. Only the focused `(Document, View)` pair is ever live
   (the swap-trick, `workspace.rs:275-314`), so there is no path for state that must be shared across all
   buffers or that must point *into another buffer*.

2. **Buffers have no lifecycle model.** A buffer is either shown in a window or an implicit "loaded but
   not shown" hole; there is no `Disposition` (Listed/Unlisted/Hidden) and no buffer **kind** (ordinary /
   scratch / nofile / interactive), so features that need a *special* buffer (the editable `q:` window, a
   picker results buffer) have nowhere to live, and Vim's `'hidden'` semantics cannot be expressed.

This RFC decides that **editor-global state is owned by the `Workspace`, not the per-View `EditorState`**,
and lent into the transient `EditorState` per command by the swap-trick; and that the buffer registry
gains a `Disposition` and a `BufferKind`. This is the concrete unblock for uppercase/global marks
(`mA`-`mZ`), buffer/file pickers, the editable command-line window (`q:`/`q/`/`q?`), the `"#` register,
and the "other buffers" insert-completion source. The deeper field-level design is in
[buffer-arena-and-global-state.md](../../design/buffer-arena-and-global-state.md); the pre-existing
Workspace/Session, positions-history, and register designs
([view-window-workspace.md](../../design/view-window-workspace.md),
[positions-history.md](../../design/positions-history.md),
[register-model.md](../../design/register-model.md)) already specify the *shapes* — this RFC decides the
**ownership migration and its slicing**, not new data structures from scratch.

## Motivation / Problem

The original blocker framing ("the editor is single-document with split windows over one buffer") is
**out of date**: F-007 landed the arena and the TUI drives a `Workspace`, not a single `EditorState`
(`apps/tui/src/app/session.rs:209`). This RFC corrects that and targets what remains.

Grounded inventory of what exists vs. what blocks each feature:

| Blocked feature | State today | Actual blocker |
|---|---|---|
| `:ls` / `:bnext` / `:bprev` | **works** (`workspace.rs:986,1008`) | none — remove from the list |
| Uppercase/global marks `mA`-`mZ` | `named_marks: [Option<usize>; 26]`, a-z only, raw offsets (`editor/mod.rs:233`); set/get guarded to lowercase (`:323-336`) | no cross-buffer mark store keyed by `(DocumentId, Anchor)`; marks are per-View |
| `"#` alternate-file register | `alt: Option<DocumentId>` exists (`workspace.rs:111`); `"%` (current file) register is wired, `"#` is not | register store is per-View and holds text, not a buffer/file reference; no bridge from `alt` |
| Buffer / file picker | `buffers()` returns rows (`workspace.rs:986`); a read-only cmdline overlay exists | no picker over the buffer set as a first-class special view; no cross-buffer read for a file picker preview |
| Editable command-line window `q:` | a **read-only** history list-overlay (`apps/tui/src/input/cmdwin.rs:1-3`) | Vim's `q:` is a real *editable scratch buffer* seeded with history whose line executes on `<CR>`; there is no scratch **BufferKind** and no write-back-then-execute path |
| "Other buffers" completion | `keyword_completion()` reads `self.focused()` only (`workspace.rs:1179-1187`); comment says other sources deferred (`session.rs:1249`) | no cross-buffer read API; the swap-trick only reconstitutes the focused pair |

The shared root cause is that **state Vim/Emacs treat as global-to-the-editor is trapped inside one
View**, and the swap-trick gives cross-buffer code nowhere to stand. Fixing this once, in the arena,
unblocks all five.

## Guide-level explanation

From a user's view nothing about single-buffer editing changes. What becomes possible:

- **Registers are global.** Yank in buffer A, `:b B`, paste — you get the text (today a split/other buffer
  can hold a stale copy). Macros in `a` are visible everywhere. This is Vim's model.
- **`mA` sets a global mark**; from any buffer `` `A `` jumps to that file+position (opening/showing the
  buffer if needed). `:marks` shows lower-case (buffer-local) and upper-case (global) marks together.
- **`` `` ``/jumplist crosses files.** `CTRL-O` after jumping into another file returns you to the prior
  file, not just the prior line in the current one.
- **`<C-^>` / `:b#` / `"#`** all agree on one alternate buffer; `"#` in insert/`:` splices the alternate
  file name.
- **Buffer picker & file picker** open as special views listing the arena's buffers (and, for files,
  workspace paths), with a live preview; choosing one runs `focus_buffer`.
- **`q:` / `q/` / `q?`** open a real, editable buffer of your command/search history; edit any line, press
  `<CR>` on it to run it.
- **`i_CTRL-N`/`i_CTRL-P`** offer keywords from every loaded buffer, not just the current one.

For a plugin/frontend author, the contract is: *the frontend drives a `Workspace`; editor-global state is
read/written through `Workspace` methods, never by reaching into a `View`.* The swap-trick stays the only
way a `Command` mutates a buffer.

## Reference-level explanation

### R1. Ownership: global state moves to `Workspace`

Introduce a `Workspace`-owned aggregate (illustrative name `GlobalStore`; concrete type lives in code per
D-038) that holds the state that is editor-global in Vim/Emacs:

- the **register store** (`RegisterStore`, moved off `View`);
- the **global mark table** — `mA`-`mZ` and special global marks — as `(DocumentId, Anchor)` entries
  (the `NamedMap` container of C-POSHIST / D-027);
- the **cross-buffer jumplist / changelist tail** — entries become `(DocumentId, Anchor)` (the `Ring`/
  `CursoredList` containers of D-027) so a jump that changed buffers returns to the right buffer;
- the **alternate buffer** `alt` (already on `Workspace`) and the `"#`/`"%` bridge that renders it as a
  file-name register.

**Buffer-local** state stays per-View / per-Document: cursor, mode, selection/anchor, viewport `top`,
`curswant`, the lowercase marks `a`-`z`, the Emacs `mark`, `last_visual`, replace/block-insert sessions,
and per-view config snapshots. The split between "global" and "buffer-local" follows the Vim/Emacs
semantics table in [buffer-arena-and-global-state.md](../../design/buffer-arena-and-global-state.md) §Ownership.

The **swap-trick is extended to lend the global store in**: `Workspace::apply` (and the ~20 sibling ops)
build the transient `EditorState` from `(Document, View, &mut GlobalStore)` instead of `(Document, View)`,
run the unchanged planner/commit pipeline, and return it. The planner/commit already treat registers/marks
as fields reachable through `EditorState`; the change is *where they are borrowed from*, not their API.
Because ruse is single-threaded and one command runs at a time (D-002), the mutable borrow of the one
global store for the duration of a command is sound — the same argument that makes the existing swap-trick
sound (`workspace.rs:15-18`).

Anchors (D-023/INV-ANCHOR) are required for the global mark/jumplist entries: an offset into *another*
buffer must survive edits made to that buffer while it is not focused, so global positions are stored as
anchors in each `Document`'s `AnchorStore`, referenced from the `Workspace` store by `(DocumentId,
AnchorId)`. Lowercase marks may migrate to anchors in the same slice or stay raw-offset (they already snap
on commit); the RFC requires anchors only where a position crosses the buffer boundary.

### R2. Buffer lifecycle: `Disposition` + `BufferKind`

Add to the buffer registry (per buffer, parallel to `docs`/`names`):

- `Disposition ∈ { Listed, Unlisted, Hidden }` — exactly the enum already specified in
  [view-window-workspace.md §8.1](../../design/view-window-workspace.md). `Listed` appears in `:ls`, pickers,
  and `:bnext`; `Unlisted` is loaded but hidden from the normal list (help/preview/picker-results);
  `Hidden` is Vim's `'hidden'` — a buffer that survives losing its last window without being wiped.
- `BufferKind ∈ { Ordinary, Scratch, NoFile, Interactive }` (aligned with INV-BUFFER-KIND and the
  view-window-workspace kinds). `Scratch`/`NoFile` back the `q:` window and picker-results buffers;
  `Interactive` is the terminal/PTY buffer kind (F-011, already special-cased frontend-side).

`remove_buffer`/`close_focused` become disposition-aware: a `Hidden` or modified buffer is retained
(subject to the `'hidden'`/`E89` guard) rather than always wiped. The frontend-side per-`DocumentId` maps
(`Files`, highlighters, folds, terminals — `apps/tui/src/app/dispatch.rs:65`, `session.rs`) must be
retired in lockstep with a buffer leaving the arena; the RFC makes buffer retirement emit an `Effect`
(or return the retired id set) so the frontend can prune those maps deterministically (today
`close_focused`/`remove_buffer` retire the core buffer but the frontend must scan).

### R3. The unblocked features, concretely

- **`mA`-`mZ` / global marks:** `set_global_mark(name, DocumentId, Anchor)` and
  `goto_global_mark(name) -> Option<(DocumentId, offset)>` on `Workspace`; the frontend `focus_buffer`s the
  target then places the cursor. `named_marks`'s lowercase guard (`editor/mod.rs:323`) is complemented by a
  Workspace-level uppercase table.
- **`"#` register:** a read-only special register resolved (like `"%`, already wired via
  `set_special_registers`, `workspace.rs:336`) from `alt` + the frontend's `Files` name.
- **Buffer/file picker:** a special view (`BufferKind::Scratch`, `Disposition::Unlisted`) whose rows come
  from `buffers()` (buffer picker) or a cross-buffer/workspace read (file picker); selection runs
  `focus_buffer`. The overlay/keymap-layer mechanics are already designed in
  [view-window-workspace.md §7](../../design/view-window-workspace.md).
- **Editable `q:`:** open a `Scratch`/`NoFile` buffer seeded with history; on `<CR>` the current line is
  extracted and executed through the existing Ex/normal dispatch — the same driven-command re-entry model
  `:g`/`:normal` already use. Replaces the read-only overlay in `cmdwin.rs`.
- **"Other buffers" completion:** `keyword_completion` iterates `buffer_order` (Listed buffers) reading
  each `Document`'s bytes, instead of only `focused()`.

### R4. What does NOT change

INV-DOC-VIEW is preserved — no view-local state moves onto a `Document`, and a `Document` still never
references a `View`. The two-crate boundary (D-039) is unchanged: all new state is in `ruse-core`; IO
(paths, fs, clipboard) stays in `apps/tui`. The plan/commit/transaction pipeline (D-001/INV-TXN) and the
undo model are untouched. This is a re-home of ownership plus two enums, not a rewrite.

## Reference Invariants

- **INV-DOC-VIEW** (depends) — the migration must not move view-local state onto the Document; global state
  goes to the `Workspace`, a third owner, never to `Document`.
- **INV-HANDLE** (depends) — buffers/views are referenced by `DocumentId`/`ViewId`; global marks/jumps
  reference `(DocumentId, AnchorId)`, never raw cross-buffer offsets. (Generational handles remain a
  post-MVP hardening, unchanged by this RFC.)
- **INV-ANCHOR** (depends) — any position that crosses the buffer boundary (global marks, cross-buffer
  jumplist) is an anchor in the target `Document`'s store, so it survives edits made while that buffer is
  unfocused.
- **INV-NO-GLOBAL-STATE** (depends, and the subtle one) — the global store is **owned by the `Workspace`
  component and reached by handle/method**, not a process-wide `Arc<Mutex<_>>`. "Editor-global" here means
  *workspace-scoped*, which is exactly what this invariant permits (one component owns it); it does NOT
  introduce ambient global mutable state.
- **INV-BUFFER-KIND** (depends/extends) — `BufferKind` selects the mutation contract; `Scratch`/`NoFile`/
  `Interactive` buffers keep their existing exemptions.
- **INV-TXN / INV-UNDO** (depends) — unchanged; buffer mutations still go through transactions.

No new INV-* is introduced; the RFC re-homes state within the existing invariant set.

## Failure modes & Recovery

- **Stale cross-buffer reference** (a global mark into a wiped buffer): amended at acceptance (see
  [the added rule](#added-rule-retirement-degrades-cross-buffer-positions-instead-of-dropping-them)). Anchors in a
  retired `Document` go with it, but the Workspace store first degrades each entry into a named buffer to
  `(path, line, col)` and re-attaches it when the path is opened again; only an unnamed buffer's entries are
  dropped. An entry that still cannot resolve is a no-op/bell (Vim behaviour), never a panic.
- **Retirement / map desync:** the failure today is silent leakage of frontend maps; R2 makes retirement
  emit the retired ids so the frontend prunes deterministically. A dangling `Files` entry becomes an
  assertable inconsistency in debug builds.
- **`q:` executing a malformed line:** goes through the normal Ex error path (typed error → status line),
  identical to typing the line at `:`.

## Security impact

None new. `q:` executes commands, but only lines the user themselves edited and confirmed with `<CR>` —
identical trust to the `:` command line. No new IO surface in core (INV per D-039). Cross-buffer reads for
completion/pickers stay within already-loaded buffers.

## Performance impact

- Register/mark access moves from a `View` field to a `Workspace` field reached through the swap-trick —
  O(1), no measurable change; one extra mutable borrow threaded per command.
- Global mark/jumplist entries are anchors: anchor update cost is already bounded (INV-ANCHOR), and the
  global sets are small (bounded like `MAX_CHANGES = 100`).
- "Other buffers" completion is O(total bytes of listed buffers) per completion *start*; bounded by
  reading Listed buffers only and cache-able. Not on the per-keystroke path.
- No change to the buffer text store (`Arc<Vec<u8>>`, D-042) or the parse/render hot paths.

## Compatibility & Migration

No persistent format changes (session save/restore is out of scope). The change is internal:

- `View` loses `registers`/`pending_register` and the global-mark/global-jump portions; `Workspace` gains
  the global store. `EditorState::from_parts`/`into_parts` gain the global-store borrow. These are
  `pub(crate)` / crate-internal seams (`editor/mod.rs:686`), used only by the Workspace swap-trick and the
  parity oracle — the public `apply_command`/`cursor()` surface is unchanged.
- The parity oracle drives `EditorState` directly (`apps/tui/tests/parity_compare.rs`); it must be given a
  global store to borrow. Slice 2 provides a test constructor so the oracle keeps measuring true Vim
  behaviour (registers-global is *more* faithful, not less).
- `:ls`/`:bnext`/`:bprev`/`:bd` behaviour is unchanged except that `Hidden`/modified buffers survive
  `:bd`/last-window-close where Vim's `'hidden'` says they should.

Migration is staged (see Slicing); each slice is independently shippable and leaves the tree green.

## Observability

Buffer retirement emits its retired-id set (R2) — the hook a frontend/health check uses to verify no
`DocumentId`-keyed map leaks. `:ls`/`:marks`/`:registers`/`:jumps` already snapshot through the swap-trick
and continue to; after migration `:registers`/`:marks` (uppercase) reflect the global store. No new tracing
categories required; existing `tracing` (D-040) covers the frontend dispatch.

## Alternatives

1. **Leave state per-View, sync on demand.** Copy registers/marks between Views when focus changes.
   Rejected: this is what the split-clone does and it is the bug — divergence is unavoidable, and it gives
   cross-buffer marks nowhere to point.
2. **Put global state on a `Document`.** Rejected: violates INV-DOC-VIEW (a Document would know about
   editor-global concerns) and makes "same buffer in two views" ambiguous about which copy is authoritative.
3. **A process-global singleton (`static`/`Arc<Mutex>`).** Rejected: violates INV-NO-GLOBAL-STATE and the
   deterministic single-executor model (D-002); the `Workspace` is already the correct single owner.
4. **Do the whole thing in one PR.** Rejected: it is a ~20-call-site swap-trick change plus an oracle
   refactor; the slicing below keeps each step reviewable and green.
5. **Model `q:` as engine overlay state forever (keep `cmdwin.rs`).** Rejected for the *editable* variant:
   Vim's `q:` is a real editable buffer, and re-implementing buffer editing inside the overlay duplicates
   the arena. The read-only overlay may remain as a fallback until the scratch-buffer slice lands.

## Rejected approaches

See Alternatives 2 and 3 — putting global state on the Document, or in a process-global singleton — are the
two tempting shapes that violate INV-DOC-VIEW / INV-NO-GLOBAL-STATE respectively and must not be
re-litigated. The `Workspace` is the single legitimate owner of editor-global state.

## Trade-offs

- **Threading a `&mut GlobalStore` through every swap-trick call site** is boilerplate (~20 methods). The
  trade is accepting that churn once vs. permanently blocking five features and shipping a known
  register-divergence bug. The churn is mechanical and covered by the existing per-op tests.
- **Anchors for cross-buffer positions** cost more than raw offsets, but raw offsets into an unfocused,
  editable buffer are unsound — anchors are the only correct representation (INV-ANCHOR).

## Re-evaluation conditions

- If a **second client** (D-012) is enabled, "workspace-global" registers/marks may need a client-scope
  split (some registers are Vim-global, some are arguably per-client). This RFC scopes them to the single
  workspace/client; multi-client re-opens the scope question.
- If **session persistence** (mksession/shada) lands, the global store gains a serialized on-disk form —
  that promotes the register/mark byte encoding to a persistent-format *contract* decision (as D-055 notes
  for macros), superseding the "internal only" stance here.
- If profiling shows the per-command global-store borrow is a bottleneck (not expected), the borrow shape
  is revisited.

## Open questions

Answered at acceptance; see [Open questions, answered](#open-questions-answered). The questions as
proposed:

1. **Lowercase marks: migrate to anchors now or later?** They currently snap on commit and work; anchors
   are strictly required only for cross-buffer marks. Proposal: keep lowercase raw-offset+snap in slice 3,
   migrate opportunistically. Maintainer call.
2. **Which registers are truly global vs. buffer-local?** Vim: all registers global. Emacs kill-ring is
   global; some registers are frame/buffer-scoped. Proposal: follow D-026 (one shared store) — global —
   and let the Emacs profile layer any buffer-scoping as policy. Confirm.
3. **`q:` execution re-entry:** reuse the `:normal`/`:g` driven-command executor, or a dedicated path?
   Proposal: reuse it (one re-entry model). Confirm no unwanted recursion (a `q:` line containing `q:`).
4. **Global jumplist model:** Vim's jumplist is per-window but entries carry a file; is a per-View
   jumplist-of-`(DocumentId,Anchor)` faithful enough, or is a single workspace jumplist wanted? Proposal:
   per-View list whose entries are `(DocumentId, Anchor)` (matches Vim: each window has its own jumplist,
   entries can be in other files). Confirm against the nvim oracle.
5. **Generational handles (INV-HANDLE):** should this RFC pull generational `DocumentId`/`ViewId` forward
   (retired-slot reuse becomes safe), or keep the `None`-hole + assert model? Proposal: out of scope; track
   separately. Confirm.
6. **Scope of the file picker's cross-buffer/workspace read** — loaded buffers only, or a workspace file
   scan (CAP-FS/CAP-SEARCH)? Proposal: buffer picker in this RFC's scope; file picker reuses CAP-FS/SEARCH
   and is a thin consumer. Confirm the boundary.
