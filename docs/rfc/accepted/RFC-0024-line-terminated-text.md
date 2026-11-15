---
doc: rfc
project: ruse
title: "RFC-0024: Line-terminated buffer text, with the final newline as 'endofline'"
summary: >
  ruse keeps a buffer as the file's bytes, so a buffer cannot tell "one empty line" from "two empty lines and
  no final newline", and an emptied buffer loses its one line when a command adds lines to it (#1298, #730).
  This RFC makes the text line-terminated in memory: empty, or ending in a newline. Whether the file ends in a
  newline becomes only the `'endofline'` flag, applied when text is read and when it leaves the buffer.
audience: [maintainers, contributors, llm-agents]
status: accepted
related:
  - ../../design/persistence-and-recovery.md
  - ../../architecture/architecture.md
  - ../../../spec/DECISIONS.md
  - ../../invariants/reference-invariants.md
  - ./RFC-0007-transaction-engine.md
  - ./RFC-0008-document-model.md
  - ./RFC-0023-per-transaction-journal.md
---

# RFC-0024: Line-terminated buffer text, with the final newline as 'endofline'

- **Status:** accepted — D-065 decided (design), approved by the maintainer 2026-11-15
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-15
- **Decision link:** D-065

<!-- Hard-to-reverse: the shape of the text every core function, journal and trace sees. -->

## Summary

A Document's text is either empty or ends in `\n`. The last line's newline is always in the buffer, as Vim
always has it; whether the FILE ends in one is the buffer's `'endofline'` (`FileFormat::final_newline`), set
when the file is read and applied whenever the text leaves the buffer (a write, a pipe, a filter, a byte
count). The empty text is Vim's `ML_EMPTY` buffer: it shows one empty line, writes zero bytes, and a command
that adds lines treats it as that one empty line. A transaction never leaves non-empty text without its final
newline: the Document keeps it.

