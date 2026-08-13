---
doc: rfc
project: ruse
title: "RFC-0015: Caret gravity — the Emacs profile's caret is between-character, not Vim's on-character"
summary: >
  The parity comparator (apps/tui/tests/emacs_parity_compare.rs) shows ~half its divergences trace to one
  root cause: ruse's core inherits Vim's ON-character Normal-mode caret (the line-/buffer-end position is
  the last character), while the Emacs profile must model Emacs point, which is BETWEEN-character (end = the
  slot after the last character). This RFC decides that the Emacs profile uses a between-character caret
  everywhere, introduces a view-level CaretGravity{OnChar, BetweenChar} policy that gates the Normal-mode
  edit clamp in commit(), and keeps Vim/Neovim byte-identical (OnChar is the default). Design-only: the
  code lands in a separate implementation slice whose acceptance is the caret-family fixtures flipping
  divergent -> verified.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ./RFC-0014-emacs-input-profile.md
  - ./RFC-0004-input-profiles.md
  - ../../design/emacs-cursor-and-mark-fidelity.md
  - ../../design/input-engine.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/parity/upstreams.yaml
---

# RFC-0015: Caret gravity — the Emacs profile's caret is between-character

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-13
- **Decision link:** D-050 (proposed by this RFC; recorded on acceptance)
- **Builds on:** RFC-0014 / D-049 (the Emacs profile), RFC-0004 (input profiles), D-003 (Document ≠ View — the caret is View-local).
- **Evidence:** `apps/tui/tests/emacs_parity_compare.rs` vs the pinned emacs-30.2 oracle (`tools/parity/emacs_oracle.py`); tally 10/23 verified, and ~half the 13 divergences are this one cause.

## Summary

ruse's core models a Vim caret: in Normal mode the caret rests **on** a character, so the last valid
position on a non-empty line is its **last character**, never the empty slot after it. The Emacs profile
(F-012) is non-modal and must model **Emacs point**, which sits **between** characters — the end of a line
or buffer is the slot *after* the last character. Today the Emacs profile drives the core in `Mode::Normal`,
so a Normal-mode **edit clamp** in `commit()` pulls point back off that after-last slot, and every Emacs
edit lands one column short of Emacs.

This RFC decides: **the Emacs profile's caret is between-character everywhere**, not only for the non-edit
motions that already happen to land there. The mechanism is a **view-level `CaretGravity` policy** — a
single `View` field, gating the on-character clamp(s) — chosen over reusing `Mode::Insert` (which carries
unrelated Insert semantics). Vim/Neovim keep `OnChar` and stay byte-identical.

## Motivation / Problem

The Emacs command-semantics oracle, its corpus, and the ruse comparator (RFC-0014 evidence layer, D-043)
give an oracle-backed map of where ruse's Emacs profile is not Emacs-faithful. The single largest family of
divergences is the caret:

| fixture | ops | ruse point | emacs point | note |
|---|---|---|---|---|
| `kill_line` | `kill-line` | 5 | 6 | edit clamp pulls point off the after-last slot |
| `copy_region_then_yank` | `set-mark…` `kill-ring-save` `yank` | 5 | 6 | yank is an edit → clamped |
| `kill_region_then_yank_at_end` | … `yank` | 5 | 6 | same |
| `previous_line` | `previous-line` | 6 | 7 | column preserved across the between-char end (curswant) |

Non-edit motions already land between-character today: `move-end-of-line` on `"hello world"` verifies at
point 11 (the after-last slot), because motions are not subject to the edit clamp. The divergence is
specifically on **edits**, which localizes the cause precisely.

The seam is the Normal-mode edit clamp in `commit()` (`crates/core/src/editor/mod.rs`, ~L1205):

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

Because the Emacs profile runs the core in `Mode::Normal`, this Vim rule fires on Emacs edits it should not
govern. This is a **profile-semantic** decision (which caret model an input profile presents), not a local
bug fix, so it is recorded rather than patched silently.

## Guide-level explanation

