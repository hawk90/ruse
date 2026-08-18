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
  - ../rfc/proposed/RFC-0015-emacs-caret-gravity.md
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
failure**: the harness only asserts it ran. On the seed corpus the tally opened at **10/23 verified, 13
divergent** and reached **23/23 — the whole seed corpus is Emacs-faithful** — before the corpus was expanded
to 36 fixtures (**32/36**, then **33/36** after `EmacsKillLine`; see "Corpus expansion round 1" below). Path: Family 1 (caret gravity)
10→12 (`kill_line`, `previous_line`); Family 3 part 1 →14 (`delete_char`, `kill_region`); Family 2
(word-motion) →18 (`forward_word`, `kill_word`, `kill_word_from_mid`, `kill_region_word`); Family 3 part 2
→23 (`beginning_of_buffer`, `end_of_buffer`, `copy_region_then_yank`, `kill_region_then_yank_at_end`,
`kill_word_then_yank`). Neovim comparator stayed 143/143 throughout.

Those divergences are an oracle-backed specification of where ruse's Emacs profile is not yet
Emacs-faithful. This doc classifies them and decides, per family, whether and how ruse closes them — so the
fixes are principled and sequenced rather than reactive. The comparator is the acceptance test for each
slice: a closed finding is a fixture that flips divergent → verified with no regression elsewhere.

> **Status — Family 1 LANDED.** `CaretGravity{OnChar, BetweenChar}` (D-050) now gates the Normal-mode edit
> clamp AND the charwise-paste cursor rule; `EditorState::set_cursor` seeds `curswant`; `RUSE_PROFILE=emacs`
> installs `BetweenChar` via `Workspace::set_caret_gravity`. Tally 10→12: `kill_line` and `previous_line`
> verified. The yank fixtures (`copy_region_then_yank`, `kill_region_then_yank_at_end`) now diverge ONLY on
> the mark — their point half is fixed — so they flip when Family 3 lands. Vim/Neovim byte-identical
> (nvim comparator 143/143, full suite green).

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

### Decision (D-050 / RFC-0015): the Emacs profile uses a non-modal, between-character caret

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
core already supports the position; we are only choosing when to keep it. **The decision is now recorded as
D-050 / [RFC-0015](../rfc/proposed/RFC-0015-emacs-caret-gravity.md);** the implementing slice must audit the
*other* Normal-mode on-char behaviours (horizontal-motion end clamp, the `curswant` end column) for the same
gate, not only the edit clamp. **Ratchets:** `kill_line`, `copy_region_then_yank`,
`kill_region_then_yank_at_end`, and the point half of the yank fixtures. (`previous_line` also needs the
curswant column model to count the between-char end — a sub-item of the same slice.)

## Family 2: word-motion semantics (`forward-word` ≠ Vim `w`) — LANDED

> **Status — Family 2 LANDED.** A new `Motion::EmacsWordFwd` lands point at `word_end_excl` (AFTER the last
> word char — Emacs point), where Vim `e`/`WordEnd` rests ON it; its operator span is the same
> `[cursor, word_end_excl)` as `WordEnd`, so `Delete(EmacsWordFwd)` IS `kill-word`. Both the `M-f`/`M-d`
> keys and the `forward-word`/`kill-word` M-x names now map to it (Vim `w`/`e` unchanged). Tally 14→18:
> `forward_word`, `kill_word`, `kill_word_from_mid` verified, and `kill_region_word` too (its word-text/kill
> plus the Family-3 kill-region mark now both align). nvim 143/143, full suite green.

Emacs `forward-word` stops at the **end of the current word**; Vim `w` (ruse `Motion::WordFwd`) jumps to the
**start of the next word**. The registry previously mapped `forward-word → Move(1, WordFwd)`, the wrong
motion:

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

- **`delete-char` must not kill** (D-026) — **LANDED.** A new `Command::DeleteForward(count)` deletes
  forward via the no-register `edit` path (not `edit_yank`), clamped at BUFFER end so it crosses newlines
  (Emacs has no EOL boundary), and is threaded through the F-022 trace codec (`delete_forward`). The Emacs
  registry maps both `delete-char` and `C-d` onto it; Vim `x` (`DeleteUnder`, which yanks) is unchanged.
  **Ratcheted:** `delete_char` verified.
- **`kill-region` / `kill-ring-save` keep the mark; `yank` sets it; `beginning-of-buffer` / `end-of-buffer`
  push it** (D-027 / D-049, the mark ring) — **ALL LANDED.** `kill-region` keeping the mark (part 1) was a
  one-line fix to the Emacs-only `Command::KillRegion`. For `yank` and the buffer jumps (part 2), the
  originally-mooted "`View` profile signal" turned out to be the **wrong** mechanism: these ops diverge from
  their Vim lookalikes in *target and cursor too*, not just the mark, so **D-051** models them as distinct
  commands (the `DeleteForward` pattern), keeping the core profile-agnostic. `Command::EmacsYank` = the
  gravity-aware charwise paste **plus** set the mark at the insertion start; `Command::EmacsBufferEdge{start}`
  = jump to the ABSOLUTE buffer edge (fixing `end_of_buffer`'s point 11→16 vs Vim `G`'s first-non-blank)
  **plus** push the mark at the old point. **Ratcheted:** `beginning_of_buffer`, `end_of_buffer`,
  `copy_region_then_yank`, `kill_region_then_yank_at_end`, `kill_word_then_yank` — corpus complete at 23/23.

