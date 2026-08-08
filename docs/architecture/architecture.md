---
doc: design
project: ruse
title: "ruse Architecture Design"
summary: >
  Architecture for a Rust-based, TUI-first editor platform targeting feature parity
  with Neovim, Vim, and Emacs. Covers input profiles (Vim/Emacs/Native), the Semantic
  Command Layer, a stable plugin API, the client/remote boundary, and terminal capability.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../parity/README.md        # feature-parity goals
  - ../anti-patterns/anti-patterns.md # design traps to avoid
---

# ruse Architecture Design

> This document starts from the question "How should we port/redesign Neovim in Rust?"
> Conclusion up front: **not a full port, but "a Rust-based Neovim-compatible core + a stable
> plugin API"** is the realistic project. The goal is not to reproduce every internal structure
> of Neovim, but to inherit — at the **feature-parity** level (see parity.md) — the *editing
> language* of Vim/Neovim and the *command/buffer/extension model* of Emacs, while building an
> ecosystem foundation that **breaks less** than Neovim from day one.

---

## 0. Why Not a Full Port (Scale Rationale)

### 0.1 Porting Cost Estimate

Porting Neovim to Rust while preserving current features and compatibility realistically takes
**~4–7 years with 10–20 full-time developers, i.e. ~60–140 person-years**.

Solo, the naive division is 20+ years — but in practice, tracking upstream change and maintenance
means a solo full-compat port likely **never finishes**.

| Goal | Team | Estimated time |
| --- | --- | --- |
| Minimal Vim-like editor in Rust | 1–2 | 1–2 yr |
| Limited core supporting Neovim RPC/UI | 3–5 | 2–3 yr |
| 80% compatibility usable for general users | 8–12 | 3–5 yr |
| 95%+ compat incl. plugins/Vimscript/behavior | 10–20 | 4–7 yr |
| Effectively a drop-in replacement incl. bugs/edge cases | 20+ | 7+ yr |

Here "80%" is **user-perceived major functionality**, not lines of code. The last 10–20% can
consume more than half of the total schedule.

### 0.2 Why It Takes So Long

Neovim is not just a text-editor core. Its current structure entangles all of the following:

- Editing state machine and command handling inherited from Vim
- Vimscript evaluator
- Ex command system
- Regex, search, substitution
- undo tree
- buffer/window/tab/page state
- marks, registers, mappings, abbreviations
- autocmd and re-entrant event handling
- terminal emulator
- TUI input/output and broad terminal compatibility
- libuv-based async event loop
- job/process/channel management
- MessagePack-RPC API
- External UI protocol
- Embedded Lua 5.1/LuaJIT environment
- Tree-sitter, LSP, diagnostics, extmark
- Decades of accumulated Vim-compatible behavior

The core C code lives in `src/nvim/`; buffer text is stored in `memline.c`'s line-segment tree;
the event loop, UI events, API generation, and Lua bridge are tightly coupled. The external
ecosystem also depends on MessagePack-RPC, API call ordering, and async event rules.
**Implementing only a similar protocol will not make existing clients fully compatible.**

### 0.3 The Hardest Parts

1. **Translating C into Rust syntax is only part of the work.** Mechanically portable data
   structures/utilities are relatively easy. The hard part is the contracts implicit in the C
   code: global state, pointer identity, the meaning of NULL, setjmp/longjmp control flow,
   re-entrancy, state mutation during callbacks, partial recovery after errors, macro-based
   specialization, shared mutable state. Wrapping all of these in `Arc<Mutex<_>>` or `RefCell`
   compiles but wrecks performance and structure. **State ownership and the event execution model
   must be redesigned from scratch.**

2. **Vimscript compatibility.** You need more than a grammar parser; you must reproduce Vim's
   peculiar type coercion, truthiness, scope and variable lookup, function calls and partials,
   dictionary/list reference semantics, error codes and error timing, command parsing, the
   expression/command interaction, re-entrancy mid-evaluation via autocmd, and the undocumented
   behavior plugins rely on. Cutting scope to "Lua plugins only" shrinks the schedule
   dramatically — but that's closer to a **new editor** than a Neovim port.

