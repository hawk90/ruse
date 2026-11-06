---
doc: render-and-frontends
project: ruse
title: "ruse Render Model & Frontend Separation"
summary: >
  How ruse separates a reusable core engine from thin frontends, splits Command (mutation) from Query
  (read), and lowers a single semantic Render Tree (paint IR) to multiple backends (ANSI, Kitty, SIXEL,
  GUI, Web). Includes the compatibility-vs-enhanced render profile (pinned per client-view), the full
  transformation pipeline, and the thin-app/engine split.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md
  - ../protocols/versioning-and-evolution.md
  - ../invariants/reference-invariants.md
  - ../parity/terminal.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Render Model & Frontend Separation

ruse is structured as **core → command/query → render IR → multiple frontends**: a Rust core owns
document/edit/render, and TUI/GUI/Web/remote frontends sit on top.

## 1. Core / Frontend Separation

The core owns Document, Transaction, Command, Query, Undo, Anchor, Workspace. Frontends (TUI, GUI, Web,
Remote Client) sit on top.

```
Editor Core                    Frontends
├─ Document                    ├─ TUI
├─ Transaction                 ├─ GUI
├─ Command                     ├─ Web
├─ Query                       └─ Remote Client
├─ Undo
├─ Anchor
└─ Workspace
```

**Governing principle:** *the TUI is not the editor — it is the first client of the core.* Holding this
lets GUI/Web attach later without rewriting the core. (Invariant: core has no knowledge of a specific
frontend — see [architecture.md](../architecture/architecture.md) §3.1; guards CORE-6/8/18/20.)

## 2. Command / Query Separation (CQRS, applied at boundaries)

The core splits **mutation (Command)** from **read (Query)**.

| Command (mutation) | Query (read) |
| --- | --- |
| `insert_text`, `delete_range`, `change_selection`, `open_document`, `split_view` | `get_visible_lines`, `get_cursor_position`, `get_diagnostics`, `get_render_snapshot`, `get_available_commands` |

Plugins never touch the document directly:

```
Plugin → CommandRequest → Validation → Transaction → State update
```

