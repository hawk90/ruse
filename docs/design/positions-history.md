---
doc: positions-history
project: ruse
title: "ruse Unified Positions-History Model"
summary: >
  Resolves DECISIONS D-027 and the blocking gap V-6. One anchor-based positions-history subsystem
  (C-POSHIST) over the D-023 anchor store reproduces every surface's navigation history through
  PLUGGABLE membership + traversal policies rather than five bespoke stores: Vim jumplist (cursored
  list, `n` is a jump / `j` is not) + `m{a-z}` buffer-local, `m{A-Z}` global-persistent, and special
  marks; Vim changelist (`g;`/`g,`); Emacs per-buffer mark ring + global mark ring with pop-rotate
  semantics; and Helix/Kakoune selection SETS. Point-rings (single positions) and selection-sets
  (ranges) coexist because every entry is a Selection over the same anchor store, and a bare caret is
  a degenerate one-caret selection — so single-selection extends to multi-selection with no type
  rewrite (design-requirements §4 / NAT-5). Global marks and bookmarks persist via re-anchorable
  detached coordinates (ties D-005).
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - register-model.md
  - editing-language.md
  - persistence-and-recovery.md
  - ../parity/vim.md
  - ../parity/emacs.md
  - ../parity/native-style.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Unified Positions-History Model