3. **Event loop and re-entrancy.** Neovim doesn't receive one event, handle it, and finish.
   RPC, job callbacks, timers, and autocmd arrive even while waiting for input, and certain paths
   spawn **nested event loops**. Because state inherited from Vim was not designed to receive
   arbitrary async events directly, events are delivered in special ways. Dropping Tokio in
   directly during the Rust rewrite can make this *harder*. Preserving Neovim's observable
   ordering requires a model close to a **single-threaded deterministic executor**.

4. **Compatibility testing.** The core deliverable of a porting project is a **differential
   testing system**, more than the Rust code itself. The same input must be fed to stock Neovim
   and the Rust version and compared: buffer contents, cursor/selection, window layout, redraw
   events, error messages, RPC responses, extmark positions, undo/redo state, autocmd call order,
   file-save results, terminal byte stream. Behavior without explicit tests must be checked
   against a stock Neovim binary used as an **oracle**.

### 0.4 Recommended Strategy If You Do Full Port (Strangler)

Incremental replacement beats a full rewrite. (This project instead chooses the redesign in §1,
but it is recorded for reference.)

- **Phase 1: Call a Rust library from C — 6–12 months.**
  Replace highly independent pieces first: UTF-8/string utilities, MessagePack codec, JSON, path
  handling, immutable data structures, diff, some parsers, protocol validation. Keep the C ABI
  boundary narrow.
  ```
  Neovim C core
     ├── Rust static library
     │     ├── msgpack
     │     ├── path
     │     ├── diff
     │     └── parser
     └── Lua/LuaJIT
  ```
- **Phase 2: A standalone Rust host — 1–2 years.**
  The Rust process owns the main event loop, process/job, RPC/channel, filesystem abstraction,
  logging, external UI connection. Keep the editing state machine in the C library for now.
- **Phase 3: Replace editing subsystems — 2–4 years.**
  Replace per-subsystem: buffer storage, marks/extmarks, undo, window/layout, input mapping,
  command execution, expression evaluator. At each step, select C vs Rust implementation via a
  **runtime flag**.
- **Phase 4: Remove Vimscript and legacy — 2+ years.**
  Move the evaluator and command system last. The least predictable stretch.

### 0.5 Realistic Staffing Example (Team of 12)

| Area | People |
| --- | --- |
| Editing core / buffer / undo | 3 |
| Vimscript / Ex command | 2 |
| Event loop / RPC / job | 2 |
| TUI / terminal / platform | 2 |
| Lua/API/plugin compatibility | 1 |
| Testing / fuzzing / CI | 2 |

12 people × 5 years does not land at exactly 60 person-years. Accounting for team communication,
continuous upstream Neovim change, and release stabilization, **~70–90 person-years** is a
realistic midpoint.

### 0.6 Two Goals and the Choice

- **Goal A** — "Written in Rust, but users use it like existing Neovim":
  skilled systems developers, **12–15 people, ~5 years, ~70–100 person-years**.
- **Goal B** — "A new Rust editor inheriting Neovim's philosophy":
  **3–5 people, 2–3 years** can produce something quite usable.

The difference is large. A new editor can drop awkward compatibility and use Rust-friendly
structures from the start — rope, generational arena, typed handles, explicit command
transactions. A full port prioritizes **preserving every quirky behavior** over good Rust
architecture.

> **ruse's choice:** closer to Goal B, a redesign. But it targets Vim/Neovim/Emacs at the
> **feature-parity** level (see parity.md); it does not replicate internal structures or the
> script runtime. Getting clean subsystem boundaries right from the start yields "a platform that
> grows an ecosystem yet breaks less than Neovim."

---

## 1. Input Philosophy: Three Official Profiles

