---
doc: editing-language
project: ruse
title: "ruse Editing-Language Composition Engine (D-025)"
summary: >
  Resolves the blocking gap V-1: the atomic Semantic Command Layer cannot express Vim's two-level
  operator+motion grammar. Defines a first-class core subsystem (C-EDITLANG) that composes
  operator + count + (motion | text-object) into a typed Range and emits a single Transaction — with the
  exclusive→inclusive→linewise promotion, a re-parameterizable change-intent for dot-repeat, and
  plugin-registrable operators (g@). editor.delete_selection is a *sibling* of operator+motion, not its
  generalization.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - register-model.md
  - persistence-and-recovery.md
  - ../invariants/reference-invariants.md
  - ../parity/vim.md
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->
<!-- design-code-ack: ChangeIntent — design ahead of code: v0 records {lead, insert} (the operator / insert-entry command plus the insert-session commands captured until <Esc>), replayed verbatim by `.` at the new cursor; the fuller {operator, count, target, inserted_text} decomposition is the intended model, deferred. -->

# ruse Editing-Language Composition Engine (D-025)

## Problem

`architecture.md §1.1` once modeled `dw / diw / dd` as surfaces of one atomic `editor.delete_selection`.
That is the *selection* model — wrong for Vim: at `dw` there is **no selection**. Vim composes
`[count1] operator [count2] {motion | text-object}`, where the motion yields a **typed range** that is
*promoted* before the operator consumes it (governing `d}`, `d/pat`, `cw`-acts-like-`ce`, …). The atomic
command layer (INV-CMD-SEMANTIC) has no place for this composition, so operator-pending, dot-repeat, and
custom operators are unreachable. This is the V-1 NO-GO for Vim L2.

## Goals

- A core composition engine (`C-EDITLANG`) that turns operator + count + motion/object into **one
  Transaction**, reproducing the VIM-MOT-PROMOTE checklist exactly.
- Dot-repeat as a re-parameterizable **change-intent**, distinct from transaction replay and macro
  command-lists (VIM-REPEAT-DOT).
- **Plugin-registrable operators** (`g@`/`operatorfunc`) that are dot-repeatable.
- Command→input re-entry for `:normal`/`:global` (V-9).

## Non-goals

- Not the Emacs/Native selection-editing path (that uses `editor.delete_selection` on an existing
  selection — a *sibling*, §7). Not the register data model (that is [register-model.md](register-model.md), D-026).

## v0 — single-key edits (SHIPPED)

`r{char}` replaces the character under the cursor (via the input engine's `Awaiting::ReplaceChar` state — the
key after `r` is the replacement; ctrl-r stays redo), leaving the cursor on it; it is a no-op at end-of-line
or over a newline. `~` toggles the ASCII case of the char and moves right (a non-letter just moves). `J` joins
the current line with the next on a single space, dropping the next line's leading indent (no-op on the last
line). Deferred: counted `r`/`~`, non-ASCII case folding, and `J`'s Vim niceties (no space before `)`, keep
existing trailing space).

## v0 — insert-entry commands (SHIPPED)

Beyond `i`/`a`, the common insert entries ship as their own semantic commands (so a trace records the intent):
`o`/`O` open a new line below/above and enter Insert (an edit — insert `\n` at the line end / start), `A`
appends at the line end, `I` inserts before the first non-blank. Deferred: autoindent (the new line copies no
indentation yet), and the Visual-mode meanings of `o`/`I`/`A` (swap-ends / block-insert).

## v0 — small-word vs WORD motions (SHIPPED)

