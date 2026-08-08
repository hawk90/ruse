---
doc: parity-emacs
project: ruse
title: "Parity: Emacs Interaction & Extension Model"
summary: >
  Feature-parity target set for GNU Emacs — the interaction and extension model (not Elisp):
  prefix keys & keymap layering, M-x command model, point/mark/region & mark ring, kill ring,
  universal argument, keyboard macros, major/minor modes, buffer-local vars & hooks, minibuffer/
  completion/isearch, transient maps, dired & everything-is-a-buffer, comint, help, undo/narrowing/
  rectangles/registers/bookmarks. Grounded in the GNU Emacs & Elisp manuals.
audience: [maintainers, contributors, llm-agents]
status: draft
source_of_truth: false
verified_against_upstream: false
sources_root: https://www.gnu.org/software/emacs/manual/html_node/
related:
  - README.md
  - vim.md
  - native-style.md
  - ../architecture/architecture.md
---

# Parity: Emacs Interaction & Extension Model

> **⚠️ NOT THE SOURCE OF TRUTH (D-043).** This file is hand-authored and has never been checked
> against a pinned upstream revision. The parity source is the machine-derived census in
> [`spec/parity/inventory/`](../../spec/parity/inventory/), generated from the SHA pins in
> [`spec/parity/upstreams.yaml`](../../spec/parity/upstreams.yaml). These tables survive as *human
> annotation* — reading, grouping and intent — and are migrating onto census IDs. Do not add rows
> here to record a newly discovered upstream feature: humans classify, they do not enumerate.


Emacs is ruse's model for the **command / buffer / extension** dimension (the Emacs Style profile, and
Native Style's command-discovery + special-view layers). We target the **semantic models**, not the
keystrokes alone — several Emacs concepts differ *in kind* from a conventional editor and must be
reproduced as models, not shortcuts. Out of scope: running Elisp / actual Emacs packages (L3).

> **Load-bearing rule:** reproduce these *semantics*, not just bindings — kill ring ≠ clipboard,
> region ≠ selection, undo entries are themselves undoable, every surface is a buffer.

## EMACS-KEYMAP — Prefix keys & keymap layering
Source: `Prefix-Keys`, `Active-Keymaps`, `Searching-Keymaps`, `Controlling-Active-Maps`.
- Prefix keys (`C-x C-c C-h M-g C-x 4 C-x r ESC`) bind to *sub-keymaps*, not commands; nesting.
- **Lookup precedence (highest first)** — the key extension mechanism to reproduce.

> **MIGRATED TO THE CENSUS (D-043).** The precedence stack is now enumerated by runtime introspection at the
> pin as the [`keymap_tier`](../../spec/parity/inventory/emacs/keymap_tier.yaml) surface
> (`emacs.keymaptier.*`), and what ruse promises for it is
> [`keymap-layers.yaml`](../../spec/parity/contracts/keymap-layers.yaml) `profiles.emacs-style` — a depth-9,
> unsealed, buffer-selected stack. **EMACS-KEYMAP-2 is dropped**; F-012 cites the nine census ids directly.
>
> **The hand-authored table this replaced had SIX ranks. The census counted NINE.** That gap is the concrete
> cost of enumerating by hand: `emulation-mode-map-alists` and `minor-mode-overriding-map-alist` were missing
> entirely, and `local-map` (a text/overlay property) was conflated with `current-local-map` (the major-mode
> map) — two different mechanisms under one row. Three of the nine had nowhere to live, and one of the
> six was two tiers wearing one name.
>
> A second measurement from the same census sizes the stakes: **613 of Emacs's 1,952 keyboard bindings live
> in major-mode maps** — a tier Vim's model has no seat for at all. That is what forced D-045 (one ordered
> layer stack) and then D-046 (scope and provenance are different axes).

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-KEYMAP-1 | Prefix keys select sub-keymaps; multi-key nesting | L1 | Equivalent | high |
| EMACS-KEYMAP-3 | Per-buffer / per-text-span keymaps | L1 | Equivalent | med |

EMACS-KEYMAP-3 survives on purpose: "per-buffer / per-text-span" is a statement about *what selects a layer*
(the buffer, and the text under point), which the tier enumeration does not by itself assert. Forcing it onto
a census id would claim evidence the census does not carry — the same cut vim.md makes for VIM-MODE-3/8.

**Semantic model:** key resolution is dynamic and compositional — a minor mode shadows a major-mode key
without either knowing. In ruse this is the **scope axis** — the layer stack (D-045) — with the Context Key
Resolver evaluating `when` inside the winning layer and **provenance** deciding among what is left (D-046).

