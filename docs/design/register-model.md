---
doc: register-model
project: ruse
title: "ruse Unified Register / Kill-Ring Model"
summary: >
  Resolves DECISIONS D-026. One store — the Register Store — reproduces BOTH the Vim register set
  ("" "0 "1-"9 "- "a-"z/"A-"Z "_ "= "+/"* read-only) and the Emacs kill ring (bounded ordered ring,
  kill-ring-max, consecutive-kill coalescing, post-yank yank-pop) exactly. Each profile surface is a
  thin mapping over the shared store; the Vim numbered shift-ring and the Emacs kill ring are two
  policies/views over the same typed slots. Includes the superset data model, per-surface operation
  mapping tables, register-type paste geometry, the clipboard bridge (+/*/OSC-52), and the
  differential test corpus that proves both surfaces.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - ../parity/vim.md
  - ../parity/emacs.md
  - ../parity/common.md
  - ../parity/terminal.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Unified Register / Kill-Ring Model

Resolves **D-026** (open, BLOCKING for COM-11). Closes the "one store, two surfaces" question for
[VIM-REG](../parity/vim.md#vim-reg--registers--put), [EMACS-KILL](../parity/emacs.md#emacs-kill--kill-ring--yankyank-pop),
and [COM-11](../parity/common.md). Adds the `C-REGISTER` capability referenced by D-026.

## Problem

Vim registers and the Emacs kill ring look like the same concept ("copied text lives somewhere") but
differ *in kind*:

- Vim exposes ~40 **named addressable** slots (`""`, `"0`, `"1`–`"9`, `"-`, `"a`–`"z`, `"_`, `"=`,
  `"+`/`"*`, read-only `"%` `"#` `".` `":` `"/`). Each slot carries a **type** (charwise/linewise/blockwise)
  that governs paste geometry. Deletes shift a numbered *ring*; `"0` is yank-only; small deletes go to `"-`.
- Emacs exposes **no addressable slots**: one bounded ordered **kill ring** with head-of-ring semantics,
  **consecutive-kill coalescing**, and a transient **yank-pop** interaction valid *only* immediately after a
  yank. It is shared across buffers.

[COM-11](../parity/common.md) mandates a **single unified model** with an optional OS-clipboard bridge and
[OSC-52](../parity/terminal.md#term-osc52--clipboard-over-the-stream) fallback — each surface reproducing its
own semantics over the shared store. The naive failure (guarded by anti-pattern EMACS-5) is "kill ring = a
clipboard-history list"; the second failure is "two stores, sync them," which desynchronizes the moment a
Vim delete must also become the Emacs ring head. This doc specifies the superset store and proves both
surfaces are reproducible over it.

## Goals

- **G1** One authoritative `RegisterStore` (per workspace/session) — no second store, no sync layer.
- **G2** Reproduce every VIM-REG behavior **exactly**: numbered-ring shift on multi-line deletes, `"0`
  yank-only, `"-` small-delete, named `"a`–`"z` + append `"A`–`"Z`, blackhole `"_`, register-type paste,
  Visual-`p` swap.
- **G3** Reproduce every EMACS-KILL behavior **exactly**: bounded ordered ring (`kill-ring-max` default 120),
  consecutive-kill coalescing, `yank-pop` cycling valid only right after a yank, ring shared across buffers.
- **G4** The Emacs ring is a **view/policy** over the store's typed slots, not a parallel data structure.
- **G5** Clipboard bridge for `"+`/`"*` and Emacs `select-enable-clipboard`, with OSC-52 write-always /
  read-opt-in.
- **G6** A differential test corpus that both surfaces must pass.

## Non-goals

- Running Vimscript / Elisp (L3 non-goal, D-007). `"=` evaluates only enough of a ruse expression surface to
  yield text; general Vimscript eval is out (VIM-SCRIPT).
- Persistent cross-session registers (`viminfo` `A-Z 0-9`) — that is [VIM-STATE](../parity/vim.md#vim-state)
  serialization, tracked separately (Open questions OQ-4). This doc defines the *live* model.
- Emacs *registers* (`EMACS-EDIT-4`, position/rect/window-config slots) — a superset of text registers,
  deferred to D-027 / a follow-up (Open questions OQ-5). This doc covers text/kill only.
- Rectangles/blockwise **editing** grammar (D-025); this doc only stores the `Block` geometry and defines its
  paste, not how a blockwise range is produced.

## v0 scope — what ships now (the rest of this doc is the deferred target)

Everything below the Goals is the **full** unified model (all ~40 Vim slots, the numbered ring, the Emacs
kill ring, the clipboard bridge). Per RFC-0012 that superset is **deferred**; building it before the editor
is used daily is exactly the over-investment the pivot rejects. What **ships in v0** is the smallest slice
that gets the *hard-to-retrofit* semantics right:

- **One unnamed register** (`""`), held on the frontend `EditorState` — not the per-workspace store yet.
- **A charwise/linewise type** governing **paste geometry**. This is the load-bearing decision: charwise
  pastes insert inline next to the cursor; linewise pastes open a whole new line below (`p`) / above (`P`).
  Getting this type wrong later would mean re-recording every trace, so it is pinned and tested now.
- **Operations:** `y{motion}` / `yy` yank; `d` / `c` / `x` capture the removed span (Vim's unnamed-register
  fill); `p` / `P` paste. Linewise content is normalized to end with a newline so paste is uniform even for a
  last-line yank with no trailing newline.

**Deferred, and purely additive over the v0 `Register` type** (no rework of the above): named slots
(`"a`–`"z` / append `"A`–`"Z`), the numbered delete-ring (`"0`–`"9`, `"-`), the blackhole `"_`, the Emacs
kill ring + `yank-pop` + consecutive-kill coalescing, blockwise geometry, and the OS-clipboard / OSC-52
bridge. The store grows from "one typed slot" to "many addressable typed slots + policy views"; the type and
its paste geometry stay. Implementation: `crates/core/src/register.rs`, wired through `editor::plan`/`commit`.

## Terminology

See [spec glossary](../../spec/glossary.yaml) and [PROJECT.md]. New local terms:

- **Register Store** — the single owner of all copied/cut text for a workspace session (`C-REGISTER`).
- **Slot** — one addressable, typed entry: `{ id, RegisterType, content }`.
- **Numbered shift-ring** — the ordered slots `"1`…`"9` with Vim's shift-on-write discipline.
- **Kill-ring view** — an ordered index over recent yank/kill slots giving Emacs head-of-ring + yank-pop.
- **Coalescing window** — the transient state that merges consecutive Emacs kills into the ring head.
- **Yank-pop state** — the transient state (valid only immediately after a yank) driving `M-y`.

## Invariants

This doc depends on and is governed by:

- **INV-NO-GLOBAL-STATE** — the Register Store is a component that **owns** its state and is reached by a
  typed handle; it is *not* an `Arc<Mutex<GLOBAL>>`. "Shared across buffers" means one owner reachable by
  many views, not ambient global mutable state.
- **INV-HANDLE** — slots are referenced by a generational `SlotId`, never raw indices; a stale `SlotId`
  (evicted by ring bound) resolves to *absent*, an expected typed outcome, not a panic.
- **INV-TXN / INV-UNDO** — a *paste* mutates a Document and therefore goes through a Transaction; a *yank/kill*
  reads the Document and writes the Register Store, which is session state, **not** Document text, so it is
  **not** itself an undoable Document transaction (undoing a paste removes inserted text; it does not "un-kill"
  the ring). The store is explicitly outside the Document undo history.
- **INV-ORIGIN** — every write to the store records its origin (UserInput | Macro | Plugin | Lsp | AiAgent |
  RemotePeer); a macro replaying `dd` shifts the ring identically to interactive `dd`.
- **INV-CMD-SEMANTIC** — surface operations (`vim.delete`, `emacs.kill-region`, …) are semantic commands with
  typed args (target register, register-type intent); keymaps resolve onto them. `"a`/`M-y` are *arguments*,
  not distinct commands.
- **INV-CAP-DEGRADE** — the clipboard bridge is a capability on the confidence ledger; OSC-52 read absent
  degrades (register still works via internal store), never disappears.
- **INV-ASYNC-ORDER** — clipboard bridge I/O (OSC-52 round-trips) is async; responses carry a request id +
  revision and stale results are dropped; the store's own writes are synchronous on the deterministic executor.

No new `INV-*` is minted here (per the reference-invariants single-registry rule). See Open questions OQ-1
for whether `INV-REGISTER` should be added to the registry.

## Proposed design

### 1. The superset data model (typed slots + rings + special slots)

One store. Every unit of copied text is a **typed slot**. All surface differences are *addressing policy*
and *transient interaction state* over these slots.

```rust
/// Geometry that governs paste — the load-bearing VIM-REG-TYPE / EMACS rectangle bit.
enum RegisterType {
    Char,   // characterwise: inserted inline at the cursor
    Line,   // linewise: inserted on its own new line(s)
    Block,  // blockwise / rectangle: inserted as a column, one fragment per row
}

/// The atomic stored unit. Immutable once sealed (append builds a new content).
struct Slot {
    id:      SlotId,        // generational handle (INV-HANDLE)
    ty:      RegisterType,
    content: Content,       // see below; not a bare String
    origin:  Origin,        // INV-ORIGIN
    // width metadata for Block paste; grapheme-aware (TERM-WIDTH). None for Char/Line.
    block_widths: Option<Vec<usize>>,
}

/// Content keeps line structure explicit so Line/Block paste never re-guesses newlines.
struct Content {
    text: Rope,             // the bytes; typed positions per INV-POS-TYPED
    // For Char: one logical run (may still contain embedded '\n' from e.g. a charwise multi-line motion).
    // For Line: N lines, each conceptually newline-terminated.
    // For Block: N fragments (rows) split at newlines; each is one column cell-run.
}

/// The whole store, owned by one component, reached by handle (INV-NO-GLOBAL-STATE).
struct RegisterStore {
    // --- Vim-addressable structure ---
    unnamed:  SlotId,                 // ""  — the "last touched" pointer (see §1.1)
    yank0:    Option<SlotId>,         // "0  — last YANK only
    numbered: [Option<SlotId>; 9],    // "1.."9 — the delete/change SHIFT-ring (index 0 == "1)
    small_delete: Option<SlotId>,     // "-  — last delete of < 1 line
    named:    HashMap<char, SlotId>,  // "a.."z  ('A'..'Z' are append-writes into the same lowercase key)

    // --- Special slots (behavioral, not stored as normal slots) ---
    // "_  blackhole: writes are dropped, reads yield empty. No SlotId ever allocated.
    // "=  expression: computed on read from the expression surface; not a stored slot.
    // "+  system clipboard, "* primary selection: bridged slots (§5), lazily read/written.
    // "%  "#  "."  ":"  "/" : read-only projections of editor state (filename/alt/insert/cmd/search).

    // --- Emacs kill-ring VIEW/policy over the SAME slots (§2) ---
    kill_ring: KillRingView,

    max_kill: usize,                  // kill-ring-max, default 120 (bounds the ring VIEW, §2.3)
    arena:    SlotMap<SlotId, Slot>,  // backing storage; eviction is by ring bound + GC of unreferenced
}
```

**Every produced text lands in `arena` as a `Slot`.** Vim addressing (`unnamed`, `yank0`, `numbered`,
`small_delete`, `named`) and the Emacs `kill_ring` are all just **pointers (`SlotId`) into the same arena**.
A single `dd` produces one `Slot` that is simultaneously pointed at by `unnamed`, `numbered[0]` ("1), and
pushed as `kill_ring` head — no copying, no sync.

#### 1.1 `""` unnamed is a pointer, not storage

The unnamed register is an **alias pointer** to "the slot the last delete/change/yank wrote" (VIM-REG-1).
Writing `"a` also repoints `unnamed` at that slot (unless the write was to the blackhole `"_`). Reading `""`
dereferences the pointer. This is what makes `"add` then `p` paste the just-deleted text: both `"a` and `""`
point at the same `Slot`.

#### 1.2 Special slots are behaviors, not entries

| Register | Behavior |
| --- | --- |
| `"_` blackhole | Write: content discarded, **no** `Slot` allocated, `unnamed` **not** repointed, numbered ring **not** shifted, kill ring **not** touched. Read: empty. |
| `"=` expression | Read: evaluate ruse expression surface → ephemeral `Slot` (type = `Char`, or `Line` if result ends in `\n`). Never stored. |
| `"+` / `"*` | Bridged (§5). Read pulls from clipboard/primary into an ephemeral `Slot`; write pushes to it (and mirrors to internal store per config). |
| `"%` `"#` `".` `":` `"/` | Read-only projections computed from editor state on read. Writes are an `ErrorCode::ReadOnlyRegister` (INV-ERR-CLASS), not a panic. |

### 2. The Emacs kill ring as a VIEW/policy over the same store

The kill ring is **not** a second container. It is an **ordered list of `SlotId` pointing into the same
arena**, plus two pieces of transient interaction state.

```rust
struct KillRingView {
    order: VecDeque<SlotId>,   // most-recent at FRONT; bounded to max_kill (§2.3)
    // Transient interaction state — see §2.1, §2.2. Both reset by any non-participating command.
    coalesce: Option<CoalesceState>,  // active during a run of consecutive kills
    yankpop:  Option<YankPopState>,   // active only immediately after a yank
}

struct CoalesceState {
    head: SlotId,      // the slot consecutive kills are merging INTO
    edge: KillEdge,    // whether the next kill appends (forward) or prepends (backward)
    last_cmd_kind: KillCmdKind,
}
enum KillEdge { Front, Back }   // C-k / C-d-ish kills append; backward-kill-word prepends.

struct YankPopState {
    // Where the just-yanked text was inserted, so M-y can REPLACE it in the Document.
    inserted_range: AnchorRange,  // INV-ANCHOR
    ring_index: usize,            // current position in `order`; M-y advances it
}
```

#### 2.1 Consecutive-kill coalescing (EMACS-KILL-3)

A "kill" command (`C-w`, `M-w` is a *copy* — see note, `C-k`, `M-d`, `backward-kill-word`, …) checks whether
the **immediately preceding command was also a kill** (`last_cmd_kind` still set — nothing else ran in
between). If so, it does **not** push a new ring entry: it **merges** its text into the current head `Slot`
(building a new sealed `Content`) at the `edge` implied by direction — forward kills append, backward kills
prepend. If not consecutive, it pushes a fresh entry to `order.front()` and starts a new `CoalesceState`.

> `M-w` (copy-region-as-kill) pushes a fresh entry and, in Emacs, *does* set `this-command` as a kill for the
> purpose of a *following* kill coalescing onto it. We model that with `CoalesceState` set but `edge = Back`
> default; a copy never merges into a *prior* entry.

#### 2.2 Yank-pop, valid only immediately after a yank (EMACS-KILL-2)

`C-y` (yank) inserts `order[0]` at point and records `YankPopState { inserted_range, ring_index: 0 }`.
`M-y` (yank-pop) is **only valid while `yankpop` is `Some`** — i.e. the last command was a yank or yank-pop.
It: (a) deletes `inserted_range` from the Document, (b) advances `ring_index` (wrapping), (c) inserts
`order[ring_index]`, (d) updates `inserted_range`. Any command that is not yank/yank-pop clears `yankpop`;
a later `M-y` then errors (`ErrorCode::YankPopNotAfterYank`) exactly like Emacs's "Previous command was not a
yank".

Both delete+insert of a `M-y` go through **one Document Transaction** (INV-TXN) so a single `u`/`C-/` undoes
the whole pop.

#### 2.3 How coalescing + yank-pop coexist with Vim's numbered-ring shifting over ONE store

This is the crux of D-026. Two different "rings" (Vim numbered `"1`–`"9`, Emacs kill `order`) live over the
same slots without contradiction because **they are independent indices with independent push rules, both
fed by the same slot-producing events**:

| Event (any surface) | Produces | Vim indexing reaction | Emacs indexing reaction |
| --- | --- | --- | --- |
| **Yank / copy** (≥0 lines) | new `Slot` | set `yank0`, set `unnamed`; **numbered ring NOT shifted** | push new head to `order`; start/refresh `CoalesceState` |
| **Delete/change ≥ 1 line** | new `Slot` | **shift** `"1`→`"2`…`"8`→`"9` (drop old "9), put new in `"1`; set `unnamed` | if consecutive-kill → **coalesce into head**; else push new head |
| **Delete/change < 1 line (small)** | new `Slot` | write `"-`; set `unnamed`; **numbered ring NOT shifted** | same coalesce/push rule as above |
| **Blackhole `"_`** | nothing | no change | no change |

Key reconciliations:

- **Vim never coalesces; Emacs coalescing never shifts the numbered ring.** When an Emacs `C-w` coalesces
  into the head slot, the numbered ring's `"1` pointer is *not* moved to the merged slot — Vim's shift only
  fires for a delete that produced a *distinct new ≥1-line slot*. A coalesced kill re-seals the *same* head
  `Slot`'s `Content`; `"1` still points at whatever the last full-line Vim delete produced. In practice a user
  rarely mixes surfaces mid-run; but the model is total: coalescing is an Emacs-view mutation of head content,
  Vim shifting is a numbered-index push, and they read the same arena without one corrupting the other.
- **`yank0` is untouched by any delete** — Vim `"0` is yank-only, so a delete never writes `yank0`; the Emacs
  kill of the same event pushes to `order` regardless. `"0` and `order[0]` therefore legitimately differ after
  a delete (that is correct: Vim `"0` holds the last *yank*, Emacs head holds the last *kill*).
- **`unnamed` follows the most recent write of any kind**, matching Vim; the Emacs head follows kill/yank
  pushes. After a yank they coincide; after a delete `unnamed` = the deleted slot and Emacs head = same slot,
  but Vim `"0` still = the earlier yank.

### 3. Per-surface mapping tables (the parity proof)

#### 3.1 Vim surface (VIM-REG-*)

Notation: *touched* register(s) written; *ring* = numbered shift behavior. "small" = `< 1` line.

| Operation | Slot produced | Writes | Numbered ring | `unnamed` `""` | `"0` |
| --- | --- | --- | --- | --- | --- |
| `yy` / `Y` (linewise yank) | `Line` | `"0` | unchanged | → slot | → slot |
| `yw` (charwise yank) | `Char` | `"0` | unchanged | → slot | → slot |
| `dd` (linewise delete, ≥1 line) | `Line` | `"1` | **shift 1→2…8→9**, new in `"1` | → slot | unchanged |
| `3dd` (multi-line) | `Line` (3 lines) | `"1` | **shift**, new in `"1` | → slot | unchanged |
| `dw` when spanning ≥1 line | `Char`/`Line` | `"1` | **shift** | → slot | unchanged |
| `x` / `dl` small-delete (<1 line) | `Char` | `"-` | unchanged | → slot | unchanged |
| `daw` small (<1 line) | `Char` | `"-` | unchanged | → slot | unchanged |
| `"ayy` | `Line` | `"a` (replace) + mirror `"0`? **no** | unchanged | → slot | unchanged¹ |
| `"Ayy` (append) | `Line` | `"a` **appended** (new sealed content = old ⧺ new) | unchanged | → slot | unchanged |
| `"add` | `Line` | `"a` (replace) | unchanged² | → slot | unchanged |
| `"_dd` (blackhole) | none | nothing | unchanged | unchanged | unchanged |
| `p` / `P` | reads `""` | — | — | — | — |
| `"ap` | reads `"a` | — | — | — | — |
| Visual `p` (paste over selection) | see §3.3 | swaps | — | replaced text → `""` | — |

¹ Explicit named yank `"ayy` sets `"a` and `""`; Vim also still sets `"0` on *any* yank. So `"ayy` writes
`"a`, `"0`, and `""` all to the slot. (Corrected: named **yank** updates `"0`; named **delete** does not.)
² A named **delete** `"add` writes `"a` and `""` but Vim does **not** shift the numbered ring when an explicit
register is named for a delete? — It **does** still fill `"1` per Vim (a named delete writes both `"a` and the
numbered ring). Model: a delete always feeds the numbered ring unless target is `"_`; a named register is an
*additional* destination. (See differential test T-08.)

Read side (`p`/`P`/`gp`/`gP`/`]p`/`[p`) dereferences the addressed slot and pastes per its `RegisterType`
(§4).

#### 3.2 Emacs surface (EMACS-KILL-*)

| Operation | Reads/produces | Ring `order` | Coalesce | Yank-pop state |
| --- | --- | --- | --- | --- |
| `M-w` copy-region-as-kill | produce `Char`/`Line`* | push new head | set (copy; edge Back) | clear |
| `C-w` kill-region | produce | if prev=kill → **coalesce into head**; else push | set/refresh | clear |
| `C-k` kill-line | produce | consecutive `C-k` → **append-coalesce** (edge Front) | set/refresh | clear |
| `M-d` kill-word (fwd) | produce | consecutive → append-coalesce | set/refresh | clear |
| `backward-kill-word` | produce | consecutive → **prepend-coalesce** (edge Back) | set/refresh | clear |
| `C-y` yank | read `order[0]`, insert | unchanged | clear | **set** (index 0, inserted_range) |
| `M-y` yank-pop | replace inserted with `order[++i]` | unchanged | clear | **advance** (only if was set, else error) |

*Region type: an Emacs kill/copy is `Char` unless the region is a whole-line span, in which case ruse tags it
`Line` so a Vim `p` of an Emacs kill pastes linewise (cross-surface consistency). Rectangle kills
(`C-x r k`) produce `Block`. This is a ruse enrichment; pure Emacs has no type but pastes literally, which
`Char` reproduces.

**Provable reproducibility:** every Emacs op above only ever (a) pushes/merges a `SlotId` in `order`, or (b)
reads `order[i]`. Every Vim op only ever manipulates `unnamed`/`yank0`/`numbered`/`small_delete`/`named`
pointers and reads them. Both operate on the **same `arena`**. Neither writes the other's index. Therefore the
two surfaces are independent reproducible policies over one store (G4). The only shared, deliberately-coupled
pointers are the slot contents themselves and `unnamed`, whose "last write wins" rule matches both surfaces'
notion of "the thing you just cut/copied."

#### 3.3 Visual-`p` swap (VIM-REG-RING)

Visual-mode `p` (or `P`) over a selection: (1) capture the selection's text + its type into a **new `Slot`**;
(2) paste the source register's content over the selection in one Transaction; (3) set `unnamed` `""` to the
captured (replaced) slot. Vim's exact quirk: after Visual-`p`, `""` holds the *replaced* text, so a following
`p` pastes what was just overwritten. The register you pasted *from* is unchanged unless it was `""` itself
(then the replaced text lands in `""`, which is the documented Vim behavior we reproduce). Test T-06.

### 4. Register TYPE → paste geometry

`RegisterType` is stored on the `Slot` at produce-time and **fully determines** paste geometry at read-time.
The paster never re-inspects the text to guess.

| Type | `p` (after) | `P` (before) | Cursor after |
| --- | --- | --- | --- |
| `Char` | insert inline after cursor char | insert inline before cursor char | on last inserted char |
| `Line` | open new line(s) **below** current, insert content as whole lines | new line(s) **above** | first non-blank of first pasted line (`gp`/`gP` = line after) |
| `Block` | insert as a **column**: fragment *k* goes into row *k* at the paste column, padding short rows with spaces to reach the column | column starting at cursor column | top-left of block |

- **Linewise** never splits the current line; it always lands on its own physical lines (matches `dd`+`p`).
- **Blockwise** uses `block_widths` (grapheme-cell widths, TERM-WIDTH-aware) to keep the column rectangular;
  rows shorter than the paste column are space-padded; `$`-blocks (ragged right) skip padding.
- `]p`/`[p` = linewise paste with **reindent** to current line; only valid for `Line` (else falls back to `p`).
- A `Char` slot whose text contains embedded `\n` (from a charwise multi-line motion) pastes inline and wraps
  onto real lines — distinct from `Line`, which owns whole lines. This distinction is a required test (T-05).

### 5. Clipboard bridge (`"+`/`"*`, Emacs bridge, OSC-52)

The bridge is a **capability-gated I/O adapter**, not part of the core ring. It has three backends resolved
via the capability ledger (INV-CAP-DEGRADE): native OS clipboard, X11/Wayland primary selection, and OSC-52
over the terminal stream ([TERM-OSC52](../parity/terminal.md#term-osc52--clipboard-over-the-stream)).

```rust
struct ClipboardBridge {
    clipboard: ClipboardSink,   // "+  ⇄ system clipboard
    primary:   Option<ClipboardSink>, // "* ⇄ primary selection (X11/Wayland; else None → falls back to "+)
    osc52_read_enabled: bool,   // default FALSE (security)
}
```

Behavior:

- **Write** (`"+y`, `"*y`, or `clipboard=unnamed`/`unnamedplus`; Emacs `select-enable-clipboard`): push the
  produced slot's text to the sink. OSC-52 **write is always attempted** (works over SSH/tmux where OS APIs
  don't) with tmux passthrough wrapping (TERM-PROBE-3); on OS-native availability, prefer it and use OSC-52 as
  fallback. Writes carry the `RegisterType` only *internally*; the OS clipboard is plain text (types are lost
  crossing the boundary — a read from `"+` yields `Char`, or `Line` iff the payload ends in `\n`).
- **Read** (`"+p`, `M-y` when clipboard is a ring source, `clipboard=unnamedplus` yank source): read from OS
  API when available. **OSC-52 read is opt-in** (`osc52_read_enabled`, default off) and requires explicit
  user confirmation per SEC-7 — silent OSC-52 read is a clipboard-exfiltration vector. If read is unavailable,
  the register degrades to the last value the editor itself wrote (INV-CAP-DEGRADE) — it never errors the paste.
- **Emacs `interprogram-cut/paste`**: modeled as the same bridge — a yank optionally seeds the ring head from
  the clipboard if it changed since last check; a kill optionally writes the clipboard. This is the Emacs
  clipboard bridge expressed over the identical `ClipboardBridge`.
- **Async ordering:** OSC-52 read is a stream round-trip; its response carries a request id + revision and a
  stale reply is dropped (INV-ASYNC-ORDER). The paste that requested it either waits on the deterministic
  executor or uses the degraded value if the ledger says read is unsupported.

### 6. `C-REGISTER` capability

D-026 asks to "Add `C-REGISTER`." It is the versioned contract exposing register operations to
commands/plugins (INV-CONTRACT-FIRST): `read(RegisterAddr) -> Option<Slot-snapshot>`,
`write(RegisterAddr, RegisterType, text, origin)`, `paste(target_view, RegisterAddr, placement)`,
`kill_ring_push/coalesce`, `yank / yank_pop`, `clipboard_sync`. Plugins receive **snapshots** (INV-QUERY-SNAPSHOT,
INV-PLUGIN-NO-CORE), never the live `arena`. `RegisterAddr` is a typed enum
(`Unnamed | Yank0 | Numbered(1..=9) | SmallDelete | Named(char) | Blackhole | Expression | Clipboard | Primary | ReadOnly(kind)`).

## Failure modes

- **Read-only register write** (`"%` etc.) → `ErrorCode::ReadOnlyRegister` (typed error, INV-ERR-CLASS), no
  state change.
- **`M-y` not after yank** → `ErrorCode::YankPopNotAfterYank`; ring untouched.
- **Stale `SlotId`** (evicted by `max_kill` bound) → resolves to *absent*; paste of an absent register is a
  no-op with a status note, not a panic (INV-HANDLE).
- **`"=` evaluation error** → typed error surfaced to the command line; no slot produced.
- **Clipboard backend unavailable** → degrade to internal store value (INV-CAP-DEGRADE); status ledger notes
  "clipboard read unsupported."
- **OSC-52 read without opt-in** → treated as unsupported (deny by default), not an error.

## Recovery behavior

The Register Store is **session state**, not Document state: it is *not* in the undo history and *not* in the
save/recovery journal (D-005) by default. On crash/recovery the store is empty (or restored only if VIM-STATE
persistence is enabled, OQ-4). Undoing a paste removes inserted Document text via the normal Transaction undo
(INV-UNDO); it does not restore or alter ring/register pointers (matches both Vim and Emacs: undo does not
"un-kill").

## Security impact

- **OSC-52 read** is the primary risk (silent clipboard exfiltration by hostile terminal output / remote
  peer). Default-off, explicit opt-in + confirmation (SEC-7, TERM-OSC52-2).
- **Bracketed-paste payloads** entering a register are stored as inert text; escape sequences are neutralized
  at input (TERM-PASTE-2, SEC-5) *before* reaching the store, so a later `p` cannot re-inject control
  sequences into the terminal beyond normal rendered text.
- **Origin tagging** (INV-ORIGIN): AI/RemotePeer/Plugin writes to the store are attributable; an AI-agent kill
  is reviewable (SEC-15) before its paste is applied.

## Performance impact

- Slots are reference-counted `SlotId`s into one arena; `dd` allocates one `Content`, pointed at by up to
  three indices — no text copy for multi-indexing.
- Numbered-ring "shift" is a pointer rotation over `[Option<SlotId>; 9]`, O(9). Kill-ring push is a `VecDeque`
  front-push with bounded eviction, O(1) amortized.
- Coalescing seals a **new** `Content` (old ⧺ new) — O(len) in the merged text; consecutive kills are the only
  copying path and are bounded by the run length, matching Emacs.
- Eviction: entries drop off `order` past `max_kill`; a `Slot` unreferenced by any Vim index *and* off the
  ring is GC'd. Vim-addressable slots (`"a`–`"z`, `"1`–`"9`, `"0`, `"-`) are retained regardless of ring bound.

## Compatibility impact

- Satisfies COM-11 (unified model) and unblocks Vim (F-003) / Emacs (F-012) register parity at L2.
- `RegisterType` and the numbered-ring discipline are load-bearing for the VIM-REG "get-it-exactly-right"
  checklist item #3.
- `C-REGISTER` is additive to the command contract set (INV-ADDITIVE); command IDs are namespaced (D-006).

## Observability

- Store operations emit typed events `{op, RegisterAddr, RegisterType, origin, revision}` for the event model
  and for macro/AI review.
- A `:registers` / `describe-registers` view renders slot id, type, and a content preview (EMACS-BUFFER-2
  style buffer). The kill ring is inspectable as an ordered list with the coalesce/yank-pop transient state
  shown when active.

## Alternatives

- **A1 — Two indices, one arena (chosen).** Vim addressing and the Emacs ring are independent pointer sets
  over one `Slot` arena. Chosen because it makes both surfaces total and provably non-interfering (§2.3, §3.2)
  while sharing storage.
- **A2 — Emacs ring *is* the Vim numbered ring.** Reuse `"1`–`"9` as the kill ring. Rejected: Emacs ring is
  120-deep and includes yanks/copies and coalesced entries; Vim numbered ring is 9-deep, deletes-only, no
  coalescing, and `"0` is yank-only. Forcing one structure to be both breaks `"0`-untouched-by-delete and the
  120-entry bound simultaneously.

## Rejected approaches

- **R1 — Kill ring simplified to a clipboard history list.** A flat "recent clipboard" list drops
  coalescing, drops the post-yank-only validity of `yank-pop`, and drops register *types*. Explicitly the
  EMACS-5 anti-pattern ("kill ring ≠ clipboard"). Rejected.
- **R2 — Separate Vim and Emacs stores with a sync layer.** Two stores + a syncer desynchronizes the instant a
  Vim delete must be the Emacs ring head, or a coalesced kill must be visible to `p`. Sync ordering becomes a
  race (fights INV-ASYNC-ORDER) and doubles the eviction/GC logic. Rejected in favor of one store (G1).
- **R3 — Store text untyped and infer geometry at paste.** Guessing linewise-vs-charwise from trailing `\n`
  loses the `Char`-with-embedded-newline vs `Line` distinction (T-05) and cannot represent `Block` at all.
  Rejected: type is produced-at-write metadata (VIM-REG-TYPE).
- **R4 — Make yank/kill undoable Document transactions.** Would let `u` "un-kill." Contradicts both Vim and
  Emacs and pollutes the Document undo tree with non-Document state. Rejected: the store is session state
  outside INV-TXN/INV-UNDO (see Invariants).

## Migration strategy

Greenfield (no prior register impl). Land the `RegisterStore` + `C-REGISTER` contract behind the input engine
(F-003) before either surface reaches L2. The Vim surface mapping (§3.1) and Emacs surface mapping (§3.2) are
implemented as command handlers over the same store; the differential corpus (below) gates the merge.

## Test strategy

Differential tests (TEST-2 corpus, required to pass). Each asserts store state and/or resulting buffer.

- **T-01 numbered-ring shift after multi-line deletes.** `dd` on lines A,B,C in turn → `"1`=C, `"2`=B, `"3`=A;
  a 4th `dd` (D) → `"1`=D,`"2`=C,`"3`=B,`"4`=A. Confirm `"9` drops off after 9 deletes.
- **T-02 `"0` untouched by deletes.** `yyjdd` → `"0` = the yanked line, `"1` = the deleted line, `""` = deleted
  line. `p` uses `""` (deleted); `"0p` pastes the yank.
- **T-03 `"-` small-delete.** `x` / `diw` on a single-line span → `"-` set, numbered ring unchanged, `"0`
  unchanged. A following `dd` shifts numbered ring but leaves `"-` intact.
- **T-04 yank-pop cycle.** Kill "one","two","three" (non-consecutive) → ring = [three,two,one]. `C-y` inserts
  "three"; `M-y` → "two"; `M-y` → "one"; `M-y` → wraps to "three". A non-yank command then `M-y` →
  `YankPopNotAfterYank`.
- **T-04b coalescing.** Three consecutive `C-k` at line start → **one** ring entry = the whole line (appended
  in order), not three entries. A cursor move between kills breaks the run → separate entries.
- **T-05 register-type paste.** (a) `Line` slot `p` opens a new line; (b) `Char` slot containing `"a\nb"` `p`
  inserts inline/wraps without opening a fresh owned line; (c) `Block` slot `p` inserts a padded column across
  rows. Assert exact buffer geometry for each.
- **T-06 Visual-`p` swap.** Select "foo", `p` with `""`="bar" → buffer has "bar", and `""` now = "foo"; a
  following `p` pastes "foo".
- **T-07 cross-surface head coherence.** Emacs `C-w` of a whole line, then Vim `p` → pastes linewise (type
  carried); Vim `dd`, then Emacs `C-y` → yanks the deleted line (shared arena / `unnamed`↔ring head coherence),
  while Vim `"0` is unchanged from any earlier yank.
- **T-08 named delete feeds numbered ring.** `"add` then another `dd` → `"a` = first line, `"1` = second
  line, `"2` = first line (named delete also shifted the ring). `"ayy` sets `"a`, `"0`, and `""`.
- **T-09 blackhole isolation.** `"_dd` leaves `""`, `"0`, `"1`, `"-`, and the kill ring all unchanged.
- **T-10 append register.** `"ayy` then `"Ayy` → `"a` holds both lines concatenated in order, type `Line`.
- **T-11 clipboard bridge + OSC-52 read denial.** `"+y` writes OS clipboard (and attempts OSC-52 write);
  `"+p` with OSC-52 read disabled and no OS API degrades to last internally-written value, no error; enabling
  read requires explicit confirmation.

Property tests (against invariants): random interleavings of Vim/Emacs ops never (a) panic, (b) leave a
dangling `SlotId` that resolves to a wrong slot (INV-HANDLE), or (c) mutate the numbered ring on a yank / the
`yank0` slot on a delete.

## Open questions

- **OQ-1** — Should `INV-REGISTER` (or `INV-CLIPBOARD`) be minted in the reference-invariants registry to
  formalize "register store is session state outside the Document undo history / shared by handle"? Currently
  expressed via existing invariants. Decide with `spec validate` maintainers (D-022).
- **OQ-2** — Exact set of ruse ops that count as a "kill" for coalescing (`C-k`, `M-d`, `C-w`, zap, …) and the
  precise `M-w`-then-kill coalescing rule; needs validation against real Emacs edge cases.
- **OQ-3** — `Block` paste padding for ragged `$`-blocks and mixed-width (CJK/emoji) columns — interaction
  with TERM-WIDTH ambiguous-width handling; finalize with the width model (TERM-WIDTH-2).
- **OQ-4** — Persistence: which slots survive a session (Vim `viminfo` persists `"0`–`"9`, `"a`–`"z`,
  uppercase). Tie to VIM-STATE + recovery journal (D-005); default is non-persistent here.
- **OQ-5** — Emacs *registers* (EMACS-EDIT-4: positions, rectangles, window configs) as a superset of this
  text store, and overlap with the positions-history model (D-027).
- **OQ-6** — Whether `clipboard=unnamed`/`unnamedplus` default should route all yanks through the bridge for
  the Vim profile out of the box (usability vs surprise / exfiltration on paste).

## Reference Invariants

INV-NO-GLOBAL-STATE, INV-HANDLE, INV-TXN, INV-UNDO, INV-ORIGIN, INV-CMD-SEMANTIC, INV-CAP-DEGRADE,
INV-ASYNC-ORDER, INV-POS-TYPED, INV-ANCHOR, INV-ERR-CLASS, INV-QUERY-SNAPSHOT, INV-PLUGIN-NO-CORE,
INV-CONTRACT-FIRST, INV-ADDITIVE (see
[../invariants/reference-invariants.md](../invariants/reference-invariants.md)).
