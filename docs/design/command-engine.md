---
doc: command-engine
project: ruse
title: "ruse Command Engine, Context Evaluator & Palette (C-COMMAND, C-CONTEXT)"
summary: >
  Specifies the Semantic Command Layer (C-COMMAND) and the context/`when`-expression evaluator
  (C-CONTEXT) that back architecture.md §1–§2. Defines the Command descriptor (stable namespaced id,
  typed arguments, `when` availability predicate, declared side-effects, undoable flag, client/workspace
  execution location, doc/autocomplete metadata); the registry + dispatch pipeline; the `when`-expression
  grammar/evaluator and how the Context Key Resolver consumes it under the priority ABI; the context-aware
  command palette; the bounded CommandOutcome enum; macros as command sequences; command-id alias/
  deprecation; and how `:normal`/`:global` re-enter the input engine (D-025).
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md          # §1 Context Key Resolver + priority ABI, §2 Semantic Command Layer
  - editing-language.md                       # D-025 C-EDITLANG, :normal/:global re-entry
  - register-model.md                         # D-026 C-REGISTER (typed-arg example, outcome via txn)
  - ../parity/emacs.md                         # EMACS-CMD (M-x), EMACS-HELP, discovery
  - ../parity/vim.md                           # VIM-EX (:command/:global/:normal), VIM-MAP
  - ../invariants/reference-invariants.md      # INV-CMD-SEMANTIC, INV-PRIORITY, …
  - ../../spec/DECISIONS.md                     # D-006 (id stability), D-008 (keymap priority)
---

# ruse Command Engine, Context Evaluator & Palette (C-COMMAND, C-CONTEXT)

> Scope: the two kernel components `C-COMMAND` (Semantic Command Layer) and `C-CONTEXT`
> (context/`when`-expression evaluator) from [`spec/PRD.yaml`]. This doc turns the prose contract in
> [architecture.md §2](../architecture/architecture.md) into concrete Rust types and algorithms, and
> specifies the Context Key Resolver's use of `when`-expressions under the [§1.4 priority ABI](../architecture/architecture.md).
> It does **not** cover the input engine's key decoding (`C-INPUT`) or the operator/motion composition
> that sits above it ([editing-language.md](editing-language.md), D-025) beyond the command↔input
> re-entry boundary; and it does **not** cover the register data model ([register-model.md](register-model.md), D-026).

## Problem