Word motions now use Vim's character classes. Small-word `w`/`b`/`e` recognize **three classes** — whitespace,
*word* (ASCII alphanumeric + `_`, plus all non-ASCII bytes so a multibyte identifier is one word), and
*punctuation* — so `foo.bar` is three words (`foo`, `.`, `bar`). `W`/`B`/`E` are the **WORD** motions: two
classes (whitespace vs not), so `foo.bar` is one WORD. Both share one class-parameterized scanner (`big:
bool`); class changes only fall on char boundaries, so positions stay valid. `cw`/`cW` still behave like
`ce`/`cE` (don't eat the trailing whitespace). Deferred: text objects (`iw`/`aw`) are still 2-class
(whitespace vs not) — upgrading them to the same three classes is a follow-up.

## v0 — bracket-match motion `%` (SHIPPED)

`%` (`Motion::MatchBracket`) jumps between the pairs `()`, `[]`, `{}`. It is **nesting-aware** (depth counted
per pair type, so `([)]` matches `(`↔`)` exactly as Vim), and the match may be on **another line** (only the
initial "which bracket" scan is line-local: if the cursor is not on a bracket, the first bracket forward on
the current line is matched). As an operator target it is **charwise-inclusive** — `d%` deletes both brackets
and everything between. `count%` (jump to a percentage of the file) is a *different* Vim feature and is
**deferred**; bare `%` ignores any count for now. Deferred too: language-aware matching (strings/comments) and
configurable `matchpairs`.

## v0 — line-jump motions (SHIPPED)

`gg` (first line), `G` (last line), and `{count}G` / `{count}gg` (go to line N) ship as motions, landing on
the target line's **first non-blank** char (Vim). They are **linewise** under an operator: `dG` deletes from
the cursor line through the last line, `dgg` through the first — `op_range`/`change_range` compute the
whole-line span between the cursor and the target (`change` keeps the final newline, leaving a line to type
into). `Motion::{GotoLine, LastLine}` carry no data (the line number rides in the command's count);
`target` resolves them directly rather than via the repeat loop (a count is an absolute line, not a repeat).
Input: bare `G` reads the count (or jumps to the last line when none); `gg` is a two-key prefix wired through
the input engine's `Awaiting::GSecond` state (see input-engine.md v0). Deferred: `gj`/`gk` display-line
motions, and the rest of the `g`-prefix family.

## v0 — char-search motions (SHIPPED)

The v0 editor ships `f`/`F`/`t`/`T` char-search as ordinary motions, so they compose with the grammar for
free — bare moves (`fx`), operator targets (`dtx`, `d2f)`), and Visual-mode extension all fall out of the
same `Motion` plumbing. Semantics: `f`/`t` search forward, `F`/`T` backward; `f`/`F` land **on** the
`count`-th match, `t`/`T` stop one char **short** of it; the operator range is inclusive *through* the landing
for forward search (`dfx` removes `x`, `dtx` stops before it). Search is confined to the **current line**
(never crosses a newline), matching Vim.

The char argument rides inside the motion — `Motion::FindChar { ch, forward, till }` (still `Copy`) —
resolved in `motion::target` / `motion::char_span`, and serialized in a trace as a single whitespace-free
token `find_char:<codepoint>:<fwd>:<till>` so the `<count> <motion>` line form is unchanged. The input engine
holds a one-key `pending_find` state (the key after `f` is the target) plus `last_find` for `;` (repeat) and
`,` (repeat reversed). Deferred: Vim's `t`-repeat adjacency quirk, and search across a count that spans lines.

## Model

### Range — the typed motion result

```rust
pub enum RangeKind {
    CharwiseInclusive,   // f, t, e, $, %, ...
    CharwiseExclusive,   // w, b, 0, /pat, ...
    Linewise,            // j, k, G, dd, ...
    Blockwise { cols: ColSpan },  // <C-v> forced
}
pub struct Range { pub start: Anchor, pub end: Anchor, pub kind: RangeKind }
```

Positions are **anchors** (INV-ANCHOR), so a range stays valid across the (single) transaction that consumes it.

### Motion / TextObject → Range

```rust
pub trait Motion   { fn resolve(&self, cx: &EditCx, count: Count) -> Range; }
pub trait TextObject { fn resolve(&self, cx: &EditCx, count: Count, around: bool) -> Range; }
```

- Motions declare their natural `RangeKind`. Text objects (`iw`, `a{`, `it`) are Range producers, not motions
  (fixes anti-pattern VIM-5 "collapse text object and motion into one type").
- `dw`/`de`/`d$` are **not** separate commands — they are the `d` operator applied to the `w`/`e`/`$` motions
  (fixes VIM-4).

### Promotion (VIM-MOT-PROMOTE) — applied before the operator consumes the range

```
1. exclusive→inclusive: if an exclusive motion ends in column 1, move end to the end of the previous line
   and make the range CharwiseInclusive.
2. exclusive→linewise: if (1) applied AND the motion started at/before the first non-blank of its start
   line, make the range Linewise.
3. forced motion after an operator overrides kind: v = force charwise (toggle incl/excl),
   V = force Linewise, <C-v> = force Blockwise.
```

`cw`/`cW` special-case: when the cursor is on a non-blank, `c`+`w` behaves like `c`+`e` (no trailing
whitespace) — implemented as an operator-specific range adjustment, preserving the Vi wart (VIM-OP-CW).

### Operator — consumes a Range, writes a Register, emits a Transaction

```rust
pub trait Operator {
    fn apply(&self, range: Range, reg: RegisterId, count: Count, cx: &mut EditCx) -> CommandOutcome;
    fn is_undoable(&self) -> bool { true }
}
```

- The operator reads the range's `RangeKind` to set the **register type** (char/line/block) it writes — the
  bridge to [register-model.md](register-model.md) (D-026): paste geometry follows the stored type.
- Built-in operators: `d c y > < = gu gU g~ ! gq g? zf`. `x`=`dl`, `D`=`d$`, `C`=`c$`, `s`=`cl`, `S`=`cc`
  are aliases that construct the operator+motion internally.
- Result is a **single `CommandOutcome::Transaction`** (one undo group — D-005) or a `Composite`/`AsyncTask`
  for `=`/`!` (external filter). The outcome enum is bounded (no unlimited effect system; anti-pattern CMD-14).

### Operator-pending state machine

```
Normal --[count1]--> Normal(count1)
       --operator--> OperatorPending{op, count1}
OperatorPending --[count2] motion|object--> resolve Range --promote--> op.apply --> Transaction --> Normal
OperatorPending --same-op (dd/yy/cc)------> Linewise current [count] line(s) --> op.apply
OperatorPending --<Esc>------------------> Normal (abort, no change)
```

Operator-pending is a **transient state axis**, not a special case sprinkled through the code (fixes VIM-1/
VIM-12). Effective repetition = `count1 × count2` (VIM-CNT); counts are consumed only here, not per-operator.

> **Mode axes:** input mode / operator-pending / count-buffer / register-prefix are **independent state
> axes**, not one giant `EditorMode` enum (avoids combinatorial explosion — design-requirements §9).

## Change-intent (dot-repeat) — VIM-REPEAT-DOT

Distinct from undo transactions and macro command-lists:

```rust
pub struct ChangeIntent {
    pub operator: OperatorId,
    pub target: RepeatTarget,      // Motion | TextObject | ForcedLinewise
    pub count: Count,
    pub inserted_text: Option<Rope>,  // for c/i/a/o insert sessions
    pub register: RegisterId,
}
```

`.` **re-resolves** `target` against the *current* cursor and re-applies the operator (optionally with a new
count), replaying `inserted_text` for change/insert. It does not blindly replay a byte diff (that would be
wrong at a new location) and it is not a macro (which replays keys). One `ChangeIntent` is recorded per
top-level change.

## Plugin-registrable operators (g@) — VIM-OP-9

`g@` sets the pending operator to a plugin-provided `operatorfunc`; the engine resolves the motion/range as
usual and hands the plugin a **Range + snapshot**, receiving a `CommandOutcome` (transaction request) back —
so custom operators compose with motions/objects and are **dot-repeatable** via the same `ChangeIntent`.
Plugins never mutate the buffer directly (INV-PLUGIN-NO-CORE); they return a transaction request.

## Command → input re-entry (:normal / :global) — V-9

The input engine is **drivable as a library**: a command may push a key/command sequence into a **batched
execution context** with a synthetic cursor. `:global` is two-pass (mark matching lines via anchors, then
run the command per line); `:normal {keys}` feeds Normal-mode keys from a command. Re-entry runs on the
single-threaded deterministic executor and is guarded against re-entrant mutation (INV-ASYNC-ORDER); each
driven change is still a Transaction.

## Relation to the Semantic Command Layer (§7)

`C-EDITLANG` sits **between input and the command/transaction layer**: it composes operator+motion+count+
register and **emits a Transaction** which flows through the normal pipeline
(`architecture.md` ARCH-FLOW-001). `editor.delete_selection` is a **sibling operator** that takes an
existing selection → Range (used by Emacs/Native selection editing and by Visual-mode operators), **not** the
generalization of `dw/dd`. Both ultimately produce a Range consumed by an operator.

## Reference Invariants
- **INV-CMD-SEMANTIC** — bindings resolve onto semantic commands; the editing-language engine composes them,
  it does not bypass them.
- **INV-ANCHOR** — motion ranges are anchor-based.
- **INV-TXN** — a composed change is exactly one Transaction (one undo group).
- (See [../invariants/reference-invariants.md](../invariants/reference-invariants.md).)

## Failure modes / Recovery
- Invalid range (end < start, outside buffer) → assert (invariant violation), not error (stability §1).
- A plugin operator that times out / errors → the pending operation is aborted with no partial mutation
  (preflight, stability §13); operator-pending returns to Normal.

## Performance impact
- No per-combination command registration (one operator × one motion table, not `O(ops×motions)` commands) —
  avoids anti-pattern VIM-3/PERF. Range resolution is O(motion), promotion O(1).

## Compatibility impact
- Directly enables the Vim L2 "get-it-exactly-right" checklist (VIM-MOT-PROMOTE, VIM-OP-CW, VIM-CNT,
  VIM-REPEAT-DOT). Emacs/Native reach the same operators via the selection→Range sibling path.

## Alternatives / Rejected approaches
- **Rejected: `editor.delete_selection` as the generalization of `dw/dd`.** No selection exists at `dw`;
  loses range-kind/promotion, dot-repeat, and custom operators (the V-1 finding).
- **Rejected: register a command per operator×motion (`dw`, `de`, `d$`, …).** Combinatorial explosion; can't
  express counts/objects/custom operators (VIM-3/4/5).
- **Rejected: model motions as commands.** Motions aren't actions; they're range producers consumed by an
  operator or used for cursor movement.
- **Rejected: an unbounded effect system for operator results.** Hides control flow; the `CommandOutcome`
  enum is bounded (CMD-14).

## Trade-offs
- A dedicated composition engine + Range IR is more machinery than a keymap tier — accepted: it is the only
  structure that reproduces Vim's editing language and is load-bearing for dot-repeat and custom operators.

## Open questions
- Exact `RepeatTarget` serialization for macro/`ChangeIntent` interplay when a macro contains a `.`.
- Blockwise operator semantics for ragged-right blocks and virtual columns (`VIM-MODE-4` blockwise + `$`).
- Interaction of `=`/`!` external-filter operators with the single-transaction rule (may need a
  Composite outcome + preflight).