This is not merely a key-conflict problem; it is about how to **lock down the product's input
philosophy and the ecosystem ABI**. Declare three **official profiles** from the start:

- **Vim Style**
- **Emacs Style**
- **Native Style**

> **Do not make Hybrid a separate profile.** Native Style should *be* "our redesign of the best of
> Vim and Emacs." Naming it "Hybrid" makes it sound like "a mode that clumsily mixes the two" and
> keeps you perpetually dragged along by Vim/Emacs compatibility.

### 1.1 A Profile Is Not Just a Keymap

The three profiles **share the same command core but have different input grammars and state
machines.**

```
                       Semantic Commands
                               │
              ┌────────────────┼────────────────┐
              │                │                │
       Vim Input Engine  Emacs Input Engine  Native Engine
              │                │                │
       Operator/Motion    Prefix/Region     Context/Intent
```

Example: `editor.delete_selection` is a single command, but its surface differs.

| Profile | User input |
| --- | --- |
| Vim | `d`, `dw`, `diw`, `dd` |
| Emacs | `C-w`, `C-k`, region-based commands |
| Native | `d` after selection, or a command action |
| Command palette | `Delete Selection` |
| RPC/macro | `editor.delete_selection` |

**Core principle:** never put Vim and Emacs commands **into the same key space.** Different
profiles mean conflicts simply do not occur.

> **Correction (verification V-1):** the row above is a simplification and is *wrong for Vim* if taken
> literally. `dw`/`diw`/`dd` are **not** surfaces of one atomic `editor.delete_selection` — there is no
> selection at `dw`. Vim composes `operator + count + motion|text-object`, where the motion yields a
> **typed range** (`char/line/block`, inclusive/exclusive) that undergoes exclusive→inclusive→linewise
> promotion *before* the operator consumes it. That composition is a first-class core concern — the
> **editing-language engine** (`C-EDITLANG`), not a keymap tier — and it also underpins dot-repeat and
> plugin-registrable operators (`g@`). `editor.delete_selection` is a *sibling* of operator+motion (used by
> Emacs/Native selection editing), not its generalization. Design tracked in **DECISIONS D-025**.

### 1.2 Resolve Conflicts Only Within a Profile

Say a Git plugin recommends `Vim: g s`, `Emacs: C-c g s`, `Native: Space g s`. If the current
user is on the Vim profile, the Emacs recommendation **never even enters the keymap resolver.**

```
Active profile: Vim

Loaded:
- Vim core bindings
- Vim plugin recommendations
- User Vim overrides
- Current buffer/view bindings

Not loaded:
- Emacs bindings
- Native bindings
```

So Vim and Emacs default commands never directly conflict. The real problem is **plugin-vs-plugin
conflict within the same profile.**

```
Plugin A: g s → git.status
Plugin B: g s → search.symbol
```

Do **not** let the last-loaded plugin silently win. Ask the user to resolve it.

```
Key conflict: `g s`

Current
  git.status         Git Integration

Requested
  search.symbol      Symbol Search

Resolution
  Keep current
  Replace
  Reassign
  Use only in specific context
```

**Until the conflict is resolved, keep the new binding disabled** — that is the safe default.

### 1.3 Context Makes Most Conflicts Disappear

A key need not always mean the same thing.

```
Text buffer + Normal mode:   s → substitute
Git status view:             s → stage
Debugger view:               s → step
File picker:                 s → search input character
```

So a binding's identity is not one key but this tuple:

```rust
pub struct BindingKey {
    profile: ProfileId,
    sequence: KeySequence,
    context: ContextExpression,
    priority: BindingPriority,
}
```

Declaration example:

```toml
[[keybindings]]
profile = "vim"
keys = "s"
command = "editor.substitute"
when = "view.kind == 'text' && input.mode == 'normal'"

[[keybindings]]
profile = "vim"
keys = "s"
command = "git.stage"
when = "view.kind == 'git-status'"
```

These two are **not a conflict** — their contexts are mutually exclusive. In contrast, the
following **is** a real conflict, which the editor must detect **statically**:

