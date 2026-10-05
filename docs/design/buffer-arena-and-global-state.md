---
doc: buffer-arena-and-global-state
project: ruse
title: "Buffer arena — workspace-global state, dispositions, and kinds"
summary: >
  The field-level design behind RFC-0018/D-057: how editor-global state (registers, uppercase/global
  marks, the cross-buffer jumplist, the alternate file) moves from per-View `EditorState` to a
  Workspace-owned store lent in by the swap-trick, and how buffer `Disposition` + `BufferKind` are added.
  Specifies the contract (ownership split, algorithms, edge cases), not the concrete Rust types (D-038).
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../rfc/proposed/RFC-0018-multi-buffer-arena.md
  - view-window-workspace.md
  - positions-history.md
  - register-model.md
  - persistence-and-recovery.md
  - ../../spec/DECISIONS.md
---

# Buffer arena — workspace-global state, dispositions, and kinds

<!-- code-blocks: illustrative — concrete types live in crates/core/src/{workspace,editor,register}.rs
     and are the source of truth (D-038). Blocks here specify the CONTRACT, not the definition. -->

## Problem

The multi-buffer arena exists (`crates/core/src/workspace.rs`, F-007). What does not exist is the layer
that lets state be **shared across buffers** or **point into another buffer**. Concretely, three fields
that Vim/Emacs treat as editor-global are stored on `View` and cloned on `:split`:

- `registers: RegisterStore` — `crates/core/src/editor/mod.rs:167`
- `named_marks: [Option<usize>; 26]` (a-z only, raw offsets) — `:233`
- `jumps: Vec<usize>` / `jump_idx` (single-buffer offsets) — `:255-258`

`:split` clones the whole `View` (`workspace.rs:802-816`), so these diverge between panes; and the
swap-trick (`workspace.rs:275-314`) only ever reconstitutes the **focused** `(Document, View)` into an
`EditorState`, so cross-buffer code has nowhere to stand. Buffers also have no lifecycle model
(no Listed/Unlisted/Hidden, no kind), so a special buffer (editable `q:`, picker results) has no home.

This blocks: uppercase/global marks (`mA`-`mZ`), the `"#` register, buffer/file pickers, an editable
`q:`, and cross-buffer keyword completion. (`:ls`/`:bnext`/`:bprev`/`:bd` already work.)

## Goals

- **G1** Editor-global state is owned by the `Workspace`, reached by method/handle (INV-NO-GLOBAL-STATE),
  and lent into the transient `EditorState` per command via the extended swap-trick.
- **G2** Registers become global across buffers (fixes the split-clone divergence; Vim model, D-026).
- **G3** Uppercase/global marks and the cross-buffer jumplist are `(DocumentId, Anchor)` entries in the
  Workspace store (INV-ANCHOR; the C-POSHIST containers of D-027).
- **G4** Buffers gain a `Disposition` (Listed/Unlisted/Hidden) and a `BufferKind` (Ordinary/Scratch/
  NoFile/Interactive), enabling `'hidden'` semantics and special buffers.
- **G5** Buffer retirement is observable so frontend `DocumentId`-keyed maps prune deterministically.
- **G6** INV-DOC-VIEW, the two-crate boundary (D-039), INV-TXN/INV-UNDO are all preserved.

## Non-goals

- **Session save/restore** (mksession/shada) — the serialized form of the global store; deferred
  (view-window-workspace.md §8.3). Keeps this change internal (no persistent-format contract).
- **Multi-client scope** (D-012) — whether some registers become per-client; single workspace/client here.
- **Generational handles** (INV-HANDLE) — the `None`-hole + assert model is kept; hardening is separate.
- **Recursive layout tree / tab pages** — layout stays the flat MVP list (workspace.rs:20-23).
- **The full multi-selection Selection set** (NAT-5, D-027) — orthogonal; not required here.

## Terminology

Uses the glossary (spec/PROJECT.md) and view-window-workspace.md terms (Buffer, View, Window, Buffer
list). New local terms:

- **Editor-global state** — state Vim/Emacs share across all buffers of one editor session: registers,
  uppercase/global marks, the alternate file. *Workspace-scoped*, not process-global.
