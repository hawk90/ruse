---
doc: rfc
project: ruse
title: "RFC-0020: Ex edits are recorded in the trace as resolved commands"
summary: >
  Ex edits (`:s`, `:d`, `:m`, `:t`, `:sort`, `:j`, `:>`/`:<`, `:g`, `:normal`, `:r`, `:{range}!`) run core
  functions straight from the TUI session, so they never reach the command trace and a saved trace of a session
  that used them replays to a different document with no warning. This RFC decides that they are recorded as
  resolved semantic commands: one `Command::Ex(ExCmd)` with resolved line numbers and the literal pattern and
  flags used, external bytes recorded as data, `:g`/`:normal` as a group of their children, and only the
  replay-relevant `:set`s. The trace stays `v1` with additive verbs.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/DECISIONS.md
  - ../../design/command-engine.md
  - ../../design/editing-language.md
  - ./RFC-0012-collapse-to-two-crate-editor.md
  - ./RFC-0007-transaction-engine.md
---

# RFC-0020: Ex edits are recorded in the trace as resolved commands

- **Status:** proposed
- **Author(s):** Claude Opus 5.5 (design agent), for maintainer review
- **Created:** 2026-11-03
- **Decision link:** D-061

<!-- Hard-to-reverse: this adds verbs to the trace file format (RFC-0012's product pillar) and a variant to core's
     `Command` enum, the boundary every input engine produces into. -->

## Summary

The trace (`crates/core/src/trace.rs`, RFC-0012) promises that the same initial document and the same trace give
the same final document. Today only commands that pass through `run_cmd` (`apps/tui/src/app/dispatch.rs`) are
recorded; Ex edits call `Workspace` methods straight from `Session::execute_ex_one`
(`apps/tui/src/app/session.rs`), so they are missing from the trace, and `&`/`g&` return before `run_cmd`. This RFC
decides, following [command-engine.md §7.3](../../design/command-engine.md), that every Ex edit is recorded as
**resolved semantic commands**: what ran, with every cursor-relative or history-relative input already resolved,
so replay needs no Ex parser, no cursor context and no IO.

## Motivation / Problem

**The bug.** Record a session that runs `:%s/a/b/g`, `:g/x/d` or `:sort`, then `:trace save t.trace`. `ruse
--replay t.trace file` prints the document *without* those edits, or with later commands applied at the wrong
places, and reports success. Nothing warns that the trace is incomplete. This breaks the determinism contract
RFC-0012 makes a product pillar, and the trace corpus cannot cover Ex edits at all.

It also departs from the AGENTS.md boundary "Input → semantic `Command` → editor": the Ex path is input that
mutates through a side door (#1105, split from #892 after #1094 merged the two Ex dispatches).

**Why the obvious fixes are not enough.** Recording the Ex *text* would replay `:s//X/` against whatever the last
search is at replay time, `:.,+3d` against wherever the cursor is, and `:r !date` against today's clock. Each
of those is a determinism leak. The record has to hold what the command *resolved to*.

## Guide-level explanation

Nothing changes for a user typing Ex commands. What changes is the trace:

```text
# ruse-trace v1 doc_hash=1f3a…
set tabstop 8
set shiftwidth 4
enter_insert
insert_char 104
enter_normal
ex_substitute 1 12 g 61 62
ex_begin global
ex_delete 3 3 34
ex_delete 5 5 34
ex_end
ex_read_text 7 6865790a
```

- `:%s/a/b/g` on a 12-line buffer records the lines it covered (1 to 12), the flags, and the pattern and
  replacement as hex (`61` is `a`), even if the user typed `:%s//b/g` and the pattern came from the last search.
- `:g/x/d` records each delete it ran, at the line it hit when it ran, inside one group, so replay is one undo
  step as it was live.
- `:r !date` records the bytes `date` printed. Replay inserts those bytes; it never runs `date`.
- `:set sw=4` records `set shiftwidth 4`. `:w`, `:e`, `:cd` record nothing.

The verb spellings above are illustrative; the code is the source of truth for the format (D-038).

## Reference-level explanation

### R1. One `Command::Ex(ExCmd)` variant, resolved

Core's `Command` gains one variant, `Command::Ex(ExCmd)`. `ExCmd` is a core type naming the resolved form of each
Ex edit. Every field is a value, never a reference to editor state:

- **Ranges are resolved line numbers.** `SubRange::CurrentLine`/`WholeFile` and every address form (`.`, `$`,
  `'a`, `/pat/`, `+3`, `\/`) are resolved by the frontend before the command runs, and the record carries the
  1-based inclusive `(first, last)` that ran. Destinations (`:m`/`:t`) record the resolved insert index, not
  `LineAddr::Current`/`CurrentOffset`.
- **Patterns and flags are literal.** `:s` records the pattern and replacement actually used, after the empty
  pattern became the last search, `~` in the replacement became the previous replacement, and `&`/`:&&`/`g&`
  became the remembered substitute. The flags are the effective ones (`g` already XORed with `'gdefault'`, `i`/`I`
  resolved). The `c` confirm flag records the user's per-match answers in order, so replay needs no prompt.
- **Registers are named, not read.** `:d x` and `:y x` record the register name; the text moves inside the core
  on replay as it did live.

Pure edits recorded this way: `:s` and its repeats (`&`, `g&`, `:&`, `:&&`, bare `:s`), `:d`, `:m`, `:t`/`:co`,
`:sort`, `:j`, `:>`/`:<`, `:retab`, `:pu`. Each replays by calling the same `EditorState` method the `Workspace` op already calls
(`substitute`, `delete_lines_into`, `move_lines`, `copy_lines`, `sort_lines`, `join_lines`, `shift_lines`,
`retab`, `put_lines`), so live and replay share one implementation.

### R2. IO-dependent edits record their bytes

The core stays IO-free (D-039). An Ex edit whose input comes from outside the document records the bytes the
frontend obtained, as data:

- `:r file` and `:r !cmd` → `ReadText { after_line, bytes }` (the bytes after the BOM/CRLF normalisation the
  frontend applies, as `read_lines` receives them).
- `:{range}!cmd` → `FilterReplace { range, bytes }` (the filter's output that replaced the range).

Replay never opens a file or spawns a process. The command line that produced the bytes may be kept as a
comment for the reader, never executed.

### R3. `:g` and `:normal` record as a group of their children

`:g` and `:normal` are drivers (command-engine.md §7.3): their effect is to run other commands. They record as
`ExCmd::Begin { kind }`, then their children, then `ExCmd::End`. Replay opens one undo block at `Begin` and
closes it at `End`, as `Session::as_normal` does live (#852), so `u` after replay undoes what one `u` undid live.

- A `:g` child is the resolved form of what ran on each marked line, at the line number it had *when it ran*
  (earlier deletes have already shifted it). The pattern that marked the lines is not needed on replay.
- A `:normal` child is each `Command` its keys produced, plus the cursor placement `:normal` does per line of
  its range. The keys themselves are not recorded: the command level survives keymap changes (RFC-0012).
  Today these children already reach `run_cmd` (#1096) but without the group and the placement.
- Groups nest (`:g/x/normal …`), bounded by the live limits (`MAX_NORMAL_DEPTH`, E192); an `End` without a
  `Begin` is a parse error.

### R4. Non-edit Ex commands, and replay-relevant state

`:w`, `:e`, `:b`, `:cd`, `:split`, `:trace` and the other non-editing commands are not recorded. The exception is
state the core reads while applying a command. `:set` records `ExCmd::Set { option, value }` when the option is
one the core consults during an edit, for example `tabstop`, `shiftwidth`, `expandtab`, `textwidth`,
`ignorecase` and `smartcase`. The rule: an option is recorded if and only if `apply_command` or an `EditorState`
Ex method reads it. Because a user's config sets these before any key, the trace also writes their values at
its start, after the header, so a different config on the replaying machine does not change the result.

### R5. Format compatibility

The trace stays `v1`. The new verbs are additive (INV-ADDITIVE, D-006): a v1 trace written before this RFC has
none of them and replays exactly as before. A trace that uses them fails to parse on an older binary with
`BadCommand` (an unknown verb), which is the honest failure; there is no silent skip.

## Reference Invariants

- **INV-CMD-SEMANTIC** (depends) — Ex edits become semantic commands on the same `Command` boundary as keys.
- **INV-TXN / INV-UNDO** (depends) — each child is still its own transaction; the group reproduces live undo
  grouping, nothing more.
- **INV-ORIGIN** (depends) — replayed commands carry their replay origin as other trace commands do.
- **INV-ADDITIVE** (depends) — the trace format grows by verbs only; `v1` traces stay valid.
- **INV-FAIL-BOUNDED** (depends) — a malformed group or verb is a typed `TraceError`, never a panic.
- Core stays IO-free (D-039): no invariant is added; R2 is how the existing boundary is kept.

## Failure modes & Recovery

- **Unknown verb on an older binary** → `TraceError::BadCommand { line }`; the replay refuses to start.
- **Unbalanced group** (`End` with no `Begin`, or end of file inside a group) → a typed parse error.
- **A resolved range out of bounds at replay** means the trace does not belong to this document; `doc_hash`
  already catches a different starting document, and an out-of-range line is clamped exactly as the live
  command clamps it, so replay of a valid trace is still exact.
- No recovery surface: a trace is a file read once.

## Security impact

A trace can now hold file and process output (R2), so a saved trace may carry more of the user's data than the
document. RFC-0012 already notes that traces carry document content; the same caution applies. Replay is safer
than before, not riskier: it never runs a shell command or reads a file named in the trace.

## Performance impact

Recording is one `Command` clone per Ex edit, plus the bytes for `:r`/`:!`, which are already in memory. A `:g`
over N lines records N children; that is what it does live. Not on any per-keystroke path.

## Compatibility & Migration

- Old traces replay unchanged (R5).
- `Command` gains one variant; the `to_line`/`from_line` round-trip test (`round_trips_every_variant` in
  `crates/core/src/command.rs`) covers it.
- `Session::execute_ex_one` changes from "call the `Workspace` op" to "build the resolved `ExCmd`, run it through
  `run_cmd`". `run_cmd` records it and applies it, so recording cannot be forgotten per verb.
- `&`/`g&` stop returning before `run_cmd`; they resolve to `ExCmd::Substitute` and go through it.

## Observability

`:trace save` reports the command count, which now includes Ex edits. A replay that hits an unknown verb names
the line. Nothing else is new.

## Alternatives

1. **Record the raw Ex text** (`ex "%s//b/g"`). Rejected: replay would re-resolve the empty pattern against the
   replay-time last search, `.` and marks against the replay-time cursor, and `:r !cmd` by running the command
   again. Each is a determinism leak, and the core would need the Ex parser, which lives in `apps/tui`.
2. **One `Command` variant per Ex verb** (`Command::Substitute`, `Command::Sort`, …). Rejected: it spreads about
   a dozen variants across the key-command namespace, where `plan` would have to know each, and it gives no home
   for the `Begin`/`End` group. One `Ex(ExCmd)` keeps Ex edits one family with one apply path.
3. **Record only the effect, as an `EditList` of byte edits.** Rejected: it replays the bytes but not the
   behaviour around them (registers filled by `:d x`, marks moved, the last substitute that `&` repeats, the
   cursor `:s` leaves), and the corpus would stop testing the Ex implementations at all. It is also brittle to
   read and review.

## Rejected approaches

Raw Ex text (Alternative 1) is the tempting one and must not be re-litigated: it is shorter, but it moves
history- and IO-dependence into replay, which is exactly what the trace exists to exclude.

## Trade-offs

- The frontend must resolve every input before running the command. It already does most of this; the cost is
  making it explicit and complete.
- A trace grows by one line per `:g` child instead of one per `:g`. Accepted for exact undo grouping and for
  independence from the marking pattern.
- Recording process output in the trace (R2) trades file size and data exposure for determinism.

## Re-evaluation conditions

- If session persistence or a trace `v2` lands, the verbs here are folded into whatever that format is.
- If Ex parsing moves into the core, recording the Ex text plus a resolution log could be reconsidered.
- If a non-Ex frontend path (search under cursor, an LSP edit, a plugin) is found to mutate outside `run_cmd`,
  it gets the same treatment, and this RFC's rule becomes general rather than Ex-specific.

## Open questions

1. Whether registers filled *before* the trace starts (and the clipboard registers `"+`/`"*`) should be
   snapshotted into the trace header, by the same "external input is data" rule as R2. Not needed for Ex edits;
   it affects `p`.
2. Whether an `\=` replacement whose value depends on anything outside the document should record the text
   it produced per match instead of the expression (D-025 owns the expression surface).