A **caret** (cursor / point) can rest in two ways relative to text:

- **On-character** (Vim Normal mode) — the caret occupies a character cell; on a non-empty line it can rest
  at most on the last character. Operators act on the cell under the caret. This is `CaretGravity::OnChar`.
- **Between-character** (Emacs point; also Vim **Insert** mode) — the caret sits in the gap between two
  characters, and the end position is the gap after the last character. This is `CaretGravity::BetweenChar`.

An input profile chooses its gravity. **Vim/Neovim → `OnChar`.** **Emacs → `BetweenChar`.** A user notices
it as: after `C-k` (kill-line) on `hello`, Emacs leaves point at the (now empty) end of the line; ruse's
Emacs profile must do the same, not clamp point back onto `o`.

Gravity is a property of the **View** (the caret is View-local, D-003), constructed from the active input
profile. It does not change the Document, the transaction model, or any wire/persistent format.

## Reference-level explanation

Add a two-variant policy and a `View` field:

```rust
// illustrative — canonical home is crates/core (D-038); this RFC fixes semantics, not the literal type.
enum CaretGravity {
    OnChar,       // Vim Normal: caret rests ON a char; line end = last char.
    BetweenChar,  // Emacs point / Vim Insert: caret rests BETWEEN chars; line end = after last char.
}
// View gains `caret: CaretGravity` (default OnChar). The Emacs profile constructs its EditorState/View
// with BetweenChar.
```

The edit clamp gains one condition:

```rust
if plan.is_edit && st.view.mode == Mode::Normal && st.view.caret == CaretGravity::OnChar {
    // …unchanged…
}
```

**Audit obligation (part of the implementing slice, not deferrable):** the edit clamp is *one* place the
core assumes on-character gravity. The slice MUST sweep `commit()`/`plan`/motion for the other Normal-mode
on-char assumptions and gate each on `CaretGravity::OnChar`:

1. **The edit clamp** above — the primary seam.
2. **Horizontal-motion end clamp** — the rule that a rightward motion cannot pass the last character
   (Vim `l`/`$` stop on the last char; Emacs `C-f` can rest after it). Any `line_end`/`prev_boundary`
   Normal-mode clamp on motions.
3. **curswant end column** — `$`/`<End>` set `curswant = MAXCOL`; the between-char end is one column further
   right, so vertical moves (`previous-line`/`next-line`) must preserve the after-last column. This is why
   `previous_line` diverges; it is a sub-item of the same slice.

The slice is **complete** only when the caret-family fixtures verify AND the Neovim comparator + full suite
are unchanged — i.e. no `OnChar` path shifted.

### Why a View policy, not `Mode::Insert`

Vim's Insert mode already parks the caret between-character (the append position at end-of-line). Reusing
`Mode::Insert` for the Emacs profile would get the caret position "for free" but drag in **unrelated Insert
semantics**: blockwise-insert replicate sessions (`block_insert`), the Replace `<BS>` stack, dot-repeat
extension, and undo-grouping that treats Insert as one coalesced edit. The Emacs profile is non-modal — it
is neither Vim-Normal nor Vim-Insert — so gravity must be **orthogonal to Mode**. A dedicated `CaretGravity`
field is the minimal, honest expression of that orthogonality.

## Reference Invariants

- **INV-DOC-VIEW** (depended on): the caret and its gravity are View-local; the Document is unaware. A
  second View of the same buffer under a different profile can hold a different gravity — the field lives
  where that is already legal.
- **INV-PROFILE-ISOLATION** (depended on): gravity is selected by the input profile; the Vim path must be
  provably unchanged (`OnChar` default + gated clamps). The implementing slice's regression evidence (Vim +
  Neovim comparator byte-identical) is the discharge of this invariant.
- No new invariant is introduced; this RFC constrains an existing degree of freedom (which gravity a
  profile presents) rather than adding a kernel concept.

## Failure modes & Recovery