## Sequencing

1. **Caret slice** (Family 1) — the root cause; unblocks the most fixtures and Family 2's verification. The
   decision is recorded (D-050 / RFC-0015); the slice adds `CaretGravity`, gates the on-char behaviours, and
   extends curswant.
2. **Word-motion slice** (Family 2) — registry remap + Emacs-word motion; verifies once Family 1 lands.
3. **Register/mark slice** (Family 3) — no-yank delete (D-026) + mark-ring activation (D-027/D-049); can
   proceed in parallel with 1–2 since it is mark/register-local.

Each slice cites the comparator tally as its acceptance evidence: the named fixtures move divergent →
verified, with the neovim comparator and the full test suite unchanged. `undo` stays vim↔nvim only
(buffer-undo-list is not normalizable — upstreams.yaml oracles.emacs hazard #4).

## Corpus expansion round 1 — deeper `kill-line` / kill semantics (13 new fixtures, tally 23→36)

With the seed corpus exhausted (all 23 verified), the corpus was expanded to probe deeper semantics of the
already-shipped commands rather than only their happy path. 13 oracle-captured fixtures were added; the tally
is now **32/36 verified, 4 divergent**. Nine of the new fixtures verified immediately — welcome confidence
that the shipped commands hold on multi-line buffers, repeated application, and backward regions
(`delete_char_at_eol`, `delete_char_twice`, `forward_word_twice`, `backward_word_from_mid`,
`end_of_line_multiline`, `next_line_then_end`, `kill_region_backward`, `kill_line_then_yank_at_bol`,
`kill_line_from_bol`). The four divergences are the next oracle-backed targets:

| fixture | ruse | emacs | finding |
|---|---|---|---|
| `kill_line_at_eol` | ✅ **CLOSED** — now joins the next line, kill `"\n"` (was: `Delete(1, LineEnd)` no-op at EOL) | joins the next line, kill `"\n"` | Closed by `Command::EmacsKillLine` (D-051 distinct-command). Emacs `kill-line` at end-of-line kills the **newline**; `EmacsKillLine` kills to EOL, or the terminating `\n` when point is already at EOL, and is inert at end-of-buffer. Vim `D` (`Delete(1, LineEnd)`) is untouched. |
| `kill_line_whole_then_join` | ↗ text/point now `["bar"]`, point 0 (via `EmacsKillLine`); kill still `"\n"` | `["bar"]`, kill `"foo\n"` | **Half-closed**: `EmacsKillLine` fixed the text/point (the EOL join). Still divergent on the KILL field — KILL ACCUMULATION: consecutive kills append to one kill-ring entry (`foo` then `\n` → `"foo\n"`); ruse overwrites the register on each kill. Needs `last-command`-style kill-append tracking in the core (a real feature, larger than one command). |
| `transpose_chars` | unresolved (`C-t`) | `"bac"`, point 2 | Registry gap — `transpose-chars` is not in `emacs_command_by_name`; ruse has no transpose command yet. |
| `capitalize_word` | unresolved | `"Foo bar"`, point 3 | Registry gap — `capitalize-word` (and the `upcase`/`downcase` family) unimplemented. |

Oracle fidelity note: capturing `kill_line_whole_then_join` faithfully required a correctness fix to the
oracle itself — `call-interactively` sets `this-command` but not `last-command`, so a raw op sequence never
triggers kill-accumulation. The probe now threads `last-command` (promoting the prior `this-command` before
each call) exactly as Emacs's command loop does, and a `--selftest` case guards it. Existing fixtures are
byte-identical (none contained consecutive kills), so the fix only made the new multi-kill fixture truthful.

Sequencing of the new work: (1) `EmacsKillLine` (the EOL-newline kill, ratchets `kill_line_at_eol` and the
text/point half of `kill_line_whole_then_join`); (2) kill-accumulation (ratchets the kill half of
`kill_line_whole_then_join` and enables faithful multi-kill fixtures broadly); (3) the transpose / case
command family (closes the two registry gaps and opens a new coverage area). Each remains its own governed
slice with the comparator tally as acceptance.

**Step (1) landed — `Command::EmacsKillLine`.** `kill-line` (`C-k`) is now its own command rather than
Vim's `Delete(1, LineEnd)` (D-051): with text before the line end it kills that text into the register; with
point already at the line end it kills the terminating `\n` (joining the next line); at end-of-buffer it is
inert. This closed `kill_line_at_eol` (tally 32/36 → **33/36**) and fixed the text/point half of
`kill_line_whole_then_join` (which stays divergent on the kill field, awaiting step (2) kill-accumulation).
The Vim `D` path and the Neovim parity axis (143/143) are untouched — the two profiles diverge by carrying
distinct `Command`s over one shared core, not by profile-gating a shared command.
