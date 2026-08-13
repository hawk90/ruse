---
doc: emacs-cursor-and-mark-fidelity
project: ruse
title: "Emacs Profile: Cursor & Mark Fidelity — findings roadmap from the parity comparator"
summary: >
  Turns the 13 divergences the Emacs parity comparator (apps/tui/tests/emacs_parity_compare.rs) surfaces
  against the pinned Emacs oracle into a principled fix roadmap, so ruse's Emacs profile is made faithful by
  DECISION, not by piecemeal patching. Classifies the findings into three families — the between-character
  CARET MODEL (the root cause of ~half), WORD-MOTION semantics (forward-word ≠ Vim `w`), and REGISTER/MARK
  semantics (delete-char must not kill; kill-region / yank / buffer-jumps set or push the mark) — locates the
  exact core seam for each, and sequences the slices so each one ratchets named fixtures from divergent to
  verified. Divergence is DATA here, never a failure (the comparator's contract); this doc decides which
  divergences ruse closes and how.
audience: [maintainers, contributors, llm-agents]
status: draft
source_of_truth: false
verified_against_upstream: false
related:
  - input-engine.md
  - positions-history.md
  - register-model.md
  - editing-language.md
  - ../parity/emacs.md
  - ../../spec/DECISIONS.md
  - ../../spec/parity/upstreams.yaml
---

<!-- code-blocks: illustrative — the concrete types shown are NOT normative; the canonical home is code (internal types) or spec/contracts/ (cross-boundary), per D-038. -->

## Why this doc exists

The Emacs command-semantics oracle (`tools/parity/emacs_oracle.py`, #152), its seed corpus
(`tests/parity/emacs/fixtures/corpus.yaml`, #153), and the ruse comparator
(`apps/tui/tests/emacs_parity_compare.rs`, #154) together form the Emacs half of the parity evidence layer
(D-043). The comparator replays each fixture's Emacs command NAMES through ruse's real M-x registry
(`emacs_command_by_name`) + core and compares `{text, point, mark, kill}` to what the pinned emacs-30.2
produced. Its contract — identical to the Neovim comparator — is that **a divergence is a finding, not a
failure**: the harness only asserts it ran. On the seed corpus the tally is **10/23 verified, 13 divergent**.

Those 13 divergences are an oracle-backed specification of where ruse's Emacs profile is not yet
Emacs-faithful. This doc classifies them and decides, per family, whether and how ruse closes them — so the
fixes are principled and sequenced rather than reactive. The comparator is the acceptance test for each
slice: a closed finding is a fixture that flips divergent → verified with no regression elsewhere.

## The root cause of the largest family: the caret model

ruse's core inherits Vim's **on-character** cursor: in Normal mode the caret rests *on* a character, and the
line-/buffer-end position is the *last character*, not the slot after it. Emacs is non-modal and its **point
is between-character**: the end position is the empty slot *after* the last character. Concretely, the seam
is the Normal-mode edit clamp in `commit()` (`crates/core/src/editor/mod.rs`, ~L1205):

```rust
// Vim never rests the Normal-mode cursor on the newline: after an edit that leaves it beyond the final
// char of a non-empty line, pull it back onto the last char.
if plan.is_edit && st.view.mode == Mode::Normal {
    let le = line_end(b, st.view.cursor);
    let ls = line_start(b, st.view.cursor);
    if st.view.cursor == le && ls < le {
        st.view.cursor = prev_boundary(b, le);   // <-- pulls point off the between-char slot
    }
}
```

The Emacs profile drives the core in `Mode::Normal` (the default), so **every Emacs edit gets Vim-clamped**.
Non-edit motions already land between-character (`move-end-of-line` verifies at point 11 on `"hello world"`),
which is why the caret family shows up specifically on *edits*:

| fixture | ruse point | emacs point | cause |
|---|---|---|---|
| `kill_line` | 5 | 6 | edit clamp pulls point off the between-char slot |
| `copy_region_then_yank` | 5 | 6 | yank is an edit → clamped |
| `kill_region_then_yank_at_end` | 5 | 6 | same |
| `kill_word_then_yank` | (text/point) | | clamp + the word-motion family below |
| `previous_line` | 6 | 7 | column preservation across the between-char end (curswant) |

### Decision (proposed): the Emacs profile uses a non-modal, between-character caret

ruse should model the Emacs profile's caret as between-character everywhere, not just for non-edit motions.
The recommended seam is a **view-level caret policy** rather than reusing `Mode::Insert` (which carries
unrelated Insert semantics — blockwise-insert sessions, replace stacks, dot-repeat extension):

```rust
// illustrative
enum CaretGravity { OnChar /* Vim Normal */, BetweenChar /* Emacs point, Vim Insert */ }
// View gains `caret: CaretGravity`; the Emacs profile constructs its EditorState with BetweenChar.
// The edit clamp above gains `&& st.view.caret == CaretGravity::OnChar`.
```

This is minimal blast radius (one struct field, one clamp condition, profile-init), keeps Vim byte-identical
(`OnChar` is the default), and a between-char caret is exactly what Vim's Insert mode already does — so the
core already supports the position; we are only choosing when to keep it. The implementing slice mints the
Decision (a new D-0xx) and must audit the *other* Normal-mode on-char behaviours (e.g. horizontal-motion
clamps) for the same gate, not only the edit clamp. **Ratchets:** `kill_line`, `copy_region_then_yank`,
`kill_region_then_yank_at_end`, and the point half of the yank fixtures. (`previous_line` also needs the
curswant column model to count the between-char end — a sub-item of the same slice.)

## Family 2: word-motion semantics (`forward-word` ≠ Vim `w`)

Emacs `forward-word` stops at the **end of the current word**; Vim `w` (ruse `Motion::WordFwd`) jumps to the
**start of the next word**. The registry currently maps `forward-word → Move(1, WordFwd)`, which is
semantically the wrong motion:

| fixture | ruse | emacs |
|---|---|---|
| `forward_word` | point 4 (next word start) | point 3 (end of `foo`) |
| `kill_word` | text `"bar baz"`, kill `"foo "` | text `" bar baz"`, kill `"foo"` |
| `kill_word_from_mid` | `"foobaz"` / `"bar "` | `"foo baz"` / `"bar"` |

`kill-word` inherits the same error (it is `forward-word` under a kill). The fix is in the **registry mapping**
(`emacs_command_by_name`) plus, if no existing motion matches Emacs's word boundary, a dedicated
Emacs-word motion — Vim `e` (`WordEnd`) lands on the last char (on-char), one short of Emacs's after-word
point, so this family also depends on the caret decision to fully verify. **Ratchets** (with Family 1):
`forward_word`, `kill_word`, `kill_word_from_mid`, and the text/kill halves of `kill_region_word`,
`kill_word_then_yank`.

## Family 3: register & mark semantics

Small, mostly independent fidelity choices tied to existing decisions:

- **`delete-char` must not kill** (D-026). Emacs `delete-char` discards the character; ruse maps it to
  `DeleteUnder(1)`, which — being Vim `x` — writes the char to the unnamed register. The Emacs profile needs
  a delete that does not touch the kill ring (a no-yank delete Command, threaded through the planner + the
  F-022 trace codec). **Ratchets:** `delete_char`.
- **`kill-region` / `kill-ring-save` keep the mark; `yank` sets it; `beginning-of-buffer` / `end-of-buffer`
  push it** (D-027 / D-049, the mark ring). ruse clears the mark on `kill-region` and never sets it on yank
  or buffer-jumps. These are the mark-ring's activation semantics; they belong with the mark-ring slice
  (C-POSHIST pop-rotate), not the caret work. **Ratchets:** the mark half of `kill_region`,
  `kill_region_word`, `copy_region_then_yank`, `kill_word_then_yank`, `beginning_of_buffer`, `end_of_buffer`.

## Sequencing

1. **Caret decision + slice** (Family 1) — the root cause; unblocks the most fixtures and Family 2's
   verification. Mint the Decision, add `CaretGravity`, gate the on-char behaviours, extend curswant.
2. **Word-motion slice** (Family 2) — registry remap + Emacs-word motion; verifies once Family 1 lands.
3. **Register/mark slice** (Family 3) — no-yank delete (D-026) + mark-ring activation (D-027/D-049); can
   proceed in parallel with 1–2 since it is mark/register-local.

Each slice cites the comparator tally as its acceptance evidence: the named fixtures move divergent →
verified, with the neovim comparator and the full test suite unchanged. `undo` stays vim↔nvim only
(buffer-undo-list is not normalizable — upstreams.yaml oracles.emacs hazard #4).