- **Global store** — the illustrative name for the `Workspace`-owned aggregate holding editor-global state.
- **Cross-buffer position** — a position that names a buffer other than the one it is stored against:
  `(DocumentId, Anchor)`. Requires an anchor because the target buffer is editable while unfocused.

## Invariants

- **INV-DOC-VIEW** — no view-local state moves onto a Document; global state goes to the Workspace.
- **INV-NO-GLOBAL-STATE** — the global store is one component's owned state, reached by method, not an
  ambient singleton.
- **INV-ANCHOR** — cross-buffer positions are anchors in the target Document's store.
- **INV-HANDLE** — buffers/views/anchors referenced by typed id.
- **INV-BUFFER-KIND** — `BufferKind` selects the mutation contract.
- **INV-TXN / INV-UNDO** — unchanged.

## Proposed design

### Ownership: what is global vs. buffer-local

The dividing line follows Vim/Emacs semantics, not implementation convenience.

**Global (moves to the `Workspace` global store):**

| State | Today | Rationale |
|---|---|---|
| `RegisterStore` (unnamed, `a`-`z`, `"0`, `"1`-`"9`, `"-`, `"+`/`"*` mirror) | `View.registers` (`editor/mod.rs:167`) | Vim registers are global; D-026 "one shared store" |
| Uppercase/global marks `mA`-`mZ`, special globals | absent (`named_marks` a-z only) | Vim `mA`-`mZ` are global; D-027 `NamedMap` |
| Alternate buffer `alt` + `"#`/`"%` bridge | `alt` on Workspace (`workspace.rs:111`); `"#` absent | one alternate per window-set; already partly here |
| Last search pattern / last Ex line (`"/`, `":`) | synced per-command (`set_special_registers`, `workspace.rs:336`) | already effectively global; formalize |

**Buffer-local (stays on `View`, or per-Document):**

