---
doc: view-window-workspace
project: ruse
title: "ruse View / Window / Layout & Workspace/Session Model (C-VIEW, C-WORKSPACE)"
summary: >
  Product-ready design for the presentation and container tier: the Buffer/View/Window typed-handle
  model, view-local state (cursor/selection/viewport/folds) that never lives in the Document
  (INV-DOC-VIEW), the layout TREE (h/v splits, tabs, focus, resize, sizing constraints) with a
  stale-layout epoch guard, the strict separation of layout lifecycle from view and document lifecycle
  (leak prevention when a window closes), the same buffer open in many views with independent cursors,
  buffer KINDS and the interactive-view write-back contract, narrowing as a DOCUMENT-level restriction
  (V-27) reconciled with INV-DOC-VIEW, the overlay/float/modal/popup layer and how modal input survives
  it, and the Workspace/Session model — listed/hidden/unlisted buffers, multi-root, session save/restore
  (mksession/shada-like), and the workspace↔machine settings scope. Status is a rendered Health Registry
  (INV-STATUS). Renders through the single Render Tree (INV-RENDER-IR); it does not paint cells.
audience: [maintainers, contributors, llm-agents, implementers-in-any-language]
status: draft
related:
  - ../architecture/architecture.md          # §7 UI/workspace, §3 state ownership
  - ../rfc/proposed/RFC-0008-document-model.md  # Document/View/handle/kind noun (INV-DOC-VIEW, D-003)
  - ../rfc/proposed/RFC-0002-workspace-architecture.md # layer model + interactive write-back
  - ../parity/workspace.md                    # WS-* surfaces, virtual-doc kinds, V-14 write-back
  - ../parity/vim.md                          # VIM-WIN windows/tabs/buffers, hidden
  - ../parity/emacs.md                        # EMACS-BUFFER, EMACS-EDIT-2 narrowing (V-27)
  - persistence-and-recovery.md               # session / journal ties (D-005)
  - positions-history.md                      # marks / global-mark ring / jumplist (per-view vs per-doc)
  - stability-and-observability.md            # §11 Health Registry, §13 preflight
  - render-and-frontends.md                   # Render Tree lowering, per-client-view profile
  - ../invariants/reference-invariants.md
---

# ruse View / Window / Layout & Workspace/Session Model

> This doc specifies **C-VIEW** (View/Window/layout, `depends_on: C-RENDER, C-ANCHOR`) and
> **C-WORKSPACE** (Workspace + buffers, `depends_on: C-DOCUMENT, C-VIEW`) as declared in
> [spec/PRD.yaml](../../spec/PRD.yaml). It is the *container* tier that sits above the Document noun
> ([RFC-0008](../rfc/proposed/RFC-0008-document-model.md)) and inside the workspace layer model
> ([RFC-0002](../rfc/proposed/RFC-0002-workspace-architecture.md)). Those two RFCs fix *what a Document
> and a buffer-kind are*; this doc fixes *how Views are laid out, focused, resized, torn down, and
> persisted*, and how the Workspace owns the buffer list. It cites the Document/kind/handle/anchor model
> by ID and does not re-derive it.

Stable IDs introduced here (others may cite): **VW-HANDLE**, **VW-VIEWSTATE**, **VW-LAYOUT**, **VW-LIFE**,
**VW-MULTI**, **VW-NARROW**, **VW-OVERLAY**, **VW-SESSION**, **VW-SETTINGS**. No new `INV-*` is minted
(registry rule; [reference-invariants.md](../invariants/reference-invariants.md), D-022).

## Problem

Neovim and Vim entangle *buffer text*, *window/cursor state*, *tab layout*, and *file identity* in one
apparatus ([architecture.md §0.2](../architecture/architecture.md)). The consequences are the recurring
bugs ruse exists to prevent: a cursor stored on the buffer so "the same file in two splits" shares one
cursor; a window that, when closed, keeps its buffer and its background LSP/parse tasks alive (a leak);
a resize event that lands mid-layout-change and paints a **stale layout**; "narrowing" that is
alternately a per-window or per-buffer concept nobody can predict; a floating completion popup that
*eats* an in-flight operator-pending state; and a session file (`:mksession` / `shada`) that either
saves nothing useful or saves view-local ephemera it cannot faithfully restore.