## EMACS-CMD — M-x and the command model
Source: `M-x`, `Defining-Commands`, `Interactive-Call`.
- Every action is a named **interactive command**; `M-x` runs any by name (with completion) and shows its
  key binding. The `interactive` spec declares *how args are gathered* (region, prefix arg, prompt, file).
- Discovery: `C-h b` (bindings), `C-h w` (where-is), `C-h a` (apropos-command), `M-X` (buffer-relevant).

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-CMD-1 | Named command registry; invoke by name independent of any key | L1 | Equivalent | high |
| EMACS-CMD-2 | Commands self-describe argument acquisition | L2 | Equivalent | med |
| EMACS-CMD-3 | Discovery: bindings list, where-is, apropos | L1 | Equivalent | med |

**Semantic model = ruse's Semantic Command Layer exactly** (architecture §2): stable names, typed args,
context-aware availability, palette discovery. Keybindings are a convenience layer over the registry.

## EMACS-REGION — Point, mark, region, mark ring
Source: `Mark`, `Mark-Ring`, `Persistent-Mark`.
- Region = span between **point** and **mark**; mark persists and can be active/inactive.
- `C-SPC` set mark, `C-SPC C-SPC` push without activating, `C-x C-x` exchange, `C-u C-SPC` pop mark ring,
  `pop-global-mark` across buffers. `transient-mark-mode` highlights region transiently.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-REGION-1 | Region = point↔mark; mark is a first-class saved position | L2 | Adapted | med |
| EMACS-REGION-2 | Mark ring (per-buffer) + global mark ring = navigation history | L1 | Equivalent | med |
| EMACS-REGION-3 | Active vs inactive region independent of highlight (transient-mark) | L2 | Adapted | med |

**Semantic model:** region ≠ selection; mark doubles as a navigation-history stack. In ruse, unify with
Vim marks/jumplist ([vim.md](vim.md) VIM-MARK) and Helix/Kakoune selections ([native-style.md](native-style.md)).

## EMACS-KILL — Kill ring & yank/yank-pop
Source: `Kill-Ring`, `Appending-Kills`, `Clipboard`.
- `C-w` kill, `M-w` copy, `C-k` kill-line, `C-y` yank, `M-y` yank-pop (only after a yank; cycles the ring).
- Ring holds `kill-ring-max` (default 120); consecutive kills **coalesce**; shared across buffers; optional
  bidirectional clipboard bridge.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-KILL-1 | Ordered kill-ring history (not a single clipboard slot) | L2 | Equivalent | high |
| EMACS-KILL-2 | `yank-pop` cycling valid only immediately after yank | L2 | Exact | med |
| EMACS-KILL-3 | Consecutive-kill coalescing; shared across buffers | L2 | Exact | med |
| EMACS-KILL-4 | Optional OS clipboard bridge (source/sink) | L1 | Equivalent | high |

**Semantic model:** kill ring ≠ clipboard. ruse unifies **Vim registers + Emacs kill ring** into one
model (see [vim.md](vim.md) VIM-REG, architecture §3). Guards anti-pattern EMACS-5.

## EMACS-ARG — Universal / prefix argument
Source: `Arguments`, `Prefix-Command-Arguments`.
- `C-u` (raw, default 4), `C-u C-u` (16), `C-u 30`, `M-30`, `M--`, `C-<digit>`.
- Commands read **raw** (`P`, "was any C-u given?") vs **numeric** (`p`) and interpret per-command (repeat
  count / toggle / mode).

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-ARG-1 | Uniform pre-command argument channel; raw vs numeric distinction | L2 | Equivalent | med |
| EMACS-ARG-2 | Per-command interpretation (count/toggle/mode) | L2 | Equivalent | med |

Guards anti-pattern EMACS-3 (special-casing per command). Conceptually parallels Vim counts (VIM-CNT).

## EMACS-MACRO — Keyboard macros
Source: `Keyboard-Macros`, `Keyboard-Macro-Counter`.
- `C-x (`/`F3` start, `C-x )`/`F4` end, `C-x e` replay, `C-u N C-x e` repeat, `0 C-x e` until error,
  `kmacro-name-last-macro`, `kmacro-bind-to-key`, `kmacro-edit-macro`, macro **ring**, insertable **counter**.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-MACRO-1 | Record/replay command sequences; repeat count; until-error | L1 | Equivalent | med |
| EMACS-MACRO-2 | Promote to named/bound command; macro ring; editable as text | L2 | Equivalent | low |
| EMACS-MACRO-3 | Auto-incrementing counter for numbered edits | L1 | Equivalent | low |

**Semantic model:** macros are first-class command sequences — matches Vim macros (VIM-REPEAT). ruse
records **commands**, not raw keys (guards anti-patterns EMACS/VIM "raw key replay", CMD-19).

