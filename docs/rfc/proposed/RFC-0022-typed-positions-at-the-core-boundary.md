---
doc: rfc
project: ruse
title: "RFC-0022: Typed positions at the core boundary"
summary: >
  INV-POS-TYPED says coordinates are never an untyped `usize`, but every core position API (`View::cursor`,
  the `Workspace` and `EditorState` editing methods, `Document` queries) takes and returns `usize`; the
  typed `BytePos`/`CharPos`/`GraphemePos` exist almost only inside `pos.rs`, `anchor.rs` and `edit.rs`
  (#889). This RFC makes the claim and the code agree. A position that crosses the core's public API
  carries its unit; arithmetic inside a function may unwrap it; line and column indices are named
  indices, not untyped coordinates. A source-scan test enforces the boundary.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/DECISIONS.md
  - ../../invariants/reference-invariants.md
  - ../../../spec/POLICY.yaml
  - ../proposed/RFC-0008-document-model.md
---

# RFC-0022: Typed positions at the core boundary

- **Status:** proposed
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-04
- **Decision link:** D-063

<!-- Hard-to-reverse: this fixes the shape of the core's public position API, which every frontend calls. -->

## Summary

A byte position that crosses the core's public API is a `BytePos`. That covers parameters and returns of
`pub` functions in `crates/core`, `pub` fields, and `Command`/`ExCmd` payloads. Char, grapheme, UTF-16
and screen-cell positions keep their own types where they cross. Inside a function, code may unwrap a
position to `usize` for arithmetic and wrap it again before it leaves. Line and column numbers are
indices: they stay `usize` and are named for their unit (`line`, `col`, `row`). A test that scans the
source fails on a `pub` core signature that passes a byte position as a bare `usize`.

## Motivation / Problem

#889: INV-POS-TYPED and F-002 acceptance 4 claim distinct, non-interchangeable position types, but about
130 `pub` core functions take or return positions as `usize`. Because the claim is wrong, review trusts a
guard that does not exist. The real hazard behind TEXT-1/TEXT-2 is mixing units, for example a char
index passed where a byte offset is expected, or an LSP UTF-16 column used as a byte column. That
mixing happens at boundaries: between the core and the frontend, the core and a protocol, and the core
and the renderer. Inside a function that works on one unit, a newtype adds noise but no safety.

## Guide-level explanation

<!-- code-blocks: illustrative — the Rust shown is NOT normative; the canonical home is code, per D-038.
     The block fixes the boundary rule, not the exact signatures. -->

```rust
// core public API: the unit is in the type
pub fn cursor(&self) -> BytePos;
pub fn set_cursor(&mut self, at: BytePos);
pub fn line_of(&self, at: BytePos) -> usize;          // a line index, named as one

// inside a function: unwrap for arithmetic, wrap before returning
fn word_end(b: &[u8], at: BytePos) -> BytePos {
    let mut i = at.0;
    while i < b.len() && is_word(b[i]) { i += 1; }
    BytePos(i)
}
```

## Reference-level explanation

1. **Boundary.** The rule covers the public surface of `crates/core`: `pub` functions and methods,
   `pub` struct fields, and the payloads of `Command`, `ExCmd`, `Edit` and `Transaction`. `pub(crate)`
   items and private items are inside the boundary.
2. **Units.** A byte gap or offset is a `BytePos`. Char, grapheme, UTF-16 and screen-cell positions use
   their own types (`CharPos`, `GraphemePos`, a UTF-16 column type at the LSP boundary, `CellCol`). A byte
   *length* or *count* is a `usize` named as a length (`len`, `count`, `n`).
3. **Indices.** Line numbers and column numbers stay `usize`. They are not interchangeable with byte
   positions: a column is always "column in its unit" and the name says which (`col` = byte column,
   `vcol` = screen column, as in nvim). Typing them is not part of this RFC (see Re-evaluation).
4. **Ranges.** A byte range crossing the boundary is `Range<BytePos>` or a `(BytePos, BytePos)` pair.
5. **Enforcement.** A source-scan test (the `no-usize-coords` evidence named in `spec/POLICY.yaml`) parses
   `pub fn` signatures and `pub` fields in `crates/core/src`. It fails when a parameter, return or field
   whose name says position (`at`, `pos`, `offset`, `cursor`, `start`, `end`, `from`, `to`, `anchor`) is a
   bare `usize`. Exceptions go in a short allow-list with a reason each, and the list can only shrink.
6. **Trace format.** `Command`/`ExCmd` text encoding is unchanged: a `BytePos` encodes as its number, so
   v1 traces stay valid (INV-ADDITIVE).

## Reference Invariants

- **INV-POS-TYPED** (amended): "Positions that cross the core's public API are typed by unit (byte / char /
  grapheme / UTF-16 column / screen cell); inside a function a position may be unwrapped for arithmetic.
  Line and column indices are named indices." *Guards:* TEXT-1/2. *Evidence:* `no-usize-coords` scan.
- F-002 acceptance 4 is amended the same way.

## Failure modes & Recovery

Migration is mechanical. A missed call site is a compile error, so it cannot fail silently. The risk is a
wrong wrap that hides a unit bug, for example `BytePos(char_idx)`. Review focuses on each `BytePos(`
built from a value that did not come from a byte operation.

## Security impact

None directly. Typed protocol boundaries (the LSP UTF-16 column) reduce the chance of an out-of-range
edit built from a peer's position.

## Performance impact

None. `BytePos` is a `Copy` newtype over one `usize`.

## Compatibility & Migration

The public API of `ruse-core` changes shape. Its only consumer is `apps/tui` in the same workspace, and the
change lands in one PR per slice:

1. `View`/`EditorState`/`Workspace` cursor and editing APIs, `Document` queries, `apply_edits`.
2. `Command`/`ExCmd` payloads (no format change).
3. The scan test, with the allow-list empty or explained.

## Alternatives

1. **Type every position everywhere** (planner internals included). Rejected: thousands of mechanical
   wraps inside single-unit code, no added safety, and a permanent cost to every motion and operator.
2. **Amend the invariant to "the canonical unit is the byte" and keep `usize`.** Rejected: it gives up
   the guard at exactly the boundaries where units do mix (frontend, LSP, renderer).
3. **Type line and column indices now as well.** Deferred: line and column are already named for their
   unit, and the core has no conversion between them that a type would protect. Re-evaluate below.

## Rejected approaches

Alternative 2 is the tempting one. It is not acceptable: a byte offset given as a char index is still an
unchecked conversion at the frontend, which is the case INV-POS-TYPED exists for.

## Trade-offs

Frontend code wraps and unwraps at call sites, which costs some verbosity. In exchange, every unit
conversion between the core and the frontend shows up as an explicit expression.

## Re-evaluation conditions

- A bug is found that mixed line or column units (for example a byte column used as a screen column):
  type `col`/`vcol` too.
- A second consumer of `ruse-core` appears (a plugin host, a GUI): the boundary rule extends to it.

## Open questions

None.