The [architecture](../architecture/architecture.md#7) principle — **buffer ≠ view ≠ window ≠ file**,
"everything is a workspace view/buffer" — and [INV-DOC-VIEW](../invariants/reference-invariants.md) fix
the *nouns*. What is still unspecified, and what this doc closes, is the **layout tree and its lifecycle**:
how Windows tile, split, focus, resize, and — critically — how closing a Window is decoupled from
retiring a View is decoupled from unloading a Document, so nothing leaks and nothing is lost. It also
closes **narrowing ownership** (V-27), the **overlay/modal layer** vs the tiling base, and the
**Workspace/Session** container (buffer list, hidden buffers, multi-root, save/restore, settings scope).

## Goals

- **G1** A three-noun handle model — `DocumentId` / `ViewId` / `WindowId` — with view-local state
  (cursor, selection(s), viewport, fold state, mode-view bits) owned **only** by the View, never the
  Document (INV-DOC-VIEW). *(VW-HANDLE, VW-VIEWSTATE)*
- **G2** A concrete **layout tree**: h/v splits, tab pages as *window layouts* (VIM-WIN-1), focus, a
  deterministic **resize/sizing** algorithm with per-window constraints, and the full `C-w` / `:split` /
  `gt` surface expressed over it. *(VW-LAYOUT)*
- **G3** **Layout lifecycle ≠ view lifecycle ≠ document lifecycle.** Closing a Window detaches a View;
  retiring a View releases its handles/anchors and **cancels its scheduler tasks**; a Document is
  unloaded only when nothing (no view, no listed-buffer entry) references it. No orphaned tasks, no
  dead-document leaks. *(VW-LIFE)*
- **G4** The **same buffer in N views** with independent cursors/viewports/folds, sharing exactly one
  revision stream and one anchor store; an explicit *cloned/linked* variant where views may or may not
  share view-local facts. *(VW-MULTI)*
- **G5** Buffer **kinds** and the **interactive-view write-back** contract from the View side (edits →
  typed domain CommandRequests, preflighted) — binding [RFC-0002](../rfc/proposed/RFC-0002-workspace-architecture.md)
  §write-back to the layout engine.
- **G6** **Narrowing as a Document-level restriction** (V-27) that confines *all* ops, reconciled with
  INV-DOC-VIEW, plus independent narrowing via an *indirect document*. *(VW-NARROW)*
- **G7** An **overlay/float/modal/popup** layer distinct from the tiling base, with a z-order and a rule
  that **modal input state survives** overlay open/close (operator-pending, prefix, minibuffer). *(VW-OVERLAY)*
- **G8** The **Workspace/Session** model: listed/unlisted/hidden buffers, multi-root, versioned session
  save/restore (mksession/shada-like), and the **workspace↔user↔machine settings scope**. *(VW-SESSION, VW-SETTINGS)*
- **G9** Status/health surfaced only by rendering the **Health Registry** (INV-STATUS); the layout never
  owns health state. Output lowers through the single **Render Tree** (INV-RENDER-IR); no view paints cells.
- **G10** **Performance:** an edit re-renders only the affected view(s), never the whole layout tree.

## Non-goals

- The Document / typed-coordinate / anchor / identity / **kind** model itself — owned by
  [RFC-0008](../rfc/proposed/RFC-0008-document-model.md). We *use* `DocumentId`, `Snapshot`, `Anchor`,
  and `BufferKind`; we do not redefine them.
- The transaction/undo contract and the save/recovery **journal** — owned by
  [RFC-0007](../rfc/proposed/RFC-0007-transaction-engine.md) / [persistence-and-recovery.md](persistence-and-recovery.md). Session persistence here reuses
  that versioned format; it does not invent a second one.
- The **positions-history** model (marks `a-z`/`A-Z`, mark ring, jumplist, changelist) — owned by
  [positions-history.md](positions-history.md) (D-027). We fix only *which owner* (view vs document vs
  workspace) each history is keyed to, and how session restore rehydrates them.
- The **register/kill-ring** store — [register-model.md](register-model.md) (D-026); it is workspace-session
  state, cited only where session scope matters.
- Multi-**client** concurrent editing and per-client render pinning (D-012, V-13) — post-MVP; the
  View-local ownership here is the substrate that makes it an addition, not a rewrite. Render *profile*
  pinning is per client-view (INV-RENDER-PROFILE), owned by [render-and-frontends.md](render-and-frontends.md).
- The Render Tree node vocabulary and per-frontend lowering — [render-and-frontends.md](render-and-frontends.md).

## Terminology

Uses the [spec glossary](../../spec/glossary.yaml) (Workspace / Document / View / Window / Buffer). New
local terms:

- **Window** (a.k.a. *pane*) — a **layout slot** that hosts exactly one View at a time. A leaf of the
  layout tree. (`WindowId`.) A Window is *not* an OS window and *not* a View.
- **View** — the presentation of one Document in a Window: owns cursor(s), selection(s), viewport,
  fold state, and view-local mode bits. (`ViewId`.) One Document → many Views (VW-MULTI).
- **Layout tree** — the binary/n-ary tree of splits whose leaves are Windows; one per **tab page**.
- **Tab page** — a named top-level layout tree (a *window layout*, VIM-WIN-1), **not** a buffer-as-tab.
- **Overlay** — a floating/modal/popup surface drawn *above* the tiling base, outside the layout tree.
- **Buffer list** — the Workspace's registry of loaded Documents with a `listed`/`unlisted`/`hidden`
  disposition (Vim `:ls`/`hidden` semantics; EMACS buffer list).
- **Restriction (narrowing)** — a Document-level sub-span that confines all operations (V-27).
- **Indirect document** — a second `DocumentId` that shares the base text/revision stream but carries
  its own Restriction and views (Emacs *indirect buffer*).
- **Session** — the serialized, restorable projection of a Workspace (layout + buffer list + view
  positions + marks), distinct from the crash **journal** (which is per-document, [persistence](persistence-and-recovery.md)).

## Invariants

Depends on and enforces (registry: [reference-invariants.md](../invariants/reference-invariants.md);
not redefined here):

- **INV-DOC-VIEW** — the spine: Document ≠ View ≠ Window ≠ File; a Document never references a View;
  view-local state is never on the Document; one Document, many Views. Governs VW-VIEWSTATE, VW-MULTI,
  and the narrowing reconciliation (VW-NARROW).
- **INV-HANDLE** — `DocumentId` / `ViewId` / `WindowId` / anchor / snapshot ids are generation-checked
  typed handles; a freed-generation handle is an **assert**, not an error (governs the dead-document /
  dead-window failure modes).
- **INV-BUFFER-KIND** — a buffer's kind selects the mutation contract; drives the interactive write-back
  path and what "close a view of it" means.
- **INV-STATUS** — status is a per-component state machine aggregated into a Health Registry; the layout
  *renders a subscription*, never owns health.
- **INV-RENDER-IR** — every surface (tiled window, overlay, status line) lowers through one Render Tree;
  no view emits backend bytes.
- **INV-QUERY-SNAPSHOT** — a View renders from an immutable Document snapshot at a revision; interactive
  views render from a domain-state DTO. Never a live mutable object.
- **INV-ASYNC-ORDER** — resize/relayout/write-back/async-result ordering is preserved by the deterministic
  executor; every async result carries a request-id + revision (or a layout **epoch**, §Layout) and stale
  results are dropped — this is the stale-layout guard.
- **INV-ANCHOR** — view cursors and the Restriction span are anchors, surviving edits; per-view anchor
  updates are batched by the edit set (not `O(anchors × edits)`).
- **INV-TRUST-1** — a workspace/session file cannot silently execute code or override security-sensitive
  settings (governs VW-SETTINGS scope and session restore).
- **INV-SCHED-1** — a View's background work (parse/index/decoration) is owned by the central scheduler;
  closing a View **cancels/supersedes** its tasks (governs VW-LIFE leak prevention).
- Also relied on: **INV-ORIGIN**, **INV-ERR-CLASS**, **INV-FAIL-BOUNDED**, **INV-ADDITIVE**,
  **INV-REMOTE-FIRST** (multi-root remote), **INV-PROTOCOL-VERSIONED** (session format).

## Proposed design

### 1. The three handles and who owns what (VW-HANDLE, VW-VIEWSTATE)

Ownership is partitioned one-fact-one-owner, extending
[RFC-0008 §1](../rfc/proposed/RFC-0008-document-model.md):

| Fact | Owner | Handle | Never on |
| --- | --- | --- | --- |
| Text bytes, revision, encoding/EOL, **kind**, **Restriction** (§6) | **Document** | `DocumentId` | View, Window |
| Cursor(s), selection(s), viewport/scroll, **fold state**, view-mode bits, per-view jumplist | **View** | `ViewId` | Document, Window |
| Split geometry, focus, tab membership, sizes, overlay stack | **Layout/Window** | `WindowId` | Document, View |
| Buffer list, services, roots, session, settings scope | **Workspace** | (the workspace) | — |

```rust
/// A layout slot. Hosts one View; is a leaf in exactly one tab's LayoutTree (or is detached).
pub struct WindowId(Generational);
/// A presentation of one Document. Belongs to at most one Window at a time (may be detached/hidden).
pub struct ViewId(Generational);
// DocumentId is defined by RFC-0008; we only hold it.

pub struct View {
    id: ViewId,
    document: DocumentId,            // the ONLY link View→Document (one-way; INV-DOC-VIEW)
    // ---- view-local state: NEVER stored on the Document ----
    cursors: SmallVec<[Caret; 1]>,   // multi-cursor: primary + secondaries (Helix/Native, Emacs mark)
    selections: SmallVec<[Selection; 1]>, // region(s); Vim Visual / Emacs region live here, not on Doc
    viewport: Viewport,              // top line-anchor, left col, height/width in cells, scrolloff
    folds: FoldState,                // fold ranges (anchor pairs) + open/closed — VIEW-LOCAL, see §1.1
    mode_view: ViewModeBits,         // e.g. wrap on/off, number/relnumber, conceal level, list
    jumplist: JumpListHandle,        // per-view (splits have independent jumplists; positions-history)
    render_cache: RenderCacheSlot,   // §11 perf: keyed by (doc revision, viewport, layout epoch)
    kind_view: KindViewState,        // interactive/generated view-side write-back state (§5)
}

pub struct Caret { anchor: Anchor, affinity: Affinity, goal_col: Option<CellCol> } // INV-ANCHOR
pub struct Viewport { top: Anchor, left: CellCol, height: u16, width: u16, scrolloff: u8 }
```

**Cursor is an anchor, not an offset** (INV-ANCHOR): editing in view *A* shifts view *B*'s cursor
correctly because both resolve against the same anchor store on the shared Document — but each View holds
its *own* Caret anchors, so they move **independently** where they sit (§4).

#### 1.1 Folds are view-local; fold *definitions* may be document-derived

Fold *state* (which ranges are collapsed and their open/closed toggle) is **view-local** — two splits of
one file fold independently, matching Vim (`:set foldmethod`, `zM`/`zR` are per-window). Fold *ranges*
may be **computed** from a document-scoped source (indent/syntax/marker via the parse service), but the
computed ranges are a snapshot the View copies into its `FoldState` as anchor pairs; the View owns the
open/closed bit. This keeps INV-DOC-VIEW intact (no fold state on the Document) while allowing shared
fold *providers*. `foldmethod=manual` folds are purely view-local anchor pairs.

### 2. The layout tree (VW-LAYOUT)

Each **tab page** owns one `LayoutTree`; the Workspace owns an ordered list of tab pages plus the current
tab. A tab is a *window layout* (VIM-WIN-1), never a buffer.

```rust
pub struct LayoutTree {
    root: LayoutNode,
    focus: WindowId,          // the focused leaf; exactly one per tab
    epoch: LayoutEpoch,       // bumped on ANY structural/size change — the stale-layout guard (§Failure)
}

pub enum LayoutNode {
    Leaf(WindowId),
    Split {
        dir: SplitDir,                 // Horizontal (stacked rows) | Vertical (side-by-side cols)
        children: Vec<Child>,          // ordered; ≥2
    },
}
pub struct Child { node: LayoutNode, size: SizeSpec }

pub enum SplitDir { Horizontal, Vertical }

/// Per-child sizing intent; resolved against the parent's available cells (§2.2).
pub enum SizeSpec {
    Weight(f32),          // proportional (default 1.0): flex share of leftover space
    Fixed(u16),           // exact cells (e.g. a 30-col file tree), clamped to min/max
    Percent(f32),         // share of the parent axis
}
pub struct WindowConstraints { min: u16, max: Option<u16>, winfixwidth: bool, winfixheight: bool }
```

**Why a tree, not a grid.** A binary/n-ary split tree reproduces Vim's `C-w s`/`C-w v` nesting and
`:only` exactly, degrades to a narrow terminal by collapsing low-priority branches (parity
`priority-based degradation`, [workspace.md](../parity/workspace.md#principles)), and lowers cleanly to a
GUI/Web frontend (each leaf is a rectangle) without the Kernel knowing the frontend (INV-DOC-VIEW /
ARCH-FORBID). A grid cannot express arbitrary nesting; a per-feature widget tree reproduces Neovim's
divergence (rejected in RFC-0002).

#### 2.1 Focus, traversal, and the window surface

Focus is a single `WindowId` per tab. The `C-w` / `:…` / `gt` surface (VIM-WIN) maps onto tree ops:

| Surface | Tree operation |
| --- | --- |
| `:split` / `C-w s`, `:vsplit` / `C-w v` | replace focused `Leaf(w)` with a `Split{dir, [old, new]}`; new leaf hosts a View of the *same* buffer (VW-MULTI) |
| `C-w h/j/k/l`, `C-w w`/`C-w p` | geometric neighbor / cyclic / previous — resolved by the leaf's screen rectangle, not tree position |
| `C-w H/J/K/L` | move the focused window to the far edge (reparent subtree) |
| `C-w r` / `C-w x` | rotate / exchange siblings |
| `C-w =` | reset all `SizeSpec` to `Weight(1.0)` and re-solve (§2.2) |
| `C-w _` / `C-w \|` | maximize focused height/width (set siblings toward min, respecting `winfix*`) |
| `C-w c` / `:close`, `C-w o` / `:only` | close focused leaf / close all others (§3 lifecycle) |
| `C-w q` / `:quit` | close the View; if it is the last window, quit the tab/workspace per policy |
| `:tabnew`, `gt`/`gT`, `:tabmove`, `:tabclose`, `:tabdo` | tab-page list ops (a new tab = a fresh single-leaf LayoutTree) |
| `:windo` / `:bufdo` | iterate views / buffers (see §8) |

Geometric neighbor resolution computes each leaf's absolute cell rectangle from the current solve (§2.2),
then picks the adjacent rectangle in the requested direction (nearest overlap on the perpendicular axis),
matching Vim's `wincmd h/j/k/l`.

#### 2.2 The resize / sizing solve (deterministic)

Given a tab's outer rectangle, the solve is a single top-down pass, deterministic (INV-ASYNC-ORDER — no
async in the solve):

```
solve(node, rect):
    match node:
      Leaf(w):
          assign rect to window w; clamp to WindowConstraints (min/max, winfix*).
      Split{dir, children}:
          axis_len = rect.len_along(dir) − separators(children.len())
          # 1. satisfy Fixed and Percent (clamped to each child's min/max, winfix*)
          # 2. distribute the REMAINDER to Weight children in proportion to their weights
          # 3. if sum(min) > axis_len: enter DEGRADED distribution — shrink lowest-priority
          #    branches to min, then hide (collapse) branches past the priority cutoff (parity degradation)
          # 4. round with a stable largest-remainder rule so totals are exact and stable frame-to-frame
          for (child, sub_len) in resolved:
              solve(child.node, rect.slice_along(dir, sub_len))
```

Properties: (a) **stable** — the largest-remainder rounding means a 1-cell terminal resize does not
reshuffle unrelated windows; (b) `winfixwidth`/`winfixheight` windows (e.g. a file tree, a help pane)
keep their size while siblings absorb the delta (Vim `winfix*`); (c) when the terminal is too small,
**degrade by priority** rather than produce negative sizes — low-priority branches (an auxiliary preview)
collapse to zero and are marked hidden, never rendered at an impossible size (INV-CAP-DEGRADE spirit).
Every solve bumps `LayoutTree.epoch`.

### 3. Layout lifecycle ≠ view lifecycle ≠ document lifecycle (VW-LIFE) — leak prevention

This is the load-bearing decoupling ([RFC-0002](../rfc/proposed/RFC-0002-workspace-architecture.md),
[RFC-0008 §1](../rfc/proposed/RFC-0008-document-model.md), design-requirements §11). Three lifetimes,
three owners, released bottom-up:

```
Window (layout slot)  ─hosts→  View (presentation)  ─references→  Document (content)
   Layout owns                 View owns view-local            Workspace owns via buffer list
```

**Closing a Window** (`C-w c`, `:close`, `:only`, tab close) does exactly this, in order:

```rust
fn close_window(ws, tab, w: WindowId) -> Result<()> {
    let view = tab.layout.detach_leaf(w)?;        // 1. structurally remove the leaf; collapse parent
    tab.layout.reflow_focus_after_close(w);       //    focus moves to a deterministic neighbor
    tab.layout.epoch.bump();                      //    invalidate render/geometry (stale-layout guard)
    retire_view(ws, view);                        // 2. decide the View's fate (below)
    Ok(())
}

fn retire_view(ws, view: ViewId) {
    // A View may be reused (Vim: closing a split does NOT kill the buffer; the View may persist
    // detached if policy says so) OR fully released. Default policy:
    if ws.no_other_window_hosts(view) && !ws.view_is_pinned(view) {
        ws.scheduler.cancel_view_tasks(view);     // 3. CANCEL parse/index/decoration/LSP-didClose owned
                                                  //    by this view (INV-SCHED-1) — no orphaned tasks
        let doc = ws.view(view).document;
        ws.free_view(view);                       // 4. release ViewId generation + its anchors/folds
        ws.gc_document_if_unreferenced(doc);      // 5. Document lifetime is SEPARATE (below)
    }
}
```

**Document lifetime is decoupled from every view** (Vim `hidden`, EMACS buffers persist without a
window). A Document is unloaded only when it is referenced by **neither** a live View **nor** a *listed*
buffer-list entry:

```rust
fn gc_document_if_unreferenced(ws, doc: DocumentId) {
    let listed  = ws.buffer_list.disposition(doc);  // Listed | Unlisted | Hidden
    let hosted  = ws.any_view_references(doc);
    match (listed, hosted) {
      // A "hidden" buffer (Vim 'hidden' + unsaved, or a listed background buffer) is retained WITHOUT a view.
      (Listed | Hidden, _) => {}                   // keep loaded; it is intentionally in the background
      (Unlisted, true)     => {}                    // e.g. a help/preview view still open
      (Unlisted, false)    => ws.unload_document(doc), // truly unreferenced → unload (flush journal first)
    }
}
```

Guarantees this closes:

- **No leaked background tasks.** Closing the last View of a buffer cancels its scheduler-owned work
  (parse, index, decoration providers, LSP `didClose`) — the dangling-view failure RFC-0002 names,
  enforced by INV-SCHED-1. A buffer kept *hidden* keeps only the work a hidden buffer legitimately needs
  (e.g. it stops per-frame decoration but keeps its journal open).
- **No dead-document leak.** A View cannot outlive its Document silently: a `DocumentId` in a `View`
  whose generation was freed is an **assert** (INV-HANDLE), meaning a lifetime bug, not a runtime error —
  so "view holding a dead document" is caught, not tolerated (see Failure modes).
- **No lost work on close.** Closing a *modified* buffer's last window does **not** unload it; it becomes
  a hidden buffer (Vim `hidden`) so its journal ([persistence](persistence-and-recovery.md)) stays live;
  an explicit `:bdelete`/`kill-buffer` on a modified buffer goes through save-preflight (stability §13),
  never silent loss.
- **Unloading flushes.** `unload_document` flushes/rotates the crash journal and drops the anchor store;
  it is the only place a Document's memory is reclaimed.

### 4. The same buffer in many views (VW-MULTI)

One `DocumentId`, N `View`s. What is **shared** vs **independent**:

| Fact | Shared across views of one Document | Independent per View |
| --- | --- | --- |
| Text, revision, undo/redo tree, journal | ✅ (one Document; an edit in any view advances the one revision) | — |
| Anchor **store** (the mechanism) | ✅ (one store on the Document) | — |
| **Cursor(s), selection(s)** | — | ✅ each View's Carets are its own anchors |
| Viewport / scroll / folds / mode-view | — | ✅ |
| Jumplist / changelist position | — | ✅ per-view (positions-history / INV-DOC-VIEW; splits differ) |
| Marks `a`–`z`, changelist *contents* | ✅ buffer-scoped (Document-keyed, positions-history) | — |

So editing in split *A* is instantly visible in split *B* (same revision, same snapshot source), each
keeps its own cursor and scroll, and both cursors track edits correctly because they are anchors in the
same store that the transaction updates once (batched, INV-ANCHOR). This is the exact "same file in two
splits" behavior Vim gets right and that a cursor-on-buffer model gets wrong (the RFC-0008 §1 precondition).

**Linked/cloned views.** `:split` and `C-w v` create a *new* View of the same Document (independent
cursor). A **clone** operation (Emacs `clone-indirect-buffer` *without* narrowing; a "duplicate view")
may optionally *copy* the source View's cursor/viewport as a starting point but thereafter they diverge.
There is no mode where two Views of the *same* Document share a live cursor — that would be a single View
shown twice, which the layout expresses by hosting one `ViewId` in… no: a `WindowId` hosts exactly one
`ViewId`, and a `ViewId` is hosted by at most one Window, so "one view, two windows" is not representable —
by construction (VW-HANDLE) each window is an independent view. This is deliberate: it removes the
"whose cursor is it" ambiguity entirely.

### 5. Buffer kinds and interactive-view write-back (from the View side)

Kind is a Document property ([RFC-0008 §5](../rfc/proposed/RFC-0008-document-model.md), INV-BUFFER-KIND);
the *View* is what turns a user gesture into the kind-appropriate mutation. The write-back contract is
[RFC-0002 §write-back](../rfc/proposed/RFC-0002-workspace-architecture.md) / [workspace.md V-14](../parity/workspace.md);
this doc pins the **View-side descriptor** that a WS-* surface supplies:

```rust
pub enum BufferKind { Editable, ReadOnly, Generated, Streaming, Interactive } // owned by RFC-0008

/// A View of an Interactive buffer carries a write-back descriptor: how a buffer edit
/// (wdired rename, Magit stage line) becomes a typed domain CommandRequest.
pub struct WriteBackDescriptor {
    // Read side: how to project domain-state DTO → renderable lines (INV-QUERY-SNAPSHOT).
    render: fn(&DomainSnapshot) -> RenderModel,
    // Write side: interpret a buffer edit against the last render as a domain CommandRequest.
    interpret_edit: fn(edit: &BufferEditIntent, &RenderModel) -> Option<CommandRequest>,
}
```

Per-kind view behavior:

| Kind | View mutation path | On "edit" gesture |
| --- | --- | --- |
| **Editable** | text Transaction (INV-TXN) | edit → transaction on the Document |
| **Read-only** | none | edit rejected at preflight; status note |
| **Generated** (help, output) | none | not editable; a "refresh" re-runs the producer |
| **Streaming** (log/PTY/build) | append path (INV-BUFFER-KIND) | user input goes to the PTY, not a Document; scrollback is a bounded ring |
| **Interactive** (dired/wdired, Magit, tree, debugger) | `interpret_edit` → `CommandRequest` → preflight → service → re-query → re-render | e.g. wdired rename line → `fs.rename{from,to}`; Magit `s` → `git.stage{path}` |

The View **never** keeps a shadow text buffer "in sync" under an interactive surface — the rendered lines
*are* a projection of domain state (CQRS, RFC-0002). A stale write-back (edit computed against an old
DTO) is rejected at preflight on revision/generation mismatch (INV-ASYNC-ORDER, INV-HANDLE) and the view
re-renders. This is why closing an interactive View (§3) cancels its *domain* subscriptions, not a text
journal (there is none — [persistence §8](persistence-and-recovery.md) buffer-kind table).

### 6. Narrowing = a Document-level restriction (VW-NARROW, V-27, EMACS-EDIT-2)

Emacs narrowing "confines all operations (search, motion, transaction) to a sub-span." The verification
finding **V-27** fixes ownership: the restriction lives on the **Document**, *not* the View —
[emacs.md EMACS-EDIT-2](../parity/emacs.md) "owner = Document, not View (V-27)."

```rust
// On the Document (RFC-0008 owns the Document; this is the field VW-NARROW adds to it).
pub struct Restriction {
    span: AnchorRange,       // start..end anchors; survives edits (INV-ANCHOR)
    origin: Origin,          // who narrowed (user / plugin / org-edit) — INV-ORIGIN
}
// Document { …, restriction: Option<Restriction>, … }
```

**Why Document, not View, and how that coexists with INV-DOC-VIEW.** INV-DOC-VIEW forbids *view-local*
state (cursor/scroll/fold) on the Document and forbids the Document *knowing about* a View. A Restriction
is neither: it is a property of the **content model** ("operations on this buffer see only [start,end)"),
exactly like the buffer's kind or encoding. It does not reference any View. Views *observe* it (their
cursors clamp into it, motions/searches stop at its edges, transactions are validated against it at
preflight) but do not own it. So:

- **All Views of a narrowed Document are narrowed** — matching Emacs, where narrowing is buffer-local and
  every window on that buffer sees it. A cursor in any View is clamped to the span; a paste/search past
  the edge is refused at preflight (stability §13), not silently clipped.
- **Transactions respect it** — a text Transaction whose edits fall outside `span` fails preflight
  (INV-TXN base-revision check plus a range-in-restriction check). Widening (`widen`) clears the
  Restriction; this is itself an observable Document event (INV-STATUS surfaces `[Narrowed]`).
- **Independent narrowing** (two regions of one file narrowed differently at once) is an **indirect
  document**: a distinct `DocumentId` sharing the base text/revision stream but carrying its **own**
  `Restriction` (Emacs *indirect buffer*). Views of the indirect document narrow independently; edits
  still advance the one shared revision. This keeps "narrowing is a Document fact" total: independent
  narrowing means independent Documents-over-one-text, not View-local restrictions.

Vim has no narrowing surface; the Restriction simply stays `None` for Vim-profile buffers. The mechanism
is profile-neutral: a plugin (org-mode-like) narrows via a namespaced command with an `Origin`.

### 7. Overlays: floats, popups, modals (VW-OVERLAY) — and how modal input survives them

The layout tree (§2) is the **tiling base**. Floating/overlay surfaces are a **separate ordered layer**
above it — not leaves of the tree — so a completion popup or a fuzzy-finder does not reshuffle the tiling.

```rust
pub struct OverlayStack { layers: Vec<Overlay> }   // back-to-front z-order; last = topmost
pub struct Overlay {
    id: OverlayId,
    view: ViewId,                 // an overlay hosts a View too (everything is a view/buffer)
    placement: Placement,         // Anchored{to: AnchorSite, offset} | Centered | Cursor | Fixed(rect)
    modality: Modality,           // NonModal | Modal { input_scope: InputScope }
    z: ZClass,                    // Popup < Float < Dialog < Command < System
    focusable: bool,
}
pub enum AnchorSite { CursorCell, WindowRect(WindowId), ScreenEdge(Edge) }
```

Placement resolves **after** the tab solve (§2.2), against the current `LayoutEpoch`, so a float anchored
to the cursor follows a resize deterministically. Overlays lower through the **same Render Tree**
(INV-RENDER-IR) as tiled windows — a float is not a special backend path.

Use of kinds inside overlays: a completion popup is a **Generated** list view; a fuzzy finder is an
**Interactive** view (its query line is Editable, its results Generated); the command line / minibuffer
is an Editable single-line view; a debugger hover is Read-only. All reuse §5.

**How modal input survives overlays — the crucial rule.** *Input mode is not owned by the layout.* Per
[editing-language.md](editing-language.md) "Mode axes," operator-pending / count-buffer / register-prefix
/ Insert-vs-Normal / Emacs-prefix are **independent state axes on the input engine**, not on any
Window/View/overlay. Therefore:

- Opening an overlay (e.g. autocompletion pops up mid-`ciw`) does **not** clear operator-pending: the
  pending operator lives on the input engine, untouched. Dismissing the overlay resumes exactly where the
  gesture was.
- A **Modal** overlay declares an `input_scope` and rides the **priority ABI tier 1 ("temporary state")**
  ([architecture §1.4](../architecture/architecture.md), INV-PRIORITY): while a modal command line /
  picker is up, its keymap outranks the underlying view's, but the resolver is the *same* one — no
  separate event path. Closing the modal pops tier 1 and the previous context resumes.
- Focus is **saved and restored** around a modal: `OverlayStack` records the previously-focused
  `WindowId`; on dismissal focus returns to it (and to the same `ViewId`, whose cursor/anchors are intact
  because they were never touched).
- A non-focusable, non-modal overlay (a signature-help float) never receives input at all; keys route to
  the focused tiled window as usual.

This is the concrete fix for "a floating popup eats my operator-pending / my prefix": those states are
architecturally *elsewhere*, and overlays interact with input only by pushing/popping the tier-1
temporary-state layer, never by owning mode.

### 8. Workspace & Session (VW-SESSION)

The **Workspace** is a Built-in Service ([RFC-0002](../rfc/proposed/RFC-0002-workspace-architecture.md)),
owning the whole container tier:

```rust
pub struct Workspace {
    documents:  SlotMap<DocumentId, Document>,   // loaded buffers (content owned here)
    views:      SlotMap<ViewId, View>,
    buffer_list: BufferList,                     // disposition of each Document (below)
    tabs:       Vec<TabPage>,                     // each owns a LayoutTree
    current_tab: usize,
    overlays:   OverlayStack,
    roots:      Vec<WorkspaceRoot>,              // MULTI-ROOT (below)
    services:   ServiceRegistry,                 // LSP/Git/… clients (RFC-0002 layer)
    settings:   SettingsResolver,               // scope chain (§ VW-SETTINGS)
    session:    SessionModel,                    // save/restore projection
    health:     HealthRegistrySub,              // subscription only (INV-STATUS)
}
```

#### 8.1 Buffer list: listed / unlisted / hidden (VIM-WIN, EMACS-BUFFER)

```rust
pub enum Disposition { Listed, Unlisted, Hidden } // Vim :ls (listed) / :ls! (unlisted) / 'hidden'
pub struct BufferEntry { doc: DocumentId, disposition: Disposition, alt: bool /* the "#" alternate */ }
```

- **Listed** — appears in `:ls`, buffer pickers, `:bnext`/`:bprev`, `next-buffer`. Kept loaded without a
  window (a background buffer). This is the state a modified buffer enters when its last window closes
  under `hidden` (§3), so no work is lost.
- **Unlisted** — loaded but hidden from the normal list (help, preview, some plugin buffers). Still a real
  Document; GC'd only when *also* view-less (§3).
- **Hidden** — Vim's `hidden`/`bufhidden` nuance: a buffer that must survive losing its window even when
  modified. Modeled as `Listed`+retained; the disposition is what §3's GC consults.

`:bufdo` iterates listed buffers; `:windo` iterates the current tab's windows; `:tabdo` iterates tabs.
Each is a driven command sequence over the deterministic executor (the `:normal`/`:global` re-entry model,
[editing-language.md](editing-language.md)), not a special loop.

#### 8.2 Multi-root

`roots: Vec<WorkspaceRoot>` where a root is a typed `WorkspacePath` (`local` ≠ `workspace`/remote path;
INV-REMOTE-FIRST). Multiple roots (a mono-repo's sub-projects, or a local + remote pair) each carry their
own trust decision, LSP/Git service scoping, and file-watcher registration. A Document's `ResourceId`
(RFC-0008 §4) resolves under exactly one root; a remote root's runtime is negotiated
([remote-runtime.md](remote-runtime.md), INV-REMOTE-FIRST). Adding/removing a root is an explicit
workspace op (re-scopes services, never silently trusts new code — INV-TRUST-1).

#### 8.3 Session save / restore (mksession / shada-like) — VW-SESSION

A **Session** is the restorable projection of the Workspace, distinct from the per-document crash
**journal** ([persistence-and-recovery.md](persistence-and-recovery.md)). It is a **versioned** document
(INV-PROTOCOL-VERSIONED / INV-ADDITIVE) so future fields extend it without breaking older readers.

What a session **persists** (mksession + shada, filtered by fidelity):

```rust
pub struct SessionModel {
    schema_ver: u16,
    tabs:   Vec<SavedTab>,          // layout tree geometry (split dirs, SizeSpec, focus) per tab
    buffers: Vec<SavedBuffer>,      // resource identity (WorkspacePath/ResourceId), disposition, kind
    views:  Vec<SavedView>,         // per view: buffer ref + cursor/viewport as a RESOLVED position + folds
    roots:  Vec<WorkspacePath>,     // multi-root set
    marks:  SavedMarks,             // GLOBAL marks (A–Z, 0–9) + per-buffer marks — see below
    registers: Option<SavedRegisters>, // optional (shada); D-026 persistence is opt-in (register-model OQ-4)
    jumplist_global: SavedJumps,    // shada global jumplist; per-view jumplists are ephemeral
}
```

Fidelity rules (what is faithfully restorable vs regenerated vs dropped):

- **Layout geometry** (tabs, split tree, sizes, focus) — persisted and restored exactly.
- **Buffers** — persisted by *identity* (`WorkspacePath`/`ResourceId`), not content: on restore the file
  is reopened (content comes from disk / the crash journal, not the session), so the session never
  duplicates or staleness-conflicts with document bytes. A buffer whose resource is gone restores as a
  missing-file placeholder, not an error.
- **View positions** — cursor/viewport saved as a **resolved typed position** (line/col) plus a best-effort
  re-anchor on reopen; folds saved as ranges re-resolved against reopened content. These are *positions*,
  not anchors (anchors are live objects), consistent with positions-history restore.
- **Marks** — the split matters for persistence and is **owned by positions-history** ([positions-history.md](positions-history.md)):
  *global* marks (`A`–`Z`, numbered `0`–`9`) and the *global* jumplist/mark-ring are **workspace/session**
  scoped and persisted here; *per-buffer* marks (`a`–`z`) are Document-keyed and are persisted with the
  buffer's identity. This doc defers the ring semantics to positions-history and only fixes the session
  *scope* boundary. The crash-recovery tie (a session referencing buffers with live journals) is
  [persistence §4](persistence-and-recovery.md): reopening a session with unrecovered journals surfaces
  the three-way recovery, never auto-applies.
- **Not persisted** (ephemeral, regenerated): render caches, decoration providers, LSP/Git service state,
  the register store *unless* shada opt-in (D-026 OQ-4/OQ-6), transient overlays, operator-pending/prefix
  input state, per-view jumplists.

Restore is **trust-gated**: a session file cannot execute code and cannot silently re-grant permissions
(INV-TRUST-1) — reopening a workspace re-runs the trust decision before any service starts.

#### 8.4 Settings scope (VW-SETTINGS)

Config resolves through a fixed scope chain (design-requirements scope IDs: process-local · session-local ·
workspace-persistent · globally-stable/**machine**), aligning with the keymap priority ABI
([architecture §1.4](../architecture/architecture.md)):

```
machine/global (user profile)  <  workspace (project)  <  buffer-local (mode)  <  view-local (window opt)  <  session-ephemeral
   lower precedence  ────────────────────────────────────────────────────────────────────►  higher
```

- **Machine/global vs workspace:** a workspace (`.ruse/…` in a repo root) may set project settings
  (formatter, tab width, enabled plugins) but **must not override security-sensitive or machine-scoped
  settings** (trust decisions, credential paths, remote exec policy) — INV-TRUST-1 / ARCH-FORBID
  ("a workspace must not override security-sensitive settings"). The `SettingsResolver` classifies each
  key's *max writable scope*; a workspace write to a machine-only key is rejected, surfaced, not applied.
- **Buffer-local** settings (`setlocal`, major-mode config, dir-local) shadow workspace for that Document;
  **view-local** window options (`:setlocal` window opts like `wrap`, `number`) live in `View.mode_view`.
- Config schema is versioned (architecture §4.5); unknown keys degrade with a deprecation note, never a
  hard failure (INV-ADDITIVE).

### 9. Status & health (INV-STATUS)

The status/health of every subsystem the layout touches (Document engine, renderer, each LSP, Git, remote
runtime, terminal capability, per-buffer derived save-state) is a **per-component state machine aggregated
into the Health Registry** ([stability §11](stability-and-observability.md), INV-STATUS). The status line —
itself a View lowered through the Render Tree — **subscribes** to the registry and renders a projection
(`[+]` modified, `[✎ journaled]`, `[⚠ disk changed]` from [persistence §1](persistence-and-recovery.md);
`[Narrowed]` from §6; `LSP: rust-analyzer Recovering`). The layout **never owns** health state and never
mutates it; "why did that indicator change?" is answered by the registry's status-change log events
(stability §11.3), not by the UI. Per-window/per-view status (e.g. which buffer is modified) is a
projection keyed by the focused/hosted `DocumentId`, not a separate store.

## Failure modes

- **View holding a dead Document** — a `View.document` whose `DocumentId` generation was freed. This is a
  **lifetime bug**, so it is an **assert** (INV-HANDLE / INV-FAIL-BOUNDED), triggering a recovery snapshot
  and safe shutdown, *not* a swallowed error. §3's ordering (unload a Document only when view-less and
  unlisted) makes it unreachable in correct code; the assert catches regressions. It is never "handled" by
  rendering an empty pane, which would hide the bug.
- **Resize during a layout change → stale layout** — a resize event, an async decoration/render result, or
  an overlay placement computed against an **old** geometry. Guard: every structural/size change bumps
  `LayoutTree.epoch`; every render frame, decoration request, and overlay placement carries the epoch it
  was computed for (INV-ASYNC-ORDER — "responses carry a revision"). A result whose epoch ≠ current is
  **dropped** and recomputed, so a mid-relayout frame is never painted (architecture §6.2 "do not render
  stale layout during resize"). The solve itself (§2.2) is synchronous on the deterministic executor, so
  no two solves interleave.
- **Split with no space** — the degraded distribution (§2.2) collapses lowest-priority branches to hidden
  rather than producing negative/zero-invalid rectangles; a split that cannot fit even at min is refused
  with a typed `ErrorCode::NoRoomForWindow` (INV-ERR-CLASS), matching Vim's `E36`.
- **Closing the last window of a tab / of the workspace** — closing the last leaf of a tab closes the tab;
  closing the last window of the last tab triggers the quit-policy (prompt if any modified/hidden buffer
  has unsaved work; save-preflight, stability §13). Never silent loss.
- **Closing a modified buffer's last window** — does **not** unload it; becomes a hidden/listed background
  buffer with a live journal (§3). Explicit `:bdelete!`/`kill-buffer` on modified content goes through
  save-preflight.
- **Interactive write-back against stale domain state** — rejected at preflight on revision/generation
  mismatch (INV-ASYNC-ORDER, INV-HANDLE); the view re-queries and re-renders (RFC-0002 failure modes).
- **Edit outside a narrowing restriction** — refused at preflight (§6), not clipped; status surfaces the
  refusal.
- **Overlay outlives its anchor** — a float anchored to a cursor site whose Document/View closed: the
  overlay is dismissed with its host View's retirement (§3), never left dangling; its `ViewId` is released.
- **Session references a missing/renamed resource** — restore places a missing-file placeholder buffer
  (identity-based restore, §8.3), never fabricates content and never errors the whole restore.

## Recovery behavior

- **Layout corruption / invalid tree** (e.g. a session with an impossible split) → refuse the tree, fall
  back to a single-leaf tab hosting the first restorable buffer, surface a typed warning; never crash the
  restore. Layout is regenerable, so it is safe to reset.
- **Core-invariant failure** in the container tier (freed handle, tree invariant broken) → INV-FAIL-BOUNDED
  recovery snapshot (documents flush journals via persistence §8) then safe shutdown; reopen uses the
  crash-recovery path per document + session restore per §8.3.
- **Document unload on close** flushes/rotates its journal first (§3); an unload during shutdown is ordered
  after journal fsync so no unjournaled work is dropped.
- The container tier holds **no** independent durable state that could be corrupted: content lives in
  Documents (journaled), positions in positions-history, session is a regenerable projection. Recovery of
  the container is therefore "rebuild from documents + session," never "recover the layout's own data."

## Security impact

- **Session/settings as an untrusted input** — a workspace-scoped session or config file cannot execute
  code, cannot re-grant permissions, and cannot override machine-scoped/security-sensitive settings
  (§8.3/§8.4, INV-TRUST-1, ARCH-FORBID). Reopening a workspace re-runs the trust decision before services
  start.
- **Multi-root trust** — each root carries its own trust level; a remote root runs with remote-runtime
  authority, never local-client authority (INV-REMOTE-FIRST, architecture §10).
- **Interactive write-back** routes every mutation through a capability-gated, preflighted domain service
  (§5, RFC-0002 security): a workspace/plugin cannot smuggle a filesystem/Git write by "editing a buffer."
- **Overlay/terminal-output** stays an untrusted principal: an overlay's rendered content (e.g. an
  AI-proposed float, an LSP hover) is treated as untrusted UI markup lowered through the Render Tree —
  it cannot emit escape sequences or steal input focus outside the modal contract (INV-RENDER-IR).

## Performance impact

- **An edit re-renders only affected views, never the whole layout (G10).** A Transaction on `DocumentId`
  D invalidates the render cache of exactly the Views whose `document == D` and whose viewport overlaps the
  edit (usually one, sometimes a few splits). Each `View.render_cache` is keyed by
  `(document_revision, viewport, layout_epoch)`; an unrelated window with a matching key is **not**
  repainted. Output is a render **diff** (architecture §6.2, §9), not a full-frame reassembly.
- **The layout solve is O(nodes)**, run once per structural/size change (not per edit, not per frame),
  bumping `epoch` so caches invalidate precisely. A pure text edit does **not** trigger a solve.
- **Anchor updates are batched by the edit set** across all Views of a Document (INV-ANCHOR) — off
  `O(anchors × edits)`; each View's cursors update in the one pass.
- **Snapshots, not clones** — a View renders from an immutable Document snapshot (INV-QUERY-SNAPSHOT); N
  splits of one buffer share one snapshot per revision, no per-view copy.
- **Closing a View reclaims work** — canceling its scheduler tasks (§3) prevents background parse/index/
  decoration from accumulating for windows the user closed (INV-SCHED-1).
- **Overlays** lower through the same diffed Render Tree; a popup repaints only its rectangle plus the
  cells it uncovered, not the whole screen.

## Compatibility impact

- Delivers the container half of **F-007** (one buffer, many views) and **F-008** (workspace/session)
  onto the Document noun; unblocks the WS-* surfaces ([workspace.md](../parity/workspace.md)) which are all
  Views over this layout.
- Reproduces **VIM-WIN** (windows/tabs/buffers, `hidden`, `C-w`/`gt`) and **EMACS-BUFFER** (buffer list,
  everything-is-a-buffer) and **EMACS-EDIT-2** narrowing at parity, plus Vim `:mksession` / `shada`-class
  session persistence — without cloning Vim's window-state entanglement (VIM-WIN-1: tabs are layouts, not
  buffers).
- Session and settings formats are **versioned** (INV-PROTOCOL-VERSIONED / INV-ADDITIVE) so multi-client
  (D-012) and remote (D-013) extend them additively; per-client render pinning (INV-RENDER-PROFILE) attaches
  at the client-view without changing this model.
- No new `INV-*`; the interactive write-back and status contracts are the ones RFC-0002 already binds.

## Observability

- `:debug windows` / `:debug layout` renders the current tab's layout tree, each leaf's `WindowId`,
  hosted `ViewId` → `DocumentId`, resolved rectangle, `SizeSpec`, and `LayoutEpoch` — so "why is this pane
  this size / why did it not resize" is inspectable.
- `:ls` / `list-buffers` renders the buffer list with disposition (Listed/Unlisted/Hidden), kind, and each
  Document's derived save-state (from persistence §1) via the Health Registry projection.
- Every window-close/split/resize/session-restore emits a typed event with `origin` (INV-ORIGIN) and the
  affected handles, so leaks (a task not cancelled on close) and stale-layout drops (epoch mismatch) are
  visible in the event/log stream (stability §11–§12).
- An anchor→typed-coordinate resolver (RFC-0008 observability) answers "why is this cursor/fold/decoration
  here" per View.

## Alternatives

1. **Chosen: split *tree* + separate overlay layer + three handles + Document-owned restriction.** Each
   fact one owner; layout, view, and document lifetimes independently released; input mode lives on the
   engine so overlays can't eat it. Reproduces Vim/Emacs window+buffer+narrowing behavior exactly while
   staying frontend-independent.
2. **Fixed VSCode-style sidebar/panel/editor grid** — rejected below.
3. **Cursor/viewport on the Document (Vim/Neovim shape)** — rejected below.
4. **Floats as leaves of the layout tree** — rejected below.
5. **Narrowing as View-local state** — rejected below.
6. **Session stores document content** — rejected below.

## Rejected approaches

- **VSCode grid cloned to the TUI.** A fixed sidebar/panel/editor grid does not degrade into a narrow
  terminal and couples layout to one frontend (RFC-0002 rejects the same for the workspace substrate;
  architecture §7). ruse uses a split tree with priority-based degradation (§2.2).
- **Cursor/selection/viewport on the Document.** The exact INV-DOC-VIEW bug: it makes "same file in two
  splits" share a cursor, corrupts state when two views edit, and blocks GUI/remote/multi-client
  (RFC-0008 §1, anti-pattern #2). View-local state stays on the View (§1), always.
- **One View shown in two Windows (shared live cursor).** Rejected: it reintroduces "whose cursor is it"
  and a shared-mutable-view aliasing hazard. By construction a `WindowId` hosts one `ViewId` and a
  `ViewId` is hosted by at most one Window (§4), so every window is an independent view — the ambiguity
  cannot arise.
- **Floats as layout-tree leaves.** Rejected: a popup would then participate in the tiling solve and
  reshuffle real windows; overlays are a separate z-ordered layer (§7) placed *after* the solve.
- **Narrowing as View-local state.** Rejected by V-27: narrowing must confine *all* operations
  (search/motion/transaction) on the buffer, which are Document-level facts; a View-local restriction
  would let one split edit outside another split's "narrowing," contradicting Emacs semantics. It is a
  Document restriction; independent narrowing is an indirect Document (§6).
- **Input mode owned by the focused window/overlay.** Rejected: it is exactly what makes a popup "eat"
  operator-pending/prefix. Mode is an independent axis on the input engine ([editing-language.md](editing-language.md));
  overlays only push/pop the tier-1 temporary-state layer (§7).
- **Session stores document bytes.** Rejected: it duplicates content, goes stale against disk and the
  crash journal, and bloats the session. Session stores buffer *identity* and reopens from the resource
  (§8.3); content recovery is the journal's job (persistence §4).
- **Unloading a Document when its last window closes.** Rejected: breaks Vim `hidden` and loses unsaved
  work; Document lifetime is decoupled and GC'd only when both view-less and unlisted (§3).

## Trade-offs

- **Three handles + two lifecycles** (window vs view vs document) are more machinery than "a buffer with a
  window," but they are the irreducible structure that prevents the cursor-sharing, task-leak, and
  dead-document bugs the whole design exists to avoid. The cost is compile-time (typed handles) and paid
  once in the core.
- **Document-owned narrowing + indirect documents** is more than a per-view flag, but it is the only model
  that makes "narrowing confines all ops" total across splits while allowing independent narrowing (§6).
- **Overlay layer separate from the tiling tree** doubles the placement paths (solve, then place), in
  exchange for popups that never disturb real windows and modal input that never corrupts mode.
- **Identity-based session restore** trades "reopen from disk/journal" (a re-read) for never duplicating or
  staleness-conflicting document content — the right trade given the journal already owns recovery.
- **Epoch-guarded async** adds an epoch to every render/placement/decoration result, but it is the
  mechanism that makes stale-layout painting structurally impossible (INV-ASYNC-ORDER).

## Migration strategy

Greenfield (no prior view/window/session impl). Land order, respecting `depends_on`
(C-VIEW → C-RENDER, C-ANCHOR; C-WORKSPACE → C-DOCUMENT, C-VIEW): (1) the three handles + view-local state
+ single-leaf layout; (2) the split tree + solve + `C-w` surface + tabs; (3) the lifecycle/leak model
(§3) with scheduler task cancellation; (4) multi-view of one buffer (§4); (5) buffer list + kinds +
interactive write-back (§5); (6) narrowing (§6); (7) overlays (§7); (8) Workspace, multi-root, session
save/restore, settings scope (§8). Session and settings formats are versioned from v1 (INV-ADDITIVE) so
later multi-client/remote work extends them without a break. This resolves **V-11** (C-WORKSPACE was
absent from the early build order): the workspace stage is placed here, early, ahead of the remote stage.

## Test strategy

- **Property (invariants):** random split/close/resize/focus sequences never (a) leave an orphaned leaf or
  a tree with a 1-child Split, (b) leave focus on a freed `WindowId`, (c) leave a live scheduler task for a
  retired View (INV-SCHED-1), or (d) unload a Document still referenced by a view or listed entry (§3).
  Random edit sequences: cursors in all Views of one Document stay valid anchors (INV-ANCHOR) and never
  desync from the shared revision.
- **Differential (parity, TEST-2):** `C-w s/v/c/o/=/_/|/HJKL/hjkl`, `gt/gT/:tabmove/:tabclose`,
  `:ls`/`hidden`/`:bnext`, and `:mksession`→restore against a Vim oracle for layout geometry and cursor
  restoration; Emacs `narrow-to-region`/`widen` + indirect-buffer independence; wdired rename / Magit stage
  as domain CommandRequests (not text transactions), V-14.
- **Lifecycle/leak:** close the last window of a modified buffer → buffer becomes hidden, journal stays
  live, no task leaked; `:bdelete!` on modified → save-preflight fires; unload only when view-less+unlisted.
- **Stale-layout guard:** inject a resize between a solve and a render/decoration result → the stale-epoch
  result is dropped and recomputed; assert no frame is painted at the old geometry (architecture §6.2).
- **Overlay/modal input:** pop an overlay mid-`ciw`/mid-operator-pending → operator-pending survives;
  dismiss → gesture resumes; focus returns to the prior window/view with cursor intact.
- **Narrowing:** an edit/search/paste crossing a Restriction edge is refused at preflight; all Views of a
  narrowed Document are narrowed; an indirect Document narrows independently while sharing revision.
- **Perf:** an edit in a 4-split layout repaints only the affected view(s); assert unrelated views' render
  caches are reused (revision/viewport/epoch key unchanged); no full-tree solve on a text edit.
- **Settings scope:** a workspace file attempting to override a machine/security-scoped key is rejected and
  surfaced (INV-TRUST-1); buffer-local and view-local shadowing resolves in ABI order.

## Open questions

- **OQ-1** — Detached-View reuse policy: should closing a split ever *retain* a detached `ViewId` (its
  cursor/folds) for a later re-open in a new window, or always retire it? Default here is retire-when-
  view-less-and-unpinned (§3); pinning semantics need a real workflow to validate.
- **OQ-2** — Exact session fidelity for view-local ephemera beyond cursor/viewport/folds (e.g. multi-cursor
  sets, transient marks) — depends on positions-history (D-027) landing; the session scope boundary (§8.3)
  is fixed, the ring contents are deferred.
- **OQ-3** — Whether the register store (§8.3) is persisted by default with the session — ties to
  register-model OQ-4/OQ-6 (usability vs exfiltration on paste); default here is opt-in.
- **OQ-4** — Multi-client (D-012): per-client View vs shared View when two clients focus the same buffer —
  the View-local model makes it an addition, but the *focus/overlay* semantics across clients (whose modal
  is it) need the D-012 decision.
- **OQ-5** — Precise priority-cutoff policy for degraded layout (§2.2): which branch classes collapse first
  in a too-small terminal, and whether it is user-configurable per surface.
- **OQ-6** — Indirect-document scope (§6): how many indirect documents over one base are practical, and
  their interaction with the crash journal (do indirect documents share one journal? — they share the base
  text, so likely yes; confirm with persistence §8).
- **OQ-7** — Interaction of `winfixwidth`/`winfixheight` with priority degradation when even the fixed
  windows cannot all fit (which fixed window yields first).

## Reference Invariants

This doc depends on / enforces (registry: [reference-invariants.md](../invariants/reference-invariants.md);
not redefined here):

- **INV-DOC-VIEW** — Document ≠ View ≠ Window ≠ File; view-local state (cursor/selection/viewport/folds)
  lives only on the View; the Document never knows a View; one Document, many Views. Narrowing is a
  Document *content* fact, not view-local state, and still references no View (§1, §4, §6).
- **INV-HANDLE** — `DocumentId`/`ViewId`/`WindowId`/anchor/snapshot are generation-checked handles; a freed
  handle (dead document/window) is an assert, not an error (§1, §3, Failure modes).
- **INV-BUFFER-KIND** — kind selects the mutation contract; the View turns a gesture into a text
  Transaction, an append, or a domain CommandRequest accordingly (§5).
- **INV-STATUS** — status is a rendered Health Registry subscription; the layout never owns health (§9).
- **INV-RENDER-IR** — every window/overlay/status surface lowers through one Render Tree; no view paints
  cells (§2, §7, §9).
- **INV-ASYNC-ORDER** — the layout epoch + revision guard drops stale resize/render/decoration/write-back
  results on the deterministic executor (§2.2, §5, Failure modes).
- **INV-ANCHOR** — cursors, viewport top, folds, and the narrowing Restriction are anchors; per-Document
  anchor updates are batched across all Views (§1, §4, §6, Performance).
- **INV-QUERY-SNAPSHOT** — Views render from immutable Document snapshots / domain DTOs (§4, §5, Performance).
- **INV-SCHED-1** — a View's background work is scheduler-owned and cancelled on retirement (§3, leak prevention).
- **INV-TRUST-1** — session/settings cannot execute code or override machine/security-scoped settings;
  multi-root trust is per-root (§8.3, §8.4, Security).

Also relied on: **INV-ORIGIN**, **INV-ERR-CLASS**, **INV-FAIL-BOUNDED**, **INV-ADDITIVE**,
**INV-PROTOCOL-VERSIONED**, **INV-REMOTE-FIRST**, **INV-PRIORITY**, **INV-RENDER-PROFILE**, **INV-TXN**,
**INV-UNDO** (see the registry).
