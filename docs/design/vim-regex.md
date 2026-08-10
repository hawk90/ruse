---
doc: vim-regex
project: ruse
title: "ruse Regex Abstraction & Vim-Dialect Engine"
summary: >
  Resolves DECISIONS D-028. Adds the `C-REGEX` component: a `Regex` abstraction with a single
  magic-aware Vim-dialect FRONT-END that parses any surface pattern into a ruse-owned regex IR, and
  PLUGGABLE ENGINE back-ends behind one trait — a default engine (wrapped Rust `regex`, linear-time,
  no backtracking) for internal/LSP/fast-search paths, and an OWNED Vim-dialect engine (Pike-VM/NFA
  with a bounded-backtracking fallback) that supports the atoms Rust `regex` cannot: `\zs`/`\ze`
  match-start/end reset, lookbehind `\@<=`/`\@<!`, backrefs, and the magic levels `\v \V \m \M`.
  Chooses build-the-engine (option a) over scope-Vim-search-to-divergent (option b). Covers
  IR-vs-native lowering, magic-level normalization, `\zs`/`\ze` (which has no PCRE equivalent),
  `'gdefault'`, engine selection, and the ReDoS/backtracking risk with its mitigations. Resolves V-8.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - editing-language.md
  - register-model.md
  - ../parity/vim.md
  - ../invariants/reference-invariants.md
  - ../../spec/DECISIONS.md
  - ../../spec/dependencies.yaml
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

# ruse Regex Abstraction & Vim-Dialect Engine

