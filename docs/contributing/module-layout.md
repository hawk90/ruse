---
doc: module-layout
project: ruse
title: "ruse — Module Layout and Test-File Conventions"
summary: >
  How Rust source is laid out in crates/core and apps/tui, as the codebase already does it (measured,
  not a new rule). A module with children uses `x.rs` + `x/`; the 16 `x/mod.rs` directories are older
  and are not renamed. Unit tests sit in an inline `#[cfg(test)] mod tests`, or in an out-of-line file whose name is `tests.rs` or `<topic>_tests.rs`;
  integration tests are one file per target under `<crate>/tests/`. Covers RA-STRUCT-025 and RA-STRUCT-026.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - size-ceilings.md
  - ../operations/testing-and-benchmarks.md
  - ../../spec/review-axes.yaml
  - ../reviews/2026-11-16-v0.2.1.md
  - ../reviews/2026-11-23-v0.2.2.md
---

# ruse — Module Layout and Test-File Conventions

This file writes down what the code already does. Each rule gives the measured count it rests on and the
known exceptions. It does not ask for existing files to be renamed or moved. The counts are
re-measured by `python3 tools/ruse.py docs check` (see [Re-measuring](#re-measuring)).

## Rules (summary)

| # | Rule | Evidence |
| --- | --- | --- |
| L1 | A module with child files is `x.rs` + `x/`. Do not create a new `x/mod.rs`. | 37 of 53 non-test module directories; every directory added since v0.2.2 |
| L2 | Leave the existing `x/mod.rs` directories as they are. Move one only as part of a split of that module that you are making anyway. | 16 directories, listed below |
| T1 | A small test module stays inline: `#[cfg(test)] mod tests { ... }` at the end of the file. | 96 files |
| T2 | An out-of-line test file of module `x` is `x/tests.rs`, declared by `#[cfg(test)] mod tests;`. | 21 files (2 of them T3 hubs) |
| T3 | Several topic suites for one module are `<topic>_tests.rs`, each declared by that module with `#[cfg(test)] mod <topic>_tests;`. | 26 files beside the module, 34 under a test directory |
| T4 | Every out-of-line unit-test file name is `tests.rs` or ends in `_tests.rs`. | all but 3 helper files |
| T5 | An integration test is one file per target in `<crate>/tests/`, named for the behavior it covers; shared helpers are `tests/<name>/mod.rs`. | one target per file in each crate |

## Module layout

### L1: `x.rs` + `x/`

A module that has child modules lives in `x.rs`, and its children in `x/`. Example: `editor/commit.rs` with
`editor/commit/`; `app/session.rs` with `app/session/`.

Measured module directories (a directory under `src/` holding `.rs` files):

| Crate | `x.rs` + `x/` (non-test) | `x.rs` + `x/` (test-only) | `x/mod.rs` |
| --- | --- | --- | --- |
| `crates/core` | 12 | 2 (`editor/tests`, `workspace/tests`) | 8 |
| `apps/tui` | 25 | 1 (`input/unit_tests`) | 8 |
| Total | 37 | 3 | 16 |

- All 15 module directories added in v0.2.2 use `x.rs` + `x/` (v0.2.2 review, RA-STRUCT-026). v0.2.3 added
  none; v0.2.4 added 2 (`editor/substitute`, `persist/codec`), both `x.rs` + `x/`.
- A module tree may mix the two forms: `editor/mod.rs` has children `editor/commit.rs` + `editor/commit/`.
  That is expected while L2 holds.

### L2: the `x/mod.rs` exceptions

These 16 directories use `x/mod.rs`. They come from the first `apps/tui` layout (August 2026) and from the
v0.2.1 `crates/core` splits, before the practice settled.

- `crates/core/src/`: `command/`, `editor/`, `editor/cindent/`, `editor/ex_cmd/`, `editor/planner/`,
  `motion/`, `pattern/`, `workspace/`.
- `apps/tui/src/`: `app/`, `caps/`, `input/`, `lsp/`, `persist/`, `remote/`, `terminal/`, `ui/`.

Renaming one is a pure move with no behavior change. Do it only inside a PR that already splits that module,
so the history of the file is not churned for layout alone.

Integration-test helpers under `<crate>/tests/` are not exceptions: see T5.

## Unit tests

Unit tests live inside `src/`, next to the code they test. Pick the form by how much test code there is and
how many topics it covers. Moving tests out of line is not required by the size ceilings: lines of a
`#[cfg(test)]` module do not count toward a file ([size-ceilings.md](size-ceilings.md)).

### T1: inline `mod tests`

`#[cfg(test)] mod tests { use super::*; ... }` at the end of the file. This is the default and the most
common form.

### T2: `tests.rs`

When the test module outgrows the file, move it to `tests.rs` in the module's directory and declare it
with `#[cfg(test)] mod tests;`. So for `folds.rs` the file is `folds/tests.rs`, e.g.
`persist/journal/writer/tests.rs`. Two of them (`editor/tests.rs`, `workspace/tests.rs`) are T3 hubs.

### T3: `<topic>_tests.rs`

When one module's tests split by topic, each topic is a child module `<topic>_tests`, declared by the
module under test. Two shapes are in use:

- **Beside the module's other children**: `ui/render.rs` declares `caret_tests`, `window_tests`, ... in
  `ui/render/`. All in `apps/tui` (`app/dispatch/`, `app/session/`, `highlight/`, `input/`,
  `input/ex/`, `input/set_cmd/`, `persist/rj01/`, `ui/render/`). A module may also keep a `tests.rs` next to them
  (`app/dispatch/`).
- **Under a test hub**: a hub module file declares one `<topic>_tests` per topic in its own directory.
  Used when a module has many topic suites.

  | Hub | Topic files |
  | --- | --- |
  | `crates/core/src/editor/tests.rs` + `editor/tests/` | 12 |
  | `crates/core/src/workspace/tests.rs` + `workspace/tests/` | 8 |
  | `apps/tui/src/input/unit_tests.rs` + `input/unit_tests/` | 14 |

  The hub for a new module is `tests.rs` + `tests/`. `input/unit_tests` is the one hub with another name.

### T4: name test files `tests.rs` or `*_tests.rs`

The size scanner (`tools/rusekit/arch/size_scan.py`) treats a file as test code only by its path: under a
`tests/` or `benches/` directory, or named `tests.rs`, `test.rs` or `*_tests.rs`. A test-only file with any
other name is measured as production code.

Known exceptions, test-only modules with other names (each declared under `#[cfg(test)]`):

| File | Role |
| --- | --- |
| `crates/core/src/editor/ex_cmd/samples.rs` | sample `ExCmd` values shared with `command/tests.rs` |
| `apps/tui/src/input/keymap_check.rs` | keymap consistency checks |
| `apps/tui/src/persist/atomic/fault.rs` | fault injection for atomic-write tests |

No source file is named `test.rs`.

## Integration tests and benches

### T5: `<crate>/tests/`

- One file per Cargo test target, named for the behavior it covers (`file_roundtrip.rs`,
  `reference_model.rs`).
- In `apps/tui/tests/`, a target that drives the real binary through a pseudo-terminal starts with `pty_`.
  Targets for one Vim behavior family start with `vim_`.
- Shared helpers are `tests/<name>/mod.rs` (`apps/tui/tests/support/mod.rs`,
  `apps/tui/tests/parity_ratchet/mod.rs`), included with `mod support;`. This is the one place `mod.rs` is
  the rule: Cargo builds every `tests/*.rs` as its own target, so a helper written as `tests/support.rs`
  would become a test target.
- A submodule of a single target uses `x.rs` + `x/` (`crates/core/tests/pattern_oracle.rs` with
  `pattern_oracle/cases.rs`).
- A proptest regression file sits beside its target and is committed (`properties.proptest-regressions`).
- Fixtures go in `apps/tui/tests/fixtures/`.

Benches are one file per target in `<crate>/benches/`, named for what they measure (`edit_apply.rs`,
`journal_replay.rs`).

## Re-measuring

```sh
# module directories: mod.rs vs x.rs + x/
for d in $(find crates/*/src apps/*/src -mindepth 1 -type d); do
  if [ -f "$d/mod.rs" ]; then echo "mod.rs $d"; elif [ -f "$d.rs" ]; then echo "x.rs $d"; fi
done | sort
grep -rlE '^\s*mod tests\s*\{' crates/*/src apps/*/src | wc -l      # T1
find crates/*/src apps/*/src -name tests.rs | wc -l                 # T2 and hubs
find crates/*/src apps/*/src -name '*_tests.rs' | wc -l             # T3
```

`python3 tools/ruse.py docs check` re-measures this file with the same definitions
(`tools/rusekit/docs/module_layout.py`):

- **Errors** (fail CI): the structure the rules name. The L2 lists must name exactly the `x/mod.rs`
  directories (and the `x/mod.rs` counts must match them), the T3 directory list must name exactly the
  directories with a `<topic>_tests.rs` beside a module, the hub table must list exactly the test hubs, and
  "all but N helper files" must match the T4 table. These change only when a PR breaks or extends a rule,
  and that PR updates this file.
- **Warnings** (advisory): the volatile tallies (the evidence column, the module-directory table, topic
  files per hub). Adding a file makes them drift; `python3 tools/ruse.py docs check --write` rewrites them
  in place when someone wants them current.

When you reword a sentence that carries a count, update its pattern in `CLAIMS` too: a claim whose text no
longer matches is an error.