| State | Home | Rationale |
|---|---|---|
| cursor, mode, `top` (scroll), `curswant`, `anchor`, `caret` gravity | `View` | INV-DOC-VIEW; two views differ |
| lowercase marks `a`-`z` | `View` (per-buffer in Vim) | Vim `a`-`z` are buffer-local |
| Emacs `mark`, `last_visual` (`` `< ``/`` `> ``/`gv`) | `View` | region/selection is view/buffer-local |
| change list `changes`, `change_start/end` (`` `[ ``/`` `] ``), `last_insert` (`` `^ ``) | `View` | Vim change marks are buffer-local |
| replace/block-insert sessions, `auto_indent_pending` | `View` | transient per-session |
| indent/text_width/search_case config snapshot | `View` (until a config loader) | per-buffer overridable |

**Jumplist** is the nuanced one: Vim's jumplist is **per-window**, but each entry may point into another
file. So it stays per-View, but each entry becomes `(DocumentId, Anchor)` instead of a bare offset
(G3). This matches Vim exactly and is confirmed against the nvim oracle (RFC-0018 OQ-4).

### The extended swap-trick

Today (`workspace.rs:275-314`):

```rust
// illustrative
let view = self.views[vid].take();
let doc  = self.docs[slot].take();
let mut st = EditorState::from_parts(doc, view);
let effects = apply_command(&mut st, cmd);
let (doc, view) = st.into_parts();
self.docs[slot] = Some(doc);
self.views[vid] = Some(view);
```

After (the global store is lent in for the command's duration):

```rust
// illustrative — `global` is the Workspace-owned store, borrowed mutably per command
let view = self.views[vid].take();
let doc  = self.docs[slot].take();
let mut st = EditorState::from_parts(doc, view, &mut self.global);
let effects = apply_command(&mut st, cmd);
let (doc, view) = st.into_parts();   // returns the borrow; global stays in the Workspace
self.docs[slot] = Some(doc);
self.views[vid] = Some(view);
```

**Soundness:** ruse is single-threaded and runs one command at a time (D-002); no other buffer/view is
reachable during the command (other Views hold only ids), so a unique `&mut` to the one global store for
the command is sound — the same argument that makes today's `take()` swap sound. The `~20` sibling ops
(`substitute`, `global`, `delete_lines`, `yank_lines`, `join_lines`, `shift_lines`, `move_lines`,
`copy_lines`, `put_lines`, `read_lines`, `filter_lines`, `sort_lines`, `apply_edits`, the snapshot
readers, …) each take the same extra borrow. The planner/commit code reaches registers/marks through
`EditorState` accessors already; only the accessor's backing field changes owner.

### Cross-buffer marks and jumps

```rust
// illustrative Workspace API
fn set_global_mark(&mut self, name: char /* A-Z */, id: DocumentId, at: usize);
fn goto_global_mark(&self, name: char) -> Option<(DocumentId, usize)>; // resolves the anchor now
```

- `set_global_mark` creates an anchor in the target `Document`'s `AnchorStore` (INV-ANCHOR) and records
  `(DocumentId, AnchorId)` in the Workspace `NamedMap`.
- `goto_global_mark` resolves the anchor to a current offset; the **frontend** then `focus_buffer`s the
  target (opening it if the frontend still has its `Files` path but the buffer was wiped is a frontend
  concern — global marks that survive a wipe are a session-persistence feature, out of scope).
- Retiring a `DocumentId` prunes its entries from the Workspace store (marks, jumplist entries) so no
  entry dangles.
- `:marks` merges the focused View's lowercase marks with the Workspace uppercase marks.

### Buffer lifecycle: Disposition + BufferKind

```rust
// illustrative — aligned with view-window-workspace.md §8.1 and INV-BUFFER-KIND
enum Disposition { Listed, Unlisted, Hidden }
enum BufferKind  { Ordinary, Scratch, NoFile, Interactive }
```

- Stored parallel to `docs`/`names` in the arena (or folded into a per-buffer meta struct).
- `buffers()` (`:ls`) filters to `Listed`; `:ls!` includes `Unlisted`.
- `Hidden`: `close_focused`/`remove_buffer` retain a `Hidden` (or modified, under the `'hidden'` option)
  buffer without a window instead of wiping it. The `E89`/unsaved guard stays with the caller.
- `Scratch`/`NoFile` back the editable `q:` buffer and picker-results buffers; they are `Unlisted` and
  exempt from `:w` (no `Files` entry). `Interactive` is the existing terminal/PTY kind (F-011).

### Retirement observability

`close_focused`/`remove_buffer` return (or emit an `Effect` carrying) the set of `DocumentId`s actually
retired, so the frontend prunes its parallel maps (`Files`, highlighters, folds, terminals —
`dispatch.rs:65`, `session.rs`) in lockstep. A debug-build assert flags a `Files` key with no live buffer.

### The unblocked features (consumer sketches)

- **`"#` register:** resolved like `"%`: `set_special_registers` gains an `alt_file` param the frontend
  fills from `alt` + `Files`. Read-only.
- **Buffer picker:** a `Scratch`/`Unlisted` buffer + overlay (view-window-workspace.md §7) whose rows are
  `buffers()`; `<CR>` runs `focus_buffer(id)`.
- **File picker:** same overlay; rows from a CAP-FS/CAP-SEARCH scan (thin consumer, its own feature) with a
  cross-buffer read for preview.
- **Editable `q:`/`q/`/`q?`:** a `Scratch` buffer seeded with history lines; `<CR>` on a line extracts it
  and runs it through the `:normal`/`:g` driven-command re-entry executor. Replaces `cmdwin.rs`'s
  read-only overlay.
- **Other-buffers completion:** `keyword_completion` iterates `buffer_order` (Listed) reading each
  `Document`'s bytes rather than only `focused()` (`workspace.rs:1179-1187`).

## Failure modes

- Global mark/jump into a wiped buffer → resolve returns `None` → no-op/bell (Vim), never a panic.
- Retirement/map desync → surfaced by R2's retired-id set + debug assert.
- `q:` malformed line → normal Ex typed-error path (status line).

## Recovery behavior

No new recovery surface. Buffer text still recovers via the existing journal/panic-hook path
(persistence-and-recovery.md, D-005/D-040). The global store is in-memory only (no persistence in scope).

## Security impact

None new (see RFC-0018 §Security). `q:` runs only user-edited, `<CR>`-confirmed lines; no new core IO.

## Performance impact

Register/mark access stays O(1) (field re-homed). Cross-buffer positions are anchors (bounded update cost,
INV-ANCHOR); global sets are small (bounded ~100). Other-buffers completion is O(listed bytes) per
completion *start*, not per keystroke; cache-able. Text store and parse/render hot paths unchanged (D-042).

## Compatibility impact

Internal only. `View` loses `registers`/global-mark/global-jump portions; `Workspace` gains the store;
`from_parts`/`into_parts` gain the borrow (`pub(crate)` seam, used by the swap-trick + oracle). Public
`apply_command`/`cursor()` unchanged. No persistent format. The parity oracle
(`apps/tui/tests/parity_compare.rs`) gets a test constructor providing a global store (registers-global is
*more* Vim-faithful).

## Observability

Retirement emits retired ids (map-leak hook). `:ls`/`:marks`/`:registers`/`:jumps` snapshots continue via
the swap-trick; uppercase `:marks` and `:registers` reflect the global store post-migration.

## Alternatives

See RFC-0018 §Alternatives (per-View + sync; state on Document; process-global singleton; one big PR;
overlay-forever `q:`) — all rejected there with rationale.

## Rejected approaches

- **Global state on `Document`** → violates INV-DOC-VIEW.
- **Process-global singleton** → violates INV-NO-GLOBAL-STATE + D-002.
Do not re-litigate; the `Workspace` is the single owner.

## Migration strategy (slices)

1. **Slice 1 — buffer lifecycle + `"#` + other-buffers completion (no ownership move).**
   Add `Disposition`/`BufferKind`; make retirement disposition-aware and observable; wire `"#`; iterate
   `buffer_order` in `keyword_completion`. Unblocks the buffer picker's data, `"#`, `'hidden'`, cross-buffer
   completion. No swap-trick signature change.
2. **Slice 2 — register store to the Workspace (the ownership move).**
   Move `RegisterStore` off `View`; extend `from_parts`/`into_parts` + the ~20 swap-trick ops to lend the
   global store; give the oracle a test constructor. Fixes the split-clone register-divergence bug. This is
   the reviewable core of D-057.
3. **Slice 3 — uppercase/global marks + cross-buffer jumplist.**
   Add the Workspace `NamedMap` for `mA`-`mZ` as `(DocumentId, Anchor)`; convert jumplist entries to
   `(DocumentId, Anchor)`; wire `` `A ``/`:marks` merge; frontend `focus_buffer`-then-place. Optionally
   migrate lowercase marks to anchors.
4. **Slice 4 — editable `q:`/`q/`/`q?`.**
   A `Scratch` buffer seeded with history; `<CR>` extract-and-execute via the driven-command executor;
   replace the read-only overlay. Depends on slice 1 (BufferKind).
5. **(Consumer, separate feature) — file picker** over CAP-FS/CAP-SEARCH, reusing slice 1's special-buffer
   + overlay.

Each slice leaves the tree green and is independently shippable.

## Test strategy

- **Slice 1:** unit tests for `Disposition` filtering in `:ls`/`:ls!`; `'hidden'`/`:bd` retention;
  retirement returns the correct retired-id set; `"#` resolves to the alternate; completion pulls a keyword
  that exists only in another Listed buffer.
- **Slice 2:** the existing per-op swap-trick tests stay green; a new test: yank in buffer A, `:b B`, paste
  → text present (the divergence bug's regression test); split then yank in one pane → visible in the other
  pane (Vim). The parity oracle suite stays green.
- **Slice 3:** `mA` in file A, `:b B`, `` `A `` → focus returns to A at the marked position after edits to
  A while unfocused (anchor survival); jumplist `CTRL-O` crosses files; nvim-oracle fixtures for
  cross-file jumps.
- **Slice 4:** open `q:`, edit a line, `<CR>` runs the edited command; `<CR>` on an unedited history line
  reruns it; error line → status error; no infinite recursion on a `q:` line that opens `q:`.
- **Property tests:** global marks/jumplist entries never resolve into a retired buffer (retirement prunes);
  registers survive N random `:b` switches unchanged.

## Open questions

Mirror RFC-0018 §Open questions (lowercase-marks-to-anchors timing; global-vs-buffer register scope; `q:`
re-entry reuse; per-View vs. single jumplist; generational handles scope; file-picker read boundary). The
maintainer resolves these at review before slice 2 (the ownership move) lands.