```
same profile
+ same key sequence
+ overlapping context
+ same priority
```

### 1.4 Lock Priority Like an ABI

One reason the Neovim ecosystem gets complicated is load order and mapping overrides. Here, lock
the priority rules **like an ABI** (lower number = higher priority):

**SCOPE axis** — *which keymaps are consulted, in what order* (decided; D-045 layer stack):

1. **Temporary state** — Vim operator-pending, Emacs prefix, popup navigation
2. **Active widget/view** — Git, file tree, debugger, picker
3. **Buffer-local mode** — Rust, Markdown, terminal, diff

**PROVENANCE axis** — *within the winning layer, whose binding wins* (principle locked, ordering open; D-008):

4. **Workspace override**
5. **User profile override**
6. **Plugin explicit binding**
7. **Plugin suggested binding**
8. **Built-in profile default**

> **These are two axes, not eight tiers (D-046).** The original single list conflated "is this keymap in
> scope?" with "who registered this binding?", which is why the whole of D-008 stayed open: the half that
> needs real plugins to validate (4–8) was holding open the half three upstream censuses already attest
> (1–3). A binding carries a `(layer, provenance)` pair. Resolution walks **layers** by rank; within the
> layer that binds, **provenance** picks the winner. `sealed` and `unmatched_key` belong to the layer only,
> and a plugin never installs a layer — only bindings into one. The mechanism is
> [`spec/parity/contracts/keymap-layers.yaml`](../../spec/parity/contracts/keymap-layers.yaml).
>
> **Verification V-28 is now satisfied by construction.** It required tier 3 to be "not flat — stacked minor
> modes are an *ordered* sub-list, and text-span (overlay) keymaps rank just above the major-mode map."
> Under the layer stack a sub-list member is simply another layer, so the exception dissolves rather than
> needing to be carried. The Emacs census is what made this visible: **613 of its 1,952 keyboard bindings
> live in major-mode maps**, i.e. old tier 3 alone had to hold a nine-deep stack of its own.

The most important distinction is **Plugin explicit binding vs Plugin suggested binding.**
**By default a plugin cannot forcibly register a global key.** A plugin only *suggests*; the user
accepts via install flow or preset. Exception: **keys used only inside a special view may be
provided by the plugin directly.**

### 1.5 Native Style Is Its Own Editing Language

Building Native Style as "some Vim keys + some Emacs keys" just yields a bundle of conflicts.
Native Style has its **own principles**. In one rule:

> **Use modal grammar for text, command grammar for actions, and context-specific action grammar
> for special screens.**

| Domain | Input model |
| --- | --- |
| Text editing | Vim-style modal/operator concepts |
| Command discovery | Emacs-style named commands and prefix discovery |
| Special views | Magit-style transient actions |
| Search / input line | Readline/Emacs-style line editing |
| Multiple selection | Helix/Kakoune-style selection model |
| Workspace | VSCode-style command/context |

Example:

```
Text buffer
  d + object       delete
  c + object       change

Command layer
  Space f          Files
  Space g          Git
  Space l          Language
  Space d          Debug

Transient Git layer
  s                stage
  u                unstage
  c                commit
  p                push
```

This is not a mix of Vim and Emacs; it is **a new grammar that picks the right input model per
domain.**

---

## 2. Semantic Command Layer (the Real Ecosystem ABI)

Ecosystem stability is decided not by keymaps but by the **Semantic Command API and versioning
policy** beneath them. Separating these two layers lets users use their preferred input language
while plugin authors barely think about Vim/Emacs differences.

### 2.1 A Plugin Provides One Command

A plugin does **not** build per-profile implementations; it registers **one semantic command.**

```
org.example.git.stage
```

Input profiles wire it up differently.

```
Vim Style    → s  or  <leader>gs
Emacs Style  → C-c g s
Native Style → Space g s
Palette      → Git: Stage Selection
```