[architecture.md §2](../architecture/architecture.md) declares that the *ecosystem ABI* is the set of
**semantic command IDs**, not keymaps: a plugin registers **one** command; every input profile (Vim/
Emacs/Native) and the palette are surfaces over it (EMACS-CMD-1, VIM-MAP-1's `<Plug>` replacement). It
lists the command contract as a bullet list (§2.3) but leaves the concrete shape undefined:

- What *is* a Command descriptor — how are arguments typed (not `Vec<String>`), how is availability
  declared, how are side-effects/undoability/execution-location expressed for the palette, transaction
  system, and remote boundary to consume?
- The `when` predicate appears in three places — keybindings (`view.kind == 'text' && input.mode ==
  'normal'`, §1.3), command availability (§2.3 "availability is judged by context"), and palette filtering
  (§7 "context action discovery") — but no **grammar, type discipline, or evaluator** is specified, and
  the [Context Key Resolver](../architecture/architecture.md) that arbitrates the priority ABI has no
  defined interface to it.
- What may a command *do*? Without a **bounded** result type, command handlers grow an unbounded effect
  surface (anti-pattern CMD-14) — mutating UI directly, leaving partial mutation on failure, hiding
  control flow.
- Discovery (M-x / `C-h b` / where-is / apropos, EMACS-CMD-3, EMACS-HELP) and macros (record **commands**,
  not raw keys, EMACS-MACRO / VIM-REPEAT / anti-pattern CMD-19) both need the registry + metadata to be
  first-class, not bolted on.

This doc closes that gap so that [editing-language.md](editing-language.md) (emits `CommandOutcome`),
[register-model.md](register-model.md) (`C-REGISTER` surface ops are commands with typed args), the input
engine, and the palette all bind against one stable contract.

## Goals

- **G1** — A concrete `Command` descriptor: stable namespaced `CommandId`, a typed argument *schema* +
  typed argument *values*, a `when` availability predicate, declared `SideEffect`s, an `undoable` classifier,
  an `ExecLocation` (client/workspace/either), and metadata sufficient to **generate** docs + autocompletion
  with no manual upkeep (EMACS-HELP, architecture §2.3; guards CMD-20).
- **G2** — A `CommandRegistry` + deterministic **dispatch pipeline**: resolve id → check availability
  (`when` + capabilities + trust) → coerce typed args → route to the correct execution location → apply the
  bounded `CommandOutcome` through the transaction/view/scheduler layers.
- **G3** — `C-CONTEXT`: a total, side-effect-free **`when`-expression** grammar + evaluator over a typed,
  snapshotted **Context** (`view.kind`, `input.mode`, `buffer.language`, `selection.count`, …). One
  evaluator serves keybinding resolution, command availability, and palette filtering (one contract, guards
  CMD-4 / UI-14).
- **G4** — The **Context Key Resolver** consumes `C-CONTEXT` to arbitrate the [§1.4 priority ABI](../architecture/architecture.md)
  and to detect *real* conflicts statically (same profile + same sequence + overlapping context + same
  priority — INV-PROFILE-ISOLATION).
- **G5** — A **context-aware palette**: ranked, grouped, discoverable listing of *available* commands with
  their live bindings and gathered-argument prompts — not a flat string list (architecture §7).
- **G6** — A **bounded** `CommandOutcome` enum (`NoChange | Transaction | ViewChange | AsyncTask |
  Composite`) — no unbounded effect system (CMD-14); commands never mutate UI/global state directly and leave
  no partial mutation on failure (architecture §2.3, §3.3).
- **G7** — **Macros as command sequences** (EMACS-MACRO, VIM-REPEAT): record the dispatched command +
  gathered args, not raw keys (CMD-19); and **command→input re-entry** for `:normal`/`:global` (delegating
  to [editing-language.md](editing-language.md), D-025 / V-9).
- **G8** — **Alias + deprecation** for command IDs (D-006): renames provide a shim and a removal window;
  configs/keymaps/macros keep resolving.

## Non-goals

- Not the key-decoding / operator-pending / count state machine (that is `C-INPUT` + `C-EDITLANG`,
  [editing-language.md](editing-language.md)); this doc starts once a *semantic command + args* has been
  produced (or is invoked by palette / macro / RPC / another command).
- Not the register/kill-ring data model ([register-model.md](register-model.md), D-026) — used here only as
  an example of typed args and a `Transaction` outcome.
- Not running Vimscript/Elisp (L3 non-goal, D-007). `:command`-defined *user commands* wrap a semantic
  command or a bounded macro body; they do not evaluate a script language. The `when`-expression is a small
  **total** DSL, deliberately *not* a general expression language (Non-goal by construction, §"Proposed
  design / 4").
- Not the final 8-tier ordering numbers (open per **D-008**); this doc specifies the *mechanism* that
  consumes whatever ordering the ABI fixes, and treats the tiers as provisional data.
- Not the plugin transport/versioning wire format (`C-PLUGIN`, D-004/INV-PROTOCOL-VERSIONED); this doc
  defines what the command contract *is*, not how it is marshalled across the WASM/process boundary.

## Terminology

See [spec glossary](../../spec/glossary.yaml) and [architecture.md](../architecture/architecture.md). New
local terms:

- **Command** — a semantic, named, invokable unit (INV-CMD-SEMANTIC). Identity is a `CommandId`, never a key.
- **Command descriptor / `CommandSpec`** — the static, registerable metadata for a command (id, arg schema,
  `when`, side-effects, undoable, exec location, doc). Distinct from the **handler** (the code) and an
  **invocation** (id + concrete arg values).
- **`when`-expression** — a boolean expression in the `C-CONTEXT` DSL over Context Keys, used for keybinding
  applicability, command availability, and palette filtering.
- **Context** — the immutable, per-evaluation snapshot of Context Keys (`view.kind`, `input.mode`, …) that a
  `when`-expression is evaluated against (INV-QUERY-SNAPSHOT).
- **Context Key Resolver** — the component ([architecture.md §12](../architecture/architecture.md)) that,
  given the active Context + priority ABI, resolves a key sequence to a command and reports static conflicts.
- **CommandOutcome** — the bounded enum a handler returns; the *only* way a command affects the world.
- **Dispatch** — the pipeline from `CommandId` + raw args to an applied `CommandOutcome`.

## Invariants

This doc **depends on and enforces** (does not mint — single-registry rule, D-022):

- **INV-CMD-SEMANTIC** — a Command has a stable namespaced id, typed args, decoupled from any binding/
  command-line string; keymaps resolve *onto* commands. *This doc is its primary realization.* Guards
  CMD-1/6/7/8.
- **INV-PRIORITY** — key resolution follows the fixed priority ABI (temporary state → active view →
  buffer-local mode → workspace → user → plugin-explicit → plugin-suggested → built-in); plugins cannot force
  global keys. *The Context Key Resolver §5 implements this.* Guards PROFILE-3/6/7.
- **INV-PROFILE-ISOLATION** — profiles never share a key space; a real conflict = same profile + sequence +
  overlapping context + same priority, detected statically (§5.3). Guards PROFILE-1/4/5.
- **INV-QUERY-SNAPSHOT** — the Context and any data a command reads are immutable snapshots, never live core
  objects (§4.2, §6). Guards CMD-14/15, PLUGIN-2.
- **INV-TXN / INV-UNDO** — a mutating command's outcome is a `Transaction` (base revision + origin); undoable
  by logical unit (§3, `CommandOutcome::Transaction`). Guards TEXT-9/10/12/13.
- **INV-ORIGIN** — every dispatch carries an `Origin` (UserInput | Macro | Plugin | Lsp | AiAgent |
  RemotePeer); the outcome's transaction inherits it (§3.4).
- **INV-PLUGIN-NO-CORE** — a plugin command receives snapshots/handles and returns an outcome *request*; it
  never mutates core directly (§3.3, §6).
- **INV-CONTRACT-FIRST / INV-ADDITIVE / INV-PROTOCOL-VERSIONED** — the descriptor is a contract independent of
  Rust types; evolution is additive; ids/schemas/aliases are versioned (§8). Guards D-006, ECO-1/2.
- **INV-ASYNC-ORDER** — `AsyncTask` outcomes carry a request id + revision; stale results are dropped and are
  applied only back on the deterministic executor (§3.4, §"Failure modes"). Guards ASYNC.
- **INV-TRUST-1 / INV-CAP-DEGRADE** — availability also gates on trust/capability; a capability-gated command
  degrades rather than vanishing when a capability is absent (§4.4). Guards TRUST, CAP.
- **INV-ERR-CLASS** — expected dispatch failures are typed `CommandError`s; impossible states assert (§"Failure
  modes").

## Proposed design

### 0. Where C-COMMAND / C-CONTEXT sit

```
  key event ─▶ C-INPUT ─▶ (C-EDITLANG composes operator+motion, D-025) ─┐
  palette / M-x  ─────────────────────────────────────────────────────┤
  macro replay (command list) ───────────────────────────────────────┤─▶  Invocation{ id, args, origin }
  :Ex / :command / RPC ───────────────────────────────────────────────┤
  another command (Composite / :normal / :global) ────────────────────┘
                                                                        │
                                              ┌─────────────────────────▼──────────────────────────┐
                                              │  C-COMMAND dispatch (§3)                             │
                                              │  resolve id → availability(when+cap+trust) →         │
                                              │  coerce typed args → route ExecLocation → handler    │
                                              └─────────────────────────┬──────────────────────────┘
                                                                        ▼
                                     CommandOutcome  ── Transaction ▶ C-TRANSACTION (undo/journal)
                                        (bounded, §3.1) ─ ViewChange ▶ View (cursor/viewport/mode)
                                                        ─ AsyncTask  ▶ C-SCHED (INV-ASYNC-ORDER)
                                                        ─ Composite  ▶ re-enter dispatch (bounded)
                                                        ─ NoChange   ▶ (status only)

  C-CONTEXT (§4)  ── evaluates `when` for:  keybinding resolution (§5) · command availability (§3,§4.4) · palette filter (§7)
```

`C-CONTEXT` has **no dependencies** (kernel leaf); `C-COMMAND` depends on `C-TRANSACTION`, `C-QUERY`
(snapshots), and `C-CONTEXT` (per PRD). Everything upstream (input, palette, Ex, macros, plugins, RPC) is a
producer of `Invocation`s; everything downstream consumes a bounded `CommandOutcome`.

---

### 1. The Command descriptor (`CommandSpec`)

The **static** registerable contract. Author-facing (a plugin/core module declares it); the host stores it in
the registry; docs + autocomplete + palette + help are all **generated** from it (G1, EMACS-HELP-2, CMD-20).

```rust
/// Stable, namespaced identity — the ecosystem ABI (D-006, INV-CMD-SEMANTIC).
/// Rendered as "core.editor.delete" / "org.example.git.stage". Case-sensitive, dot-separated.
/// `namespace` = everything before the last two segments' domain root; validated at register time.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CommandId(Arc<str>);          // interned; cheap to clone/compare/route

pub struct CommandSpec {
    pub id:        CommandId,
    pub title:     LocalizedStr,          // "Delete Selection" — palette/menu label (i18n via C-CONFIG)
    pub doc:       DocSpec,               // summary + long help; source of EMACS-HELP docstring (§9)
    pub category:  CommandCategory,       // Editing | Navigation | Git | Debug | Workspace | … (palette grouping)

    pub args:      ArgSchema,             // TYPED schema, not Vec<String> (§2)
    pub when:      Option<WhenExpr>,      // availability predicate (§4); None = always available
    pub capabilities: CapabilitySet,      // required capabilities; absence → degrade/hide (INV-CAP-DEGRADE)
    pub trust:     TrustRequirement,      // min principal / workspace-trust to run (INV-TRUST-1)

    pub effects:   SideEffectSet,         // DECLARED side effects (§1.1) — manifest-level, not discovered at runtime
    pub undoable:  Undoable,              // §1.2
    pub exec:      ExecLocation,          // Client | Workspace | Either (§1.3)

    pub palette:   PaletteVisibility,     // Listed | Hidden | ContextOnly (§7)
    pub repeatable: Repeatability,        // how `.`/macro/repeat-mode treat it (§7-macros, VIM-REPEAT-DOT link)
    pub api_since: ApiVersion,            // additive-evolution bookkeeping (INV-ADDITIVE)
}
```

Notes:

- `title`/`doc`/`category` exist **only** for humans and generated surfaces; they never affect resolution.
- The descriptor is **data**, not a trait object — `INV-CONTRACT-FIRST`: changing the Rust handler type is
  not an API change; changing `id`/`args`/`effects` is (§8).
- A plugin ships its `CommandSpec`s in its manifest (architecture §4.4: "features must not be discoverable
  only after runtime execution"); the host validates namespace ownership before registering.

#### 1.1 Declared side-effects

Side effects are **declared**, not inferred, so the palette, trust prompts, and AI-review can reason about a
command *before* running it (architecture §2.3; SEC-15).

```rust
bitflags! {
    pub struct SideEffectSet: u32 {
        const DOC_MUTATE   = 1<<0;  // edits an editable Document (⇒ expect Transaction outcome)
        const VIEW_MUTATE  = 1<<1;  // cursor/selection/viewport/mode
        const FS_READ      = 1<<2;
        const FS_WRITE     = 1<<3;  // save, rename, delete on disk
        const PROCESS_SPAWN= 1<<4;  // runs a subprocess / external filter (`!`, `:make`)
        const NETWORK      = 1<<5;
        const CLIPBOARD    = 1<<6;  // OSC-52 / OS clipboard (register-model §5)
        const CONFIG_WRITE = 1<<7;  // mutates settings
        const REGISTER     = 1<<8;  // writes the Register Store (register-model.md)
        const REMOTE       = 1<<9;  // affects workspace-runtime state
    }
}
```

Invariant (checked in dev/test): the returned `CommandOutcome` must be **consistent** with `effects` — a
command declaring no `DOC_MUTATE` that returns a `Transaction` is an assertion failure (INV-ERR-CLASS),
catching under-declared effects.

#### 1.2 Undoable classifier

```rust
pub enum Undoable {
    /// Produces one undo group per invocation (default for DOC_MUTATE). One `u`/`C-/` reverses it.
    Yes,
    /// Explicitly non-undoable Document effect belonging to an ephemeral/append-only buffer
    /// (terminal/PTY, streaming log) — INV-BUFFER-KIND exception; not journaled per-keystroke.
    EphemeralBuffer,
    /// No Document mutation at all (navigation, view, query). Nothing to undo.
    NotApplicable,
}
```

This is *declaration*; the actual undo grouping is enforced by `C-TRANSACTION` when the `Transaction`
outcome is applied (INV-UNDO — by logical unit, D-005 grouping by `TransactionOrigin`). A `Composite` (§3.1)
maps to **one** undo group by default (matches D-025: a composed operator+motion is one Transaction).

#### 1.3 Execution location (client vs workspace)

The [client/remote boundary](../architecture/architecture.md) (INV-REMOTE-FIRST, D-011) is first-class, so a
command must declare *where it runs* — a plugin may not decide a remote command's location at call time
(architecture §5.1).

```rust
pub enum ExecLocation {
    /// Runs on the local TUI client: clipboard, image render, local file picker UI, window layout.
    Client,
    /// Runs in the (possibly remote) workspace runtime: fs, git, LSP, build, PTY. Dispatch is
    /// marshalled over the D-031 framed protocol when the workspace is remote.
    Workspace,
    /// Location-agnostic pure/semantic command (e.g. editor.delete_selection over an in-memory
    /// Document snapshot); the resolver picks the side that owns the target Document.
    Either,
}
```

Dispatch (§3) routes on `exec`: a `Workspace` command invoked from a remote client becomes a protocol
request carrying `Invocation` + revision; its `AsyncTask`/`Transaction` result returns over the same channel
and is applied on the client's deterministic executor (INV-ASYNC-ORDER, INV-REMOTE-FIRST).

---

### 2. Typed argument passing (not `Vec<String>`)

Arguments are a **typed schema** (for validation/prompting/docs) plus **typed values** (for the handler) —
never a positional string vector (architecture §2.3; guards CMD against stringly-typed args).

```rust
pub struct ArgSchema { pub params: Vec<ParamSpec> }

pub struct ParamSpec {
    pub name:     &'static str,          // "register", "count", "direction"
    pub ty:       ArgType,
    pub required: bool,
    pub default:  Option<ArgValue>,
    pub acquire:  Acquire,               // how the arg is GATHERED when absent (Emacs `interactive`, EMACS-CMD-2)
    pub doc:      &'static str,
}

/// A closed set of argument *types* — extensible only additively (INV-ADDITIVE).
pub enum ArgType {
    Bool, Int, Float, Str, Enum(&'static [&'static str]),
    Count,                     // Vim count / Emacs numeric prefix (EMACS-ARG numeric `p`)
    RawPrefix,                 // Emacs raw universal argument (`P`: "was C-u given, how deep") — distinct from Count
    RegisterAddr,              // register-model.md typed RegisterAddr enum
    Range,                     // an Ex/motion range → typed Range (editing-language.md), NOT a string
    Pattern,                   // a search pattern → C-REGEX IR (D-028), NOT a raw string
    Path { workspace: bool },  // workspace path ≠ local path (INV-REMOTE-FIRST)
    DocumentId, ViewId,        // typed handles (INV-HANDLE)
    CommandRef,                // a CommandId (for meta-commands: repeat, describe-command)
    Json,                      // opaque plugin-defined payload, schema-validated by the plugin's config schema
}

/// Concrete values handed to the handler. Coercion happens once, at dispatch (§3).
pub enum ArgValue { Bool(bool), Int(i64), /* … mirrors ArgType … */ Register(RegisterAddr),
                    Range(Range), Pattern(RegexIr), Path(WorkspacePath), Doc(DocumentId), /* … */ }

/// Acquisition strategy when the caller didn't supply the arg (the Emacs `interactive` spec, EMACS-CMD-2).
pub enum Acquire {
    FromContext(ContextKey),       // e.g. selection, current register-prefix, count buffer
    Prompt { kind: PromptKind },   // minibuffer/palette prompt (EMACS-MINI-1, completion via one contract)
    PickFile { workspace: bool },
    None,                          // must be supplied by caller or defaulted, else CommandError::MissingArg
}
```

**Why this matters concretely:** `core.editor.put` (Vim `p`) takes `register: RegisterAddr` (defaulting from
the active register-prefix via `Acquire::FromContext`) and `placement: Enum(["after","before","reindent"])` —
so the same command backs `p`, `P`, `]p`, `"ap`, and the palette entry "Paste from register…", with the
register chosen by a completing prompt. No handler ever parses a string. Macros and RPC carry `ArgValue`s
directly (§7), so replay is exact and location-independent.

---

### 3. Registry & dispatch

#### 3.1 The bounded `CommandOutcome`

A handler's **only** effect on the world (G6, CMD-14). Bounded on purpose — no `Box<dyn Effect>` open set.

```rust
pub enum CommandOutcome {
    /// The command decided to do nothing (guard passed but no-op). Status line may note it.
    NoChange,

    /// A Document mutation. THE ONLY WAY to change text (INV-TXN). Carries base_revision + origin;
    /// applied by C-TRANSACTION which assigns the new revision and undo grouping (INV-UNDO).
    Transaction(TransactionRequest),

    /// View-local change: cursor/selection/viewport/mode/fold. Never mutates Document (INV-DOC-VIEW).
    ViewChange(ViewEdit),

    /// Defer work to the scheduler (LSP, git, build, external filter `!`). Carries a request id +
    /// base revision; the eventual completion re-enters dispatch as a follow-up outcome, and a stale
    /// result (revision moved) is dropped (INV-ASYNC-ORDER, INV-SCHED-1).
    AsyncTask(TaskSpec),

    /// An ORDERED, BOUNDED sequence of sub-outcomes applied atomically as one logical unit
    /// (one undo group by default). Used by aliases (`x`=`dl`), transient-menu actions, and
    /// operator results that need txn + view move together. Depth-limited (§"Failure modes").
    Composite(Vec<CommandOutcome>),
}
```

- `Transaction` / `ViewChange` are separated so the [Document≠View](../architecture/architecture.md) boundary
  (INV-DOC-VIEW, D-003) holds: a delete emits `Composite[Transaction(delete), ViewChange(move cursor)]`.
- `Composite` is **bounded** (a `Vec`, expanded once, not a re-entrant effect stream) and depth-limited to
  stop a command from becoming a control-flow engine. `:normal`/`:global` are *not* modeled as an unbounded
  `Composite` — they re-enter the input engine explicitly (§7.2).
- Commands **never** return "mutate the UI" — rendering is a downstream lowering of Document/View/semantic-
  view state (INV-RENDER-IR). A command that wants to open a results buffer returns a `ViewChange`/workspace
  action over the semantic view model, not escape bytes (architecture §7, §4.4).

#### 3.2 Registry

```rust
pub struct CommandRegistry {
    specs:   HashMap<CommandId, RegisteredCommand>,   // id → spec + handler
    aliases: HashMap<CommandId, AliasTarget>,         // §8: old id → new id (+ deprecation window)
    by_ns:   Namespaces,                              // ownership: who may register under a namespace
    index:   PaletteIndex,                            // incrementally-maintained (NOT rebuilt per palette open, PERF)
}

struct RegisteredCommand { spec: CommandSpec, handler: Handler }

/// Handler is either an in-process Rust fn (core/built-in) or a plugin proxy (over the versioned protocol).
enum Handler {
    Native(Box<dyn Fn(&mut ExecCx, ArgBinding) -> Result<CommandOutcome, CommandError> + Send>),
    Plugin(PluginCommandProxy),   // marshals Invocation → plugin, receives an OUTCOME REQUEST (INV-PLUGIN-NO-CORE)
}
```

- Registration validates: namespace ownership (a plugin may only register under its manifest namespace,
  architecture §2.2), no duplicate id (unless it's a declared alias, §8), `ArgType`s known, `when` parses
  (§4). Failure is a typed error at load, never a silent last-wins (architecture §1.2).
- `index` is updated on register/unregister, so palette open is a **query**, not a re-index (architecture §9:
  "do not fully re-index the palette on every call").

#### 3.3 Dispatch pipeline (deterministic, on the single-threaded executor — D-002)

```rust
pub fn dispatch(reg: &CommandRegistry, inv: Invocation, cx: &mut CoreCx)
    -> Result<Applied, CommandError>
{
    // 1. RESOLVE (with alias redirect, §8)
    let cmd = reg.resolve(&inv.id)                      // follows alias; may emit a deprecation warning once
                 .ok_or(CommandError::UnknownCommand(inv.id.clone()))?;

    // 2. AVAILABILITY — the SAME gate the palette uses (§4.4). Order: trust → capability → when.
    let ctx = cx.context_snapshot();                    // immutable Context (INV-QUERY-SNAPSHOT)
    availability(&cmd.spec, &ctx, cx)?;                 // Err(NotAvailable{reason}) if any gate fails

    // 3. COERCE ARGS — typed, once. Missing args are ACQUIRED per ParamSpec.acquire (§2).
    let bound: ArgBinding = bind_args(&cmd.spec.args, inv.args, &ctx, cx)?;   // CommandError::{MissingArg,BadArg}

    // 4. ROUTE by ExecLocation. Workspace+remote ⇒ marshal over the runtime protocol and return AsyncTask.
    let outcome = match (cmd.spec.exec, cx.workspace_locality()) {
        (ExecLocation::Workspace, Locality::Remote) => remote_dispatch(cmd, bound, cx)?, // ⇒ AsyncTask
        _ => run_handler(&cmd.handler, bound, inv.origin, cx)?,                          // native or plugin
    };

    // 5. RECORD for macro/repeat (COMMANDS, not keys) BEFORE applying effects (§7.1).
    cx.trace.record(&inv, &cmd.spec, cx.origin_is_user());

    // 6. APPLY the bounded outcome through the right subsystem (§3.1). Preflight so failure ⇒ NO partial mutation.
    apply_outcome(outcome, inv.origin, cx)               // Transaction⇒C-TRANSACTION, Async⇒C-SCHED, …
}
```

Key properties:

- **No partial mutation on failure** (architecture §2.3, §3.3, stability §13): steps 2–4 are *preflight*; a
  `Transaction` is validated against `base_revision` and applied atomically in step 6. If a `Composite` fails
  mid-way it is rolled back as one unit (it was never partially committed — the requests are validated, then
  applied).
- Plugin handlers return an **outcome request**, never a live mutation (INV-PLUGIN-NO-CORE); a plugin
  panic/timeout is caught at the proxy and surfaces as `CommandError::HandlerFailed` without aborting the
  editor (INV-PLUGIN-ISOLATED).
- `origin` (INV-ORIGIN) flows from `Invocation` into the `TransactionRequest`, so a macro-replayed `dd`
  shifts the register ring identically to an interactive one (register-model §INV-ORIGIN) and AI edits are
  reviewable (SEC-15).

#### 3.4 Async & staleness

`AsyncTask` outcomes go to `C-SCHED` (INV-SCHED-1); the task carries `base_revision`. On completion the
scheduler re-enters `dispatch`/`apply_outcome` on the deterministic executor with the *result*; if the
Document revision advanced past `base_revision` in a way that invalidates the result, it is **dropped**
(INV-ASYNC-ORDER), never applied stale. Duplicate/ superseded async commands for the same document are
coalesced/cancelled by the scheduler.

---

### 4. C-CONTEXT — the `when`-expression evaluator

A **total, side-effect-free** boolean DSL over typed Context Keys. One evaluator; three consumers
(keybindings §5, availability §3/§4.4, palette §7). Deliberately *not* a general language (Non-goal): no
loops, no user functions, no I/O, guaranteed to terminate — so it can be evaluated safely on every keystroke
and statically analyzed for conflicts (§5.3).

#### 4.1 Grammar

```ebnf
when   := or ;
or     := and ( "||" and )* ;
and    := not ( "&&" not )* ;
not    := "!" not | cmp ;
cmp    := atom ( ( "==" | "!=" | "<" | "<=" | ">" | ">=" | "=~" | "in" ) atom )? ;
atom   := key | literal | "(" when ")" ;
key    := ident ( "." ident )+ ;          // view.kind, input.mode, buffer.language, selection.count
literal:= string | int | bool ;           // 'text', 42, true
```

- Boolean/relational only; `=~` matches a Context Key against a **literal** regex; `in` tests membership in a
  literal set (`buffer.language in ['rust','go']`). Both operands' *types* are checked at parse time against
  the Context Key registry (§4.2) — a `view.kind < 3` is a **parse error**, not a runtime surprise.
- A bare key of boolean type is a valid `when` (`git.hasStagedChanges`).

#### 4.2 Context Keys (typed, namespaced, snapshotted)

Context Keys form a **typed registry** (like the command registry). Core owns the base namespaces; a plugin
may contribute keys under its own namespace (additive). Values are produced into an immutable `Context`
snapshot each evaluation round (INV-QUERY-SNAPSHOT) — the evaluator never touches live state.

| Key | Type | Meaning (examples) |
| --- | --- | --- |
| `view.kind` | Enum | `'text' 'git-status' 'file-tree' 'debugger' 'terminal' 'picker' 'help'` (architecture §1.3) |
| `view.focused` | Bool | this view has input focus |
| `input.profile` | Enum | `'vim' 'emacs' 'native'` (usually pre-filtered by isolation, §5) |
| `input.mode` | Enum | `'normal' 'insert' 'visual' 'replace' 'command' 'operator-pending' 'terminal'` |
| `input.operatorPending` | Bool | transient operator-pending axis (editing-language.md) |
| `input.prefix` | Str | active Emacs/leader prefix (`'C-x'`, `'Space g'`) |
| `buffer.kind` | Enum | `editable / read-only / generated / streaming / interactive` (INV-BUFFER-KIND) |
| `buffer.language` | Str | `'rust'`, `'markdown'` (major-mode analogue, EMACS-MODE) |
| `buffer.modes` | Set<Str> | active minor modes (ordered sub-list, architecture §1.3 V-28) |
| `buffer.readOnly` / `buffer.modified` | Bool | |
| `selection.active` / `selection.count` | Bool / Int | region/selection state (EMACS-REGION, Native multi-sel) |
| `workspace.trusted` | Bool | INV-TRUST-1 gate |
| `workspace.remote` | Bool | INV-REMOTE-FIRST (drives ExecLocation UX) |
| `cap.<name>` | Bool | capability ledger bits (INV-CAP-DEGRADE), e.g. `cap.clipboard` |
| `<plugin-ns>.<key>` | typed | plugin-contributed (e.g. `org.example.git.hasStagedChanges`) |

```rust
pub struct Context { values: HashMap<ContextKey, CtxValue> }   // immutable snapshot
pub enum CtxValue { Bool(bool), Int(i64), Str(Arc<str>), Enum(Arc<str>), Set(Arc<[Arc<str>]>) }

pub struct WhenExpr { root: Node, refs: SmallVec<[ContextKey; 4]> }  // `refs` = keys used ⇒ cheap re-eval invalidation
```

#### 4.3 Evaluation

```rust
pub fn eval(expr: &WhenExpr, ctx: &Context) -> bool {   // total; missing key ⇒ typed "unset" default, never panic
    eval_node(&expr.root, ctx)
}
```

- **Total & pure:** an unset key evaluates to its type's zero (`false` / `0` / empty) and comparisons are
  type-checked at parse, so `eval` cannot fail or diverge — safe to call on every keystroke over the active
  binding set.
- **Cheap invalidation:** `WhenExpr.refs` lets the resolver cache results and re-evaluate only when a
  referenced key changes (Context is versioned), keeping keystroke resolution off the hot path (PERF,
  architecture §9).
- Precompiled: `when` strings are parsed **once at registration** into a `WhenExpr` (Pratt parser), never
  re-parsed at dispatch.

#### 4.4 Availability = the one gate

Command availability (step 2 of dispatch, and the palette filter) is exactly:

```rust
fn availability(spec: &CommandSpec, ctx: &Context, cx: &CoreCx) -> Result<(), CommandError> {
    if !cx.trust_satisfies(spec.trust)            { return Err(NotAvailable(Untrusted)); }   // INV-TRUST-1
    if !cx.capabilities().covers(&spec.capabilities) {
        // INV-CAP-DEGRADE: a missing capability HIDES/greys the command; it does not error a bound key silently.
        return Err(NotAvailable(MissingCapability));
    }
    match &spec.when {
        Some(w) if !eval(w, ctx) => Err(NotAvailable(WhenFalse)),
        _ => Ok(()),
    }
}
```

The palette (§7) calls the *same* `availability` to decide what to list — so "shown in the palette" and
"runnable now" cannot drift (architecture §2.3 "the palette exposes only context-appropriate commands").

---

### 5. Context Key Resolver — consuming `when` under the priority ABI (INV-PRIORITY)

The resolver turns a **key sequence** into a **command invocation**, or reports a static conflict. It is the
concrete realization of [architecture §1.3–§1.4 / §12](../architecture/architecture.md) and EMACS-KEYMAP-2.

#### 5.1 Binding

```rust
pub struct Binding {
    pub profile:  ProfileId,          // isolation: a resolution only ever considers the active profile
    pub sequence: KeySequence,
    pub when:     Option<WhenExpr>,    // context predicate (compiled)
    pub tier:     PriorityTier,        // the §1.4 ABI tier (provisional numbers, D-008)
    pub sub_order:SubOrder,            // intra-tier ordering (ordered minor modes / overlay > major, V-28)
    pub command:  CommandId,
    pub args:     ArgTemplate,         // pre-bound args (e.g. placement='after' for `p`)
    pub origin:   BindingOrigin,       // builtin | userProfile | workspace | pluginExplicit | pluginSuggested
}
```

#### 5.2 Resolution algorithm

```rust
pub fn resolve(active: &ActiveKeymaps, seq: &KeySequence, ctx: &Context)
    -> Resolution
{
    // 0. Isolation: `active` already contains ONLY the current profile's bindings (INV-PROFILE-ISOLATION).
    // 1. Candidates whose sequence == seq (or is a live prefix of a longer pending seq).
    let mut cands: Vec<&Binding> = active.matching(seq)
        .filter(|b| b.when.as_ref().map_or(true, |w| eval(w, ctx)))   // context filter (§4)
        .collect();
    if cands.is_empty() { return Resolution::Unbound; }

    // 2. Order by the priority ABI: (tier, sub_order) ascending = higher priority (architecture §1.4).
    cands.sort_by_key(|b| (b.tier, b.sub_order));
    let winner = cands[0];

    // 3. If the top candidate is a plugin-SUGGESTED binding still unresolved against a conflict, it stays
    //    DISABLED until the user resolves it (architecture §1.2 "safe default"). Otherwise dispatch it.
    Resolution::Command { id: winner.command.clone(), args: winner.args.bind(ctx) }
}
```

- **Prefix / transient state** (Vim operator-pending, Emacs `C-x` prefix, transient popup) is *tier 1*
  (temporary state) and resolves first, matching EMACS-KEYMAP rank 1 and EMACS-TRANSIENT-1 — a transient map
  is just a top-tier, self-dismissing binding set.
- Availability of the *resolved command* is still checked at dispatch (§3 step 2); a key can be bound yet the
  command unavailable (then it's a `NotAvailable` status, not a crash).

#### 5.3 Static conflict detection

A **real** conflict (architecture §1.3, INV-PROFILE-ISOLATION) is: *same profile + same sequence + same tier
+ overlapping context*. The resolver computes context overlap **statically** using the parsed `WhenExpr`s
(finite typed domain ⇒ decidable): two bindings conflict iff their `when` predicates are **satisfiable
together**. Mutually exclusive contexts (`view.kind=='text'` vs `view.kind=='git-status'`) are provably
non-overlapping and reported as **not** a conflict. Detected conflicts are surfaced to the user for
resolution (Keep / Replace / Reassign / Context-scope, architecture §1.2); the new binding stays disabled
until resolved. This runs at keymap-load time, not per keystroke.

---

### 6. Plugin commands (contract boundary)

A plugin registers a `CommandSpec` (manifest, architecture §4.4) whose `Handler` is a `Plugin` proxy. On
dispatch the host sends the plugin an **invocation message** (id + typed args coerced to protocol values +
snapshots/handles it is permitted to see) and receives an **outcome request** — a `TransactionRequest` /
`ViewEdit` / `TaskSpec` / `Composite` expressed over the stable protocol, never a direct mutation
(INV-PLUGIN-NO-CORE, D-004). The host validates and applies it exactly as for native handlers (§3.3). This
is what lets [register-model.md](register-model.md)'s `C-REGISTER` ops, git.stage, etc. be plugin-provided
yet flow through the identical transaction/undo/journal path. Contribution of Context Keys and `when`-usable
predicates is likewise additive and namespaced (§4.2).

---

### 7. Palette, macros, and command→input re-entry

#### 7.1 Context-aware palette (not a flat list)

The palette is a **query over the registry filtered by `availability` (§4.4), grouped and ranked** — the
concrete realization of M-x + `C-h b`/where-is/apropos (EMACS-CMD-3) and architecture §7's "context action
discovery."

```rust
pub struct PaletteQuery { pub text: String, pub scope: PaletteScope }
pub enum   PaletteScope { AllAvailable, BufferRelevant /* M-X */, Category(CommandCategory) }

pub struct PaletteEntry {
    pub id: CommandId, pub title: LocalizedStr, pub category: CommandCategory,
    pub binding: Option<KeySequence>,   // the LIVE binding as currently resolved (accounts for shadowing) — EMACS-HELP-1
    pub available: bool,                // from availability(); ContextOnly commands appear only when true
    pub score: MatchScore,              // fuzzy/flex match (one completion contract, EMACS-MINI-1)
    pub needs_args: bool,               // ⇒ selecting it opens the arg-acquisition prompt (§2 Acquire)
}
```

- **Not flat:** entries are grouped by `category`, ranked by (context relevance, recency, fuzzy score), and
  each shows its **live** key binding computed via the resolver (§5) so discovery reflects the current keymap
  stack including shadowing (EMACS-HELP-1). `PaletteVisibility::ContextOnly` commands (e.g. git-status
  actions) appear only when their `when` holds — this is *context action discovery*, not a static menu.
- Selecting a command with `needs_args` runs its `Acquire` chain (prompt/pick/from-context, §2), then
  dispatches — so the palette can invoke *any* command, not just zero-arg ones.
- Backed by the incrementally-maintained `PaletteIndex` (no per-open re-index, PERF, architecture §9).
- `which-key`-style live continuation discovery (EMACS-TRANSIENT-3) is the same query scoped to the pending
  prefix.

#### 7.2 Macros are command sequences, not raw keys

Recording captures the **dispatched `Invocation`** (`CommandId` + concrete `ArgValue`s + origin), *not* the
keystrokes (EMACS-MACRO, VIM-REPEAT; guards CMD-19 "raw key replay"):

```rust
pub struct Macro { pub steps: Vec<Invocation>, pub counter: Option<MacroCounter> }  // EMACS-MACRO-3 counter
```

- Replay re-dispatches each `Invocation` with `origin = Macro` (INV-ORIGIN) — so a recorded `"add` shifts the
  register ring on replay identically (register-model §INV-ORIGIN), and replay is robust to keymap changes and
  works across profiles/locations (the ABI is the command, not the key).
- Macros are promotable to a **named/bound command** (EMACS-MACRO-2): a `Macro` gets a `CommandId` + generated
  `CommandSpec` (its `effects` = union of steps' effects; `undoable` grouped as one). Editable-as-text
  (EMACS-MACRO-2) = editing the `steps` list.
- **Dot-repeat (`.`) is distinct** from macros: it re-applies the last *change-intent* (a re-parameterizable
  operator+target), owned by `C-EDITLANG` ([editing-language.md](editing-language.md) VIM-REPEAT-DOT), *not*
  a command-list replay. A command's `repeatable: Repeatability` field tells `C-EDITLANG` whether an
  invocation contributes a repeatable change-intent. Emacs `repeat-mode` is a transient top-tier keymap (§5.2)
  over the same commands.

#### 7.3 `:normal` / `:global` re-enter the input engine (D-025 / V-9)

Ex commands like `:[range]g/pat/cmd` and `:normal[!] {keys}` are commands (`core.ex.global`,
`core.ex.normal`) whose *effect is to drive the input engine*, not an unbounded `Composite`. Per
[editing-language.md §"Command → input re-entry"](editing-language.md): the input engine is **drivable as a
library** — such a command pushes a key/command sequence into a **batched execution context** with a
synthetic cursor, running on the same deterministic executor and guarded against re-entrant mutation
(INV-ASYNC-ORDER). `:global` is **two-pass** (mark matching lines via anchors, then run the sub-command per
line — VIM-EX-GLOBAL), and each driven change is still its own `Transaction`. This keeps the command layer's
outcomes bounded (§3.1) while faithfully reproducing the Ex re-entry semantics; the composition machinery
lives in `C-EDITLANG`, and this doc only defines the command entry points and their `SideEffectSet`
(`DOC_MUTATE` + possibly `PROCESS_SPAWN`).

`:command[!] -nargs -range -complete Name {repl}` (VIM-EX user commands) is modeled as **registering a
`CommandSpec`** whose `ArgSchema` is derived from `-nargs`/`-range`/`-complete` and whose handler is a bounded
`:normal`-style body or a call to another command — *not* a Vimscript evaluator (L3 non-goal, D-007).

---

### 8. Alias & deprecation for command IDs (D-006)

Command IDs are the ABI (architecture §2.2); renames must not break configs/keymaps/macros/other plugins.

```rust
pub struct AliasTarget {
    pub new: CommandId,
    pub deprecated_since: SemVer,     // matches the manifest [[command_aliases]] table (architecture §2.2)
    pub remove_after: SemVer,
}
```

- `resolve()` (§3.2) follows an alias to the live command, dispatches it, and emits a **one-time**
  deprecation diagnostic (logged once, INV-ERR-CLASS) naming the new id and the removal version.
- Aliases are declared in the manifest ([architecture §2.2](../architecture/architecture.md) `[[command_aliases]]`)
  and validated by `spec validate` (D-022) so a dangling alias is a load-time error.
- Argument evolution is additive (INV-ADDITIVE): a new `ParamSpec` must be `required=false` with a default;
  removing/retyping a param is a **major** change requiring a new id + alias. `api_since` on `CommandSpec` and
  `ParamSpec` records when each surfaced.
- Namespace ownership prevents a plugin from aliasing/overriding another's command (architecture §2.2).

## Failure modes

- **Unknown command** (`dispatch` step 1) → `CommandError::UnknownCommand(id)` (typed, INV-ERR-CLASS). Palette/
  macro/RPC callers show it; never a panic.
- **Not available** (trust/capability/`when` false) → `CommandError::NotAvailable{reason}`. A *bound key* whose
  command is unavailable is a benign status note; a capability-missing command **degrades/hides** rather than
  erroring (INV-CAP-DEGRADE). Trust-failing side-effectful commands prompt (INV-TRUST-1).
- **Bad / missing argument** → `CommandError::{MissingArg, BadArg}` from `bind_args`; acquisition is retried
  or cancelled. No handler runs, so no mutation.
- **Handler failure / plugin panic / timeout** → caught at the boundary as `CommandError::HandlerFailed`; the
  plugin is isolated (INV-PLUGIN-ISOLATED); **no partial mutation** (outcome never reached `apply`).
- **Stale async result** → dropped by the scheduler (INV-ASYNC-ORDER); the originating command already
  returned; a superseded task is cancelled.
- **`Composite` too deep / cyclic** (`a`→`b`→`a`) → depth/step budget exceeded → assertion in dev, bounded
  `CommandError::CompositeBudget` in release; nothing applied.
- **`when`-expression that references an unknown/mistyped Context Key** → **parse-time** error at registration
  (never a runtime failure), so keystroke-time `eval` is total and cannot fail.
- **Over/under-declared side effects** (outcome inconsistent with `SideEffectSet`) → assertion (impossible-
  state, INV-ERR-CLASS) caught in tests, not shipped.

## Recovery behavior

Command dispatch itself holds no durable state; recovery is inherited from the outcome subsystems. A
`Transaction` outcome is journaled by `C-TRANSACTION` (D-005) and recovered via the transaction journal; a
half-applied `Composite` cannot exist (preflight + atomic apply), so there is nothing to reconcile on crash.
In-flight `AsyncTask`s are abandoned on restart (their revision no longer matches). The registry + keymaps are
rebuilt deterministically from manifests/config on startup; a plugin that fails to load leaves its commands
simply absent (safe-mode), and bindings to them resolve to `NotAvailable` rather than breaking the keymap.

## Security impact

- **Trust gate** — `TrustRequirement` on every side-effectful command; nothing runs before the workspace-trust
  decision and remote-runtime code never runs with client permissions (INV-TRUST-1, architecture §10). AI/
  RemotePeer/Plugin invocations carry a distinct `Origin`, so an AI-proposed command is reviewable before its
  outcome is applied (SEC-15).
- **Capability gating** — `NETWORK`/`FS_WRITE`/`PROCESS_SPAWN`/`CLIPBOARD`/`REMOTE` effects require the
  matching capability; absent ⇒ command unavailable, not silently permitted (architecture §10, INV-CAP-DEGRADE).
- **No ambient authority** — a plugin command cannot reach beyond its granted capabilities because it returns
  an *outcome request* the host validates, never a direct effect (INV-PLUGIN-NO-CORE).
- **Declared, auditable effects** — `SideEffectSet` makes "what can this do" answerable *before* running,
  feeding trust prompts and the palette's risk cues; the palette will not run an untrusted/over-privileged
  command silently.
- **`when`-DSL is inert** — no I/O, no code execution, total evaluation; a hostile `when` string cannot do
  more than mis-scope a binding (caught by conflict detection).

## Performance impact

- `when` strings compile to `WhenExpr` **once**; keystroke resolution filters candidates with cached, ref-
  invalidated `eval` — no per-keystroke parsing, no full-context recompute (architecture §9).
- Palette is a **query over an incrementally-maintained index** — never a per-open re-index (architecture §9).
- `CommandId` is interned (`Arc<str>`): O(1) hash/route/compare; no per-profile duplicate command
  implementations (architecture §9 "do not duplicate command implementations per profile" — one command, many
  surfaces).
- Dispatch is allocation-light on the hot path (typed `ArgValue`s, `SmallVec` candidate lists); async work is
  offloaded so editing commands stay synchronous (architecture §8 "do not make editing commands async").
- Static conflict detection runs at keymap load, not per keystroke.

## Compatibility impact

- Realizes **INV-CMD-SEMANTIC** and the architecture §2 command contract concretely, so [editing-language.md](editing-language.md)
  (`CommandOutcome` producer), [register-model.md](register-model.md) (`C-REGISTER` commands), input profiles,
  and the palette all bind one stable surface.
- Directly supports the parity targets: **EMACS-CMD-1/2/3** (named registry, self-describing args, discovery),
  **EMACS-HELP-1/2** (docs coupled to the live runtime), **EMACS-MACRO** / **VIM-REPEAT** (command-sequence
  macros), **VIM-EX** (`:command`/`:global`/`:normal` as commands + input re-entry), **VIM-MAP** (`<Plug>` →
  command IDs).
- Command IDs, arg schemas, `when`-DSL, and Context Keys are **versioned and additive** (INV-ADDITIVE,
  INV-PROTOCOL-VERSIONED); aliases give a deprecation window (D-006).
- The priority-ABI *numbers* remain provisional (D-008); this doc's resolver consumes whatever ordering the ABI
  fixes without change.

## Observability

- Dispatch emits a typed event `{ id, origin, exec, effects, outcome_kind, duration, base_revision }` for the
  event model, macro/AI review, and latency budgets (D-019).
- `describe-command` / `C-h k` (EMACS-HELP): renders a command's generated doc, arg schema, live binding
  (via §5), declared effects, and availability *reason* in the current context — a buffer (EMACS-BUFFER-2).
- The Context is inspectable (`:context` / describe-context) showing current Context Key values, so a user can
  see *why* a `when` did/didn't fire.
- Deprecated-alias hits are logged once with the replacement id.

## Alternatives

- **A1 — Commands as trait objects with open effects (rejected shape, see Rejected R1).** Chosen instead: data
  descriptor + bounded `CommandOutcome`.
- **A2 — Reuse an existing expression crate for `when`.** Rejected in favor of an owned total DSL (R2) — we need
  parse-time type checking against the Context Key registry and *static satisfiability* for conflict detection,
  which a general expression evaluator does not give and which must stay total/inert for security.
- **A3 — Let the input engine call handlers directly (skip a command layer).** Rejected: that is exactly the
  Neovim `<Plug>`/keymap coupling architecture §2 exists to avoid; macros/palette/RPC/remote all need the
  semantic command as the unit (INV-CMD-SEMANTIC).
- **A4 — Palette as a flat searchable string list.** Rejected: architecture §7 mandates context action
  discovery; a flat list cannot show availability, live bindings, grouping, or arg prompts.

## Rejected approaches

- **R1 — Unbounded effect system (`Box<dyn Effect>` / handlers mutate core directly).** Hides control flow,
  breaks Document≠View and no-partial-mutation, and defeats preflight/undo grouping — the CMD-14 anti-pattern.
  Rejected for the bounded `CommandOutcome` (§3.1).
- **R2 — `when` as a general (Turing-ish) expression / embedded script.** Non-total, unsafe on the keystroke
  hot path, and undecidable for static conflict analysis. Rejected for the small total DSL (§4). (Also keeps us
  clear of the L3 script-runtime non-goal, D-007.)
- **R3 — `Vec<String>` / positional string args.** Loses typing, prompting, completion, and doc generation;
  forces every handler to re-parse. Rejected for the typed `ArgSchema`/`ArgValue` (§2), per architecture §2.3.
- **R4 — Record macros as raw keystrokes.** Breaks across keymap changes/profiles/locations and can't carry
  origin for register/AI semantics — the CMD-19 anti-pattern. Rejected for command-sequence macros (§7.2).
- **R5 — Model `:global`/`:normal` as a giant `Composite` of outcomes.** Would smuggle an unbounded control-
  flow engine into the outcome enum. Rejected: they re-enter the input engine as a bounded, two-pass batched
  driver owned by `C-EDITLANG` (§7.3, D-025).
- **R6 — Last-loaded binding silently wins on conflict.** The Neovim failure architecture §1.2 calls out.
  Rejected: real conflicts are detected statically and the new binding stays disabled until the user resolves
  it (§5.3).
- **R7 — Availability judged only at execution time.** Then the palette would list unrunnable commands.
  Rejected: one `availability` gate feeds both the palette and dispatch (§4.4).

## Trade-offs

- **A dedicated command layer + context DSL is more machinery than direct key→function calls** — accepted: it
  is the ecosystem ABI (architecture §2) and the only structure that makes macros/palette/RPC/remote/undo/
  trust uniform. One command, many surfaces, pays for itself immediately.
- **A bounded `CommandOutcome` occasionally forces a `Composite`** where an open effect stream would be terser
  — accepted: boundedness is what guarantees no-partial-mutation, single-undo-group, and analyzability
  (CMD-14).
- **A total `when`-DSL cannot express everything a script could** — accepted and intended: totality +
  inertness are what make it safe per keystroke and statically analyzable for conflicts; richer conditions
  belong in the command handler, not the predicate.
- **Static satisfiability for conflict detection is decidable but not free** — mitigated by running it only at
  keymap-load time over the finite typed Context Key domain, never per keystroke.

## Migration strategy

Greenfield (no prior command engine). Land `C-CONTEXT` (DSL + evaluator + Context Key registry) and
`C-COMMAND` (descriptor + registry + dispatch + bounded outcome) as kernel components before the input engine
(`C-INPUT`, F-003) and palette wire onto them. Per **D-009**, the *internal* command/registration API ships
first and is dogfooded by built-ins (git, search) before any public plugin command surface is stabilized
(INV-PROMOTION, ≥2 users, D-010). The priority-ABI tiers stay provisional data (D-008) until F-003/F-016
validate them. Alias/deprecation tooling and `spec validate` cross-reference checks (D-022) land with the
registry so id stability is enforced from day one.

## Test strategy

- **Dispatch pipeline** — resolve→availability→coerce→route→apply for native, plugin, and remote handlers;
  assert no partial mutation on injected failures at each stage; assert `SideEffectSet`↔`CommandOutcome`
  consistency (assertion catches).
- **`C-CONTEXT` evaluator** — grammar round-trips; parse-time type errors rejected; totality (unset keys,
  random Contexts never panic); `refs`-based invalidation correctness; `=~`/`in` semantics.
- **Priority ABI (INV-PRIORITY)** — resolution picks the correct tier/sub-order winner across the §1.4 tiers
  incl. ordered minor modes + overlay>major (V-28); transient/operator-pending tier-1 precedence.
- **Static conflict detection (INV-PROFILE-ISOLATION)** — mutually-exclusive `when` ⇒ no conflict;
  overlapping-context same-tier same-seq ⇒ conflict reported; new binding disabled until resolved.
- **Typed args** — `Acquire` chains (prompt/pick/from-context), defaults, missing/ bad-arg errors; `Count` vs
  `RawPrefix` distinction (EMACS-ARG); `Range`/`Pattern` coerced to IR, not strings.
- **Macros (command sequences)** — record captures Invocations not keys; replay carries `Origin::Macro` and
  reproduces register-ring effects (cross-check register-model corpus); promotion to a bound command; macro is
  robust to keymap change.
- **Ex re-entry** — `:global` two-pass over anchors (VIM-EX-GLOBAL differential test), `:normal` batched
  driver produces per-change Transactions; `:command` user command registers a valid `CommandSpec`.
- **Alias/deprecation (D-006)** — old id resolves to new, one-time deprecation diagnostic, dangling alias is a
  load error; additive arg evolution accepted, breaking change rejected.
- **Palette** — `availability` parity between palette listing and dispatch; live-binding display reflects
  shadowing; `ContextOnly` visibility; incremental index (no per-open re-index).

## Open questions

- **OQ-1** — Exact final Context Key set and their namespaces (esp. plugin-contributed keys) — enumerate as
  F-003/F-016 land; needs a registry + `spec validate` check like commands.
- **OQ-2** — `Composite` depth/step budget numbers and whether user commands may nest `:normal` bodies beyond
  a fixed depth (ties D-025 `ChangeIntent`-in-macro serialization).
- **OQ-3** — Where the `when` static-satisfiability check draws the line for `=~` (regex) operands — likely
  treated as opaque (assume satisfiable) rather than solved, to keep conflict detection cheap.
- **OQ-4** — Precise mapping of Emacs `interactive` codes and Vim `:command -complete=` values onto the
  `Acquire`/`ArgType` set (completeness of the acquisition model).
- **OQ-5** — Whether `Repeatability` metadata fully covers dot-repeat's needs or `C-EDITLANG` requires
  additional per-command hints for custom-operator (`g@`) repeat (coordinate with D-025 open items).
- **OQ-6** — Palette ranking signal weights (context relevance vs recency vs fuzzy score) — tune on real usage;
  ties the one-completion-contract decision (EMACS-MINI-1, CMD-4).
- **OQ-7** — Finalization of the priority-ABI tier numbers is deferred to **D-008** (needs F-003 + real
  plugins); this doc's resolver is written to consume the eventual ordering unchanged.

## Reference Invariants

INV-CMD-SEMANTIC, INV-PRIORITY, INV-PROFILE-ISOLATION, INV-QUERY-SNAPSHOT, INV-TXN, INV-UNDO,
INV-BUFFER-KIND, INV-ORIGIN, INV-PLUGIN-NO-CORE, INV-PLUGIN-ISOLATED, INV-PROTOCOL-VERSIONED,
INV-CONTRACT-FIRST, INV-ADDITIVE, INV-PROMOTION, INV-ASYNC-ORDER, INV-SCHED-1, INV-TRUST-1, INV-CAP-DEGRADE,
INV-REMOTE-FIRST, INV-DOC-VIEW, INV-HANDLE, INV-ERR-CLASS, INV-RENDER-IR (see
[../invariants/reference-invariants.md](../invariants/reference-invariants.md)). Governing decisions:
[D-006](../../spec/DECISIONS.md) (command-id stability), [D-008](../../spec/DECISIONS.md) (keymap priority),
[D-025](../../spec/DECISIONS.md) (`C-EDITLANG` / `:normal`/`:global` re-entry), [D-026](../../spec/DECISIONS.md)
(`C-REGISTER`).
