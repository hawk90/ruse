# Runtime health check — `:checkhealth` (F-030 / CAP-HEALTHCHECK)

## Why

ruse's *project* self-verification is strong — `ruse verify`, `gov check` (auto-discovered
`tools/gov/*.py`), the parity oracles, `pr check`. But the *running editor* has no user-facing
diagnostic: a user who hits "why isn't syntax highlighting on?" or "which input profile am I in?" has
only the ntic log. Neovim solves this with `:checkhealth` — a command that reports the editor's runtime
health in an OK / WARN / ERROR list. F-030 is that analogue for ruse.

## What it reports (first slice)

Only things ruse can meaningfully check *today* (no plugins / LSP / providers yet — those report as they
land):

| check | OK when | WARN/absent when |
|---|---|---|
| input profile | a profile is active (Vim / Emacs) | — (always present) |
| caret gravity | reported (OnChar / BetweenChar) | — |
| terminal capabilities | the detected ledger (truecolor, synchronized-output, …) | a capability is absent/degraded |
| syntax grammar | a tree-sitter grammar is available for the current buffer's extension | no grammar for this file type |
| buffers | N buffers open | — |
| trace / replay | command-trace recording status | — |

Each row is `Status { Ok | Warn | Absent }` + a one-line human reason — the ok/warn/error shape Neovim uses.

## Architecture — pure builder, thin command

The core stays IO-free (INV): the check logic is a **pure function** in the frontend,

```
fn report(inputs: HealthInputs) -> HealthReport
```

where `HealthInputs` is a plain snapshot (active profile, caret gravity, terminal-cap ledger, current
file extension + the set of supported grammars, buffer count, trace status). This is fully unit-testable
with no terminal. `:checkhealth` (`Ex::CheckHealth`) gathers the snapshot from the running frontend and
renders `HealthReport` into a scratch view / the message area — a thin wiring layer over the tested
builder. This keeps the risk off `main.rs` (which has thin test coverage) and in a covered pure module.

## Non-goals (first slice)

Provider/LSP/plugin health (nothing to check yet — they report as they land); remote-runtime health
(F-017); a governance-side `ruse doctor` aggregator (that's a tooling concern, separate from the runtime
command). Injection/fold grammar health rides on F-015 #4.