### 2.2 Command IDs Are Effectively the Ecosystem ABI

Because plugins can call each other's commands, changing a command ID casually breaks configs,
keymaps, macros, and other plugins. **Enforce namespaces.**

```
core.editor.delete
core.workspace.open
org.example.git.stage
org.example.git.commit
```

When renaming a command, provide **an alias and a deprecation window.**

```toml
[[command_aliases]]
old = "org.example.git.stage_file"
new = "org.example.git.stage"
deprecated_since = "2.4"
remove_after = "4.0"
```

### 2.3 The Command Contract

- Command implementation is **decoupled** from keybindings and command-line strings.
- Arguments are passed as **typed arguments**, not `Vec<String>`.
- Commands distinguish `undoable` / `non-undoable`.
- Commands **do not modify UI directly**, do not mutate arbitrary global state, and leave no
  partial mutation on failure (respect transaction boundaries, §3).
- Availability is judged by context, not only at execution time; the palette exposes only
  context-appropriate commands.
- Command side effects are declared in the manifest.
- Execution location (local/client/remote) is distinguished.
- Command metadata generates docs and autocompletion (no manual upkeep).

---

## 3. Core State & Transaction

> This is the first of the five boundaries. See §12 "Five axes to lock first."

### 3.1 State Ownership Principles

- **Do not pile all state into one `EditorState`.** Do not mix document, view, window, and cursor
  into one object.