Resolves **D-028** (open) and **V-8** (verification.md: "Vim regex dialect has no owning component or
decision"). Specifies the `C-REGEX` component referenced by D-028 and by
[F-009](../../spec/PRD.yaml) (Search & substitute), and closes the strategy question flagged by
[VIM-SEARCH-1](../parity/vim.md#vim-search--search--substitute) and
[DEP-REGEX](../../spec/dependencies.yaml).

**Decision in one line:** take **option (a)** — ruse ships its **own** Vim-dialect engine behind a
`Regex` abstraction with pluggable engines, rather than option (b) scoping Vim search to
Intentionally-different. Vim search/substitute is a muscle-memory L2 obligation
([VIM-SEARCH](../parity/vim.md#vim-search--search--substitute) targets L2); `\zs`/`\ze`/lookbehind are
common enough in real `.vimrc`/plugins that documenting them away would fail the parity charter (D-007).

## Problem

Vim's regex is **not** PCRE and **not** the RE2/Rust-`regex` dialect:

- **Magic levels** ([`pattern.txt`](https://vimhelp.org/pattern.txt.html)): `\v \V \m \M` change, mid-pattern,
  *which characters are metacharacters*. In default magic (`\m`) `+ ? { } ( ) |` are **literal** and must be
  backslash-escaped to become operators — the opposite of PCRE. `\v` (very-magic) makes them special; `\V`
  (very-nomagic) makes almost everything literal.
- **`\zs` / `\ze`** reset the reported **match start / end** to the current position while still consuming the
  surrounding context. There is **no PCRE/RE2 equivalent** — lookbehind only approximates `\zs` and only for
  fixed-width prefixes; nothing approximates `\ze` cleanly except a lookahead that the two engines handle
  differently.
- **Lookbehind** `\@<=` / `\@<!` and **backreferences** `\1`–`\9` require capabilities the Rust `regex` crate
  deliberately **does not have**: it is a finite-automaton engine with **linear-time guarantees** and therefore
  no backtracking, no lookaround, no backrefs (DEP-REGEX: *"standard Rust `regex` lacks lookbehind/backtracking"*).
- Vim itself ships **two engines** (old backtracking + newer NFA, selectable via `'regexpengine'` /
  `\%#=1` / `\%#=2`) that can disagree on pathological patterns — a bug-for-bug surface we explicitly do **not**
  want to reproduce.
- `'gdefault'` **inverts** the `g` flag of `:substitute`, and `:s///\=expr` runs script per match.

The naive failures: (1) "just use Rust `regex`" — silently drops `\zs`/lookbehind/backrefs and mis-parses
magic literalness, so real Vim patterns either error or match the wrong span; (2) "hand-write a backtracking
PCRE engine" — imports catastrophic-backtracking (ReDoS) as a hang on the single-threaded deterministic
executor (D-002), the worst possible place for an unbounded loop. This doc specifies a component that gets Vim
semantics right **and** keeps the fast linear engine for the 90% of paths that never need a Vim atom.

## Goals

- **G1** One `Regex` abstraction (`C-REGEX`) with **pluggable engines** behind a single trait; callers pick a
  *dialect* + *context*, never an engine directly.
- **G2** A **default engine** = wrapped Rust `regex` (DEP-REGEX): linear-time, ReDoS-immune, for internal
  matching, LSP/diagnostic filtering, config globs, and Vim/plain patterns whose atoms it can represent.
- **G3** An **owned Vim-dialect engine** supporting every VIM-SEARCH-1 atom the default engine cannot:
  `\zs`/`\ze`, lookaround `\@= \@! \@<= \@<!`, backrefs `\1`–`\9`, `\< \>`, `\{-}`, `\%(...\)`, `\%[...]`,
  position atoms, and magic-level literalness.
- **G4** **Parse Vim patterns exactly once** into a magic-neutral IR (so `\v`/`\V`/`\m`/`\M` are resolved in
  the front-end, not smeared across the engines).
- **G5** Keep the Vim engine's worst case **bounded** — never a hang; pathological backref/lookbehind patterns
  abort with a typed error under a step/time budget (INV-FAIL-BOUNDED, INV-SCHED-1).
- **G6** A precise **scope statement** (Exact / Adapted / Intentionally-different) for the Vim regex surface, and
  a differential test corpus (TEST-2) that proves it.

## Non-goals

- **Running Vimscript.** `:s///\=expr` evaluates the **ruse expression surface** (enough to yield replacement
  text, `submatch()`-style access), not general Vimscript — L3 non-goal (D-007, VIM-SCRIPT).
- **Reproducing Vim's two-engine bug-for-bug divergence.** ruse exposes **one** semantic Vim engine;
  `'regexpengine'` / `\%#=1` / `\%#=2` are accepted as compatibility no-ops (§7, Intentionally-different).
- **A general PCRE engine.** We support the *Vim* dialect and the *default/Rust* dialect. We do not aim to run
  arbitrary Perl/PCRE regexes with `\K`, recursion, conditionals, atomic groups, etc.
- **Search-command surface** (offsets `/e /s+2`, `incsearch`/`hlsearch` highlight, `*`/`#` word-under-cursor,
  `:g` two-pass, `gdefault`'s host command). Those live in the **search/substitute command layer**
  ([editing-language.md](editing-language.md) / F-009); this doc defines the *engine* they call and states
  where `gdefault` is decided (§6). It does not define the Ex-command grammar.

## Terminology

See [spec glossary](../../spec/glossary.yaml). New local terms:

- **`Regex` abstraction** — the `C-REGEX` component: dialect front-ends + a `RegexEngine` trait + a router.
- **Dialect front-end** — a parser from a *surface* pattern string to `RegexIr`. Two exist: `Vim` (magic-aware)
  and `Default` (Rust-`regex`/literal syntax).
- **`RegexIr`** — ruse-owned, engine-neutral, **magic-resolved** regex abstract syntax + a `Requirements` summary
  (which capabilities the pattern needs).
- **RegexEngine** — a compile+match back-end implementing the trait: `DefaultEngine` (wraps Rust `regex`) and
  `VimEngine` (owned NFA + bounded backtracking).
- **Match-boundary override** — the `\zs`/`\ze` reset of reported match start/end, modeled as two reserved
  capture slots (§4).
- **Step budget** — the bounded work quantum a match may consume before it aborts with a typed error (§8).

## Invariants

This doc depends on and is governed by (single-registry rule — no new `INV-*` minted here; see OQ-6):

- **INV-CONTRACT-FIRST** — `C-REGEX` is a contract (dialect + engine trait + typed errors) defined independently
  of the wrapped `regex` crate's types; those types never reach the public API (DEP-REGEX `public_api_exposure:
  none`, `allowed_layers: [core]`).
- **INV-CMD-SEMANTIC** — search/substitute are semantic commands with a **pattern + flags** argument; the engine
  is an *implementation detail resolved by the router*, never a user-visible command target.
- **INV-QUERY-SNAPSHOT** — a compiled `Regex` matches against an immutable text **snapshot** (a Rope slice /
  visible-range snapshot), never live mutable Document state; incsearch decoration runs outside the paint
  critical section.
- **INV-ASYNC-ORDER** — an incremental-search match request carries a request id + revision; a superseded
  keystroke's in-flight match is dropped, never applied late (D-002 executor).
- **INV-SCHED-1** — regex work is background-schedulable; input/redraw outrank it; superseded incsearch compiles
  are **cancelled**; the step budget is enforced on the scheduler, not by spinning the executor.
- **INV-FAIL-BOUNDED** — a pattern exceeding the compile or step budget aborts with a typed error and degrades
  the *feature* (search reports "pattern too complex"); it never hangs or crashes the core.
- **INV-ERR-CLASS** — bad patterns / budget overruns are typed **Errors** (events), not panics.
- **INV-ORIGIN** — every match request records origin (UserInput | Macro | Plugin | Lsp | AiAgent | RemotePeer);
  untrusted origins get the strict budget and the deny-list (§8).
- **INV-ADDITIVE / D-006** — `C-REGEX` and its command args evolve additively; new atoms are additive IR variants.

## Proposed design

### 1. Shape: dialect front-ends → one IR → pluggable engines

`C-REGEX` is three parts, and **the boundary between them is the whole point** (this is the answer to D-028's
"translation-to-common-AST vs native-per-engine"):

```
 surface pattern ──▶  Dialect front-end  ──▶  RegexIr  ──▶  Router  ──▶  RegexEngine  ──▶  Program
   (str + opts)       (Vim | Default)      (+Requirements)   (§7)     (Default | Vim)     (matchable)
                      magic resolved HERE                  picks engine
```

- **One front-end per DIALECT, not per engine.** Magic-level normalization, `\zs`/`\ze`, offsets vocabulary, and
  Vim escaping quirks are parsed **once** in the `Vim` front-end (G4). The `Default` front-end parses the
  Rust-`regex`/literal dialect for non-Vim callers (LSP filters, config, plain find).
- **`RegexIr` is engine-neutral and magic-resolved.** By the time a pattern is IR, there is no `\v`/`\m` left —
  every quantifier/group/class is explicit. This is the **translation-to-common-AST** approach, chosen over
  **native-per-engine parsing** because (i) magic literalness must be decided exactly once or the two engines
  will disagree, and (ii) the router needs a **capability summary** (`Requirements`) to pick an engine, which
  only a parsed form can give. Each *engine* then **lowers** the IR its own way (native compile), so we still get
  native per-engine execution — just not native per-engine *parsing*.
- **The trait:**

```rust
/// The pluggable engine contract. Both engines implement it; callers never name a concrete engine.
trait RegexEngine {
    fn compile(&self, ir: &RegexIr, opts: &CompileOpts) -> Result<Program, RegexError>;
}

/// A compiled, matchable program (engine-specific inside; opaque outside).
trait Program {
    /// Leftmost match at/after `start`, within `hay` (a text snapshot; INV-QUERY-SNAPSHOT).
    fn find_at(&self, hay: &Snapshot, start: BytePos, budget: &mut StepBudget)
        -> Result<Option<Match>, RegexError>;
    fn captures_at(&self, hay: &Snapshot, start: BytePos, budget: &mut StepBudget)
        -> Result<Option<Captures>, RegexError>;
}

/// The reported span honors \zs/\ze overrides (§4); `consumed` is the true automaton span.
struct Match { start: BytePos, end: BytePos, consumed: Range<BytePos> }
```

<!-- design-code-ack: Match — the F-009 MVP `Match` (crates/core/src/pattern.rs) carries only the
     REPORTED span { start, end } (already \zs/\ze-adjusted). The `consumed` true-automaton range is
     deferred: it is needed only for search-offset math (`/e`, `/s+2`) and multi-\zs edge cases, which
     are post-MVP (the command layer PRs cover plain search + :s/:g). Design is ahead of the code by
     intent — add `consumed` when offsets land, not stale. -->


`C-REGEX`'s public surface is `compile(dialect, pattern, opts) -> Regex` + `Regex::find/find_all/captures`.
Callers pass a **`Dialect`** and a **`Context`** (§7); the router owns engine choice. Concrete engine types and
the `regex` crate are confined to `core` (DEP-REGEX `allowed_layers`).

### 2. The default engine (wrapped Rust `regex`, DEP-REGEX)

`DefaultEngine` lowers `RegexIr` to Rust-`regex` syntax and delegates. Properties we keep and rely on:

- **Linear time, no backtracking** → ReDoS-immune by construction; safe for untrusted input, huge haystacks, and
  hot incremental paths.
- **Used for:** internal/core matching, LSP/diagnostic/completion filtering, config & keymap glob-ish matches,
  the fast-search path, and any Vim/plain pattern whose IR `Requirements` are within its capability set (§7).
- **Rejects, never fakes.** If the IR needs an atom the crate cannot represent (`\zs`/`\ze`, lookbehind,
  backrefs, `\%(` is fine but `\@<=` is not), `compile` returns `RegexError::Unsupported{atom}` so the **router**
  escalates to `VimEngine` — it does not silently drop the atom. This is what makes routing safe.

`DefaultEngine` is a **wrapper** (D-034): the crate's `Regex`/`Error` types stay inside; we expose `Program` +
`RegexError`. Exit strategy is the `RegexEngine` trait swap already recorded in DEP-REGEX.

### 3. The Vim-dialect engine (owned)

`VimEngine` is **ours** (DEP-REGEX: *"Vim dialect is own"*; C-REGEX `usage: own`). It is primarily a
**Pike VM / Thompson NFA with submatch tracking** — a *linear-time* simulation that already covers most Vim
atoms the default engine lacks:

- **NFA-native (linear), no backtracking needed:** `\zs`/`\ze` (§4), `\< \>` word boundaries, `\%(...\)`
  non-capturing groups, capturing groups + submatches, `\{-}` and `\{-n,m}` non-greedy quantifiers, `\c`/`\C`
  case toggles, `\|` alternation, character classes `\d \w \s \a \l \u …`, anchors `^ $`, `\%[...]`
  optional-sequence (lowered to nested optionals in the IR), **fixed-width** lookahead/lookbehind
  `\@=`/`\@!`/`\@<=`/`\@<!`, and position atoms `\%23l \%23c \%V \%#` (evaluated against `MatchCtx`, §5).
- **Bounded-backtracking fallback ONLY where the NFA cannot go linearly:** **backreferences** `\1`–`\9` (a
  backref makes the language non-regular) and **variable-width lookbehind**. When the IR `Requirements` flag one
  of these, that program compiles to a **bounded backtracking** matcher instead of / alongside the Pike VM, and
  every such match runs under the **step budget** (§8). This mirrors why Vim needs its old backtracking engine at
  all — but ours is *bounded*, so it degrades instead of hanging.

Match semantics fixed to Vim: **leftmost** start, **greedy** quantifiers greedy / `\{-}` lazy, alternation
**leftmost-first** (try branches left→right). We deliberately do **not** expose two engines with divergent
choices (§7).

### 4. `\zs` / `\ze` — match-boundary override (no PCRE equivalent)

This is the atom with **no clean PCRE/RE2 analogue**, so it is worth stating precisely. In the Pike VM we reserve
two capture-like slots beyond the normal group slots:

- **slot `zs`** — written by an epsilon transition when control passes the `\zs` node; records "the reported
  match **start** is *here*."
- **slot `ze`** — written when control passes `\ze`; records "the reported match **end** is *here*."

The automaton still **consumes** the full pattern (the text before `\zs` and after `\ze` must still match) — it
is only the **reported span** that is overridden. So `Match.consumed` is the true automaton range `[m0, m1)`, and
the reported `start`/`end` are:

```
reported.start = zs_slot.unwrap_or(m0)
reported.end   = ze_slot.unwrap_or(m1)
```

Consequences we get for free and must test (§ Test strategy):
- `foo\zsbar` matches `bar` **only when preceded by `foo`** — reported span is just `bar` (a `:s/foo\zsbar/X/`
  replaces `bar`, leaving `foo`). Unlike lookbehind, `\zs` is **variable-width-safe** and composes with any
  preceding sub-pattern, which is exactly why lookbehind cannot replace it.
- `foo\zebar` matches `foo` **only when followed by `bar`**; reported end is after `foo`.
- Multiple `\zs`/`\ze` — **last one on the successful path wins** (Vim semantics); the slot simply gets
  overwritten by later epsilon transitions on that thread.
- `\zs` interacts with search **offsets** (`/e`, `/s+2`) at the *command* layer, which operates on the reported
  span — the engine's job ends at producing the overridden `Match`.

`DefaultEngine` cannot represent this at all → any pattern containing `\zs`/`\ze` sets `Requirements.boundary_override`
and the router sends it to `VimEngine` unconditionally (§7).

### 5. Magic-level handling (`\v \V \m \M`)

Magic is a **front-end** concern, resolved into the IR so engines never see it (G4). The `Vim` front-end is a
scanner with a current **magic mode** ∈ {VeryMagic, Magic, NoMagic, VeryNoMagic}, initialized from the effective
`'magic'` option (default **Magic**, i.e. `\m`), and toggled in-pattern by `\v \V \m \M` **from that point
onward** (Vim allows switching mid-pattern):

| Mode (`in-pattern switch`) | Special without backslash | Everything else |
| --- | --- | --- |
| `\v` VeryMagic  | letters/digits/`_` literal; **all** other punctuation special (`( ) + ? { } \| < > = @` …) — closest to PCRE | — |
| `\m` Magic (default) | `. * [ ] ^ $ ~` special; `+ ? { } ( ) \| = @ < >` need a **backslash** to be special | literal |
| `\M` NoMagic | only `^ $` special | literal (incl. `.` `*`) |
| `\V` VeryNoMagic | only `\` special | everything literal |

The scanner emits **magic-neutral IR nodes**: whether `+` is `Quant(OneOrMore)` or `Literal('+')` is decided
here by (mode, backslash) and never again. `\c`/`\C` (case-insensitive / -sensitive override) and `\Z`-style
flags are likewise folded into `CompileOpts` at parse time (interacting with `'ignorecase'`/`'smartcase'`, which
are resolved by the search-command layer and passed in as `CompileOpts.case`). The `Default` dialect has no magic
mode — it parses one fixed syntax.

This "parse once" rule is the concrete reason the design is **translation-to-common-AST**, not native-per-engine:
if each engine re-parsed magic, the two engines would inevitably disagree on an escaping edge case, recreating
Vim's own two-engine divergence that we are trying to avoid (§7).

### 6. `'gdefault'` — decided at the command layer, not the engine

`'gdefault'` **inverts** the `g` flag of `:substitute`: with `gdefault` off (default), `:s/a/b/` replaces the
**first** match per line and `:s/a/b/g` replaces **all**; with `gdefault` on, the meanings swap and `g` toggles
back. This is **not** an engine capability — the engine only offers:

- `find_at(hay, start)` — the next leftmost match (for first-per-line), and
- `find_all(hay, range)` — an iterator of non-overlapping matches (for all-per-line).

The **substitute command** ([editing-language.md](editing-language.md) / F-009) computes the effective "replace
all vs first" as **`g_flag XOR gdefault`** and then calls `find_at` once per line or iterates `find_all`. Placing
`gdefault` here (not in `C-REGEX`) keeps the engine a pure matcher and matches Vim's own layering (it is a
substitute option, orthogonal to the pattern). Documented here only because VIM-SEARCH-1 flags it as a
get-it-exactly-right item; the **Exact** obligation is on the command layer, and the differential corpus asserts
the XOR truth table (T-09).

Related command-layer items also **out of the engine** but noted for completeness: replacement items
(`& \0`–`\9` `~ \u \U \l \L \e \E \r \n \t \=expr`) are expanded by the substitute layer using the engine's
`Captures`; `:s///\=` runs the ruse expression surface (Non-goals). The engine's contribution is correct
`Captures` (including `\zs`/`\ze`-adjusted group 0).

### 7. Engine selection (router: by capability, then context/profile)

Callers pass a **`Dialect`** and a **`Context`**; the **router** picks the engine. The rule is
**capability-first, context-second**:

```rust
enum Dialect { Vim, Default }
enum Context {                 // who is asking / where the result goes
    VimSearch, VimSubstitute,  // Vim profile user patterns
    Editor,                    // plain editor find (profile-neutral)
    Lsp, Diagnostics, Config,  // internal, never Vim atoms
    Plugin{ trusted: bool }, Ai, RemotePeer,   // untrusted origins
}
```

Router algorithm:

1. **Parse** with the dialect front-end → `RegexIr` + `Requirements`.
2. **If `Requirements` need a Vim-only capability** (`boundary_override` `\zs`/`\ze`, lookbehind, backrefs,
   variable-width lookaround, position atoms) → **`VimEngine`** (only engine that can). Non-negotiable.
3. **Else the IR is within the default engine's capability set** → **`DefaultEngine`** (linear, ReDoS-free) —
   *even for `Dialect::Vim`*. A plain `/foo` or `/\d\+/` needs nothing exotic and should run on the fast, safe
   engine. **Caveat (validated gate, OQ-1):** routing a Vim pattern to `DefaultEngine` is only permitted where
   the two engines are proven **semantically equivalent** for that IR shape (leftmost-first + greedy align; empty-
   match and POSIX-longest edge cases do **not**). Until the differential corpus proves a given IR-shape class
   equivalent, that class stays pinned to `VimEngine`. Per D-010 ("don't stabilize the unvalidated"), the
   fast-path allow-list starts **empty** and grows only as tests certify shapes — so v1 behavior is
   "Vim dialect → VimEngine unless certified," and the DefaultEngine fast path is a proven optimization, not an
   assumption.
4. **Context/profile overrides for safety, not correctness:** untrusted contexts (`Plugin{trusted:false}`, `Ai`,
   `RemotePeer`) get the **strict step budget** and the **backref/lookbehind deny option** (§8); `Lsp`/`Config`
   use `Dialect::Default` and thus never reach `VimEngine`.

So: **one engine is chosen per compiled pattern**, deterministically, from capabilities + trust — the user never
selects an engine, which is the deliberate departure from Vim's `'regexpengine'` (below).

`'regexpengine'` / `\%#=1` / `\%#=2` (Vim's manual engine picker) are accepted and **ignored** as compatibility
no-ops: ruse has one semantic Vim engine, so there is nothing to switch, and the divergent-results bug they exist
to work around does not occur. This is an **Intentionally-different** item (§ Scope) — a *more* consistent
outcome than Vim's.

### 8. ReDoS / backtracking risk and mitigations

The default engine is linear and carries **no** ReDoS risk. The risk is confined to the **bounded-backtracking
fallback** in `VimEngine` (backrefs, variable-width lookbehind) — the only non-linear path. Mitigations, layered:

- **M1 — Prefer linear.** The Pike VM handles nearly all Vim atoms (§3) in linear time, *including* `\zs`/`\ze`
  and fixed-width lookaround. Backtracking is entered **only** when `Requirements` prove it unavoidable. Most Vim
  patterns therefore never touch a backtracker.
- **M2 — Step budget (hard bound).** Every match carries a `StepBudget` (a decrementing work counter, e.g. NFA
  thread-steps or backtrack-frames). Exhaustion → `RegexError::BudgetExceeded`, the match **aborts** and the
  feature degrades ("pattern too complex to complete") — never a hang (INV-FAIL-BOUNDED). Budgets are tiered by
  context (interactive incsearch: small, per-keystroke; `:%s` batch: larger; untrusted: strict).
- **M3 — Scheduler ownership + cancellation.** Matching runs as scheduler work (INV-SCHED-1); input/redraw
  outrank it; a superseded incsearch keystroke **cancels** the in-flight compile/match rather than queueing
  (INV-ASYNC-ORDER). No regex ever spins the single deterministic executor (D-002) directly.
- **M4 — Compile-time bounds.** Reject/cap pathological *compile* blowups: `\{n,m}` with huge `n` (bounded
  repetition expansion cap), excessive pattern length, and nesting depth → `RegexError::PatternTooLarge` at
  compile, before any matching.
- **M5 — Untrusted deny-list.** For `Ai`/`RemotePeer`/untrusted `Plugin` origins (INV-ORIGIN), backrefs and
  variable-width lookbehind may be **denied outright** (config-gated) so untrusted patterns are forced onto the
  linear path — no attacker-supplied pattern can even request backtracking. SEC posture: a hostile pattern is a
  **denial-of-service** vector (SEC blast-radius), bounded here to a typed error, never a stall.
- **M6 — Snapshot bound.** Interactive matching runs against a **visible-range** snapshot where possible
  (INV-QUERY-SNAPSHOT), bounding haystack size for incsearch; whole-buffer `:%s` runs as a cancellable batch task.

### 9. `C-REGEX` component & contract

`C-REGEX` (PRD.yaml: *Regex abstraction + Vim-dialect engine, build_stage: kernel*) exposes the versioned
contract (INV-CONTRACT-FIRST):

- `compile(dialect, pattern, CompileOpts) -> Result<Regex, RegexError>`
- `Regex::find(snapshot, start, budget)`, `find_all(snapshot, range, budget)`, `captures(...)`
- Typed `RegexError` (`Unsupported{atom}`, `PatternTooLarge`, `BudgetExceeded`, `Syntax{..}` — all INV-ERR-CLASS
  Errors, never panics).

Plugins receive the compiled `Regex` handle + snapshot-based results, never the internal automaton
(INV-QUERY-SNAPSHOT, INV-PLUGIN-NO-CORE). `C-REGEX` `depends_on: []` (a leaf kernel component); F-009 and the
future workspace-search bridge depend on it.

## Scope statement (Exact / Adapted / Intentionally-different)

Per D-007's per-feature grading, the Vim regex surface (VIM-SEARCH-1) is scoped as:

| Behavior | Grade | Notes |
| --- | --- | --- |
| Magic levels `\v \V \m \M` (incl. mid-pattern switch) | **Exact** | resolved in front-end (§5) |
| `\zs` / `\ze` match-boundary override | **Exact** | boundary slots (§4); no PCRE equivalent, hence the owned engine |
| Lookaround `\@= \@! \@<= \@<!` | **Exact** (fixed-width linear; variable-width lookbehind via bounded backtrack) | §3 |
| Backrefs `\1`–`\9` | **Exact**, but **budget-bounded** | may abort on pathological input (M2) instead of Vim's slow completion |
| `\< \>`, `\{-}`, `\%(...\)`, `\%[...]`, classes `\d \w \s …` | **Exact** | NFA-native (§3) |
| Position atoms `\%23l \%23c \%V \%#` | **Adapted** | supported where `MatchCtx` (line/col/visual-area/cursor) is supplied by the caller (§5); pure-string matches without a view context skip them |
| `\c` / `\C`, `'ignorecase'` / `'smartcase'` | **Exact** | folded into `CompileOpts` |
| Replacement items `& \0`–`\9` `~ \u\U\l\L\e\E \r\n\t` | **Exact** | expanded by substitute layer (§6) |
| `'gdefault'` `g` inversion | **Exact** | command layer `g XOR gdefault` (§6) |
| `:s///\=expr` | **Adapted** | ruse expression surface, not Vimscript (Non-goals, VIM-SCRIPT) |
| `'regexpengine'` / `\%#=1` / `\%#=2` two-engine selection | **Intentionally-different** | one semantic engine; picker is a no-op, divergent-result bug does not occur (§7) |
| Pathological-pattern behavior | **Intentionally-different** | bounded → typed `BudgetExceeded`; Vim may complete slowly / hang (M2) |
| Running Vimscript in patterns/replacements | **Unsupported** | L3 non-goal |

## Failure modes

- **Unsupported atom on the default engine** → `RegexError::Unsupported{atom}` from `DefaultEngine::compile`; the
  router escalates to `VimEngine` (normal path, not a user-visible error). Only surfaces to the user if the
  *dialect* was `Default` and the user typed a Vim-only atom.
- **Syntax error** (unbalanced `\(`, bad `\{`, stray `\@` context) → `RegexError::Syntax{pos, msg}`; incsearch
  shows it inline, the substitute is not applied.
- **Compile blowup** (`\{99999999\}`, giant pattern) → `RegexError::PatternTooLarge` at compile (M4).
- **Match budget exceeded** (catastrophic backref/lookbehind) → `RegexError::BudgetExceeded`; match aborts,
  feature degrades with a status note (INV-FAIL-BOUNDED); executor never stalls.
- **Position atom without context** (`\%V` in a headless string match) → the atom evaluates as non-matching (or a
  typed `Unsupported`-in-context), documented; it never panics.

## Recovery behavior

Regex compilation/matching is **stateless** w.r.t. the Document — it reads a snapshot and returns a result, so
there is nothing to recover: a failed/aborted match leaves the Document untouched (no Transaction is created
until the substitute layer applies a replacement). A `:%s` interrupted by `BudgetExceeded` mid-range applies
**no partial edit unless** the substitute layer has already committed per-line Transactions (its choice, per
D-001); the recommended mode is a single batched Transaction so an aborted substitute is atomic (all-or-nothing).
This is a substitute-layer obligation noted for F-009.

## Security impact

- **ReDoS / DoS** is the primary risk and is bounded by M2–M5. A hostile pattern (from `Ai`, `RemotePeer`,
  untrusted `Plugin`; INV-ORIGIN) cannot hang the editor: it hits the strict budget or the backref deny-list.
- **Untrusted origin patterns** run `Dialect::Default` where possible (LSP/config) and never reach the
  backtracking path unless explicitly allowed. This aligns with the SEC posture that remote/AI input carries a
  distinct, lower trust level (INV-TRUST-1) and that failures degrade with bounded blast radius (INV-FAIL-BOUNDED).
- **No code execution** from patterns: `\=` is the ruse expression surface (sandboxed, non-Vimscript), not
  arbitrary evaluation.

## Performance impact

- **Fast path stays fast:** internal/LSP/plain patterns run on the linear Rust `regex` engine — no regression vs
  using the crate directly (the abstraction is a thin trait dispatch + one IR pass).
- **Vim patterns:** the Pike VM is linear in haystack × NFA-size for all NFA-native atoms; `\zs`/`\ze`/lookaround
  add only reserved-slot bookkeeping, not backtracking. Only backref/variable-lookbehind patterns pay
  backtracking cost, and that is budget-capped.
- **Compile caching:** compiled `Regex` handles are cached by (dialect, pattern, opts) so incsearch recompiles
  only on pattern change; per-keystroke work is dominated by matching a visible-range snapshot (M6), not compiling.
- **Budgets gate CI** (D-019): incsearch match p95/p99 on a fixed corpus, including adversarial backref patterns
  that must hit `BudgetExceeded` within the deadline.

## Compatibility impact

- Unblocks **F-009** Vim search/substitute at L2 and the VIM-SEARCH "get-it-exactly-right" checklist item #9
  (Vim regex dialect + `gdefault`).
- `C-REGEX` is a leaf kernel contract; the future workspace-search bridge (ripgrep, DEP-RIPGREP) can route plain
  patterns through the same `Dialect::Default` front-end for a consistent surface, translating to ripgrep's
  Rust-`regex` dialect 1:1.
- Additive (INV-ADDITIVE): new Vim atoms are new IR variants + engine support; the contract and command args do
  not break (D-006).

## Observability

- Compile/match emit typed events `{dialect, engine_chosen, requirements, origin, budget_used, revision}` for the
  event model, macro/AI review, and perf tracing — crucially **which engine the router chose** and **why** (the
  `Requirements` that forced `VimEngine`), so a "why is this slow / why did it route to backtracking" question is
  answerable from logs.
- A `:verbose` / diagnostics view can render a pattern's parsed IR and its `Requirements` (which atoms it uses,
  whether it is linear-safe) — an explainer that also documents *why* a given pattern is `BudgetExceeded`-prone.

## Alternatives

- **A1 — Two dialect front-ends → one magic-resolved IR → pluggable engines (CHOSEN).** Parse once, lower
  per-engine, route by capability. Chosen for G4 (magic decided exactly once) and safe routing via `Requirements`.
- **A2 — Native per-engine parsing (no common IR).** Each engine parses the surface string itself. Rejected: the
  two engines *will* disagree on a magic/escaping edge case, recreating Vim's two-engine divergence we explicitly
  reject (§7); and the router would have no capability summary to pick an engine.
- **A3 — Single owned engine for everything (drop Rust `regex`).** One backtracking engine for all paths.
  Rejected: throws away the crate's linear-time ReDoS immunity on the 90% of internal/LSP/plain paths, and puts a
  hand-written backtracker on every hot path — the exact DoS surface D-002's single executor cannot afford.

## Rejected approaches

- **R1 — Option (b): scope all Vim search to Adapted/Intentionally-different (no Vim engine).** D-028's other
  branch. Rejected: `\zs`/`\ze`/lookbehind/backrefs are common in real `.vimrc`s and plugins; VIM-SEARCH targets
  L2; documenting them away fails the parity charter (D-007) for a core, muscle-memory feature. We take option (a).
- **R2 — Translate Vim patterns to Rust-`regex` syntax and use only the default engine.** Rejected: Rust `regex`
  *cannot represent* `\zs`/`\ze`, lookbehind, or backrefs at all (DEP-REGEX) — no amount of translation adds a
  capability the target engine lacks. It also mis-handles magic literalness unless we parse magic first anyway.
- **R3 — Vendor a PCRE library (pcre2/onig).** Rejected: pulls a C dependency across a trust boundary, ships an
  unbounded backtracker (ReDoS) with no step-budget hook we control, and still does not speak Vim's magic levels
  or `\zs`/`\ze` — we would need the front-end regardless. Owning the engine keeps the budget/cancellation hooks
  (M2/M3) first-class and keeps `public_api_exposure: none` honest.
- **R4 — Reproduce Vim's two engines (`'regexpengine'`) bug-for-bug.** Rejected: importing two engines that
  disagree is importing bugs; §7 exposes one semantic engine and no-ops the picker (Intentionally-different, and
  strictly more consistent).
- **R5 — `\zs`/`\ze` via lookbehind/lookahead only.** Rejected: lookbehind is fixed-width, so it cannot express
  `\(foo\)*\zsbar`; `\ze` has no lookahead form that adjusts the *reported end* without also failing on engines
  that fix match end at pattern end. The boundary-slot model (§4) is the only faithful representation.

## Trade-offs

- **We own a regex engine.** Maintenance + fuzzing cost (TEST-4-style) is real; accepted because the alternative
  (R2/R3) cannot meet Vim semantics *and* the ReDoS bound simultaneously. The Pike-VM core is well-understood and
  the backtracking fallback is small and budget-gated.
- **A conservative fast-path (§7 step 3).** Starting the DefaultEngine allow-list empty (Vim → VimEngine until
  certified) costs some performance on plain Vim searches early, in exchange for guaranteed-correct semantics
  (D-010). The allow-list is a pure optimization added under test coverage.
- **Bounded backrefs diverge from Vim** on pathological input (Intentionally-different): we return an error where
  Vim would eventually finish. Accepted — a deterministic editor cannot host an unbounded loop (D-002).

## Migration strategy

Greenfield (no prior regex layer). Land `C-REGEX` (trait + `DefaultEngine` + IR + `Vim` front-end) as a leaf
kernel component **before** F-009 reaches Vim search parity. Sequence: (1) `RegexIr` + `Default` dialect +
`DefaultEngine` (unblocks internal/LSP/plain find immediately, ReDoS-free); (2) `Vim` front-end + magic
resolution + Pike-VM `VimEngine` covering NFA-native atoms incl. `\zs`/`\ze`; (3) bounded-backtracking fallback +
budgets for backrefs/variable-lookbehind; (4) certify DefaultEngine fast-path shapes (§7 step 3) under the
differential corpus. The substitute-command layer (`gdefault`, offsets, `\=`) lands with F-009 on top of the
stable engine contract.

## Test strategy

Differential corpus (TEST-2), asserted against real Vim output; plus fuzzing (TEST-4-style) for the engine.

- **T-01 magic literalness.** `\m` (default): `a+b` matches literal `a+b`; `a\+b` matches `a`×`+`. `\v`: `a+b`
  matches `a`×; `\V`: `a.b` matches literal `a.b`. Mid-pattern switch `\va+\m.` behaves per §5.
- **T-02 `\zs` reported span.** `:s/foo\zsbar/X/` on `foobar` → `fooX`; `foo\zsbar` does **not** match `bar`
  alone (needs `foo` prefix). `\(ab\)*\zsc` on `ababc` matches/reports `c` only.
- **T-03 `\ze` reported end.** `foo\zebar` on `foobar` matches, reported end after `foo`; `:s/foo\zebar/X/` →
  `Xbar`. Multiple `\zs`/`\ze` → last-on-path wins.
- **T-04 lookaround.** `\@=` `\@!` `\@<=` `\@<!` fixed-width cases match Vim; a variable-width lookbehind routes
  to the bounded path and either matches or `BudgetExceeded` (never hangs).
- **T-05 backref.** `\(\w\+\)\s\+\1` matches a doubled word; a crafted catastrophic backref pattern hits
  `BudgetExceeded` within the interactive deadline.
- **T-06 non-greedy / boundaries.** `\{-}` lazy vs `*` greedy; `\<word\>` boundary cases; `\%(...\)` no capture.
- **T-07 routing.** `/foo` and `/\d\+/` compile on `DefaultEngine` (assert engine_chosen); `/foo\zsbar/`,
  `/\@<=x/`, `/\(a\)\1/` compile on `VimEngine` (assert Requirements forced it).
- **T-08 case flags.** `\c`/`\C` override `'ignorecase'`; `'smartcase'` interaction resolved by CompileOpts.
- **T-09 `gdefault` XOR.** Truth table: (gdefault off, no `g`) first; (off, `g`) all; (on, no `g`) all; (on, `g`)
  first. Command-layer assertion over the engine's `find_at`/`find_all`.
- **T-10 unsupported-atom escalation.** A `Dialect::Vim` pattern with `\zs` never reaches `DefaultEngine`;
  a `Dialect::Default` pattern with `\zs` yields a clean `Syntax`/`Unsupported` error, not a wrong match.
- **T-11 ReDoS bound.** Adversarial patterns × long haystacks complete or abort within budget; the deterministic
  executor's input latency is unaffected while a batch `:%s` runs (INV-SCHED-1 cancellation verified).
- **Property/fuzz.** Random Vim patterns never panic; `DefaultEngine`-vs-`VimEngine` results agree on every IR
  shape currently on the fast-path allow-list (the certification gate for §7 step 3).

## Open questions

- **OQ-1** — The exact set of IR shapes provably equivalent between `DefaultEngine` and `VimEngine` (empty-match,
  leftmost-first vs POSIX-longest, greedy edge cases) that may join the §7 fast-path allow-list. Grows under the
  differential corpus; empty at v1.
- **OQ-2** — Concrete budget numbers (thread-steps / backtrack-frames / time) per context tier, tuned on real
  workloads and gated by D-019; provisional until F-009 provides load.
- **OQ-3** — `MatchCtx` plumbing for position atoms (`\%V \%# \%23l`): how much view/cursor state the search
  command passes into the engine, and behavior for headless/plugin matches lacking it.
- **OQ-4** — Whether the workspace-search bridge (DEP-RIPGREP) should share the `Default` front-end or translate
  independently; interaction with ripgrep's own `regex` version skew.
- **OQ-5** — `\=` ruse-expression surface scope (which `submatch()`-style builtins) — coordinate with the
  editing-language / command layer (D-025) rather than fix here.
- **OQ-6** — Whether to mint an `INV-REGEX-BOUNDED` ("no unbounded match on the executor; every match is budgeted")
  in the reference-invariants registry, or leave it expressed via INV-FAIL-BOUNDED + INV-SCHED-1. Decide with the
  `spec validate` maintainers (D-022).

## Reference Invariants

INV-CONTRACT-FIRST, INV-CMD-SEMANTIC, INV-QUERY-SNAPSHOT, INV-ASYNC-ORDER, INV-SCHED-1, INV-FAIL-BOUNDED,
INV-ERR-CLASS, INV-ORIGIN, INV-ADDITIVE, INV-TRUST-1, INV-PLUGIN-NO-CORE (see
[../invariants/reference-invariants.md](../invariants/reference-invariants.md)).
