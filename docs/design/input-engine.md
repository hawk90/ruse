---
doc: input-engine
project: ruse
title: "ruse Input Engine (C-INPUT)"
summary: >
  The input-engine subsystem BELOW the editing-language and the profiles: it defines the logical key-event
  model (typed keys; modifiers as Kitty 1+bitmask; Ctrl-I≠Tab), resolves key sequences with the two-timer
  timeout policy (mapping-timeout vs key-code-timeout, ambiguous-prefix waiting), runs the keymap resolution
  algorithm across the priority ABI (INV-PRIORITY, incl. the V-28 ordered minor-mode/text-span sub-list) with
  static conflict detection (INV-PROFILE-ISOLATION), holds the independent mode axes (input mode /
  operator-pending / count / register-prefix), and dispatches each resolved sequence either to a semantic
  Command (C-COMMAND) or into the editing-language engine (C-EDITLANG). Covers bracketed-paste (no keymaps on
  paste), IME composition (no transitions mid-compose), the terminal-buffer passthrough chord, and which-key
  prefix discovery.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md          # §1 three profiles, BindingKey tuple, priority ABI
  - editing-language.md                       # C-EDITLANG (D-025) — sits ON TOP of this engine
  - register-model.md                         # C-REGISTER (D-026) — register-prefix axis feeds this
  - positions-history.md                      # C-POSHIST (D-027)
  - ../parity/terminal.md                     # TERM-KBD / TERM-PASTE / TERMIN-*
  - ../parity/vim.md                          # VIM-MAP (timeouts), VIM-MODE-6, VIM-CNT, VIM-REPEAT-DOT
  - ../invariants/reference-invariants.md     # INV-PRIORITY, INV-PROFILE-ISOLATION, INV-CMD-SEMANTIC
  - ../rfc/proposed/RFC-0004-input-profiles.md
  - ../../spec/DECISIONS.md                    # D-008, D-025, D-002
notation: "See parity/terminal.md for escape-sequence notation (ESC/CSI/OSC …); this doc does not re-emit sequences."
---

# ruse Input Engine (C-INPUT)

`C-INPUT` is the mechanical layer that turns **decoded input events into a resolved binding**, and hands that
binding to whatever will execute it. It sits **below** two things this doc is careful not to re-specify:

- **Profiles** ([architecture.md §1](../architecture/architecture.md), [RFC-0004](../rfc/proposed/RFC-0004-input-profiles.md))
  supply the *grammar and keymaps* (Vim/Emacs/Native). C-INPUT is the engine those grammars run **on**.
- **The editing-language engine** `C-EDITLANG` ([editing-language.md](editing-language.md), D-025) *composes*
  `operator + count + motion|text-object` into a Range and emits a Transaction. **That is not this doc.** This
  doc **decodes input and resolves bindings**; when a resolved binding is an operator, C-INPUT feeds
  C-EDITLANG — it never composes ranges itself.

> **One-line boundary.** C-EDITLANG answers *"what change does `dw` make?"*. C-INPUT answers *"the bytes/keys
> just arrived — which binding is `d` then `w`, is the sequence complete or still a prefix, and who receives
> it?"*.

## Problem

The command layer is atomic and profiles are grammars, but nothing between the terminal and those layers is
specified. Getting that middle layer wrong reproduces the exact Neovim/vim-emulation footguns the parity
corpus exists to catch:

- **Key identity is lossy.** `Ctrl-I`/`Tab`, `Ctrl-M`/`Enter`, `Ctrl-[`/`Esc` are the *same byte* under legacy
  encodings ([terminal.md TERMIN-2/3/4](../parity/terminal.md)); a naive model cannot bind them separately.
- **Timeouts are conflated.** Emulators routinely collapse Vim's two independent timers — mapping-timeout
  (`timeoutlen`) and key-code-timeout (`ttimeoutlen`) — into one, breaking `Esc` latency or multi-key maps
  ([vim.md VIM-MAP-1](../parity/vim.md)).
- **Resolution order is ad hoc.** Without a fixed priority ABI, last-loaded plugins silently win and
  conflicts are undetectable ([architecture.md §1.4](../architecture/architecture.md), INV-PRIORITY).
- **Mode is modeled as one enum.** Cramming input-mode × operator-pending × count × register-prefix into one
  `EditorMode` explodes combinatorially ([editing-language.md "Mode axes"](editing-language.md)).
- **Paste and IME leak into the keymap.** Applying keymaps to pasted bytes is a correctness *and* security
  bug ([terminal.md TERM-PASTE-2 / TERMIN-5/6](../parity/terminal.md)); transitioning modes mid-IME-compose
  corrupts CJK input ([architecture.md §6.1](../architecture/architecture.md)).

## Goals