- **Ban global `Arc<Mutex<EditorState>>`** (see anti-patterns #1). Do not bypass the borrow checker
  with `Rc<RefCell<T>>`.
- **Document ≠ View ≠ Window ≠ File.** Do not treat file and document as the same concept. The same
  buffer must be openable in multiple views, and view-local state must not be stored in the document.
- Plugins do not directly reference core objects; the core does not directly know about
  TUI/LSP/Git/plugins (dependency direction is one-way, no crate cycles).
- Store **stable IDs / typed handles** instead of long-lived references.

### 3.2 Text Engine

- Do not represent all coordinates as `usize`. Explicitly distinguish **byte / char / grapheme /
  UTF-16 column** and enforce via types.
- Do not expose rope or piece-table **implementation details across all layers.**
- Do not store raw offsets as long-lived positions; manage positions as **anchors (with affinity).**
  Design so anchor updates are not `O(anchors × edits)`.
- Consider large-file mode, binary/invalid-UTF-8 policy, and separation of line endings/encoding
  from document data from the start.

### 3.3 Transactions and Undo

- **Every text change goes through a transaction.** Do not scatter direct insert/delete calls.
- Manage inverse-edit generation rules consistently.
- Record undo by **logical unit, not per keystroke.** Define the undo-group / transaction
  relationship clearly.
- Make edit application order explicit and normalize overlapping edits (incl. multiline
  normalization).
- Background parsers read a **snapshot**, not the live buffer.

### 3.4 Revision and Async Results

- Async responses carry a **request ID + document revision**, and **stale results are not applied.**
- Provide cancellation tokens. Background work does not block editor shutdown.

---

## 4. Plugin Stable API

### 4.1 Do Not Expose Internal Types

This is what to **avoid.**

```rust
pub trait Plugin {
    fn activate(&mut self, editor: &mut EditorState);
}
```

If a plugin receives `EditorState` directly, changing internals breaks the whole ecosystem.
Instead, provide a **stable, message-based Host API.**

```
Plugin
  ↓ stable protocol
Extension Host
  ↓ validated command/request
Workspace Runtime
```

A plugin should see only:

- Command IDs
- Document handles
- View handles
- Snapshot IDs
- Transaction requests
- Events
- Typed UI models
- Capabilities

Internal Rope, slotmap, undo node, renderer types are **never exposed.**

### 4.2 API Layering (stable / experimental / internal)

| Layer | Audience | Example features | Guarantee |
| --- | --- | --- | --- |
| **Stable** | Most plugins | command registration, read document snapshot, transaction requests, decoration, tree/list/table view, event subscription, process execution, LSP/DAP integration | Long-term compatibility |
| **Experimental** | Early adopters of advanced features | custom render node, advanced parser integration, new remote provider, AI agent protocol | May change between releases (stated) |
| **Internal** | Official core + bundled plugins only | — | Not public to external plugins |

Distinguishing these three layers from the start reduces the Neovim-style problem of "plugins
depending on internal implementation details."

### 4.3 Stabilize Protocol Over ABI

Using Rust's native dynamic-library ABI as the ecosystem foundation is dangerous.

```
plugin.so → Rust trait ABI → sensitive to compiler version and crate versions
```

Recommended structure:

```
WASM plugin  or  external plugin process
        ↓
Versioned protocol
```

Version negotiation:

```rust
pub struct ApiVersion { major: u16, minor: u16 }

pub struct PluginManifest {
    api_requirement: VersionRange,
    capabilities: Vec<Capability>,
}
```

Compatibility rules:

- **Major change:** may be incompatible
- **Minor change:** existing features preserved, new features added
- **Patch change:** bug fixes

The host supports the previous API for several generations or provides a **compatibility shim.**

### 4.4 Isolation, Permissions, Lifecycle

- **Load plugins in isolation** so a plugin panic doesn't terminate the whole editor.
- Do **not** grant filesystem/network/process permission by default (capability model, §10 security).
- Plugins do not print escape sequences to stdout directly or modify TUI cells directly. UI is
  expressed via a **semantic UI model** with a shared TUI/GUI/Web API (do not split the plugin API
  per surface).
- Define per-plugin memory/CPU limits, timeouts, cancellation, and a shutdown model.
- Declare features via **manifest** (features must not be discoverable only after runtime execution).

### 4.5 Version the Configuration Schema Too

One source of Neovim plugin instability is that config is a Lua table, so structural changes are
hard to validate statically. Here, a plugin provides a **config schema.**

```json
{
  "configuration": {
    "sign_commits": { "type": "boolean", "default": false },
    "diff_algorithm": { "type": "string", "enum": ["myers", "minimal", "histogram"] }
  }
}
```

Then the editor can provide autocompletion, type checking, doc generation, deprecated-option
warnings, and migration.

### 4.6 Reproducibility (lockfile)

Store a per-workspace lockfile to reduce "broke one day after an update."

```toml
# workspace.lock
[[plugins]]
id = "org.example.git"
version = "2.3.1"
api = "1.4"
checksum = "..."
```

Provide two modes:

- **Rolling:** auto-update within the compatible range (individual users)
- **Locked:** pin exact versions (company/server environments)

---

## 5. Client / Remote Boundary

Match VSCode Remote, but design **TUI-first.** Do not treat remote as merely a "remote filesystem."
A structure where only files are remote while LSP/build/debugger run locally fails.

```
Local TUI Client
        ↓
Remote Workspace Runtime
        ↓
File / Git / LSP / Build / Plugin
```

### 5.1 Boundary Principles

- **Explicitly define the client/workspace-runtime boundary.** Do not require client and server to
  be the exact same version — provide **version negotiation.**
- Treat local path and workspace path as **different types.** Do not handle Windows/WSL paths via
  string substitution. Do not equate WSL with Linux.
- Provide **document-state recovery / reconnect / session resume** from the start for SSH drops.
- Trust file-watcher events but keep a **full-rescan fallback.**
- Do not re-transfer large remote files in full each time. Do not stuff images/binary blobs into
  RPC JSON directly.
- Keep request cancellation, timeout, and retry policies **consistent** (not per-command ad hoc).
- Do not let plugins arbitrarily decide a remote command's **execution location.** Distinguish
  remote vs local extensions.
- Do not allow credential forwarding or localhost port forwarding **by default.**
- Tie container lifecycle to workspace lifecycle.

### 5.2 Targets

SSH · WSL · Docker/Podman · remote filesystem · remote LSP · remote build/test · remote debugger ·
remote plugin host · session reconnect · port forwarding · local clipboard/browser/image rendering ·
client/runtime version negotiation.

---

## 6. Terminal Capability & Rendering

### 6.1 Input

- Do not identify a terminal by `TERM` value alone.
- Handle ESC/Alt ambiguity, modifier-combo differences, and focus/key event distinction.
- Do not assume everyone supports the **Kitty keyboard protocol.** Put a timeout policy on the
  legacy escape parser.
- Do not treat **bracketed paste** like normal key input, and do not apply keymaps to paste content.
- Do not trigger state transitions during IME composition.
- Do not make mouse input a required feature (optional).
- Ensure the stdin parser does not stall on partial/malformed sequences, and distinguish terminal
  query responses from user input.
- Do not lose input during startup probing.
- Provide an **escape route** for editor prefix keys inside a nested terminal buffer. Do not
  hardcode tmux/screen passthrough.

### 6.2 Rendering

- Do not use **full-screen redraw only.** Emit only changed cells via a render diff.
- Do not compute grapheme width by char count. Handle **East Asian Width / emoji / combining marks**
  correctly.
- Do not equate cursor position with logical text position.
- Do not assume true color is always supported. Emit large frames via **synchronized output.**
- Do not render stale layout during resize. Account for a terminal multiplexer filtering escapes.

### 6.3 Capability Model and Image Fallback

- Do not represent capability as just a few bools. On active-probe failure provide a **safe
  fallback** and a **user override.**
- Do not let image plugins emit Kitty/SIXEL/iTerm sequences directly. Do not confuse pixel/cell size.
- Manage image delete/reposition protocols; throttle animated-image updates.
- Do not hand a remote image blob to the renderer as a file path directly.
- **Images degrade in quality, not in feature availability:**

```
Native image
→ Unicode preview
→ metadata placeholder
→ external open
```

---

## 7. UI · Workspace Model

Combining Emacs's strengths with the Neovim ecosystem, **everything becomes a workspace view or a
buffer.**

| Feature | Target form |
| --- | --- |
| File explorer | Tree/List buffer |
| Git status | Magit-style action buffer |
| Search results | Navigable results buffer |
| Diagnostics | Problems buffer |
| Terminal | PTY-backed buffer |
| Help | Documentation buffer |
| Debugger | Stack/Variables/Console views |
| AI | Chat/Proposal/Review buffer |
| Image | Semantic media view |
| Hex/Binary | Typed binary view |
| Remote file | Remote workspace document |

Principles:

- Do not clone the VSCode layout onto the TUI verbatim. Rather than dropping features in a narrow
  terminal, apply **priority-based degradation.**
- Neither force everything into a text buffer nor build a fully custom UI per view — provide a
  **semantic view model.**
- Do not treat buffer and view as the same object. Do not merge panel focus and editor mode into one
  state.
- Use the **same command system** across TUI/GUI/Web.
- The command palette provides more than a flat list — provide **context action discovery.**
- Show the current mode/prefix in the status line.

---

## 8. Async · Event Model

Lesson from Neovim: do not funnel arbitrary async events directly into the state machine. Preserving
observable ordering requires a model close to a **single-threaded deterministic executor.**

- **Do not make every function `async fn`.** In particular, do not make editing commands async.
- Do not broadcast every event on a global event bus. Establish an **event-ordering contract.**
- Event handlers do not mutate documents directly. Do not process external events mid-transaction.
- Async responses carry request ID + document revision, and stale results are not applied (§3.4).
- Debounce watcher event storms. Do not accumulate unbounded parse requests for the same document —
  apply only the latest.
- Prevent re-entrant mutation in plugin event callbacks. A handler failure does not abort the whole
  dispatch.
- Do not manage event names as strings only (typed).

---

## 9. Performance Principles

- Do not clone the whole document per command. Do not implement snapshots as deep copies.
- Do not full-parse syntax on every keystroke. Do not treat the visible region and the whole document
  the same.
- Do not merge decorations across the whole frame every frame. Keep anchor updates off `O(anchors ×
  edits)`.
- Do not fully re-index the command palette on every call.
- Do not make plugin IPC overly chatty (e.g. per-cell RPC). Do not issue thousands of small remote
  reads.
- Respect rope chunk boundaries. Do not run UTF-16 conversion over the whole document on every LSP
  request.
- Do not overuse `Box<dyn Trait>`/heap allocation for small enum states. Do not duplicate command
  implementations per profile.
- Do not run duplicate per-plugin LSP processes; do not rebuild the workspace index per plugin.
- Refresh the TUI via render diff (no full string assembly).

---

## 10. Security · Trust Model

- Establish a **workspace trust model.** Merely opening a workspace must not execute plugin code.
- Do not grant all permissions immediately on plugin install (capability changes are explicit; no
  silent application).
- Project settings do not arbitrarily override user settings.
- Do not run remote-workspace code with local-client permissions.
- Filter terminal escape injection. Do not treat plugin output as trusted UI markup.
- Do not unconditionally allow OSC 52 clipboard, URL open, or remote port forwarding (require
  confirmation).
- Verify plugin update signatures and marketplace checksums.
- Do not grant full filesystem access to an AI agent by default; **AI changes are applied after
  review.**
- Do not store long-lived credentials in the remote runtime.

---

## 11. Release · Ecosystem Policy

Technical structure alone is insufficient; operational policy is also needed.

| Item | Policy |
| --- | --- |
| Core release | Monthly or every 6 weeks |
| Stable Plugin API | At most one major per year |
| LTS API window | At least 2 years |
| Deprecated API | Kept for at least 2 majors |
| Nightly channel | Experimental API |
| Compatibility test | Automated tests against representative plugins |

Keep a **real plugin-compatibility test suite** in the editor repo and run it on every core-change PR.

```
Plugin Compatibility CI
├─ Git plugin
├─ File tree
├─ LSP extension
├─ Debug adapter
├─ Theme
├─ Remote provider
└─ Media viewer
```

### 11.1 Version Profiles Too

Changing Vim Style's default key behavior one day is also a compatibility break. Treat profiles as
**versioned packages.**

```
vim-profile@1
emacs-profile@1
native-profile@1
```

Ship a new editing philosophy as `native-profile@2`, and do not switch existing users automatically.

```toml
[input]
profile = "core.vim@1"
```

Plugins may also provide recommended keymaps for a specific profile.

---

## 12. Final Model and What to Lock First

```
                Stable Semantic Command Layer
                            │
         ┌──────────────────┼──────────────────┐
         │                  │                  │
    Vim Profile        Emacs Profile      Native Profile
    versioned          versioned          versioned
         │                  │                  │
         └──────────────────┼──────────────────┘
                            │
                    Context Key Resolver
                            │
                   Active View / Buffer
```

**Plugins:**
1. Register stable semantic commands.
2. Optionally provide per-profile recommended keymaps.
3. Own only the keymaps inside their special views.
4. Cannot forcibly override global user keymaps.

**The editor:**
1. Detects conflicts statically.
2. Reports only real conflicts with overlapping context.
3. Applies explicit priority.
4. Versions API / command IDs / config / profiles.
5. Prevents ecosystem breakage via compatibility CI.

### 12.1 Five Axes to Lock in Docs First

Rather than solving all ~300 anti-patterns at once, lock these five boundaries in documentation
first. Get these right and more than half of the remaining anti-patterns are prevented naturally.

1. **Core State & Transaction** (§3)
2. **Input Profile & Command** (§1, §2)
3. **Plugin Stable API** (§4)
4. **Client/Remote Boundary** (§5)
5. **Terminal Capability & Rendering** (§6)

> For the full trap list see [anti-patterns](../anti-patterns/anti-patterns.md); for feature goals see
> [parity](../parity/README.md).
