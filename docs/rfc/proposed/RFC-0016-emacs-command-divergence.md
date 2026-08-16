---
doc: rfc
project: ruse
title: "RFC-0016: Emacs-vs-Vim command divergence is modeled as distinct Commands, not a profile signal"
summary: >
  When an Emacs-profile command differs from the Vim command it superficially resembles in more than caret
  gravity (D-050) — a different edit-vs-yank policy, a different motion target, or a mark side-effect — ruse
  models it as its own Command variant that the Emacs keymap resolves to, rather than adding an is-Emacs /
  profile signal to View and branching shared planners on it. The core stays profile-agnostic: a Command
  carries its full semantics (D-047) and the profile is expressed by WHICH commands its keymap resolves to
  (D-049). Records D-051; established by DeleteForward and extended to EmacsYank and EmacsBufferEdge, which
  complete the Emacs parity corpus at 23/23.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ./RFC-0014-emacs-input-profile.md
  - ./RFC-0015-emacs-caret-gravity.md
  - ../../design/emacs-cursor-and-mark-fidelity.md
  - ../../../spec/DECISIONS.md
---

<!-- code-blocks: illustrative — the Rust shown is NOT normative; the canonical home is code (internal
     types) or spec/contracts/ (cross-boundary), per D-038. These blocks fix the SEMANTIC decision (how
     Emacs/Vim command divergence is modeled), not any literal signature or line number. -->

# RFC-0016: Emacs-vs-Vim command divergence is modeled as distinct Commands

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-16
- **Decision link:** D-051 (proposed by this RFC; recorded on acceptance)
- **Builds on:** D-047 (the change-intent is the semantic unit), D-049 (Emacs is a configuration of decided machinery), D-050 / RFC-0015 (caret gravity — the one cross-cutting caret property), RFC-0014 (the Emacs profile).
- **Evidence:** `apps/tui/tests/emacs_parity_compare.rs` vs pinned emacs-30.2 — the last Family-3 divergences (`yank` / buffer jumps not setting the mark) motivated the choice; applying it takes the corpus to 23/23.

## Summary