This extends [RFC-0008 §6](./RFC-0008-document-model.md) ("encoding and line-endings are metadata,
not data") to the one line-ending it left in the data: the file's last one.

## Motivation / Problem

ruse stores a buffer as the file's bytes after line-ending and BOM conversion (F-008, `FileFormat::to_buffer`).
Every core line function then reads a final `\n` as the last line's terminator (43 sites in `crates/core`:
`motion::content_end`, four `line_count`s, `last_line_start`, the phantom-slot clamp of #677), and a file
without one simply has an unterminated last line. Two states cannot be told apart:

| Vim state (lines, `'eol'`) | ruse bytes today | ruse reads it as |
|---|---|---|
| `['']`, eol | `\n` | `['']` |
| `['', '']`, noeol | `\n` | `['']`, a line lost |
| `ML_EMPTY` (after `ggdG`) | (empty) | `['']`, and a command that adds lines loses it |

So (#1298) `x` on the only character of a last line without a final newline (`\nh`) leaves `\n`: nvim has two
empty lines, ruse has one, and the cursor is left on the slot after the newline, where `k`, `j` and `G` cannot
reach it again. And (#730) a command that adds lines to an emptied buffer loses its one empty line. A probe of
48 cases against nvim v0.12.6 (`--clean`, `'nofixeol'`, which is ruse's byte-preserving default) found 14
written files that differ, all from these two causes or from the `'eol'` flag having no effect on text that
already ends in a newline:

| Case (file, keys) | nvim writes | ruse writes |
|---|---|---|
| `a\nb\n`, `ggdGo` | `\n\n` | `\n` |
| `a\nb\n`, `ggdGO` | `\n\n` | `\n` |
| `a\nb\n`, `yyggdG:put` | `\na\nb\n` | `a\nb\n` |
| `a\nb\n`, `yyggdGP` | `a\nb\n\n` | `a\nb\n` |
| `a\nb\n`, `ggdG:r OTHER` | `\nr1\nr2\n` | `r1\nr2\n` |
| empty, `:0r OTHER` | `r1\nr2\n\n` | `r1\nr2\n` |
| empty, `o` | `\n\n` | `\n` |
| `a\n`, `:set noeol` | `a` | `a\n` |
| `a\nb\nc\n`, `:%s/\n//` | `abc\n` | unchanged |

(The other five are `:put ={expr}`, which ruse does not have; that is a separate gap, not this RFC's.)

## Guide-level explanation

What the user sees does not change except where ruse is wrong today. A file `a` with no final newline opens
as one line `a` and `[noeol]`; it saves as `a`. Deleting everything with `ggdG` gives the emptied buffer:
`:w` writes an empty file, as nvim does, and `o` there gives two empty lines. `x` on the last character of
`\nh` leaves two empty lines, and the cursor stays on the second. `:set noeol` on a file that had a final
newline drops it on the next write, and `:set eol` adds one.

## Reference-level explanation

### 1. The invariant

`INV-TEXT-TERMINATED`: a Document's text is empty or ends in `\n`. The empty text is the only `ML_EMPTY`
state; `\n` is one empty line. Line functions keep their current rule (a final `\n` ends the last line), which
is now always true, so none of the 43 core sites changes.

### 2. Reading text in

Every path that turns outside bytes into buffer text ends with one canonicalisation: non-empty text that does
not end in `\n` gets one, and `'endofline'` records that the file had none. It is `FileFormat::to_buffer`
(open, `:e`, `:e!`, `'autoread'` reload, the recovery prompt's disk text, `ruse --replay`'s start text,
`headless`'s file reads), plus recovered journal text from a journal an older build wrote (RJ01 Checkpoints of an
unterminated text). `'binary'` reads the same way: the bytes are kept, and only the final newline becomes the
flag. A read whose format is forced (`++ff=`, `++bin`, `find-file-literally`) reads `'endofline'` again
from the raw bytes in that format (`FileFormat::reading`), so a classic Mac file read as unix and reopened
with `++ff=mac` keeps its last CR. Text read INTO a buffer (`:r`, `:r !`, a filter's output, a put) is lines: each piece is terminated
where it lands, and `'endofline'` does not change (nvim: `:r` of a file without a final newline, or `:%!printf
x`, leaves `'eol'` as it was).

### 3. Text leaving the buffer

One function, `WriteOpts::encode` (already the one encoder for `:w`, `:w >>`, `:w !cmd`, `:up`, `:wq`,
`:sav`), gives the bytes: the empty text writes nothing; otherwise the final `\n` is dropped when the range
includes the last line and neither `'endofline'` nor the write policy (`'fixeol'`, Emacs
`require-final-newline`) asks for one. The same rule applies to a filter's input (`:{range}!`, and `gq` / `=` through `'formatprg'` / `'equalprg'`), whose range
text comes from `range_text`, and to the byte counts the user sees (`g CTRL-G`, `wordcount()`-style counts, the
`:w` message). nvim checks: `:w !od -c` and `:%!od -c` of `a\nb` without `'eol'` send three bytes; `:1w part`
of it writes `a\n`, `:2w part` writes `b`.

### 4. The Document keeps the final newline

`Document::apply` checks the result: if a transaction would leave non-empty text that does not end in `\n`,
the same transaction also inserts one at the end. Its inverse removes it, so undo is exact. This is what Vim
does with the last line's terminator, which no edit removes:

- typing into the emptied buffer (`ggdGia`) gives `a\n`;
- `:s` replaces the last line's `\n` (nvim matches it there, even without `'eol'`) but the terminator stays:
  `2s/\n/-/` on `a\nb\n` gives `a\nb-\n`, and `2s/\n//` changes nothing. The substitute plan puts the
  replacement before the final newline, so it is not a dropped newline that the Document must restore.

### 5. The emptied buffer adds lines to its one line

A command that adds lines treats the empty text as `\n`: `o` and `O`, linewise `p`, `P`, `gp`, `gP`, `]p`,
`[p`, `:put`, `:r`, `:r !` and `:0r`. Because the text is empty, the resulting edit is one insert of the whole
result at 0, computed against `\n`; the cursor and the change marks are placed from that result. nvim's
results are the reference (table above): `ggdGP` of `a\nb\n` gives `a\nb\n\n`, `ggdGp` gives `\na\nb\n`.

### 6. Slices

1. **Read and write (frontend).** Canonicalise on read (§2) and apply `'endofline'` on the way out (§3). Fixes
   #1298 and `:set noeol`; safe alone, because no Vim command removes the final newline and `encode` still adds
   one to the emptied buffer's typed text when `'endofline'` asks.
2. **The Document keeps it, and the emptied buffer (core).** §4 and §5. Fixes #730 and `:%s/\n//`.

Each slice adds the nvim cases above as PTY tests asserting nvim's bytes, and core unit tests for the line
structure and the cursor (the parity corpus compares lines, not the written file).

## Reference Invariants

- **INV-TEXT-TERMINATED** (new): the text is empty or ends in `\n`; enforced by `Document::apply` (§4) and the
  read paths (§2), and checked by a property test over Vim command sequences.
- **INV-TXN / INV-UNDO:** the restored newline is part of the transaction, so undo and the journal see it.
- **F-008 byte-preserve:** an unmodified buffer writes back its exact bytes; the existing `to_buffer`/`to_disk`
  property extends to the canonicalisation and its inverse in `encode`.

## Failure modes & Recovery

- An RJ01 journal written before slice 1 holds unterminated text for a file without a final newline. Its
  replayed text is canonicalised on recovery (§2). A large-file (`DiskBase`) journal of such a file hashed the
  unterminated disk text, so after the change its base no longer matches and recovery reports it unreplayable
  and keeps it (RFC-0023 §5); this needs a file over 32 MiB, without a final newline, with an unsaved journal,
  across an upgrade.
- An older journal whose only unsaved change added the final newline to a file without one replays to the
  disk text once canonicalised, so it offers nothing: the change was the `'endofline'` flag, which RJ01 1.0
  did not record. RJ01 1.1 records the format, so a journal written by this build keeps it.
- A read path that skips the canonicalisation would let the Document add a newline on the first edit, which
  the file did not have. The read paths are few (§2) and each gets a test that a file without a final newline,
  edited and saved, still has none.

## Security impact

None: no new input is trusted, and no file is written that was not before.

## Performance impact

One check of the last byte per transaction, and at most one extra byte per buffer.

## Compatibility & Migration

- RJ01 and the trace format are unchanged. Journals from older builds replay, their text canonicalised.
- A trace records a hash of its start text, and one recorded before slice 1 on a file without a final
  newline hashed the text without it. `ruse --replay` tries the line-terminated text first, then that one,
  and replays from the one that matches (`system_replay` tests both).
- LSP: `didOpen`/`didChange` carry the buffer text, which for a file without a final newline now ends in one.
  nvim sends the newline only with `'eol'`; ruse follows in slice 1 by applying §3 to the full-text sync.

## Observability

`[noeol]` stays on the status line and in the `:w` message; byte counts follow §3.

## Alternatives

- **Keep the file's bytes and add an `'endofline'` flag to the core's line model.** Emacs's model, where the
  final newline is an ordinary character. It needs the flag in every one of the 43 line-model sites and the
  four line counts, where the canonical text needs none of them; and it still needs §5 for the emptied buffer.
- **Represent the emptied buffer as `\n` plus an "emptied" flag.** It moves the `ML_EMPTY` problem into a flag
  every write must consult; the empty text already says it.

## Rejected approaches

- Fixing each divergence where it shows (the `o` planner, the `:put` planner, the `x` cursor clamp). That is
  how #1085, #1107 and #1297 went; each leaves the ambiguity in place for the next command.

## Trade-offs

- **The Emacs profile (F-012) gives up one Emacs behaviour until it is designed:** in Emacs a final newline is
  text, so `C-d` before it deletes it and the file is saved without one (unless `require-final-newline`). With
  §4 that deletion is kept out of the text, and `M->` on a buffer without `'endofline'` lands one character past
  where Emacs puts it. A follow-up issue in the Emacs profile milestone designs `'endofline'` as undoable
  Document state, so a deletion of the final newline clears it and undo restores it.
- An LSP or plugin edit that removes the final newline is kept out of the text the same way.

## Re-evaluation conditions

- The Emacs profile's done-decision (F-012): move `'endofline'` into the Document as undoable state.
- A second frontend (GUI, F-018) or a remote peer (D-013) that must share the text byte-for-byte with a file.

## Open questions

None for slice 1. For slice 2: whether a blockwise put into the emptied buffer pads like nvim (to be probed
before slice 2, and added to §5's list if it differs).