## EMACS-MODE — Major & minor modes
Source: `Major-Modes`, `Minor-Modes`.
- One major mode per buffer (local map, syntax, indent, comment, font-lock; derived-mode inheritance) +
  any number of minor modes (orthogonal toggles contributing keymap + local vars, layered above major).

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-MODE-1 | Exclusive major mode: per-buffer base behavior + keymap | L1 | Equivalent | high |
| EMACS-MODE-2 | Composable minor modes layered above major | L1 | Equivalent | high |
| EMACS-MODE-3 | Derived-mode inheritance (`prog-mode` → `python-mode`) | L2 | Equivalent | med |

**Semantic model:** behavior composed per buffer from one base layer + a stack of opt-in layers. In ruse =
**buffer-local mode** priority tier (architecture §1.4) + context bindings. Guards EMACS-6.

## EMACS-VAR — Buffer-local variables, scoping, hooks
Source: `Buffer-Local-Variables`, `Hooks`, `File-Variables`, `Directory-Variables`.
- Buffer-local values shadow global (`setq-local`); file-local (`-*- … -*-`) and dir-local (`.dir-locals.el`).
- Hooks: mode hooks (`<mode>-hook`) + general hooks (`after-save-hook`, `find-file-hook`, `post-command-hook`,
  …); normal vs abnormal; `add-hook` with buffer-local scope + depth ordering.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-VAR-1 | Scoped config: global → buffer-local → file/dir-local shadowing | L1 | Equivalent | med |
| EMACS-VAR-2 | Pervasive hook mechanism at lifecycle points | L1 | Equivalent | med |

**Semantic model:** named extension points invoked at lifecycle moments without patching core = ruse's
typed **event model** (architecture §8) + scoped config (guards EMACS-9). Workspace override vs user
override vs project settings must respect precedence (guards SEC-3).

## EMACS-MINI — Minibuffer, completion, isearch, query-replace
Source: `Minibuffer`, `Completion`, `Incremental-Search`, `Query-Replace`.
- Minibuffer = reusable prompting buffer with history rings and pluggable completion (`completing-read` +
  `completion-styles`: basic/substring/**flex**/orderless; UIs Vertico/Ivy/Helm/Ido).
- isearch `C-s`/`C-r` (incremental, live sub-keymap), `C-M-s` regex; query-replace `M-%`, `C-M-%` (interactive review loop).

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-MINI-1 | Reusable prompting substrate with history + pluggable completion (single contract) | L1 | Equivalent | high |
| EMACS-MINI-2 | Incremental search as a modal sub-keymap during the command | L1 | Equivalent | high |
| EMACS-MINI-3 | Interactive query-replace review loop (y/n/!/^/edit) | L1 | Equivalent | med |

In ruse = the **command palette + input line** (Native Style readline layer, [native-style.md](native-style.md));
completion honors one contract, not per-plugin UIs (guards UI-14, CMD-4).

## EMACS-TRANSIENT — Transient keymaps & discovery
Source: `Controlling-Active-Maps` (set-transient-map), `Repeating`, Transient manual, which-key.
- `set-transient-map` installs a temporary top-priority keymap that self-dismisses (basis for Magit-style
  transient popups with toggleable args + sub-menus). `repeat-mode`. which-key surfaces live continuations.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-TRANSIENT-1 | Temporary top-priority self-dismissing keymap (modal/repeatable clusters) | L1 | Equivalent | high |
| EMACS-TRANSIENT-2 | Transient popup menus with toggleable arguments + sub-menus | L1 | Equivalent | med |
| EMACS-TRANSIENT-3 | which-key-style live discovery of available continuations | L1 | Equivalent | med |

**This is the direct basis for Native Style's "special views = Magit-style transient actions"**
(architecture §1.5) and for prefix discovery (guards PROFILE-13). ruse special-view keymaps (git/debug)
are transient maps.

## EMACS-BUFFER — Dired & everything-is-a-buffer
Source: `Dired`, `Buffers`, `Lisp-Interaction`.
- Dired = directory as an **editable buffer** (`RET/d/x/C/R/m/!`, `wdired` edit filenames as text).
- All surfaces are buffers: `*scratch* *Messages* *Help* *Completions* *Occur* *grep* *compilation*`.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-BUFFER-1 | File manager as an editable/navigable buffer (dired/wdired) | L1 | Equivalent | med |
| EMACS-BUFFER-2 | Every surface (help, logs, results, scratch) is a searchable buffer | L1 | Equivalent | high |

**Semantic model = ruse's "everything is a workspace view/buffer"** (architecture §7, [workspace.md](workspace.md)).
Guards UI-4/UI-5 (neither force-all-text nor bespoke-UI-per-view — use a semantic view model).