Some Emacs commands look like a Vim command but behave differently. `delete-char` is Vim `x` that does not
yank. `yank` is Vim `P` that also sets the mark and rests point after the paste. `beginning-of-buffer` /
`end-of-buffer` are Vim `gg` / `G` that jump to the *absolute* buffer edge (not a line's first-non-blank)
and push the mark. This RFC decides how that divergence is expressed: **as a distinct `Command` variant the
Emacs keymap resolves to**, not by teaching the core "which profile is active" and branching shared command
planners on it. Caret gravity (D-050) remains the *only* cross-cutting profile property; everything else is
per-command.

## Motivation / Problem

Family 3 of the Emacs fidelity roadmap needed `yank` and the buffer jumps to set / push the Emacs mark. Both
route through Vim-shared commands (`Command::Paste`, `Command::Move(GotoLine/LastLine)`). Two mechanisms were
possible:

1. **A profile signal on `View`** (`profile: {Vim, Emacs}` or a bundle of behaviour flags), with shared
   planners branching on it.
2. **Distinct Emacs `Command` variants**, resolved by the Emacs keymap, that carry the divergent semantics.

The deciding observation: these ops diverge in **target and cursor too**, not only the mark. Emacs
`end-of-buffer` lands at absolute buffer length; Vim `G` lands on the last line's first non-blank — a
different position, not the same position with an extra mark write. So they are genuinely different commands,
and a profile flag on one shared command cannot express the difference without also carrying the alternate
target/cursor — at which point the "shared" command is two commands wearing one name.

## Guide-level explanation

A `Command` is the semantic unit (D-047). A profile is a keymap that resolves keys to commands (D-049). When
Vim and Emacs want *the same* behaviour, they resolve to the same command (most motions, most edits). When
they want *different* behaviour, they resolve to *different* commands. The Emacs profile therefore carries a
small set of Emacs-specific commands for the places it diverges:

- `DeleteForward` — `delete-char` / `C-d`: delete forward, cross newlines, no kill-ring write (vs Vim `x`).
- `EmacsYank` — `yank` / `C-y`: paste + set the mark at the insertion start (vs Vim `P`).
- `EmacsBufferEdge{start}` — `beginning-`/`end-of-buffer`, `M-<` / `M->`: jump to the absolute buffer edge +
  push the mark (vs Vim `gg` / `G`).

Vim's `x` / `p` / `P` / `gg` / `G` are untouched; a reader of the Vim planners sees no Emacs conditionals.

## Reference-level explanation

```rust
// illustrative — canonical home is crates/core (D-038).
enum Command {
    // …shared…
    DeleteForward(u32),               // Emacs delete-char / C-d (no yank)
    EmacsYank { count: u32 },         // Emacs yank: paste + set mark at insertion start
    EmacsBufferEdge { start: bool },  // Emacs beginning/end-of-buffer: absolute edge + push mark
}
```

`EmacsYank` reuses the shared `paste()` planner (so it inherits the gravity-aware cursor, D-050) and then
sets `set_mark = Set(insertion_start)`; an empty register stays a no-op with no mark write. `EmacsBufferEdge`
sets the cursor to `0` or the buffer length and `set_mark = Set(old_point)`. Each command serializes through
the F-022 trace codec like any other. The Emacs mark field on `View` is already Emacs-only (no Vim command
reads or writes it), so these writes are invisible to Vim even before considering the keymap.

## Reference Invariants

- **INV-CMD-SEMANTIC** (depended on): a command's identity fixes its semantics; two behaviours that differ in
  target/cursor/mark are two commands, not one command reinterpreted by ambient state.
- No new invariant. This RFC constrains *how* profile divergence is expressed, reusing D-047's command unit.

## Failure modes & Recovery

- **A shared command silently acquires Emacs-only behaviour** (regressing Vim) — prevented structurally:
  the Emacs behaviour lives in a separate command Vim never resolves to. The Neovim comparator (143/143) is
  the standing guard.
- **Command proliferation** — the accepted cost; see Trade-offs and the re-evaluation threshold.

## Security impact

None. No new input surface, format, or allocation path.

## Performance impact

Negligible — a few added enum variants and match arms; no hot-loop or allocation change.

## Compatibility & Migration

No Vim/Neovim behaviour change (the acceptance bar). Trace codec gains three verbs (`delete_forward`,
`emacs_yank`, `emacs_buffer_edge`); older traces never contained them, so replay is forward-compatible. No
persistent-format or protocol change.

## Observability

The Emacs parity comparator is the surface: applying this decision moves it to 23/23, and the Neovim
comparator stays 143/143.

## Alternatives

1. **A `View` profile signal + shared-planner gating.** Rejected as the *primary* mechanism — it encodes
   profile identity in the core and scatters `if emacs` across planners, and it cannot express the divergent
   *targets* (`end-of-buffer` ≠ `G`) without smuggling an alternate motion into the shared command anyway.
   Retained only as the re-evaluation escape hatch if variant count explodes.
2. **Overload `CaretGravity`** to also gate mark writes. Rejected — RFC-0015 was explicit that gravity is a
   caret-position property, not a profile flag; mark activation is unrelated to where the caret rests.
3. **Fine-grained behaviour flags** (`mark_on_yank`, `mark_push_on_jump`, …). Rejected — proliferates
   ambient state that shared planners must consult, the same coupling as (1) at higher granularity.

## Rejected approaches

The profile-signal and gravity-overload options are recorded here so they are not re-proposed. Both put
profile-specific knowledge in the wrong layer; the command layer already owns semantics (D-047).

## Trade-offs

- **Cost:** more `Command` variants as the Emacs profile grows; each needs a planner arm and a codec verb.
- **Benefit:** the core stays profile-agnostic and branch-free on the hot paths; Vim is provably unchanged;
  each Emacs behaviour is testable in isolation; consistent with the shipped `DeleteForward`.

## Re-evaluation conditions

If the number of Emacs-only command variants grows large enough that a profile signal + shared-planner
gating would materially cut duplication (a *scaling* threshold, not a correctness one), introduce the signal
and collapse the variants, superseding D-051.

## Open questions

- The mark here is D-027 depth-1 (a single per-buffer mark). When the full mark RING lands, `EmacsYank` /
  `EmacsBufferEdge` should PUSH onto the ring rather than overwrite the single mark; the command boundary is
  already the right seam for that, so this decision is expected to compose rather than reopen.