- **A missed on-char assumption** (a clamp not gated) → an Emacs edit still lands one short, OR a Vim path
  regresses. Caught by the two comparators: the Emacs fixture stays divergent, or a Vim/Neovim fixture
  flips. The acceptance gate (both suites unchanged + caret fixtures verified) makes a miss a red test, not
  a silent shipping bug.
- **Between-char point past buffer end** → `snap()` already clamps every committed cursor to a char
  boundary within `[0, len]`; the after-last slot is `len` of a line, which is in range. No new totality
  risk.

## Security impact

None. No new input surface, no format change, no allocation-path change.

## Performance impact

One `enum` field on `View` (one byte) and one `&&` on an already-conditional clamp. No hot-loop cost; no
allocation. The implementing slice will keep the Vim path a literally-unchanged branch.

## Compatibility & Migration

No user-visible Vim/Neovim change (that is the acceptance bar). No persistent-format or protocol change —
`CaretGravity` is in-memory View state, never serialized. Emacs-profile users get correct point behaviour
they never had (the profile is pre-1.0, F-012 in progress). No migration.

## Observability

The comparator tally IS the observability surface: `apps/tui/tests/emacs_parity_compare.rs` prints
verified/divergent per fixture, and the caret-family fixtures moving to VERIFIED is the visible signal. No
new logs/metrics.

## Alternatives

1. **Reuse `Mode::Insert` for the Emacs profile.** Rejected — see "Why a View policy" (drags in unrelated
   Insert semantics; conflates gravity with mode).
2. **Patch the clamp with an `is_emacs_profile` boolean.** Rejected — encodes the profile identity in the
   core instead of the *property* (gravity) the core actually needs; a second between-char profile (or a
   Vim `virtualedit`-style mode) would have to be OR-ed in. `CaretGravity` names the real axis.
3. **Convert point→on-char at the comparator boundary** (leave the core Vim-clamped, adjust in the Emacs
   command layer). Rejected — makes the core lie about where point is; every downstream consumer (render,
   future GUI, kill/yank span math) would need the same fudge. The caret must be correct *in the View*.
4. **Do nothing / accept the divergence.** Rejected — it is the root cause of ~half the findings and blocks
   Family 2 (word-motion) from ever fully verifying; leaving it makes the Emacs profile permanently
   off-by-one on edits.

## Rejected approaches

The `is_emacs_profile` boolean and the comparator-boundary fudge (Alternatives 2 & 3) are recorded here so
they are not re-proposed: both put profile-specific knowledge in the wrong layer. The core exposes a caret
*property*; the profile *selects* it. That is the only shape that stays correct as more profiles/frontends
consume the caret.

## Trade-offs

- **Cost:** an audit obligation — the implementing slice must find *every* Normal-mode on-char assumption,
  not just the edit clamp. Under-auditing ships an off-by-one; the comparators bound this risk but the sweep
  is real work.
- **Benefit:** one small, well-named field closes the largest divergence family, unblocks Family 2, and
  keeps Vim byte-identical. The core gains an honest vocabulary (gravity) it will reuse for any future
  non-modal or virtual-edit surface.

## Re-evaluation conditions

- A profile needs a *third* caret model that is neither on-char nor between-char (e.g. a full Vim
  `virtualedit=all` free caret past line end on empty columns) — then `CaretGravity` grows a variant or
  becomes a richer policy.
- Gravity turns out to need to vary *within* a profile by mode in a way `Mode` + `CaretGravity` cannot
  express together — would reopen the orthogonality choice.

## Open questions

- Does `curswant`'s `MAXCOL` sentinel fully express the between-char end column, or does the between-char
  end need its own sentinel distinct from Vim's `$`? (Resolvable in the implementing slice against the
  `previous_line` fixture; does not change this decision.)
- Should gravity also govern **selection** end-inclusivity (Vim Visual is inclusive of the last char; an
  Emacs region is exclusive of the char after point)? Likely yes and likely the same field, but the
  register/mark slice (Family 3) is where that surfaces; flagged, not decided here.