## EMACS-PROC — Comint / shell / term process buffers
Source: `Interactive-Shell`, `Shell-Mode`, `Terminal-emulator`.
- Subprocess-in-a-buffer via **comint** (input ring, prompt handling, completion): `shell` (line-mode),
  `term`/`ansi-term` (char-mode terminal emulation), `eshell` (Elisp shell), language REPLs.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-PROC-1 | Process I/O as buffer text with shared input-history/prompt/completion layer | L1 | Equivalent | med |
| EMACS-PROC-2 | Line-mode (full editing) vs char-mode (raw terminal) distinction | L1 | Equivalent | med |

Overlaps Vim/Neovim `:terminal` ([vim.md](vim.md) VIM-JOB, [neovim.md](neovim.md) NVIM-TERM) → ruse
PTY-backed buffers ([workspace.md](workspace.md), [terminal.md](terminal.md)).

## EMACS-HELP — Help system (command ↔ doc coupling)
Source: `Help`, `Key-Help`, `Name-Help`.
- Introspective, live: `C-h k` (what does this key run, as currently bound), `C-h f/v/w/b/m/a/o`, `C-h i`.
- Help reflects the *current* keymap stack (accounts for shadowing) and links to source + docstrings.

| ID | Capability | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| EMACS-HELP-1 | Docs coupled to the live runtime: key → command-as-bound, command → docstring + current binding | L1 | Equivalent | med |
| EMACS-HELP-2 | Self-documentation as a property of the command model, not an add-on | L1 | Equivalent | med |

In ruse: command metadata generates docs/discovery (architecture §2.3; guards CMD-20); help is a buffer
(EMACS-BUFFER-2).

## EMACS-EDIT — Undo, narrowing, rectangles, registers, bookmarks
Source: `Undo`, `Narrowing`, `Rectangles`, `Registers`, `Bookmarks`.

| ID | Capability | Semantic model | Target | Compat | Weight |
| --- | --- | --- | --- | --- | --- |
| EMACS-EDIT-1 | Undo where undo entries are themselves undoable (redo = re-undo); optional branching tree | matches Vim undo-tree (VIM-UNDO); validates ruse transaction/undo model | L2 | Equivalent | high |
| EMACS-EDIT-2 | Narrowing: **document-level** restriction region confining all ops (search/motion/txn) to a sub-span | owner = Document, not View (V-27) | L1 | Equivalent | low |
| EMACS-EDIT-3 | Rectangles: column-block region geometry with its own kill/yank | overlaps Vim blockwise (VIM-MODE-4) | L1 | Equivalent | low |
| EMACS-EDIT-4 | Registers: named per-session slots holding text/positions/rects/window-configs | unify with Vim registers (VIM-REG) | L1 | Equivalent | med |
| EMACS-EDIT-5 | Bookmarks: named persistent cross-session locations | — | L1 | Equivalent | med |

## EMACS-ECO — Capability-kind targets (NOT parity targets)
These illustrate the *category* the substrate must enable — build the same *kind* of thing, don't clone:

| Ecosystem | Capability represented | ruse relevance |
| --- | --- | --- |
| **Magit** | Full tool porcelain (git) built from buffers + transient menus | validates workspace-buffer + transient model ([workspace.md](workspace.md), EMACS-TRANSIENT) |
| **Org mode** | Structured plain-text super-mode (outline/agenda/tables/babel/export) | a major mode can host a whole structured environment |
| **TRAMP** | Transparent remote/containerized editing via pluggable transport behind the buffer abstraction | validates ruse remote design ([remote.md](remote.md), architecture §5) |

---

## "≠ What You'd Assume" Cross-Reference (semantic obligations)

| Emacs concept | Naive assumption | Model ruse must reproduce | Anti-patterns guarded |
| --- | --- | --- | --- |
| Kill ring | Clipboard | Bounded ordered history + yank-pop + coalescing + optional clipboard bridge | EMACS-5 |
| Region | Selection | Point↔mark span; mark is a persistent, ring-tracked position | EMACS-4 |
| Command | Menu action | Named, discoverable, self-describing; keys are bindings to it | EMACS-10, CMD-1 |
| Keybinding | Fixed shortcut | Runtime-layered keymap stack | EMACS-6, PROFILE-6 |
| Prefix arg | Repeat count | Raw vs numeric channel interpreted per-command | EMACS-3 |
| Undo | Linear + redo | Undo entries themselves undoable; optional branching tree | TEXT-12/13 |
| Window/panel | Special widget | A buffer in a window; help/shell/dired/logs all buffers | UI-4/5/6 |
| Register vs bookmark | Both "saved places" | Register = per-session typed slot; Bookmark = persistent cross-session location | — |
| Transient menu | Custom GUI popup | Temporary top-priority keymap + discovery popup | PROFILE-13 |