Queries return a **snapshot / DTO**, never a live mutable object — this raises stability (a stale/aliased
mutable handle can't corrupt core state).

> **Do not over-apply CQRS.** Turning every getter into a message adds complexity. Apply the strong form
> only at **mutation, remote, and plugin boundaries**; in-process reads can stay direct.
> (Guards CMD-14/15, PLUGIN-2/3; upholds INV-CMD-SEMANTIC, INV-PLUGIN-NO-CORE.)

## 3. Common Render IR (Semantic Render Tree)

ruse places a **common paint IR before any output backend**. Plugins and views produce semantic nodes;
backends lower them per capability.

```
Semantic View
    ↓
Render Tree (paint IR)        ← this is the stable, versioned contract
    ↓
Frontend Lowering
    ├─ ANSI Cell Grid
    ├─ Kitty Image
    ├─ SIXEL
    ├─ GUI GPU
    └─ Web Canvas
```

```rust
pub enum RenderNode {
    Text(TextNode),
    Image(ImageNode),
    Table(TableNode),
    Tree(TreeNode),
    Diff(DiffNode),
    Overlay(OverlayNode),
}
```

**Plugins emit `ImageNode`, never a Kitty escape sequence.** The TUI backend then lowers by capability:

```
Kitty            → real image
plain terminal   → Unicode preview
low-capability   → image-info placeholder
```

This is the same degradation ladder as the terminal parity doc ([parity/terminal.md](../parity/terminal.md)
TERM-GFX) and is a **core product differentiator**: features degrade in quality, never disappear
(INV-CAP-DEGRADE; guards TERMOUT-10/11, PLUGIN-8/9).

The Render Tree carries a `schemaVersion` and evolves additively — see
[protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md).

## 4. Compatibility Path vs Enhanced Path (pinned per client-view)

Two renderer tiers, chosen once and held:

| Compatibility Renderer | Enhanced Renderer |
| --- | --- |
| ANSI, Unicode, 256 colors, legacy keyboard | true color, Kitty keyboard, synchronized output, inline images |

**Key rule:** rather than mixing features and getting an unstable screen, **pin a render profile per
client-view.** Once `terminal profile = compatible` is decided for a client-view, it uses the ANSI/Unicode
path stably even if a later capability probe result wobbles — the backend does **not** keep switching.

> **Multi-client (verification V-13):** the profile is pinned per **client-view**, not per session/document —
> so two clients of differing capability (e.g. Kitty + a plain terminal) attached to the same document each
> lower the *shared Render Tree* at their own tier. Multi-client attach itself is post-MVP and gated by
> **DECISIONS D-012**; per-client-view pinning is the mechanism that makes it coherent.

On unsupported elements or runtime failure, the document is pinned to the compatibility path. (Guards the
RENDER anti-pattern "flipping render backend mid-session"; upholds INV-CAP-DEGRADE.)

Selection logic: probe capabilities ([parity/terminal.md](../parity/terminal.md) TERM-PROBE) → decide tier
→ record in the capability ledger with a user override → **freeze for the client-view**. Re-evaluate only
on explicit events (resize to a new terminal, user override change, reconnect).

## 5. The Transformation Pipeline (each stage dumpable)

The editor is a multi-stage transformation system; every intermediate result must be inspectable:

```
Input
  → Semantic Command        (:debug keymap / :debug command)
    → Transaction           (:debug transactions)
      → Document State       (:debug document — revision, ranges)
        → Semantic Render Tree   (:debug render-tree)
          → Terminal-specific Output  (:debug capabilities + lowering)
```

Benefit: when something looks wrong, you don't say "the terminal is weird" — you localize precisely:

```
Command:      OK
Transaction:  OK
Document rev: OK
Render Tree:  OK
Kitty lowering: placement is wrong   ← found it
```

Debug surfaces are **product features**, not ad-hoc logging — specified in
[stability-and-observability.md](stability-and-observability.md) §Debug-Surfaces.

## 6. Thin App + Reusable Engine

Keep apps thin so the TUI doesn't become a monolith; the engine is reusable.

```
ruse-core (crates/core)   ruse-tui (apps/tui)        GUI (future, F-018)
├─ document               ├─ terminal input          ├─ native window
├─ command                ├─ ANSI rendering          ├─ GPU rendering
├─ transaction, undo      ├─ clipboard               └─ OS integration
└─ registers, anchors     ├─ LSP, remote, persistence
                          └─ process bootstrap
```

`crates/core` is IO-free and reusable. `apps/tui` holds everything that does IO, so it is not thin today; a
second frontend is what would force more of it into a shared crate (D-039).

## 7. Structural Guardrails

- **No too-wide monorepo.** A root that mixes core, web studio, browser/IDE extensions, mobile, npm,
  samples, and generated outputs becomes unmanageable. ruse splits strictly early (`crates/ apps/
  extensions/ sdk/ tests/ docs/ tools/`) and keeps sample outputs / large corpora in a separate repo or
  Git LFS.
- **Too many backends early.** Follow the render sequencing under Build Order in [../README.md](../README.md).
- **Folder ≠ crate.** Two crates (D-039); `document/transaction/command/anchor` are modules inside `core`
  until a real boundary demands a split. (Guards CORE-11/12/13.)

## v0 — TUI viewport & vertical scroll (SHIPPED, not the deferred IR above)

> Sections 1–7 describe the **deferred** multi-frontend render-IR architecture (paused per RFC-0012 until a
> second frontend is actually started). This section is the **live v0**: the single `ruse` TUI binary
> renders a plain `Document` directly, with no IR. Folds (`apps/tui/src/folds.rs`) and virtual rows
> ([rich-rendering.md](rich-rendering.md)) adjust the row walk on top of this model.

**The frontend computes the viewport.** The core stays IO-free (INV-DOC-VIEW): it knows nothing of columns,
terminal size, or the scroll rule. The viewport is a single `top` offset (the first visible buffer line) stored
per View (`View.top`, so two Views of one buffer scroll independently) and recomputed each frame by the
frontend viewport pass from the cursor's row.

**Line layout (#615, #616).** `ui/line_layout.rs` lays out one buffer line as nvim v0.12.6's
`charsize_regular` does: every grapheme gets a size in virtual columns that includes what the screen inserts
for it (the `>` filler for a wide glyph that does not fit in a row's last cell, the `'showbreak'` /
`'breakindent'` head of each continuation row, the `'linebreak'` padding), so with `'wrap'` a cell's virtual
column is `row × width + col`. `'list'` / `'listchars'` decide what tabs, spaces, nbsp and the line end show.
A character nvim does not print shows transcribed, one cell per character, never raw (#1181): `^X` for a C0
control and `^?` for DEL (`<xx>` under `'display'` `uhex`), `<xx>` for a C1 control, `<xxxx>` for the format
and bidi-control characters nvim's `utf_printable` refuses. `Screen::put_styled` transcribes too, so no
control byte from a buffer, a file name or a message ever reaches the terminal. The window's gutter
(`'foldcolumn'` with nvim's `fill_foldcolumn` marks, `'signcolumn'` with the diagnostics' `E`/`W`/`I`/`H`,
then `'number'` / `'relativenumber'` / `'numberwidth'`) narrows the text area; `ui/window_view.rs` resolves a window's options
(window-local on its View, global-local over the session's globals) into a `WinView` per frame, which the
viewport pass, the painter and the key handlers share. The layout is unit-tested against 528 nvim screens.

**Scroll-to-cursor.** Before each draw, `viewport::update_topline` (nvim's `update_topline` without
'smoothscroll': `scroll_cursor_top` / `scroll_cursor_bot` / `scroll_cursor_halfway`) moves `top` so the cursor
line shows whole with `'scrolloff'` rows of context, counting wrapped rows and a closed fold as one row; a jump
further than about half a window centres the cursor line, and `'scrolljump'` is the least it scrolls. A cursor
line taller than the window shows from the row that keeps the cursor on screen (nvim's `curs_columns`
`w_skipcol`, kept on the View as `skipcol`). Without `'wrap'` the window
scrolls sideways (`View.leftcol`): nvim's `curs_columns` with `'sidescroll'` / `'sidescrolloff'`, plus
`zh` / `zl` / `zH` / `zL` / `zs` / `ze`. The screen-line motions `gj` / `gk` / `g0` / `g^` / `gm` / `g$` are
resolved by the frontend in these rows (nvim's `nv_screengo`) and handed to the engine as a `go`-style byte
target, so operators compose and the trace replays them.

**Scroll keys.** `scroll.rs` ports nvim's `move.c` scroll commands over the same rows: `CTRL-E` / `CTRL-Y`
(`scroll_redraw`, `scrollup`, `scrolldown`), `CTRL-D` / `CTRL-U` by the window's `'scroll'` and
`CTRL-F` / `CTRL-B` by a page less nvim's overlap (`pagescroll`, `scroll_with_sms`, `get_scroll_overlap`), with
the `'scrolloff'` correction (`cursor_correct`, `cursor_correct_sms`). As in nvim the page commands scroll by
rows with `'smoothscroll'` switched on and then put the top line back on a line boundary, so over wrapped
lines the view and the cursor move by different amounts. With `'smoothscroll'` the top line can start inside
a line (`skipcol`), marked `<<<`. A scroll that nvim marks `VALID_TOPLINE` is kept by the next frame until
the cursor or the text moves. Known gap: after a scroll that lands short of nvim's wanted column (the
`CTRL-U` screen column on a shorter line), a later `j` / `G` aims at the landing column, because the core has
no command that sets the wanted column alone.

**Rendering.** `render` paints each window's lines from `top` through the layout, the last line that does not
fit as `'display'` says (`lastline` `@@@`, `truncate`, or `@` rows), and places the terminal cursor on the
cursor's cell (a tab's last cell in Normal mode, as nvim). Rows after the last line show `'fillchars'` `eob`
(`~`); a closed fold's row is filled with `fold`; `vert` / `horiz` draw the split dividers. `'cursorline'`
(`'cursorlineopt'` `line` / `screenline` / `number`), `'cursorcolumn'` and `'colorcolumn'` are background
overlays at virtual columns, as nvim places them: on the row of a wrapped line that holds that column, and on
no row past the line's last. `'statuscolumn'` is not supported: its value is an expression language
(`%{...}` evaluates Vimscript), and running Vimscript is a non-goal; `:set stc=...` says
`option not supported`.

**Long messages (#1222).** The last row is both the status bar and the message area: the message follows the
mode, name and flags. A message that does not fit in the rest of that row is not cut. It is shown the way
nvim v0.12.6 shows a message longer than its command line (probed in a PTY at 80x24, `:help hit-enter`):
wrapped at any cell across the rows above, and `Press ENTER or type command to continue` on the last row
(`ui/message.rs`). `<CR>`, `<NL>`, `<Space>` and `CTRL-C` clear it and are used up; any other key clears it
and then runs as typed, `<Esc>` and `:` included. nvim's `wait_return` puts those keys back the same way. A
file message (`"name" 3L, 12B written`, `"name" [New]`) is cut at its start with `<` instead, as nvim's
default `'shortmess'` flag `t` does; when the bar's mode and name alone are wider than the screen, the file
message is cut to the whole row and the prefix is left out (#1235).

**Paging and history (#1235).** A message taller than the screen is paged from its top with nvim's
`-- More --` keys (`:help more-prompt`, probed at 80x24): `<CR>`/`<NL>`/`j`/`<Down>` one row,
`d` half a screen, `<Space>`/`f`/`<PageDown>`/`CTRL-F` a screen, `G` the end; `<BS>`/`k`/`<Up>`,
`u`, `b`/`<PageUp>`/`CTRL-B` and `g` back; `q`/`<Esc>`/`CTRL-C` end it, `:` ends it and opens the command
line, and any other key shows the key help after `-- More --`. The last page ends at the hit-enter prompt,
where `b`/`k`/`u`/`g`/`<Up>`/`<PageUp>` scroll back and `j`/`d`/`f`/`<Down>`/`<PageDown>` are ignored.
Every message is given with a kind where it is set (`app/messages.rs`: error, warning, info, echo); errors
fail the command, and all but echoes are kept in the history `:[count]mes[sages]` shows and
`:[count]mes[sages] clear` trims, sized by `'messagesopt'` `history:` (default 500). What nvim keeps and does
not keep is listed at the top of `app/messages.rs`. Differences from nvim:
- nvim's message gets a whole command-line row. ruse's shares the row with the bar, so a message that would
  fit nvim's row, but not the space left after a long file name, also asks for `<CR>`.
- The pager's first page starts at the message, not at the typed command line nvim scrolls up with it; there
  is no `g<`, `'more'`, `'cmdheight'`, or `'shortmess'` flag other than the built-in `t`; `'messagesopt'`
  `wait:` and `progress:` are checked and kept but ruse always behaves as `hit-enter`.
- Listings ruse shows in an overlay (`:reg`, `:marks`, `:jumps`, `[I`, multi-row `:p`) and `:!` output (one
  row joined with ` | `) do not go through the pager.

## Reference Invariants (this doc)

- **INV-RENDER-IR** — All output is produced by lowering a single semantic Render Tree; no view or plugin
  emits backend-specific bytes (escape sequences, GPU calls) directly. (Guards TERMOUT-10, PLUGIN-8/9.)
- **INV-RENDER-PROFILE** — A render profile (compatibility | enhanced) is pinned per client-view and not
  switched mid-session on probe noise. (Guards RENDER anti-patterns; upholds INV-CAP-DEGRADE.)
- **INV-QUERY-SNAPSHOT** — Queries return immutable snapshots/DTOs, never live mutable core objects.
  (Guards CMD-14/15.)
- Reaffirms **INV-DOC-VIEW** (core knows no frontend) and **INV-CAP-DEGRADE** (features degrade, not vanish).

## Alternatives / Rejected Ideas / Trade-offs

- **Rejected: frontends talk to the document model directly.** Fast initially; couples every frontend to
  core internals and blocks GUI/Web/remote. → command/query boundary + render IR.
- **Rejected: each view/plugin emits its own terminal escapes.** Produces unstable screens and blocks
  capability degradation. → single Render Tree + backend lowering.
- **Rejected: dynamically pick the best renderer per element.** Visual instability; hard to reason about. →
  pin one profile per client-view.
- **Trade-off:** an IR + lowering layer adds indirection and a schema to version. Accepted: it is the
  substrate for multi-frontend parity, capability degradation, and per-stage debuggability — the point of
  the architecture.
- **Trade-off:** CQRS at boundaries adds message types. Accepted only at mutation/remote/plugin edges;
  in-process reads stay direct to avoid over-design.
