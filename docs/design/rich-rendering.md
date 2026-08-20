---
doc: rich-rendering
project: ruse
title: "ruse Rich In-Buffer Rendering — Faces, Conceal, Virtual Text & Inline Images"
summary: >
  How ruse renders marked-up prose (Markdown, Org) the way Emacs does — hiding markup, applying real faces
  (bold/italic/heading/link), inserting virtual text/lines, and placing inline images — inside the editing
  buffer, in a TUI, with graceful degradation across terminals. Introduces the one missing primitive: a
  decoration model plus a display-coordinate (layout) pass that both painting and the caret consult, so
  hidden/virtual content no longer breaks the 1-byte-1-cell assumption. Scopes this as a partial,
  single-frontend reactivation of the RFC-0009 render IR (deferred by RFC-0012), not the full multi-frontend
  tree.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - render-and-frontends.md
  - view-window-workspace.md
  - terminal.md
  - ../rfc/proposed/RFC-0009-render-model.md
  - ../rfc/proposed/RFC-0005-terminal-capability.md
  - ../parity/terminal.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code
     (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Rich In-Buffer Rendering

## 1. Problem & scope

"Emacs rendering" of Markdown/Org means the editor **renders the markup in place**: `**bold**` shows as
**bold** with the `**` markers hidden, `#` headings become styled title lines, link URLs collapse to their
text, `[ ]`/`[x]` checkboxes and `TODO`/`DONE` keywords get faces, tables and rule lines draw, and
an inline image (Markdown `![alt]` + a `.png` target) shows the actual picture. Emacs does this with its display engine (faces + `invisible`/`display`
text properties + overlays + inline images); Neovim does the equivalent with `extmark`s (`virt_text`,
`virt_lines`, `conceal`, `hl_group`) + a graphics backend.

**ruse has none of this today.** The v0 TUI paints straight from buffer bytes into a flat cell grid with a
strict *1 buffer-line = 1 screen-row, column = Σ grapheme-cell-widths* model — no soft-wrap, no
display-line abstraction, no decoration/extmark layer, and syntax highlighting is **color-only**
(`highlight::Span { start, end, color }`, flattened into a per-byte color array before paint;
`apps/tui/src/ui/render.rs:182-191`). See [render-and-frontends.md §"v0"](render-and-frontends.md).

### 1.1 The TUI ceiling (what is and isn't possible)

This is honest up front because the user asked for it directly — some of this is terminal-gated.

| Emacs/GUI affordance | In ruse's TUI | Mechanism / gate |
| --- | --- | --- |
| Hide markup (`**`, `#`, link URL) | ✅ | conceal (this doc) |
| Real bold / italic / underline / strikethrough / color faces | ✅ | `CellStyle` already carries these (`screen.rs:37-44`); `flush_diff` already emits the SGR (`render.rs:436-477`) |
| Heading emphasis (color + bold + rule line) | ✅ | faces + virtual lines |
| Checkboxes, TODO keywords, tables, block quotes | ✅ | faces + virtual text/lines |
| **Larger** heading glyphs (variable font size) | ❌ | fixed cell grid — a GUI-only affordance (**F-018**); TUI substitutes color/bold |
| Inline images | ⚠️ **terminal-gated** | Kitty graphics / Sixel / iTerm2 where supported; **degrades** to Unicode/ASCII preview → alt-text placeholder elsewhere (INV-CAP-DEGRADE) |

The image row is exactly the "works some places, not others, and that's unavoidable" the user named — and it is
**already the architecture's stated posture**: features degrade in *quality*, never *disappear*
([render-and-frontends.md §3](render-and-frontends.md); RFC-0009; parity TERM-GFX).

### 1.2 In scope / out of scope

**In:** the decoration model; the display-coordinate/layout pass; faces; conceal (+ reveal-at-point); virtual
text & virtual lines; inline images with capability detection + degradation; a Markdown provider; an Org
provider. **Out:** soft-wrap and horizontal scroll (orthogonal layout features that the same layout pass
later enables, but not required here — deferred as today); the full multi-frontend Render Tree / GUI/Web
lowering (stays deferred per RFC-0012 — see §3); plugin-authored decorations (this ships core providers;
opening decoration authorship to plugins is F-016's concern).

## 2. Relationship to the existing architecture

RFC-0009 already locks a **semantic Render Tree** with an `Image` node and the exact degradation ladder we
need, plus INV-RENDER-IR / INV-CAP-DEGRADE / INV-QUERY-SNAPSHOT. **But RFC-0012 paused that whole IR "until a
second frontend exists."** Rich in-buffer rendering does **not** add a second frontend — so we must not
resurrect the full multi-frontend tree. What it *does* add is a second render **tier within the one TUI**,
and that needs exactly one new internal layer: a layout pass from *(bytes + decorations)* to *display cells*.

**Governance stance (the key call):** treat this as a **partial, single-frontend reactivation** of RFC-0009 —
build the *layout pass* and the *decoration model* as internal TUI concerns; keep the backend-neutral,
serializable, multi-frontend Render Tree deferred. Concretely: the decoration model is the in-line facet of
RFC-0009's **Semantic View Model**; the layout pass is a TUI-local lowering step, *not* the versioned wire IR.
This is a new RFC-0012 re-boundary trigger ("a second render tier appears") that reintroduces **only** the
layout pass, and it wants a Decision recording that scoping so the full IR is not smuggled in wholesale.

## 3. The core primitive — decoration model + layout pass

Two new things. Everything else (faces, conceal, virtual text, images, Markdown, Org) is a *consumer* of these.

### 3.1 Decoration model

A decoration attaches presentation to a buffer range. Ranges are **anchored** (they ride edits via the
existing `crates/core` anchor store, so decorations survive typing), and carry any of:

```rust
// illustrative, not normative
struct Decoration {
    range: AnchorRange,          // [start, end) in buffer space; may be empty (a point) for virt_text
    face: Option<Face>,          // style: fg/bg + bold/italic/underline/strike (maps to CellStyle)
    conceal: Option<Conceal>,    // hide the range from layout; optional single replacement char (e.g. ▸)
    virt_text: Vec<VirtChunk>,   // inline cells NOT backed by bytes, placed before/after the range
    virt_lines: Vec<VirtLine>,   // whole display rows inserted above/below (rules, rendered tables, image rows)
    image: Option<ImageRef>,     // a stable image handle to lower as a block (see §6)
    priority: i16,               // resolve overlap deterministically (higher wins per attribute)
}
```

Decorations are produced by **providers** bound to a *visible-range snapshot* and run **outside the paint
critical section** (INV-QUERY-SNAPSHOT). Syntax highlighting becomes the first provider (its `Span` is a
face-only decoration); the Markdown/Org providers (§7) are the rich ones.

### 3.2 The layout pass — one source of truth for paint AND caret

Today `paint_pane` (`render.rs:46-108`) and `cursor_cell` (`render.rs:487-505`) **independently re-derive
layout from raw bytes**, and they must agree byte-for-byte. Conceal and virtual cells break that
"byte-string *is* the layout" assumption in both. The fix is to derive layout **once**:

```
(buffer bytes, decorations, viewport, width)  ──layout──▶  Vec<DisplayRow>

DisplayRow = ordered DisplayCell[]
DisplayCell = { glyph, style, origin }
origin = Byte(offset)      // a real buffer byte-run — the ONLY cells motions/selection map to
       | Virtual           // virt_text / a concealed range's replacement char — no buffer offset
       | ImageCell(handle)  // part of an image block
```

The layout pass is the single owner of the buffer↔display mapping. It exposes two inverse queries that
**replace** the duplicated math:

- `display_of(byte) -> (row, col)` — used by caret placement (kills `cursor_cell`) and popup anchoring.
- `byte_of(row, col) -> byte` — used by mouse/click and by column-motion agreement.

This also finally unifies the **column split** flagged in the audit: core motions currently count *character*
columns (`motion.rs:col_of/at_col/vcol_of`, `View.curswant`) while the caret is *drawn* in *cell* columns —
they agree only for all-width-1 lines. The layout pass makes the display column authoritative;
`goal_col: CellCol` (today design-only in [view-window-workspace.md](view-window-workspace.md)) becomes real,
and vertical motion (`j`/`k`) sticks to a *display* column across concealed/virtual content — matching Emacs.

**Conceal-aware caret rule:** the caret lands on *buffer* positions; when a position is inside a concealed
range, layout maps it to the range's display slot (the replacement char, or the range edge). Motions skip the
hidden interior unless the range is revealed (§5).

### 3.3 The layout algorithm (per source line)

Input: the line's byte slice, the decorations intersecting it (sorted by `(start, -priority)`), the width,
tab width, and the *revealed set* (element extents currently un-concealed under the caret, §5). Output: one or
more `DisplayRow`s (one today; virt_lines add rows; soft-wrap — deferred — would split here).

```
col := 0; emit virt_lines(above)
walk grapheme clusters of the line by byte offset o:
  d := active decorations at o (resolve overlap by priority, §3.4)
  if d.conceal and extent(d) not in revealed:
        if o == d.range.start and d.conceal.replacement is Some(ch):
            push DisplayCell{ ch, face(d), origin: Virtual }; col += width(ch)
        else: skip                                   # hidden: no cell, no column
        continue
  if d.virt_text.before at o: push each as Virtual cells; col += Σ widths
  glyph := cluster at o
  if glyph == '\t': next := tabstop(col); push (next-col) Blank Byte(o) cells; col = next
  else: push DisplayCell{ glyph, face(d), origin: Byte(o) }; col += cluster_width(glyph)
  if d.virt_text.after ends at o: push each as Virtual cells; col += Σ widths
emit virt_lines(below)
```

Faces resolve on the same pass (no second walk). The pass is bounded to the viewport (as the frame already is)
and diffed against the previous frame by the existing `flush_diff`.

### 3.4 Overlap & priority

Decorations may overlap (a heading span containing an emphasis span containing a concealed marker). Resolution
is **per-attribute, highest `priority` wins**, deterministic: faces merge (inner overrides outer field-by-field);
`conceal` is a boolean OR gated by the revealed set; `virt_text`/`virt_lines`/`image` never conflict (they are
additive at distinct anchors). Ties break by `(start asc, len asc, provider-order)` so layout is a pure
function of (bytes, decorations) — required for the frame diff and for property tests.

### 3.5 Coordinate invariants (property-tested)

Let `L` be the layout of a line. These are the slice-1 property tests (the hard-coordinate slice):

- **P1 round-trip** — for every non-concealed byte `b`, `byte_of(display_of(b))` == `b`'s cluster start.
- **P2 monotonic** — `display_of` is non-decreasing in buffer order (row-major); no two distinct visible cluster
  starts share a `(row, col)`.
- **P3 selection = buffer** — yank/operator over `[a,b)` acts on `buffer[a..b)` regardless of conceal
  (presentation-only; upholds INV-DOC-VIEW).
- **P4 caret is real** — `byte_of(row,col)` for any cell returns a real buffer offset; Virtual/ImageCell cells
  snap to the nearest `Byte`-origin cell (the caret never rests on virtual content).
- **P5 goal column in display space** — `j`/`k` preserve the *display* column (`goal_col: CellCol`) across lines
  whose conceal/virtual content differs (matches Emacs; fixes today's char-vs-cell column split).
- **P6 identity when off** — with `render.conceal = off` and no rich provider, `L` is byte-for-byte the v0 1:1
  layout. This is the **regression guard for the slice-0 identity refactor**.

### 3.6 Edge cases

- **Wide clusters + conceal** — a concealed range boundary never splits a width-2 cluster; conceal ranges are
  snapped to cluster boundaries by the provider (grammar nodes already are).
- **Search / diagnostic inside a concealed range** — a match or an error span **force-reveals** its element
  (you must see what `n` jumped to, or where the squiggle is), via the same revealed-set mechanism as the caret.
- **Tabs** — computed in *display* columns inside the pass (the sole owner), removing the TUI-vs-core `TAB_WIDTH`
  divergence.
- **Anchors/undo** — decoration ranges are anchored (`crates/core` anchor store), so they ride edits and undo
  without re-parsing; a stale decoration past an edit is clipped, never panics.
- **Image block + scroll** — an image's `virt_lines` band is clipped at the viewport top/bottom like any rows;
  partially-scrolled images lower a cropped payload (Kitty) or a shorter placeholder (degrade rungs).

## 4. Faces & theme

`CellStyle` (`screen.rs:37-44`) already models `fg/bg/bold/underline/italic/reverse`; the paint path just
never sets more than `fg`+`reverse` (`Screen::put` hardcodes the rest; `put_styled` is the full path used by
the terminal grid and diagnostic underline). Faces are therefore the **least-disruptive** first extension —
the same shape as the existing diagnostic-underline path (`render.rs:96-103`).

A **Face** is a named style (`heading.1`, `emphasis.bold`, `markup.link`, `keyword.todo`, …) resolved through
a **theme table** to a concrete `CellStyle`, degraded per render profile (truecolor → 256 → 16). Faces are
config-visible (`theme.*`), so the census/config path (D-042) owns their defaults; tree-sitter highlight
captures map capture-name → face rather than capture-name → raw color.

## 5. Conceal (the product-shaping decision)

Conceal hides a buffer range from layout (0 cells, or 1 replacement char). The **caret behavior** is the
genuine product decision (resolved here — see D-A in §9):

- **Reveal-at-point:** an element is *revealed* (its markup shown un-concealed, editable) iff the caret's
  buffer position lies within the element's **extent** — the concealed range plus its owning grammar node
  (the whole `**bold**`, markers included), not just the marker. Practically: reveal every concealable range
  whose element's source line is the caret's line (Vim `concealcursor` unset on the current line; Emacs org's
  markers appear when point is on them). This is what makes editing rendered prose bearable.
- **`conceallevel`-style config** (Vim analogue): `render.conceal = off | markers | full`. `off` = show all
  markup (plain text, today's behavior); `markers` = hide emphasis markers but keep structure; `full` = hide
  everything concealable. **Default (chosen): `off` globally**, auto-`markers` for `.md`/`.org` buffers — a
  code editor shows raw bytes by default and only softens markup in prose filetypes. Resolved through the
  config/census path (D-042); user-overridable per buffer.
- **Motion:** horizontal motion over a fully-concealed (un-revealed) range steps to its far edge (the interior
  has no landing cells); when revealed (caret on its line), motion is ordinary grapheme motion. Vertical
  motion keeps a **display** goal column (P5). Selection/yank/operators always act on **buffer** content (you
  copy the `**`, even if hidden) — conceal is presentation-only, never mutation (INV-DOC-VIEW: the document is
  unchanged; only its display differs).
- **Force-reveal:** a search hit (`n`/`N`) or a diagnostic span inside a concealed range reveals that element
  regardless of the caret line, so you always see what you navigated to (§3.6).

## 6. Virtual text & virtual lines

- **`virt_text`** — inline cells with no backing byte, placed before/after a range: rendered link labels,
  checkbox glyphs, list bullets (`-` → `•`), and the substrate LSP **inlay hints** (F-014) will reuse.
- **`virt_lines`** — whole display rows inserted above/below a buffer line: Markdown horizontal rules, a
  rendered table drawn under its source, block-quote rails, and the **row band an inline image occupies**.

Both flow through the layout pass as `origin = Virtual` cells; caret and motion never land on them.

## 7. Markdown & Org providers

Providers consume a read-only parse **snapshot** (tree-sitter, the F-015 pipeline) and emit decorations —
they never mutate. Grammars are new deps behind the existing `grammar_for` dispatch (`highlight.rs:32`):

- **Markdown** — `tree-sitter-md` (+ its inline grammar). Map: ATX/setext headings → `heading.N` face +
  optional rule `virt_line`; emphasis/strong → face + conceal markers; inline/fenced code → face; links →
  conceal URL, face the label; images → §8; lists → bullet `virt_text`; task list items → checkbox glyph;
  block quote → rail; tables → faces now, rendered `virt_lines` later.
- **Org** — `tree-sitter-org`. Headings/`TODO`/`DONE`/priorities/tags, `#+begin_…` blocks, emphasis
  (`*bold*`, `/italic/`, `=verbatim=`), links `[[target][label]]` (conceal target, face label), tables,
  checkboxes. Org rides **entirely** on the primitives from §3–§6 — it adds a grammar + a mapping table, no
  new engine work. This is the payoff of building the primitive first.

Backing capability: repoint `CAP-MARKDOWN` (today `prd:[]`) and add `CAP-ORG` to the new feature; the provider
layer itself is a core render capability (see §9), not a plugin — plugin-authored providers are a later F-016
extension.

## 8. Inline images

An image is a **`virt_lines` block** of `ImageCell`s referencing a **stable image handle** (RFC-0009 RIR:
handles, never inlined bytes in any IR). The block reserves *N display rows × M cols*; the actual pixels are a
**lowering** concern, chosen by capability and pinned per client-view (INV-RENDER-PROFILE).

### 8.1 Capability detection (ledger additions)

Per the F-010 ledger pattern (`caps/ledger.rs`, `probe.rs`), add:

- `Capability::InlineGraphics` with a **ladder** `CapValue::Graphics(Kitty | Sixel | ITerm2 | None)` (modeled
  like `KeyEncoding`, because graphics degrade rather than toggle).
- **Probes** (ride the existing DA1-fenced batch — no timeouts): **Sixel** is reported *in the DA1 reply
  params* (a `4`) — cleanest, just parse params `classify` already discards. **Kitty graphics** = an
  `APC _G` transmit-and-query (needs an APC introducer arm in `ProbeParser`, which today only scans CSI).
  **iTerm2** = env-hint from `$TERM_PROGRAM` in `seed_env`.
- **Pixel-cell size** (needed to size an image to N cells): none exists today. Add `CSI 14 t` (text-area
  pixels) / `CSI 16 t` (cell pixel size) to the probe batch + a `t`-final arm in `classify`. Guard the
  pixel-vs-cell confusion flagged at `architecture.md:692`.
- A `RUSE_GRAPHICS=off|kitty|sixel|iterm2` override (precedence: UserOverride > Probed > EnvHint > Default).
- **When several are detected, prefer Kitty > Sixel > iTerm2** (best quality/most capable first). Kitty is the
  **first protocol implemented** (§10 slice 3b); Sixel second for legacy breadth.

Thread the resolved graphics tier off `TermGuard` into paint exactly as `sync_output` is pinned once
(`session.rs:138`) and passed to `flush_diff` — no live re-probe on frame noise.

### 8.2 The degradation ladder (INV-CAP-DEGRADE)

```
Kitty graphics / Sixel / iTerm2   → real inline image (sized via CSI 14/16 t)
256-color, no graphics            → downsampled Unicode half-block / braille preview
16-color / unknown                → a bordered placeholder: alt text + dimensions
```

Decode/scale happens off the paint critical section; the image cache is keyed by the stable handle. A runtime
lowering failure pins that node to the compatibility rung (placeholder), never tears the screen
(INV-RENDER-PROFILE). Decoding untrusted image bytes is a sandboxed, size-bounded step (a decode bomb must not
OOM the editor — bound dimensions before allocation).

## 9. Governance & spec deltas

This is **architecture-tier** (it introduces the display-coordinate boundary and reactivates part of RFC-0009).
The change set:

- **New PRD feature** — e.g. `F-031 Rich in-buffer rendering (prose mode)`, stage post-mvp. Acceptance drafted
  around: markup conceals with reveal-at-point; faces render (bold/italic/heading); a fenced image shows real
  on a capable terminal and a labelled placeholder elsewhere (degrade); Org and Markdown both render; the
  document bytes are never mutated by rendering (INV-DOC-VIEW).
- **Capabilities** (`spec/capabilities.yaml`) — repoint `CAP-MARKDOWN` (drop `prd:[]`); add `CAP-ORG`; add a
  render capability for the decoration/layout primitive (e.g. `CAP-DECORATION`, base/service); add
  `CAP-GRAPHICS` (client, replaceable, the inline-image lowering). Wire `requires` (decoration ← syntax
  snapshot; graphics ← terminal caps).
- **RFC** — an **RFC-0009 addendum** (or a new RFC referencing it) recording the *partial single-frontend
  reactivation*: the layout pass + decoration model are TUI-local; the multi-frontend Render Tree stays
  deferred. Plus a note in RFC-0005 for the graphics capability + pixel-size probe.
- **Invariants** — restate (not mint) INV-CAP-DEGRADE, INV-RENDER-PROFILE, INV-QUERY-SNAPSHOT, INV-DOC-VIEW;
  the layout pass is where INV-DOC-VIEW is *proven* for rendering (presentation ≠ mutation).
- **Deps** — `tree-sitter-md`, `tree-sitter-org`, an image decode crate (bounded).

### 9.1 Decision bodies (draft for `spec/DECISIONS.md`)

- **D-A — Conceal caret semantics.** Conceal is presentation-only (INV-DOC-VIEW); the document is never
  mutated by rendering. An element is revealed iff the caret lies within its grammar-node extent (reveal on
  the caret's source line); search hits and diagnostics inside a concealed range force-reveal. Horizontal
  motion steps over un-revealed concealed ranges to their far edge; vertical motion keeps a display goal
  column. Selection/yank/operators act on buffer bytes, not the concealed display. `render.conceal` default =
  `off` globally, auto-`markers` for `.md`/`.org`. *Re-evaluate if a filetype needs a third caret policy or if
  reveal-on-line proves too eager for dense prose (then fall back to reveal-on-element-only).*
- **D-B — Inline-graphics ladder & detection.** `Capability::InlineGraphics` is a ladder
  `{Kitty > Sixel > iTerm2 > None}`, not a bool. Detection: Kitty via `APC _G` transmit-and-query, Sixel via
  the DA1 param `4`, iTerm2 via `$TERM_PROGRAM` env-hint; precedence UserOverride > Probed > EnvHint > Default
  (the existing ledger rule). When multiple are present, prefer the highest rung. Cell pixel size via
  `CSI 14 t`/`CSI 16 t`, on the DA1-fenced batch. The tier is pinned per client-view (INV-RENDER-PROFILE);
  lowering failure falls to the compatibility rung. Images lower real → Unicode preview → labelled placeholder
  (INV-CAP-DEGRADE). *Re-evaluate when a fourth protocol or a GUI backend (F-018) appears.*
- **D-C — Partial single-frontend reactivation of RFC-0009 (RFC-0012 re-boundary).** A second render **tier**
  within the one TUI (rich in-buffer rendering) is a re-boundary trigger that reintroduces **only** the
  TUI-local layout pass + decoration model — the in-line facet of RFC-0009's Semantic View Model. The
  backend-neutral, serializable, `schemaVersion`-carrying multi-frontend Render Tree stays **deferred** until a
  real second frontend (GUI, F-018). The layout pass is an internal lowering step, not the versioned wire IR;
  nothing here forecloses the full IR later (the decoration model is a strict subset of it). *Supersede only by
  superseding this scoping when F-018's GUI backend lands.*

### 9.2 F-031 acceptance (draft)

1. In an `.md` buffer, `**x**` renders `x` bold with the `**` markers hidden; moving the caret onto that word
   reveals the markers (editable), and moving away re-hides them.
2. ATX headings render with a distinct face; **the document bytes are unchanged** — a content hash before and
   after any number of render frames is identical (INV-DOC-VIEW).
3. A fenced Markdown image (`![alt]` + a `.png` target) shows a real image on a Kitty-graphics terminal and a bordered `alt` + dimensions
   placeholder on a plain terminal — **same buffer, two render profiles** (INV-CAP-DEGRADE).
4. With `render.conceal = off`, rendering is **byte-for-byte identical** to plain syntax highlighting — no
   column drift, caret and paint agree (P6; the slice-0 identity guard).
5. Markdown and Org both render headings, emphasis, links (URL concealed, label faced), and checkboxes.
6. Selecting and yanking across concealed markup copies the **raw markup bytes** (`**`, `[[…]]`), proving
   conceal is presentation-only.

## 10. Slicing plan (honest, incremental — each slice ships something testable)

- **Slice 0 — layout pass + faces (the foundation).** Introduce the `DisplayRow`/`DisplayCell` layout pass as
  an **identity refactor first** (byte-for-byte identical output; `paint_pane` and `cursor_cell` now both
  consume it — kills the duplication). Then thread **faces**: tree-sitter captures → faces → `CellStyle`, so
  bold/italic/heading colors actually render. First visible win + the whole primitive's seam, with the
  coordinate model still 1:1. *Architecture-tier; the riskiest structural step, isolated with no behavior
  change in phase one.*
- **Slice 1 — conceal.** Hidden ranges shift display columns; reveal-at-point; `conceallevel` config; motion
  agreement. First consumer: a minimal Markdown provider (conceal emphasis markers, face headings/emphasis).
  *This is the hard coordinate slice — property-test the buffer↔display inverse and motion/selection
  invariance.*
- **Slice 2 — virtual text & virtual lines.** Bullets, checkboxes, rule lines, block rails; substrate for LSP
  inlay hints. Markdown provider grows.
- **Slice 3 — inline images.** 3a: ledger graphics detection + pixel-cell probe + degrade scaffolding (a
  placeholder block, no pixels yet — testable without a capable terminal). 3b: the **Kitty graphics** lowering
  (chosen first, §8.1) as a `virt_lines` block, with the Unicode-preview and alt-placeholder degrade rungs;
  Sixel follows. Markdown `![]()` renders.
- **Slice 4 — Org provider.** `tree-sitter-org` + a mapping table only; rides slices 0–3 entirely.

Slices 0–2 need **no** special terminal; slice 3 is the only terminal-gated one and is built degrade-first so
it's testable and useful everywhere.

## 11. Direction (decided) & remaining questions

**Decided (this review):**

1. **Markdown first** — `tree-sitter-md` is the slice-1 consumer that proves the primitive; **Org is slice 4**
   (grammar + mapping table only).
2. **conceallevel default `off`**, auto-`markers` for `.md`/`.org` (D-A).
3. **Kitty graphics first**, Sixel second (D-B, §8.1).
4. **Dedicated `F-031`** (a distinct capability), not folded under F-015/F-006.

**Still open (do not block slice 0):**

- **Reveal granularity** — reveal-on-caret-*line* (chosen default) vs reveal-on-*element*-only, if line-reveal
  proves too eager in dense prose (D-A re-eval hook).
- **Soft-wrap** — now *cheap* once the layout pass exists (a line → multiple `DisplayRow`s is already the
  pass's shape), but still **out of scope** here; a follow-up once rich rendering lands.
- **Decoration-provider budget** — the per-frame time budget for providers (ties to the open scheduler budgets,
  D-018); start unbounded-but-snapshot-bounded, measure, then cap.

## 12. Alternatives / rejected / trade-offs

- **Rejected: a separate read-only "preview pane"** (render Markdown in a second window, à la many editors).
  Not what was asked (Emacs renders *in* the buffer), and it splits editing from viewing. → in-buffer
  decorations.
- **Rejected: ship Markdown rendering as an F-016 plugin/pack** (the current `CAP-MARKDOWN` framing). Blocks
  it on the plugin epic, and the *primitive* (layout pass + conceal) must exist in core regardless — a plugin
  can't invent a display-coordinate model. → build the primitive in core; open provider authorship to plugins
  later.
- **Rejected: bolt conceal onto `paint_pane` directly** (skip byte ranges inline). It desyncs `cursor_cell`
  and every byte-indexed range test; the two layout derivations would drift. → one shared layout pass.
- **Rejected: reactivate the full RFC-0009 multi-frontend Render Tree now.** Over-scoped — there's no second
  frontend; the serializable backend-neutral tree earns its complexity only when GUI/Web arrive (F-018). →
  partial reactivation: the TUI-local layout pass only.
- **Trade-off: a layout pass adds an allocation + indirection per frame.** Bounded to the visible range and
  diffed against the previous frame (as today); providers run off the critical section. Accepted — it is the
  only correct home for conceal/virtual/images, and it fixes the pre-existing motion/caret column split as a
  side effect.
- **Trade-off: variable heading sizes are impossible in a TUI.** Accepted — substitute color/bold; real
  variable glyphs are a GUI-frontend (F-018) affordance, and the decoration model already carries the
  semantic face so the GUI can honor it later for free.

## 13. Reference invariants (this doc)

Restates, does not mint (new INV IDs live only in the invariants registry, D-022):

- **INV-DOC-VIEW** — rendering (conceal/virt/images) never mutates the document; presentation is view-only.
- **INV-CAP-DEGRADE** — images degrade real → Unicode preview → placeholder; graphics is a confidence-ledger
  ladder, never a bare `TERM` sniff.
- **INV-RENDER-PROFILE** — the graphics tier is pinned per client-view; a lowering failure falls to the
  compatibility rung, never flips mid-frame.
- **INV-QUERY-SNAPSHOT** — decoration providers are bound to a visible-range snapshot and run outside the
  paint critical section.