- A **typed key-event model** (`INPUT-EVENT`) rich enough to carry Kitty's `1+bitmask` modifiers, event
  types (press/repeat/release), and the disambiguations of [TERM-KBD-1/2/3](../parity/terminal.md) — so a
  profile can bind `Ctrl-I` and `Tab` to different commands.
- A **key-sequence resolver** (`INPUT-RESOLVE`) with an explicit **two-timer timeout policy** (`INPUT-TIMEOUT`)
  reproducing `timeoutlen`/`ttimeoutlen` and ambiguous-prefix waiting, plus `<nowait>`.
- The **priority-ABI resolution algorithm** (INV-PRIORITY) with the V-28 ordered minor-mode/text-span
  sub-list, and **static conflict detection** (INV-PROFILE-ISOLATION) at load time.
- **Independent mode axes** held as orthogonal state, read by context predicates and by the grammar driver.
- A clean **profile plug-in contract** (`INPUT-PROFILE`): dispatch resolves to a **Command** *or* drives
  **C-EDITLANG** — nothing else.
- Correct **bracketed-paste**, **IME-composition**, **terminal-buffer passthrough**, and **which-key**
  behavior — each with a cited terminal/parity obligation.

## Non-goals

- **Byte-level decoding and capability probing.** The escape-sequence parser, Kitty/legacy detection, the
  DA1-fenced probe, and the confidence ledger live in `C-TERMINAL` ([terminal.md](../parity/terminal.md),
  D-015). C-INPUT *consumes* the normalized events C-TERMINAL produces; it owns the **event vocabulary**, not
  the wire format (see [§Input-source boundary](#input-source-boundary)).
- **Operator/motion composition, promotion, dot-repeat, `g@`.** All in [C-EDITLANG](editing-language.md)
  (D-025). C-INPUT only accumulates the pending operator/count and forwards keys.
- **The register/kill-ring and marks/jumplist data models.** [C-REGISTER](register-model.md) (D-026) and
  [C-POSHIST](positions-history.md) (D-027). C-INPUT only holds the *register-prefix pending state* (the
  `"a` waiting to name the next operator's register).
- **Command semantics and the command registry.** [C-COMMAND](../architecture/architecture.md) / INV-CMD-SEMANTIC.
- **Running Vimscript / real Vim mappings' `<expr>` evaluator body** — non-goal per [vim.md VIM-SCRIPT](../parity/vim.md);
  `<expr>`-style bindings resolve to a *host-evaluated* callback returning a key sequence or command, not a
  Vimscript interpreter.

## Terminology

Uses the glossary in [spec/PROJECT.md] and the invariants registry. Local terms:

- **KeyEvent** — one logical key press/repeat/release with modifiers and (for Kitty text-reporting) associated
  text. The input vocabulary, produced by an input source.
- **KeySequence** — an ordered list of KeyEvents (`d`, then `w`; `Space`, `g`, `s`). The `sequence` field of
  the `BindingKey` tuple ([architecture.md §1.3](../architecture/architecture.md)).
- **Mode axes** — the four orthogonal state variables (§Mode axes), not one enum.
- **Pending state** — the transient accumulation (partial sequence, count digits, register prefix,
  operator-pending) that is the resolver's top-priority tier.

## Invariants

C-INPUT depends on and enforces:

- **INV-PROFILE-ISOLATION** — bindings from different profiles never share a key space; a real conflict is
  `same profile + same sequence + overlapping context + same priority`, detected **statically**.
- **INV-PRIORITY** — resolution follows the fixed priority ABI; plugins cannot force global keys. (Tiers
  **provisional** per D-008 open; the *principles* are locked.)
- **INV-CMD-SEMANTIC** — a resolved binding names a semantic command (or drives C-EDITLANG which emits one);
  bindings never carry behavior inline.
- **INV-ASYNC-ORDER** ([D-002](../../spec/DECISIONS.md)) — input is delivered to the single-threaded
  deterministic executor; timers and paste do not race the state machine.
- **INV-TRUST-1** — input capability is gated; a plugin cannot seize global input (see Security impact).

No new `INV-*` is minted here (registry rule, D-022). New local IDs below (`INPUT-*`) are section/boundary
anchors for cross-reference, not invariants.

## Proposed design

### Pipeline overview

```
 input source (C-TERMINAL: bytes → events; future: GUI, RPC :normal)
        │  normalized KeyEvent | Paste | Compose | Focus  (INPUT-EVENT)
        ▼
 ┌──────────────────────── C-INPUT ────────────────────────────────┐
 │  mode axes  ──────────────┐                                      │
 │  (input/op-pending/       │  read by predicates + grammar driver │
 │   count/register-prefix)  │                                      │
 │                           ▼                                      │
 │  INPUT-RESOLVE: gather candidates by priority ABI (INV-PRIORITY) │
 │       → evaluate context predicates → longest-match             │
 │       → INPUT-TIMEOUT (timeoutlen / ttimeoutlen / nowait)       │
 │                           │                                      │
 │        ┌──────────────────┴───────────────────┐                 │
 │  dispatch = Command                     dispatch = Operator      │
 │  (INV-CMD-SEMANTIC)                     (feed C-EDITLANG)         │
 └───────────│──────────────────────────────────│─────────────────┘
             ▼                                    ▼
         C-COMMAND                            C-EDITLANG  (composes range,
      (typed args → Transaction)              emits ONE Transaction — D-025)
```

Bracketed **Paste** and IME **Compose** events **bypass INPUT-RESOLVE entirely** (§Paste, §IME).

### Input-source boundary {#input-source-boundary}

`INPUT-EVENT` is the abstraction that keeps C-INPUT independent of the terminal — the input analogue of the
render IR boundary (INV-RENDER-IR). Any **input source** lowers its raw stream into the same event vocabulary:

| Source | Lowers | Notes |
| --- | --- | --- |
| `C-TERMINAL` | escape/UTF-8 bytes → KeyEvent/Paste/Compose/Focus | owns Kitty vs legacy decode + the [terminal.md](../parity/terminal.md) ledger; C-INPUT never sees a byte |
| GUI frontend (future) | native key/IME events → same | no escape parsing; still emits the same Compose events |
| `:normal` / `:global` / macro / RPC | synthetic KeySequence → same | the "drivable as a library" re-entry ([editing-language.md §Command→input re-entry](editing-language.md), V-9); runs on the deterministic executor, guarded against re-entrant mutation |

Because C-INPUT owns the vocabulary and the sources depend on it (not the reverse), there is no crate cycle:
`C-TERMINAL` (tui stage) implements a producer against the `C-INPUT` (input stage) event contract.

### Key-event model (INPUT-EVENT)

A KeyEvent is a typed struct, never a raw byte or char. Sketch:

```rust
pub struct KeyEvent {
    pub key: Key,               // typed key identity (below)
    pub mods: Mods,             // bitflags: SHIFT|ALT|CTRL|SUPER|HYPER|META|CAPS_LOCK|NUM_LOCK
    pub event: KeyEventType,    // Press | Repeat | Release  (Kitty event-types; TERM-KBD-1)
    pub text: Option<SmolStr>,  // Kitty "report associated text" — the produced graphemes, if any
}

pub enum Key {
    Char(char),                 // a produced character key ('a', '가', '1')
    Named(NamedKey),            // Enter, Tab, Escape, Backspace, F(1..=35), arrows, Home, KP_*, Media…
    // Tab and Enter and Escape are DISTINCT Named variants — never Char('\t')/('\r')/('\x1b').
}
```

**Kitty modifiers = `1 + bitmask` ([TERM-KBD-1](../parity/terminal.md)).** The wire encoding (`CSI unicode ; 1+bitmask u`)
is decoded by C-TERMINAL into `Mods` bitflags; C-INPUT works only with the decoded flags. This is what lets a
profile bind `Shift+Enter`, `Ctrl+Alt+j`, and key **release/repeat** — impossible under legacy encodings.

**Disambiguation ([TERMIN-2/3/4](../parity/terminal.md)) is a first-class property of `Key`, not a mods hack:**

| Chord | Legacy byte | ruse KeyEvent (Kitty on) |
| --- | --- | --- |
| `Ctrl-I` | `0x09` | `Key::Char('i')`, `mods=CTRL` |
| `Tab` | `0x09` | `Key::Named(Tab)`, `mods=∅` |
| `Ctrl-M` | `0x0D` | `Key::Char('m')`, `mods=CTRL` |
| `Enter` | `0x0D` | `Key::Named(Enter)`, `mods=∅` |
| `Ctrl-[` | `0x1B` | `Key::Char('[')`, `mods=CTRL` |
| `Escape` | `0x1B` | `Key::Named(Escape)`, `mods=∅` |

When Kitty is unavailable, C-TERMINAL reports the ambiguous form (`Key::Named(Tab)` for `0x09`, etc.) and
records low confidence; a binding that *distinguishes* `Ctrl-I` from `Tab` is then **unsatisfiable** and the
resolver documents it as degraded rather than silently mis-firing — an [INV-CAP-DEGRADE](../invariants/reference-invariants.md)
consequence surfaced at the input layer.

### Key-sequence resolution (INPUT-RESOLVE) and timeout policy (INPUT-TIMEOUT)

The resolver maintains a **pending KeySequence**. On each KeyEvent it appends and classifies the sequence
against the active candidate set (built per §Priority ABI) into exactly one of:

1. **Complete + unique** — a binding matches and nothing longer can. → dispatch immediately.
2. **Prefix-only** — no binding matches yet, but bindings exist for which this is a strict prefix. → wait for
   more keys (arm the mapping-timer).
3. **Ambiguous** — a binding matches now **and** this is also a strict prefix of a longer binding
   (`<leader>g` = "Git menu" while `<leader>gs` = "git stage"). → arm the **mapping-timer**; if it fires,
   dispatch the shorter match; if a key arrives first, re-classify.
4. **Dead** — no match and no longer prefix. → the sequence cannot resolve; flush per the profile's
   *unmatched* policy (Vim: type the keys as input in Insert mode / beep in Normal; Emacs: `key is undefined`).

**Two independent timers ([VIM-MAP-1](../parity/vim.md)) — the distinction is normative, not cosmetic:**

| Timer | Governs | Vim option | Applies to |
| --- | --- | --- | --- |
| **Mapping-timeout** (`INPUT-TIMEOUT`/map) | how long to wait on an **ambiguous multi-key binding** before taking the shorter match | `timeout` / `timeoutlen` (~1000 ms) | KeySequences (bindings) |
| **Key-code-timeout** (`INPUT-TIMEOUT`/kc) | how long to wait to see if bytes form **one physical key** (legacy `Esc`-prefixed / `Alt` chords) | `ttimeout` / `ttimeoutlen` (~25–50 ms) | *belongs to C-TERMINAL's* decode of `Alt+C ≡ ESC c` ([TERM-KBD-4](../parity/terminal.md)) |

The key-code timeout is a **decode** concern: under legacy encoding, `ESC` then `c` within `ttimeoutlen` is
one `Alt+c` KeyEvent, else two events (`Escape`, then `c`). C-TERMINAL applies it while lowering bytes.
**Under the Kitty protocol this timer is eliminated** — disambiguation reports `Alt` in the bitmask, so
`Esc`-latency footguns vanish ([TERM-KBD-4](../parity/terminal.md)). The mapping timeout is a **binding**
concern and lives here in C-INPUT, over already-decoded KeyEvents. Keeping them in separate layers is what
prevents the classic "made `Esc` laggy to support a two-key map" bug.

- **`<nowait>`** on a binding removes it from ambiguity: if it is complete, dispatch without arming the
  mapping-timer even when longer bindings share the prefix.
- **`<expr>`** bindings resolve to a host callback (not a Vimscript body — Non-goals) that returns keys or a
  command id; the result re-enters the resolver.

### Priority ABI resolution (INV-PRIORITY) {#priority-abi}

A binding's identity is the tuple `BindingKey{ profile, sequence, context, priority }`
([architecture.md §1.3](../architecture/architecture.md)). Resolution for the pending sequence:

1. **Filter by active profile.** Only the active profile's bindings are even loaded (INV-PROFILE-ISOLATION) —
   an Emacs binding never enters a Vim resolver.
2. **Gather candidates in priority-tier order** ([architecture.md §1.4](../architecture/architecture.md);
   lower number = higher priority). Tiers are **provisional** (D-008 open); C-INPUT treats the ordering as
   data, not a frozen contract:
   1. **Temporary/pending state** — operator-pending, Emacs prefix, popup nav. *This tier is C-INPUT's own
      mode-axis/pending state* (§Mode axes) — it is why operator-pending bindings outrank everything.
   2. **Active widget/view** — git status, tree, debugger, picker.
   3. **Buffer-local mode — an *ordered sub-list*, not flat (V-28).** Stacked minor modes form an **ordered**
      sub-list within the tier, and **text-span / overlay** keymaps rank *just above* the major-mode map. The
      resolver walks this sub-list in order; it must not collapse all buffer-local maps into one rank. Mirrors
      Emacs `transient > ordered-minor > major > global` precedence.
   4. Workspace override → 5. User profile override → 6. Plugin **explicit** → 7. Plugin **suggested** →
      8. Built-in profile default.
3. **Evaluate context predicates.** Within tiers, keep only bindings whose `ContextExpression`
   (`view.kind == 'text' && input.mode == 'normal'`) holds against the current active-view + mode-axis state
   (evaluated by `C-CONTEXT`). Distinct contexts are **not** a conflict — this is what makes "most conflicts
   disappear" ([architecture.md §1.3](../architecture/architecture.md)).
4. **Longest-match within the winning tier.** The highest-priority tier that yields a satisfiable
   candidate wins; ties inside a tier resolve by longest sequence (feeding the ambiguity classification above).

**Static conflict detection (INV-PROFILE-ISOLATION).** At keymap **load/registration** time (off the input
hot path), C-INPUT flags any pair that is `same profile + same sequence + overlapping context + same priority`.
On a real conflict it **keeps the new binding disabled** and surfaces the resolution prompt
(Keep / Replace / Reassign / Context-scope — [architecture.md §1.2](../architecture/architecture.md)); it does
**not** let the last-loaded plugin win. Context-overlap is decided by the predicate algebra in `C-CONTEXT`
(satisfiability of `ctx_a ∧ ctx_b`), so `text`-vs-`git-status` bindings are provably non-overlapping.

### Mode axes — independent state, not one enum {#mode-axes}

C-INPUT holds mode as **four orthogonal axes** ([editing-language.md "Mode axes"](editing-language.md);
avoids the `EditorMode` combinatorial explosion). Profiles that don't use an axis leave it at its unit value.

| Axis | Values (Vim surface) | Owner of *transitions* | Read by |
| --- | --- | --- | --- |
| **input mode** | Normal / Insert / Visual{char,line,block} / Replace / Select / Cmdline / Terminal-job | profile grammar driver | context predicates (`input.mode`), resolver |
| **operator-pending** | none / `Pending{op, count1}` | set here on operator dispatch; consumed by **C-EDITLANG** | tier-1 priority; which-key |
| **count** | count-buffer (digits `[1-9][0-9]*`, leading `0` is the motion `0`) | accumulated here | multiplied `count1 × count2` **inside C-EDITLANG** (VIM-CNT) |
| **register-prefix** | none / `"x` pending | accumulated here on `"`; consumed by the next operator/put | passed as `RegisterId` to C-EDITLANG / put command ([register-model.md](register-model.md)) |

Crucial split of responsibility: C-INPUT **accumulates** operator-pending, count, and register-prefix as
pending state and **routes** them; it does **not** apply them. The moment a motion/text-object completes the
operator-pending sequence, C-INPUT hands `{operator, count1, count2, register, motion-keys}` to C-EDITLANG,
which performs promotion, builds the Range, and emits the single Transaction (D-025). `<Esc>` clears all
pending axes (aborts operator-pending with no change).

### How a profile plugs in (INPUT-PROFILE)

A profile supplies three things and nothing more:

1. **Keymap tables** — `BindingKey` tuples resolving onto a **dispatch target**.
2. **A grammar driver** — a small state machine that owns *input-mode* transitions and decides, for a resolved
   binding, which dispatch target applies (e.g. Vim's `d` in Normal → operator; `d` in Insert → literal text).
3. **Mode-axis definitions** — which axes it uses and their value sets (Emacs uses input-mode + prefix as a
   pending sequence; it does not use operator-pending/count the Vim way).

A resolved binding has exactly **two** dispatch outcomes — this is the whole contract between C-INPUT and the
layers above:

```rust
pub enum Dispatch {
    Command { id: CommandId, args: TypedArgs },   // → C-COMMAND (INV-CMD-SEMANTIC)
    Operator { op: OperatorId },                  // → set operator-pending; hand off to C-EDITLANG
}
```

- **`Command`** covers everything atomic: motions-as-cursor-moves (no pending operator), `:w`, `p`/put,
  `editor.delete_selection` (the Emacs/Native **sibling** path where a selection already exists —
  [editing-language.md §7](editing-language.md)), palette actions, plugin commands. C-INPUT calls C-COMMAND
  with typed args; the command flows through the normal transaction pipeline.
- **`Operator`** is the *only* thing that engages C-EDITLANG: it sets the operator-pending axis, then
  subsequent keys are resolved (still through INPUT-RESOLVE, now at tier-1 priority) as a motion/text-object
  whose *keys* are forwarded to C-EDITLANG for composition. C-INPUT does not know what range `w` produces —
  that is C-EDITLANG's job.

This is the precise line between the two docs: **C-INPUT decodes + resolves + routes; C-EDITLANG composes.**

### Bracketed paste — keymaps are NOT applied {#paste}

Paste arrives as a **distinct event**, never as KeyEvents ([TERM-PASTE-1](../parity/terminal.md),
[TERMIN-5/6](../parity/terminal.md)). C-TERMINAL brackets the payload (mode `2004`) and emits
`InputEvent::Paste(text)`. C-INPUT:

- **Bypasses INPUT-RESOLVE and the mode axes entirely** — no keymap, no operator-pending consumption, no
  count. A pasted `d` is the letter `d`, not the delete operator.
- Routes the payload as a **single text-insert Command** (one Transaction) at the cursor in an insert-capable
  mode, or into the command-line/search buffer if that is focused.
- Treats the payload as **untrusted**: escape sequences inside it are stripped/neutralized by C-TERMINAL
  before the event reaches C-INPUT ([TERM-PASTE-2](../parity/terminal.md), SEC-5) — some terminals leak `ESC`
  through the brackets, so C-INPUT additionally never re-interprets paste text as events even if it contains
  control bytes.

### IME composition — no transitions mid-compose {#ime}

Composition (CJK/dead-keys) is a **Compose event stream**, not KeyEvents ([architecture.md §6.1](../architecture/architecture.md)):

```
Compose::Start → Compose::Update(preedit) … → Compose::Commit(text) | Compose::Cancel
```

While a composition is active, C-INPUT is in a **compose-guard**:

- **No mode-axis transitions and no keymap resolution.** Keys feeding the IME must not trigger operators,
  counts, or mode changes. Only `Compose::Commit(text)` produces input — routed like paste, as a text-insert
  Command (respecting the current insert-capable mode).
- `Compose::Update(preedit)` is handed to the renderer as pre-edit decoration (via the query/render path), not
  to the resolver.
- `Compose::Cancel` drops the pre-edit with no state change. `Esc` during compose cancels the composition
  first; it does **not** fall through to a mode change until composition has ended.

### Terminal-buffer passthrough chord {#passthrough}

Inside a PTY-backed terminal buffer (input-mode `Terminal-job`), keys must reach the child process, yet the
user still needs an escape route to the editor's prefix keys ([architecture.md §6.1](../architecture/architecture.md):
"provide an escape route… do not hardcode tmux/screen passthrough"). C-INPUT models this as a **configurable
passthrough chord** (Vim surface: `C-w`, then a Terminal-Normal command; Neovim's `C-\ C-n`):

- In `Terminal-job` mode the tier-2 (active-view) keymap for the terminal widget claims almost the whole key
  space and forwards KeyEvents to the PTY writer as encoded bytes.
- The **single** reserved chord is resolved by C-INPUT *before* forwarding and switches the input-mode axis to
  `Terminal-Normal`, where the normal editor keymaps apply again. The chord is a binding like any other (not
  hardcoded), so it is user-rebindable and profile-specific.
- Nothing about tmux/screen is special-cased here; multiplexer passthrough is a C-TERMINAL output concern
  ([TERM-PROBE-3](../parity/terminal.md)).

### Which-key / prefix discovery {#which-key}

Prefix discovery reuses the resolver's candidate set — it is not a second keymap. When the pending sequence is
a **prefix** (classification 2/3 above) and either the mapping-timer's discovery delay elapses or the user
requests it, C-INPUT emits a **query snapshot** (INV-QUERY-SNAPSHOT) of the continuations: for each candidate
whose sequence extends the pending prefix, `{next-key, label, group?}`, already filtered by profile + priority
+ context. The status line / popup renders it (via the render path — C-INPUT emits no bytes, INV-RENDER-IR).
Because it is the *same* candidate set the resolver would use, which-key can never disagree with what a key
actually does. Operator-pending and register-prefix pending states are surfaced the same way (what motions/
text-objects are valid after `d`, what registers after `"`).

## Failure modes

- **Dead sequence (classification 4).** No match, no longer prefix → flush per the profile's unmatched policy
  (typed as input / beep / `key is undefined`); pending axes cleared. Never a silent hang.
- **Real static conflict.** New binding kept **disabled** until the user resolves; the resolver keeps using the
  existing binding meanwhile (safe default — [architecture.md §1.2](../architecture/architecture.md)).
- **Unsatisfiable disambiguation (Kitty absent).** A binding that distinguishes `Ctrl-I`/`Tab` etc. is marked
  degraded (INV-CAP-DEGRADE); it does not mis-fire on the ambiguous byte.
- **Unknown context predicate / stale profile version.** Binding is **inert** (additive-reader tolerance,
  INV-ADDITIVE), not a resolver crash.
- **Plugin attempts a forced global key.** Rejected by the priority ABI (INV-PRIORITY); plugin may only
  *suggest* or bind within its own view.
- **Partial/malformed byte stream.** Handled in C-TERMINAL (parser does not stall — [terminal.md](../parity/terminal.md));
  C-INPUT only ever sees well-formed events, so a malformed sequence cannot wedge the resolver.

## Recovery behavior

- `<Esc>` (or the profile's cancel) is a **universal reset**: clears pending sequence + all mode axes,
  aborts operator-pending with no change, cancels an in-flight compose (compose first, then mode). This is the
  guaranteed escape hatch out of any pending state.
- A resolver in a stuck prefix after the mapping-timeout is impossible by construction — the timer forces a
  terminal classification (dispatch shorter / flush dead).
- Because input runs on the deterministic executor (INV-ASYNC-ORDER, D-002), a timer firing and a key arriving
  are ordered, never racing; recovery is deterministic and replayable (supports differential tests).

## Security impact

- **Input capability is gated (INV-TRUST-1).** A plugin cannot register a global key or observe the raw input
  stream; it *suggests* bindings (user-accepted) or owns keys inside its own view. No keylogging surface.
- **Paste is untrusted (SEC-5).** Keymaps are never applied to paste; embedded escapes are stripped
  ([TERM-PASTE-2](../parity/terminal.md)); paste cannot invoke commands.
- **`<expr>`/host callbacks** run under the same trust principal as their source (user config vs plugin) and
  cannot escalate by being bound to a key — command side effects remain capability-gated
  ([RFC-0004 Security](../rfc/proposed/RFC-0004-input-profiles.md)).
- The passthrough chord is the *only* key that crosses from a PTY child back to the editor, so a program
  running in a terminal buffer cannot spoof editor commands via its output.

## Performance impact

- **Conflict detection is static** (load time), off the per-keystroke path.
- **Resolution is O(pending-length × candidates-in-winning-tier)**, not a full keymap re-index per keystroke;
  candidate sets are indexed by `(profile, first-key)` and context predicates evaluate against a small
  active-view state.
- **No per-profile command duplication** ([architecture.md §9](../architecture/architecture.md)): one command,
  many input surfaces; C-EDITLANG is the only per-grammar cost and is shared by Vim/Native.
- **Mode axes are small enums/ints**, not boxed state; pending state is a short `SmallVec` of KeyEvents.
- Timers are scheduled on the executor, not busy-waited.

## Compatibility impact

- Directly enables the [vim.md](../parity/vim.md) input-side checklist items: `timeoutlen` vs `ttimeoutlen`
  (VIM-MAP-1), operator-pending as a transient state (VIM-MODE-6), count multiplication routing (VIM-CNT),
  and the input half of dot-repeat (VIM-REPEAT-DOT — C-INPUT records the driving key sequence; C-EDITLANG owns
  the change-intent).
- Enables the rich Kitty-based bindings ([TERM-KBD-1/2/3](../parity/terminal.md)) without the legacy Esc-timeout
  footgun; degrades cleanly when only legacy encoding is available.
- Profiles are **versioned packages** ([architecture.md §11.1](../architecture/architecture.md)); the
  provisional priority tiers (D-008 open) may change before F-003/F-016 **without** a profile major bump,
  precisely because they are documented as not-yet-stable.

## Observability

- The status line renders the **current input-mode, active prefix, operator-pending, count, and register-prefix**
  ([architecture.md §7](../architecture/architecture.md)) from a subscribed snapshot (INV-STATUS) — C-INPUT
  never draws.
- **Introspection**: key→command-as-currently-bound and command→binding (Emacs-style live help); the resolved
  tier and the winning `BindingKey` are inspectable, so "why did this key do that?" is answerable.
- Disabled bindings and conflict decisions are **visible**, not hidden.
- Every dispatched sequence carries an origin (`UserInput` | `Macro` | `Plugin` | `RemotePeer`) into the
  transaction (INV-ORIGIN), so replayed/driven input is traceable.

## Alternatives

1. **Decode bytes inside C-INPUT.** Rejected — couples the resolver to terminal quirks and blocks a future GUI
   source; the `INPUT-EVENT` boundary keeps decode in C-TERMINAL (mirrors INV-RENDER-IR for output).
2. **One `EditorMode` enum instead of axes.** Rejected — combinatorial explosion (Insert×op-pending×count×…);
   the four-axis model is the same reasoning [editing-language.md](editing-language.md) adopts.
3. **A single unified timeout.** Rejected — conflates binding-ambiguity (`timeoutlen`) with physical-key
   decode (`ttimeoutlen`), reproducing the "Esc is laggy" emulator bug (VIM-MAP-1).
4. **Trie/automaton compiled across all tiers at once.** Rejected for now — flattening the priority tiers
   (especially the V-28 ordered sub-list) into one automaton loses the tier semantics and static
   conflict-detection granularity; per-tier candidate gathering keeps the ABI legible. (Revisit if the
   per-keystroke cost is ever measured as hot.)
5. **Let the resolver compose operator+motion itself.** Rejected — that *is* C-EDITLANG (D-025); duplicating it
   in the input layer re-creates the coupling D-025 exists to remove.

## Rejected approaches

- **Applying keymaps to paste** ([TERMIN-5/6](../parity/terminal.md)) — a correctness+security bug; paste
  bypasses resolution entirely.
- **State transitions during IME compose** ([architecture.md §6.1](../architecture/architecture.md)) — corrupts
  CJK input; the compose-guard forbids it.
- **Last-loaded-plugin-wins** ([architecture.md §1.2](../architecture/architecture.md)) — replaced by static
  conflict detection + user resolution (INV-PROFILE-ISOLATION).
- **Plugins forcing global keys** — rejected by INV-PRIORITY; suggest-only, or own-view-only.
- **Hardcoding tmux/screen passthrough** — the passthrough chord is an ordinary rebindable binding; multiplexer
  handling stays in C-TERMINAL.
- **Modeling `Tab`/`Enter`/`Esc` as `Char('\t'/'\r'/'\x1b')`** — loses the `Ctrl-I`/`Ctrl-M`/`Ctrl-[`
  distinction (TERMIN-2/3/4); `Named` variants are distinct from control-char `Char`s.

## Trade-offs

- **An explicit event vocabulary + input-source indirection is more than "read a keycode".** Accepted: it is
  the only structure that supports Kitty richness today and a GUI/RPC source tomorrow without rewriting the
  resolver.
- **Two timers add state.** Accepted and deliberate — collapsing them is the specific footgun being avoided.
- **Provisional priority tiers (D-008 open) create temporary ambiguity.** Accepted: locking unvalidated tiers
  is worse (D-010); the *principles* (isolation, user-override, no forced global keys, static detection) are
  enough to be safe now.
- **The Command-vs-Operator dispatch split means the input layer knows "this key is an operator".** Accepted:
  it is the minimal knowledge needed to route to C-EDITLANG; the engine still owns all range/promotion logic.

## Reference Invariants

- **INV-PRIORITY** — resolution follows the fixed priority ABI incl. the V-28 ordered minor-mode/text-span
  sub-list; plugins cannot force global keys. (Tiers provisional — D-008 open.)
- **INV-PROFILE-ISOLATION** — profiles never share a key space; real conflicts (`same profile + sequence +
  overlapping context + priority`) are detected statically.
- **INV-CMD-SEMANTIC** — a resolved binding names a semantic command (or drives C-EDITLANG, which emits one);
  no behavior inline in bindings.
- **INV-ASYNC-ORDER** (D-002) — input, timers, paste, and compose are ordered by the single-threaded
  deterministic executor.
- **INV-CAP-DEGRADE** — Kitty-dependent disambiguation degrades (marked unsatisfiable) rather than mis-firing
  when only legacy encoding is present.
- **INV-QUERY-SNAPSHOT / INV-RENDER-IR** — which-key and status render from snapshots; C-INPUT emits no bytes.
- **INV-TRUST-1 / INV-ORIGIN** — input capability is gated; every dispatch carries an origin.
- (Registry: [reference-invariants.md](../invariants/reference-invariants.md). No new `INV-*` minted here.)

## Migration strategy

Greenfield subsystem (F-003, MVP; PRD `C-INPUT`, depends_on `C-COMMAND, C-EDITLANG, C-CONFIG`). No legacy
input path to migrate. Sequencing: land `INPUT-EVENT` + `INPUT-RESOLVE` + mode axes + the two-timer policy
against the Vim profile first (F-003), driven initially by C-TERMINAL; add Emacs/Native profiles as pure
`INPUT-PROFILE` plug-ins with no engine change. The provisional priority tiers are finalized once F-003 (this
engine) and F-016 (real plugins) exist (D-008 re-evaluation).

## Test strategy

- **Differential (parity corpus, TEST-2):** `timeoutlen`/`ttimeoutlen` boundary behavior; ambiguous-prefix
  resolution (`<leader>g` vs `<leader>gs`); `<nowait>`; `Ctrl-I`≠`Tab` etc. under Kitty and the degraded
  fallback under legacy; operator-pending routing and `<Esc>` abort.
- **Property:** resolution is deterministic and terminating for any KeyEvent stream (no stuck prefix past the
  timer); pending axes always cleared by the universal reset; paste/compose never invoke a command.
- **Static-analysis tests:** conflict detector flags exactly `same profile + sequence + overlapping context +
  priority` and *not* mutually-exclusive contexts; priority-tier ordering incl. the V-28 sub-list resolves as
  specified.
- **Security:** pasted payloads containing `d`, `:q!`, and raw `ESC` never resolve as keys/commands; a terminal
  child's output cannot trigger the passthrough chord.
- **Executor ordering:** timer-fire vs key-arrival interleavings are replayable (INV-ASYNC-ORDER).

## Open questions

- **Exact mapping-timer values and the which-key discovery delay** (separate from `timeoutlen`?) — tune on real
  use (ties D-008 re-evaluation).
- **The provisional priority tiers** (esp. plugin-explicit vs plugin-suggested) and the precise encoding of the
  tier-3 ordered sub-list (V-28) in the ABI — **open per D-008**; cannot be validated pre-F-016.
- **`<expr>`/host-callback contract** — argument surface and re-entrancy limits for a callback that returns
  keys vs a command (must not re-enter mid-transaction; INV-ASYNC-ORDER).
- **Legacy-degrade UX** — how prominently to surface an unsatisfiable disambiguation binding to the user
  (silent-degrade vs warn).
- **Select-mode / `vmap` coverage** ([vim.md VIM-MAP-1](../parity/vim.md)) — how the input-mode axis exposes
  the Visual+Select duality to context predicates without a fifth axis.
- **`:normal`/`:global` re-entry depth** and interaction with the pending mode axes when driven input arrives
  while interactive pending state exists (coordinate with [editing-language.md](editing-language.md) V-9).