Resolves **D-027** (open, BLOCKING for Vim marks F-003 L2 and Native multi-selection NAT-5). Closes the
"one anchor store, many navigation histories" question for
[VIM-MARK-1](../parity/vim.md#vim-mark--marks-jumplist-changelist),
[EMACS-REGION-2](../parity/emacs.md#emacs-region--point-mark-region-mark-ring),
[NAT-5](../parity/native-style.md) and closes gap **V-6**. Adds the `C-POSHIST` capability. This is the
positions-history sibling of the register/kill-ring work ([register-model.md](register-model.md), D-026):
same shape of answer — *one store, per-surface policies* — applied to *positions* instead of *text*.

## Problem

Five surfaces look like "a list of places you have been," but differ *in kind* — in **which commands
record an entry** (membership), in **how you move through them** (traversal), in **scope** (per-buffer vs
global), and in **element type** (single point vs a set of ranges):

- **Vim jumplist** — a per-window **cursored list** (max 100). Only *jump* commands push the pre-jump
  position; `n`/`/`/`G`/`%`/`{`/`}`/marks are jumps, `j`/`k`/`w` are not (VIM-MARK-1). Traversal is
  `<C-o>` older / `<C-i>` newer, with a live cursor into the list.
- **Vim marks** — `m{a-z}` **buffer-local named slots**, `m{A-Z}` **global + persistent** across sessions,
  plus **special marks** (`` `` `. `^ `[ `] `< `> `" `( `) `{ `} ``) that the editor auto-maintains.
  `` `m `` jumps *exact/charwise*; `'m` jumps to *first-non-blank/linewise* — one stored position, two
  read projections.
- **Vim changelist** — a per-buffer **cursored list** of edit sites; `g;` older / `g,` newer.
- **Emacs mark ring** — a per-buffer **ring** (default 16) plus the current mark; the region is
  point↔mark and the mark *doubles as a navigation stack*. `C-SPC` pushes, `C-u C-SPC` **pops with
  rotation**. A separate **global mark ring** spans buffers (`pop-global-mark`).
- **Helix/Kakoune selections** — the live state is not one caret but a **set of ranges**; navigation
  keeps/rotates/saves whole selection sets.

The naive failures this doc rejects: (1) build five unrelated stores (duplicated anchor lifecycle, five
persistence paths, no cross-surface coherence — e.g. a mark-jump must *also* push the jumplist); (2) store
raw offsets and fix them up on every edit (violates INV-ANCHOR); (3) type a cursor as a distinct thing from
a selection, so multi-selection (NAT-5) needs a rewrite (the anti-pattern native-style.md §Design
constraints names explicitly). This doc specifies **one anchor-based subsystem** whose per-surface
behavior is entirely **pluggable membership + traversal policies** over shared primitives.

## Goals

- **G1** One authoritative positions-history subsystem (`C-POSHIST`) over the existing anchor store
  (D-023/INV-ANCHOR) — no per-surface anchor bookkeeping, no offset fix-ups.
- **G2** Reproduce **VIM-MARK-1 exactly**: jumplist membership (`n` is a jump / `j` is not), `<C-o>`/`<C-i>`
  cursor semantics with truncation + line-dedup; `m{a-z}` buffer-local vs `m{A-Z}` global-persistent;
  special marks; backtick-exact vs apostrophe-linewise resolution; changelist `g;`/`g,`.
- **G3** Reproduce **EMACS-REGION-2 exactly**: per-buffer mark ring push + `C-u C-SPC` pop-with-rotation,
  `C-x C-x` exchange, `push-mark` on big motions, global mark ring + `pop-global-mark`.
- **G4** Point-rings and selection-sets **coexist over one store**: every entry is a `Selection`; a bare
  cursor is a one-caret collapsed selection, a Helix selection is an N-caret selection — same type.
- **G5** A **single caret/selection extends to multi-selection (NAT-5) with no type rewrite**
  (design-requirements §4).
- **G6** Global marks + bookmarks **persist across sessions** and re-anchor on file reload
  (VIM-STATE-1, EMACS-EDIT-5), tied to [persistence-and-recovery.md](persistence-and-recovery.md) (D-005).
- **G7** Membership is **command-metadata-driven**, so plugins that add jump/change/mark-setting commands
  participate without patching the subsystem (INV-CMD-SEMANTIC).

## Non-goals

- The **selection-editing grammar** (how a multi-selection is produced/split/aligned — Helix/Kakoune
  `s`/`S`/`&`, operator application to N ranges). That is input-engine/editing-language territory
  ([editing-language.md](editing-language.md), D-025); this doc owns the **storage + history** of
  selections, not the operators that mutate them.
- The **register/kill-ring** text model (D-026). Emacs *registers* that hold **positions** (EMACS-EDIT-4)
  and **bookmarks** (EMACS-EDIT-5) are in scope here as position kinds; register *text* slots are
  [register-model.md](register-model.md)'s (this closes its OQ-5).
- The **on-disk session/workspace file format** beyond "global marks/bookmarks serialize as re-anchorable
  detached coordinates, versioned" — the format doc is separate (persistence-and-recovery.md Non-goals).
- Running Vimscript / Elisp (L3, D-007).

## v0 scope — Visual-mode selection (shipped; the rest of this doc is the deferred target)

The full model below is the unified anchor-store-backed `Selection` set (jumplist, mark-ring, Helix/Kakoune
N-caret sets) — **deferred** (D-027, RFC-0012). What **ships in v0** is the single live selection that Visual
mode needs, built on the model's load-bearing invariant (**G4**): a selection is a range with a fixed
**anchor** and a moving **active** end, and a bare caret is the degenerate collapsed selection.

- **`Mode::Visual { line }`** — charwise (`v`) and linewise (`V`). Blockwise (`Ctrl-V`) is deferred.
- **The selection is `(anchor, cursor)`** — one `Option<usize>` anchor on the frontend `EditorState`; the
  cursor is the active end. Entering Visual sets `anchor = cursor` (collapsed); motions move the cursor and
  extend the range; any exit clears the anchor. Charwise includes the character under the higher end (Vim's
  inclusive selection); linewise spans whole lines.
- **Operators over the selection:** `d`/`x` delete, `y` yank, `c` change — each captures the span into the
  unnamed register ([register-model.md](register-model.md) v0) and returns to Normal (Insert for `c`).

**Deferred, and additive over this** (no rework of the `(anchor, active)` shape): the anchor-store-backed
`Selection` type, N-caret multi-selection (a `Vec<Selection>` — v0's one range is `N = 1`), the jumplist /
mark-ring / changelist containers, blockwise geometry, and Visual-mode `p` register-swap. The single range is
already on the D-027 trajectory: growing to multi-selection changes *how many* selections exist, not the
selection's type. Implementation: `Mode::Visual` + `EditorState::anchor` / `selection_span` in
`crates/core/src/editor.rs`, keys in `apps/tui/src/input.rs`, reverse-video paint in `main.rs`.

## Terminology

See [spec glossary](../../spec/glossary.yaml) and [PROJECT.md]. New local terms:

- **Positions-history subsystem** (`C-POSHIST`) — the single owner of all navigation-history state for a
  workspace/session, reached by typed handle (INV-NO-GLOBAL-STATE).
- **PositionRef** — an anchor-based position that is `Live` (an anchor in a loaded buffer) or `Detached`
  (a re-anchorable coordinate for an unloaded/persisted target).
- **Caret** — an `(anchor, head)` pair; a bare cursor is a *collapsed* caret (`anchor == head`).
- **Selection** — an ordered **set** of carets with a primary index. One caret today; N with NAT-5. The
  universal history element.
- **History container** — one of three structural types: **NamedMap**, **Ring**, **CursoredList**.
- **MembershipPolicy** — decides *whether* a command records an entry (and what).
- **TraversalPolicy** — maps traversal keys/commands (`<C-o>`, `g;`, `C-u C-SPC`, …) onto container moves.
- **Surface** — a concrete `(container, membership, traversal, scope)` instance (Vim jumplist, Emacs mark
  ring, …).

## Invariants

This doc depends on and is governed by:

- **INV-ANCHOR** (primary) — every stored position is an **anchor** with affinity/gravity that survives
  edits; never a raw offset. All history entries therefore update *for free* when the document changes;
  the subsystem never runs an O(entries × edits) fix-up pass — it holds handles into the shared anchor
  store, whose update cost is already bounded (D-023, PERF-6). *This doc's whole reason to exist.*
- **INV-POS-TYPED** — the `TextCoord` in a `Detached` ref is typed by unit (line/char/grapheme/cell), never
  an untyped `usize`.
- **INV-HANDLE** — anchors and entries are referenced by generational handles; a mark to a region that was
  deleted resolves to an *invalidated/clamped* position (an expected typed outcome), never a stale-pointer
  panic.
- **INV-NO-GLOBAL-STATE** — `C-POSHIST` is a component that **owns** its rings/lists/maps and is reached by
  handle; "global mark ring / global marks" means *one owner reachable by many views*, not ambient global
  mutable state.
- **INV-DOC-VIEW** — buffer-scoped history (marks `a-z`, mark ring, changelist) is keyed by **Document**;
  the jumplist is **per-view** (per-window, VIM-MARK-1). Neither lives *inside* the Document's text; a
  Document must not know about a View, so per-view jumplists are owned by `C-POSHIST` keyed by `ViewId`.
- **INV-CMD-SEMANTIC** — membership is driven by **navigation metadata on semantic commands** (`is_jump`,
  `records_change`, `sets_mark`), not by hard-coded key lists. `` `a ``, `<C-o>`, `M-y`-style suffixes are
  *arguments*, not distinct commands.
- **INV-TXN / INV-UNDO** — setting a mark / pushing the jumplist is **session state, not Document text**,
  so it is *not* an undoable Document transaction (undo does not un-set a mark, matching Vim/Emacs). A
  *motion to* a recorded place moves the caret but emits no Transaction; only edits do.
- **INV-ORIGIN** — every recorded entry carries its origin (UserInput | Macro | Plugin | Lsp | AiAgent |
  RemotePeer); a macro replaying `G` pushes the jumplist identically to interactive `G`.
- **INV-ASYNC-ORDER** — LSP "go-to-definition" is a jump: its async result carries a request id + revision;
  a stale result is dropped and does **not** push the jumplist (guards a "jump to nowhere" race).
- **INV-CAP-DEGRADE** — a persisted global mark whose file changed re-anchors by fingerprint; if the anchor
  can't be re-established it **degrades** to a clamped line position with a status note, it does not vanish.

No new `INV-*` is minted here (per the reference-invariants single-registry rule, D-021/D-022). See Open
questions OQ-6 for whether `INV-POSHIST` should be added.

## Proposed design

### 1. Anchor-based primitives (the point ⇄ range unification)

Everything a surface stores is expressed with three types. The load-bearing choice for **G4/G5** is that
`Selection` is **always a set of carets**, and a bare cursor is a set of one *collapsed* caret — there is
no separate "single cursor" type to later replace.

```rust
/// One anchor-based position. `Live` while its buffer is loaded; `Detached` when the buffer is
/// closed or the position was restored from disk and must be re-anchored on load (§6).
enum PositionRef {
    Live     { buffer: BufferId, anchor: AnchorId },        // handle into the D-023 anchor store
    Detached { file: FileKey, coord: TextCoord, fp: LineFingerprint }, // re-anchorable (INV-POS-TYPED)
}

/// A caret = anchor end + moving head. A bare cursor is `anchor == head` (a COLLAPSED caret).
/// The apostrophe/backtick and inclusive/exclusive read-projections are applied at *resolution*, not
/// stored here — one position, many projections (mirrors register-model's one-slot-many-views).
struct Caret { anchor: PositionRef, head: PositionRef, affinity: Affinity }

/// The UNIVERSAL history element. One caret today; N carets under NAT-5 — SAME TYPE, no rewrite (G5).
/// A Vim jumplist entry, an Emacs mark-ring entry, and a Helix selection are all `Selection`s; the first
/// two just happen to carry a single collapsed caret.
struct Selection {
    carets:  SmallVec<[Caret; 1]>,   // inline capacity 1 → zero-cost for the single-caret case
    primary: u16,                    // index of the "main" caret (the one motions report)
}

impl Selection {
    fn point(p: PositionRef) -> Self;         // collapsed single caret — the "point-ring" entry
    fn is_collapsed(&self) -> bool;           // all carets have anchor == head
    fn region(anchor: PositionRef, head: PositionRef) -> Self; // Emacs point↔mark
    // NAT-5 growth is `carets.push(..)` — no type change anywhere upstream.
}
```

> **Why this satisfies design-requirements §4 / NAT-5 (G5).** The rest of the subsystem — every container,
> every policy, the persistence layer, `C-POSHIST` — is written against `Selection`. Single-selection is
> the `carets.len() == 1, is_collapsed()` case. Enabling multi-selection is *adding carets to an existing
> type*, never swapping a `Cursor` type for a `Selection` type across the codebase. The single-selection
> MVP therefore cannot "become unextensible" (the guarded anti-pattern).

### 2. The three history containers (ring vs cursored-list vs named-map vs set)

All five surfaces reduce to **three container shapes** parameterized by the `Selection` element. This is
the "data structures" half of D-027.

```rust
/// (a) NAMED-MAP — addressable slots. Vim marks `m{a-z}`, `m{A-Z}`, special marks. No traversal cursor.
struct NamedMap { slots: HashMap<char, Selection> }   // value is a collapsed Selection

/// (b) RING — bounded FIFO with a ROTATE-on-pop cursor. Emacs per-buffer + global mark rings.
struct Ring {
    buf:  VecDeque<Selection>,   // most-recent at FRONT
    max:  usize,                 // mark-ring-max (16) / global (16)
    rot:  usize,                 // rotation cursor for repeated pop-rotate (C-u C-SPC / pop-global-mark)
}

/// (c) CURSORED-LIST — a bounded list with an INSERTION end and a live TRAVERSAL cursor.
/// Vim jumplist (per-view) and changelist (per-buffer). `cursor == len` means "at current / past newest".
struct CursoredList {
    entries: Vec<Selection>,
    cursor:  usize,              // traversal position; older = cursor-1, newer = cursor+1
    max:     usize,              // 100
}

/// (d) SET — the LIVE selection is itself a set of ranges (§1 Selection). Selection HISTORY (save/restore,
/// rotate) reuses (b)/(c) with the full multi-caret Selection as the element — no fourth structure.
```

The distinction matters and is not cosmetic:

| Shape | Insertion | Traversal | "Current" pointer | Used by |
| --- | --- | --- | --- | --- |
| NamedMap | keyed write (replace) | none (direct addressing) | n/a | Vim `m{a-z}`, `m{A-Z}`, special marks |
| Ring | push-front, bounded, rotate-on-pop | pop moves point **and rotates** | rotation index | Emacs mark ring, global mark ring |
| CursoredList | push-back with truncate-forward + line-dedup | older/newer step a **live cursor** | `cursor` field | Vim jumplist, changelist |
| Set | live multi-caret Selection | keep/rotate/save whole sets | primary caret | Helix/Kakoune selections |

### 3. Pluggable policies (the crux of D-027)

Surfaces differ only in **membership** (which commands record) and **traversal** (which keys move, and
how). Both are traits; a surface is a container + a policy pair + a scope.

#### 3.1 Membership is command metadata, not a key list

Every semantic command carries navigation metadata (INV-CMD-SEMANTIC). This is *the* mechanism that makes
"`n` is a jump / `j` is not" declarative and plugin-extensible (G7).

```rust
struct NavMeta {
    is_jump:        bool,   // Vim jumplist: record PRE-command position. Set on: / ? n N * # % G gg
                            //   { } ( ) [[ ]] H M L, mark-jumps (` '), :tag, :s, LSP go-to-def, `:e file`.
                            //   CLEARED on: h j k l w b e 0 $ f t ; , and all scrolling (z, C-e/C-y).
    records_change: bool,   // Vim changelist / `` `. ``: any command producing a Document Transaction.
    sets_mark:      SetMark, // Emacs push-mark: None | Explicit(C-SPC) | AutoBig (M-<, M->, isearch end,
                            //   query-replace, before a big jump) → push current mark to the ring.
}
```

```rust
/// Consulted by C-POSHIST BEFORE a command runs (for pre-position capture) and AFTER (for change sites).
trait MembershipPolicy {
    /// Return Some(entry) to record; None to ignore. `ctx` carries the command's NavMeta, the pre/post
    /// Selection, buffer/view ids, and origin.
    fn on_command(&self, ctx: &NavCtx) -> Option<Selection>;
    /// Collapse rule when the new entry is "the same place" as an existing one.
    fn dedup(&self, existing: &Selection, incoming: &Selection) -> Dedup; // Keep | ReplaceExisting | Drop
}
```

Multiple policies observe the same command — a **mark-jump command has `is_jump = true`**, so it both reads
the mark NamedMap (traversal of the marks surface) *and* triggers the jumplist's `MembershipPolicy` to push
the pre-jump position. Cross-surface coherence falls out of one command stream feeding many policies (the
exact analogue of register-model's "one event, many indices").

#### 3.2 Traversal maps keys onto container moves

```rust
trait TraversalPolicy {
    /// Move within the container and return the Selection to restore (caret motion, NOT a Transaction).
    fn go(&mut self, dir: NavDir, count: usize, c: &mut dyn HistoryContainer) -> Option<Selection>;
}
enum NavDir { Older, Newer, PopRotate, Exchange, PeekName(char) }
```

`<C-o>`→`Older`, `<C-i>`/`<Tab>`→`Newer` on the jumplist; `g;`→`Older`, `g,`→`Newer` on the changelist;
`C-u C-SPC`→`PopRotate` and `C-x C-x`→`Exchange` on the mark ring; `pop-global-mark`→`PopRotate` on the
global ring; `` `a ``/`'a`→`PeekName('a')` on the marks map (with backtick vs apostrophe as a *resolution*
flag, §4.2).

### 4. Per-surface instantiation

| Surface | Container | Scope | Membership | Traversal |
| --- | --- | --- | --- | --- |
| Vim **jumplist** | CursoredList(100) | **per-view** | push pre-position when `is_jump` (§4.1) | `<C-o>`=Older / `<C-i>`/`<Tab>`=Newer |
| Vim **marks `a-z`** | NamedMap | **per-buffer** | explicit `m{a-z}` write | `` `a ``/`'a` = PeekName (exact/linewise) |
| Vim **marks `A-Z`** | NamedMap | **global + persistent** | explicit `m{A-Z}` write | `` `A ``/`'A` cross-buffer PeekName |
| Vim **special marks** | NamedMap (derived) | per-buffer/view | lifecycle hooks (§4.3) | PeekName; some are read-only |
| Vim **changelist** | CursoredList(100) | per-buffer | push when `records_change` (`` `. `` site) | `g;`=Older / `g,`=Newer |
| Emacs **mark ring** | Ring(16) + current mark | per-buffer | `sets_mark` (Explicit/AutoBig) | `C-u C-SPC`=PopRotate, `C-x C-x`=Exchange |
| Emacs **global mark ring** | Ring(16) | global | push when a mark is set in a **different buffer** than the last | `pop-global-mark`=PopRotate |
| **Helix/Kakoune selections** | live Set (+ optional selection Ring) | per-view | selection-producing ops; save→push Ring | keep/rotate primary; save/restore sets |

#### 4.1 Vim jumplist algorithm (VIM-MARK-1, `<C-o>`/`<C-i>`)

```text
// Membership: BEFORE executing a command whose NavMeta.is_jump, with current view Selection C:
push_jump(list, C):
    dedup: remove any existing entry on C's line (line-level dedup, Vim semantics)
    truncate: list.entries.truncate(list.cursor)      // drop forward (newer) entries
    list.entries.push(collapse(C))                    // store as collapsed Selection
    if list.len > max: drop oldest (front)
    list.cursor = list.entries.len                    // cursor past the newest = "at current"

// Traversal Older (<C-o>), count n:
go_older(list, n):
    if list.cursor == list.entries.len:               // first step back from "current"
        // save where we are so <C-i> can return to it (Vim keeps the from-position)
        stash_current_into(list)                      // appends current pos if not already the tail
    list.cursor = max(0, list.cursor - n)
    return list.entries[list.cursor]                  // restore that Selection (caret motion only)

// Traversal Newer (<C-i>/<Tab>), count n:
go_newer(list, n):
    list.cursor = min(list.entries.len, list.cursor + n)
    return if list.cursor < len { entries[cursor] } else { stashed_current }
```

The **membership predicate is `NavMeta.is_jump`** — so `n`, `/`, `G`, `%`, `{`, mark-jumps, `:tag`, and
LSP go-to-def push, while `j`, `k`, `w`, `f`, scrolling do not. Adding a plugin jump command = setting one
flag (G7). Jumps are per-view because splits have independent jumplists (INV-DOC-VIEW).

#### 4.2 Vim marks + backtick/apostrophe resolution

`m{a-z}` writes `NamedMap[a] = point(current_head)` into the **buffer's** map; `m{A-Z}` writes into the
**global** map with a `Detached`-capable `PositionRef` (so it survives buffer close and session end, §6).
One stored position; the *jump command* picks the projection:

- `` `a `` → resolve to the **exact** stored `(line, col)`, charwise/exclusive (the anchor's position).
- `'a` → resolve to **first-non-blank of the stored line**, linewise.

Because a mark-jump command has `is_jump = true`, executing `` `a `` also pushes the jumplist (§4.1) and
updates the `` `` `` "position before last jump" special mark. This composition is why one command stream +
many policies beats five stores.

#### 4.3 Special marks (derived / lifecycle-maintained)

Auto-maintained by event hooks into the NamedMap; most are read-only to the user:

| Mark | Set by | Notes |
| --- | --- | --- |
| `` `` `` , `` '' `` | any jump | position **before** the last jump; set by `push_jump` |
| `` `. `` | each edit (`records_change`) | last change position = changelist head |
| `` `^ `` | leaving Insert | last insert position (`gi` target) |
| `` `[ `` `` `] `` | after any change/yank | bounds of last changed/yanked text (**plugin-critical**, VIM-MARK-1) |
| `` `< `` `` `> `` | leaving Visual | last visual selection bounds — stored as the **full Selection**, not two points |
| `` `" `` | leaving buffer | last cursor position (restored on reopen; persisted with A-Z class) |
| `` `( `) `{ `} `` | on demand | sentence/paragraph bounds, computed at read (not stored) |

`` `< ``/`` `> `` storing a whole `Selection` (not a point pair) is what lets `gv` restore a **multi-caret**
visual selection under NAT-5 with no change here (G5).

#### 4.4 Emacs mark ring + global ring (EMACS-REGION-2)

```text
set_mark(buffer_ring, new):                    // C-SPC, or AutoBig push before a big motion
    if current_mark exists: buffer_ring.push_front(current_mark)   // old mark → ring
    trim buffer_ring to max (16)
    current_mark = new
    if new.buffer != global_ring.front().buffer: global_ring.push_front(new)   // cross-buffer only

pop_rotate(buffer_ring):                        // C-u C-SPC
    if buffer_ring empty: return
    target = buffer_ring[rot]
    move point → target                         // caret motion, no Transaction
    buffer_ring.rotate_left(1); rot stays 0     // popped entry goes to the BACK → repeat visits older
    // (Emacs: point goes where the mark was; the ring is rotated so repeats walk the ring)

exchange(): swap(point, current_mark)           // C-x C-x

pop_global_mark(): pop_rotate(global_ring) → may SWITCH BUFFER to the target's buffer, then move point
```

`C-SPC C-SPC` (push without activating) = `set_mark` with the region left **inactive** (EMACS-REGION-3;
active/inactive is a flag on the current mark, independent of highlight). The region is
`Selection::region(current_mark, point)` — the very same `Selection` type, so a future multi-region Emacs
mode is, again, just more carets (G5).

#### 4.5 Helix/Kakoune selection sets

The **live** selection is a `Selection` with N carets (§1) — this *is* the multi-selection model; NAT-5 is
"allow `carets.len() > 1`," gated post-MVP but structurally present from day one. Selection *history*
(Kakoune saves selections to registers `Z`/`z`; Helix keeps a selection stack, `,` collapses to primary)
is a **Ring/CursoredList whose element is the full multi-caret Selection** — no new structure, and it
persists to the same store (position-typed registers, §7).

### 5. How point-rings and selection-sets coexist over one store (D-027's central question)

Two independent things share one substrate, exactly as register-model's numbered-ring and kill-ring do:

1. **One element type.** Jumplist/changelist/mark-ring entries are **collapsed** `Selection`s (single caret,
   `anchor == head`); Helix entries are **full** `Selection`s. The containers never know or care which —
   they store `Selection`. A "point-ring" is a ring of collapsed selections; a "selection-set history" is a
   ring of non-collapsed selections. Same code path.
2. **One anchor store.** Every `PositionRef::Live` is a handle into the D-023 anchor store. A single edit
   moves *all* carets of *all* entries of *all* surfaces at once, via the anchor store's existing
   affinity/gravity update — the positions-history subsystem stores handles and runs **no** per-entry
   fix-up (INV-ANCHOR; this is G1 and the performance win).
3. **Independent policies, no interference.** The jumplist's CursoredList cursor, the mark ring's rotation
   index, and the marks NamedMap are disjoint indices; a `C-SPC` never shifts the jumplist, a `<C-o>` never
   rotates the mark ring. The only deliberately-shared coupling is the **anchors themselves** and the
   cross-surface *command* coupling (a jump command feeds both the marks policy and the jumplist policy) —
   which is desired coherence, not corruption. (Directly parallels register-model §2.3.)

### 6. Persistence + re-anchoring (ties D-005)

Live anchors are generational handles into a *loaded* buffer — they **cannot** be serialized as-is, and
they refer to buffers that may be closed. `PositionRef` has two states and transitions on buffer lifecycle:

- **On buffer close:** every entry referencing that buffer converts `Live { buffer, anchor }` →
  `Detached { file, coord = anchor.resolve(), fp = fingerprint(line) }`. `coord` is the typed
  `(line, col)` (INV-POS-TYPED); `fp` is a cheap hash of the mark's line (± a few neighbors).
- **On buffer (re)open:** each `Detached` entry for that file **re-anchors** — locate the line by `coord`,
  validate with `fp`; if the file changed, search a small window for the fingerprint; on success create a
  fresh `Live` anchor, else **clamp** to `coord.line` (bounded) and flag the entry `shifted` with a status
  note (INV-CAP-DEGRADE — the mark degrades, never disappears).

What persists across **sessions** (serialized into the session/workspace store, **not** the Document
journal — this is session state outside INV-TXN, matching register-model §Recovery):

| State | Persists? | Form |
| --- | --- | --- |
| `m{A-Z}` global marks | **yes** (VIM-STATE-1) | `Detached` coords + fingerprint |
| `` `" `` last-position, numbered file marks | yes | `Detached` |
| Jumplist | yes (viminfo parity) | list of `Detached` coords, cursor index |
| `m{a-z}` buffer marks, changelist | session-lifetime; optional per-buffer persist | `Detached` on close |
| Emacs mark ring / global ring | **no** (Emacs does not persist these) | dropped on exit |
| **Bookmarks** (EMACS-EDIT-5) | **yes**, cross-session | named `Detached` entries in the bookmark file |

On **crash recovery** (D-005): the live rings are session state and are lost, *except* the persisted classes
above, which are restored from the session store and re-anchored on the next open of each file. This closes
persistence-and-recovery.md's explicit deferral of "positions-history persistence (D-027)" and
register-model.md's OQ-4/OQ-5 overlap. The persisted form is **versioned** (INV-ADDITIVE); an unknown
future field is ignored, not fatal.

### 7. `C-POSHIST` capability

The versioned contract (INV-CONTRACT-FIRST) exposing positions-history to commands/plugins. Plugins receive
**snapshots** (INV-QUERY-SNAPSHOT, INV-PLUGIN-NO-CORE), never the live containers or the anchor store.

```rust
// Marks
set_mark(scope: MarkScope, name: char, sel: Selection, origin: Origin)
get_mark(scope: MarkScope, name: char, proj: MarkProjection /* Exact | LineFirstNonBlank */) -> Option<Selection>
// Jumplist / changelist (cursored lists)
record_jump(view: ViewId, pre: Selection)            // called via NavMeta.is_jump
step_jump(view: ViewId, dir: NavDir, count: usize) -> Option<Selection>
record_change(buffer: BufferId, at: Selection)       // called via NavMeta.records_change
step_change(buffer: BufferId, dir: NavDir, count: usize) -> Option<Selection>
// Emacs rings
push_mark(buffer: BufferId, sel: Selection, kind: SetMark)
pop_mark(buffer: BufferId) -> Option<Selection>       // C-u C-SPC pop-rotate
pop_global_mark() -> Option<(BufferId, Selection)>
// Selection sets (Helix/Kakoune)
save_selection(view: ViewId, name: Option<char>, sel: Selection)   // Z / register
restore_selection(view: ViewId, name: Option<char>) -> Option<Selection>
// Introspection (buffer views)
jumps(view) / changes(buffer) / marks(scope) / mark_ring(buffer) -> Snapshot
```

`MarkScope` = `Buffer(BufferId) | Global`; `NavDir` per §3.2. Command IDs are namespaced (D-006);
`C-POSHIST` is additive to the contract set (INV-ADDITIVE).

## Failure modes

- **Mark inside a deleted region.** The anchor collapses to the edit boundary per its affinity (INV-ANCHOR);
  the entry stays valid at the collapsed position (Vim/Emacs both keep the mark, just moved).
- **Persisted global mark to a since-changed file.** Re-anchor by fingerprint; on miss, clamp to the stored
  line and flag `shifted` with a status note (INV-CAP-DEGRADE). Never a panic, never a silent wrong jump
  without the flag.
- **`<C-o>` / `g;` / `C-u C-SPC` on an empty container.** No-op with a status note (Vim "already at oldest"
  / Emacs "No mark set"); the traversal returns `None`.
- **Stale LSP go-to-def result** arriving after the user moved (INV-ASYNC-ORDER): dropped; does **not** push
  the jumplist.
- **Buffer closed while entries reference it.** Entries detach (§6); a jump to a `Detached` entry loads the
  buffer first (like `pop-global-mark` switching buffers), then re-anchors.
- **Stale entry handle** (evicted by a ring bound): resolves to *absent* (INV-HANDLE), a typed no-op.

## Recovery behavior

Positions-history is **session state**, not Document state: not in the undo history, not in the Document
journal (D-005). On crash/recovery the live rings/lists are empty; the **persisted classes** (§6:
`m{A-Z}`, `` `" ``, jumplist, bookmarks) are restored from the versioned session store and re-anchored as
each file reopens. Undoing an edit moves anchors (so mark positions follow the text) but does **not**
restore or re-order any ring/list/map pointer — undo does not "un-set a mark" or "un-jump," matching both
Vim and Emacs.

## Security impact

- **Persisted marks/bookmarks embed file paths + line fingerprints** — mild workspace-structure disclosure;
  they live in the session/workspace store under the same trust boundary as the recovery journal (D-005,
  INV-TRUST-1) and are redacted in diagnostic bundles per the log-PII policy (D-017).
- **Origin tagging** (INV-ORIGIN): AI/RemotePeer/Plugin-set marks and jumps are attributable; an AI agent
  driving navigation is reviewable (SEC-15).
- No new I/O surface (no clipboard/network path) beyond the existing session-store writes.

## Performance impact

- **No per-edit fix-up.** Entries hold anchor handles; the anchor store's existing bounded update covers all
  surfaces at once (INV-ANCHOR / PERF-6) — the subsystem's per-edit cost is **O(0)**, not O(entries).
- Containers are **bounded**: jumplist/changelist 100, mark rings 16, marks map 26 + specials. Push = O(1)
  amortized (list truncate-forward is O(dropped)); pop-rotate = O(1); mark write = O(1) hash.
- `Detached` entries hold no anchor and cost nothing while their buffer is unloaded; re-anchoring on open is
  O(1) fingerprint check (+ a bounded window search only on mismatch).
- `Selection` uses inline `SmallVec<[Caret;1]>` so the single-caret case is allocation-free (G5 has no MVP
  cost).

## Compatibility impact

- Unblocks Vim marks/jumplist/changelist (F-003 L2) and Native multi-selection (NAT-5); satisfies
  VIM-MARK-1, EMACS-REGION-2.
- `C-POSHIST` is additive (INV-ADDITIVE); command IDs namespaced (D-006). Closes register-model.md OQ-5
  (position registers) and persistence-and-recovery.md's D-027 deferral.
- The `Selection`-is-a-set choice is a **forward-compatibility guarantee**: enabling NAT-5 ships no breaking
  type change (design-requirements §4).

## Observability

- Store ops emit typed events `{op, surface, scope, origin, revision}` for the event model, macro/AI review.
- Introspection buffers (EMACS-BUFFER-2 style): `:jumps`, `:changes`, `:marks` (Vim) and
  describe-mark-ring / describe-bookmarks (Emacs) render entry index, buffer/file, resolved line:col, and a
  `Live`/`Detached`/`shifted` badge. The jumplist/changelist cursor and the mark-ring rotation index are
  shown so the `<C-o>`/`g;`/`C-u C-SPC` state is inspectable.

## Alternatives

- **A1 — One anchor store, per-surface pluggable policies (chosen).** Three containers + membership/traversal
  policy pairs over one anchor-backed `Selection` element. Chosen because each surface is a small,
  declarative instance (§4), cross-surface coherence (mark-jump ⇒ jumplist push) falls out of one command
  stream feeding many policies, and there is exactly one anchor lifecycle + one persistence path.
- **A2 — One universal ring for all surfaces.** A single global ring of positions with tags. Rejected: cannot
  represent per-view jumplists vs per-buffer changelists vs global marks with different bounds and different
  membership; collapses distinctions VIM-MARK-1/EMACS-REGION-2 require.

## Rejected approaches

- **R1 — Five bespoke stores.** Independent jumplist/mark/changelist/mark-ring/selection modules. Rejected:
  duplicated anchor bookkeeping (five O(entries×edits) fix-up risks), five persistence/recovery paths, and
  no clean place for the *mark-jump-also-pushes-jumplist* coupling — it becomes ad-hoc cross-calls.
- **R2 — Store raw offsets and fix them up on every edit.** Directly violates INV-ANCHOR (D-023) and is
  O(entries × edits); the exact failure the anchor decision exists to prevent (TEXT-4/5/6, PERF-6).
- **R3 — Type a cursor distinctly from a selection (`Cursor` vs `Selection`).** Rejected: multi-selection
  (NAT-5) would then be a type rewrite rippling through every container, policy, and the persistence layer —
  the "single selection becomes unextensible" anti-pattern (native-style.md §Design constraints,
  design-requirements §4). A collapsed one-caret `Selection` costs nothing (inline SmallVec) and extends for
  free.
- **R4 — Hard-code the jump-command key list.** Rejected: `n`-is-a-jump/`j`-is-not, and plugin jump commands
  (LSP go-to-def, `:tag`), must be declarative. Membership is `NavMeta` metadata on semantic commands
  (INV-CMD-SEMANTIC, G7), so plugins participate without patching the subsystem.
- **R5 — Persist live anchors.** Anchors are generational handles into a loaded buffer; serializing them is
  meaningless across sessions/reloads. Rejected in favor of `Detached` re-anchorable coordinates (§6).
- **R6 — Make mark-set / jump undoable Document transactions.** Would let `u` un-set a mark. Contradicts
  Vim/Emacs and pollutes the undo tree with non-Document state (mirrors register-model R4). Positions-history
  is session state outside INV-TXN/INV-UNDO.

## Trade-offs

- **Uniform `Selection` element vs specialization.** Storing every entry as a `Selection` costs a `primary`
  index and a length-1 inline vec even for a bare mark. Accepted: it is allocation-free and it is the entire
  mechanism that makes NAT-5 non-breaking (G5). The alternative (specialized point type) is faster to write
  once and expensive to extend later — the wrong trade for a long-horizon design.
- **Command-metadata membership vs explicit per-surface hooks.** `NavMeta` couples the command registry to
  the history subsystem (every jump/change/mark command must set a flag). Accepted: it is the only
  plugin-extensible, declaratively-testable way to get "`n` is a jump / `j` is not" right, and it keeps the
  membership rules in one auditable place.
- **Re-anchor-by-fingerprint vs exact restore.** Persisted marks can land on the wrong line if a file changed
  a lot offline. Accepted with a visible `shifted` flag (INV-CAP-DEGRADE) — Vim/Emacs have the same inherent
  limitation; degrading with a badge beats losing the mark or jumping silently wrong.
- **Per-view jumplist vs per-buffer.** Per-view (VIM-MARK-1/INV-DOC-VIEW) means splits have independent
  histories, costing a small `ViewId`-keyed map. Accepted: it is the correct Vim semantics and the natural
  owner boundary.

## Migration strategy

Greenfield (no prior positions-history impl). Land the `PositionRef`/`Caret`/`Selection` primitives and the
three containers with `C-POSHIST` behind the input engine (F-003) *before* Vim marks/jumplist reach L2. Ship
single-caret `Selection` for MVP; NAT-5 flips `carets.len() > 1` on with **no** type change (G5). Persisted
classes (§6) integrate with the session store when D-005's session serialization lands. The differential
corpus below gates each surface's merge.

## Test strategy

Differential tests (TEST-2 corpus). Each asserts container state and/or resulting caret position.

- **T-01 jumplist membership.** `/foo<CR>` then `nn` push; interleaved `jjj` do **not**; `<C-o>` walks back
  through the search jumps only. Confirms `is_jump` gating (VIM-MARK-1 checklist #6).
- **T-02 jumplist cursor + truncate.** After several jumps, `<C-o><C-o>` then a new jump discards the forward
  entries; `<C-i>` no longer returns forward; line-level dedup removes a duplicate-line entry.
- **T-03 backtick vs apostrophe.** `majj` then `` `a `` lands exact col; `'a` lands first-non-blank of the
  line; a mark-jump also pushed the jumplist and set `` `` ``.
- **T-04 global vs buffer marks.** `mA` in file X, open file Y, `` `A `` switches back to X exact position;
  `ma` (lowercase) is invisible from Y.
- **T-05 special-mark bounds.** After `y} ` , `` `[ ``/`` `] `` bracket the yanked text; after leaving Visual,
  `gv` restores the selection via `` `< ``/`` `> `` stored as a Selection.
- **T-06 changelist.** Edits at 3 sites → `g;` walks back through them, `g,` forward; `` `. `` = last change.
- **T-07 Emacs mark ring pop-rotate.** `C-SPC` at A, move, `C-SPC` at B, move; `C-u C-SPC` → B, again → A,
  again → wraps; ring rotation matches Emacs.
- **T-08 exchange + inactive push.** `C-x C-x` swaps point/mark; `C-SPC C-SPC` pushes without activating the
  region (EMACS-REGION-3).
- **T-09 global mark ring.** Set marks across files A,B,C; `pop-global-mark` visits most-recent cross-buffer,
  switching buffers, rotating.
- **T-10 anchor survival.** Random edit sequences: all marks/jumplist/changelist entries move with the text,
  never point at stale offsets (INV-ANCHOR property test).
- **T-11 persistence + re-anchor.** `mA`, quit, mutate file X offline, restart, open X: mark re-anchors by
  fingerprint; on a big change it clamps to the line and reports `shifted` (INV-CAP-DEGRADE).
- **T-12 detach/reattach.** Close X with a global-ring entry into it → entry `Detached`; `pop-global-mark`
  reloads X and re-anchors.
- **T-13 multi-selection non-regression (NAT-5).** A 3-caret Helix `Selection` saved and restored round-trips
  through the same containers as a single caret — **no** type change, proving G5.
- **T-14 undo does not un-mark.** Set a mark, edit, `u`: the mark position follows the undo (anchor moves)
  but is not removed/re-ordered; the jumplist cursor is unchanged.

Property tests (against invariants): random interleavings of all five surfaces' ops never (a) panic, (b)
leave a `Selection` with a dangling anchor resolving to a wrong position (INV-HANDLE), or (c) let one
surface's traversal mutate another's container index (§5.3).

## Open questions

- **OQ-1** — Exact jumplist edge cases Vim gets subtly right: whether the *very first* `<C-o>` from "current"
  stashes the from-position as a real entry vs a transient; `<C-o>` across a `:e` into a new buffer; count
  behavior at the ends. Validate against a real-Vim corpus before locking §4.1.
- **OQ-2** — The precise `NavMeta.is_jump` set (and `sets_mark::AutoBig` set for Emacs). Needs enumeration
  against `motion.txt` "jumps" and Emacs `push-mark` call sites; keep it data, not code.
- **OQ-3** — Changelist ↔ undo interaction: whether undo/redo inject changelist entries, and whether `g;`
  after undo behaves as Vim does. Finalize with the undo model (D-005 / editing-language.md).
- **OQ-4** — Selection-history depth and default bindings for Native Style (Helix `,`/keep vs Kakoune
  `Z`/`z` registers): a Ring, a CursoredList, or position-typed registers in `C-POSHIST`? Decide at NAT-5.
- **OQ-5** — Persisted-jumplist scope: viminfo persists a *global* jumplist while live jumplists are
  per-window; reconcile on session restore (which view inherits it).
- **OQ-6** — Whether to mint `INV-POSHIST` in the reference-invariants registry ("positions-history is
  session state outside the Document undo history; every entry is an anchor-backed Selection") or keep it
  expressed via existing invariants. Decide with `spec validate` maintainers (D-022), same as
  register-model OQ-1.

## Reference Invariants

INV-ANCHOR, INV-POS-TYPED, INV-HANDLE, INV-NO-GLOBAL-STATE, INV-DOC-VIEW, INV-CMD-SEMANTIC, INV-TXN,
INV-UNDO, INV-ORIGIN, INV-ASYNC-ORDER, INV-CAP-DEGRADE, INV-QUERY-SNAPSHOT, INV-PLUGIN-NO-CORE,
INV-CONTRACT-FIRST, INV-ADDITIVE, INV-TRUST-1 (see
[../invariants/reference-invariants.md](../invariants/reference-invariants.md)).
