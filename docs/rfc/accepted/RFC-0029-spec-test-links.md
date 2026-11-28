---
doc: rfc
project: ruse
title: "RFC-0029: Acceptance criteria and capabilities are linked to the tests that prove them"
summary: >
  PRD acceptance criteria are untagged strings and spec/capabilities.yaml has no test field, so "is this
  requirement proven?" can only be answered per feature file, never per criterion. Measured: 125 criteria
  (70 in done features), 45 capabilities, 100 feature-level trace.verify paths and zero criterion-level links.
  This RFC gives each criterion a stable id, puts the link on the test as a `covers:` marker that a scanner
  collects, checks that every marker lands on a runnable non-ignored test, and rolls out as a ratchet over
  done features rather than a one-shot backfill.
audience: [maintainers, contributors, llm-agents]
status: accepted
related:
  - ../../../spec/DECISIONS.md
  - ../../../spec/PRD.yaml
  - ../../../spec/capabilities.yaml
  - ../../../spec/parity/ratchet.yaml
  - ../../../spec/review-axes.yaml
  - ../../reviews/2026-11-08-v0.1.9.md
  - ../../operations/spec-validate.md
---

# RFC-0029: Acceptance criteria and capabilities are linked to the tests that prove them

- **Status:** accepted — D-070 decided (design), approved by the maintainer 2026-11-28 with every recommendation
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-28
- **Decision link:** D-070

<!-- Hard-to-reverse: criterion ids become references that tests, reviews and later decisions cite; once
     tests carry `covers: F-001.AC2`, renumbering a criterion is a breaking rename across the tree, exactly
     like renaming a CAP-* id. The link form also fixes where coverage truth lives (spec vs test). -->

<!-- code-blocks: illustrative — the Rust and YAML shown are NOT normative; the canonical home is code and
     spec/, per D-038. -->

## Summary

1. **Every PRD acceptance criterion gets a stable id** `F-NNN.ACk`, where k is the criterion's current
   1-based position, frozen at conversion and then never renumbered or reused (like a CAP-* id). Using the
   current position keeps the existing prose refs meaningful: 39 positional refs, 25 with a feature id
   (20 files, "F-0NN acceptance #N"). Slice 1 checks each against its criterion's text and rewrites the ones
   in code and living docs to `F-0NN.ACN`; historical records are left as written.
2. **The link lives on the test**, as a marker line in the comment block directly above a test function:
   `// covers: F-001.AC2` (also `CAP-*`). A scanner in `tools/rusekit/` collects markers into a coverage map.
   Only non-Rust evidence (parity corpus fixtures) is linked from the spec side, as `path#fixture-name`;
   a Rust test is never named from the spec.
3. **The check** (in `ruse gov check`, no build): every marker cites an id that exists; the marked item is a
   `#[test]`/`#[tokio::test]` function (a `proptest!` case included) without `#[ignore]`; a cited fixture exists and
   is not in the known-divergence list. A later CI slice cross-checks against `cargo test -- --list`.
4. **Rollout is a ratchet over per-criterion marks**: every criterion of a `status: done` feature must be
   linked or waived. Slice 1 marks each currently unlinked done criterion in PRD.yaml with
   `waiver: legacy-unlinked (#1321)`; the bucket `acceptance_unlinked` in `spec/parity/ratchet.yaml` counts
   those marks and may only fall. Planned features are not checked.
5. **Capabilities** are covered through their `prd:` features, or directly by a `covers: CAP-*` marker for
   the ones with `prd: []` that ship code. Which capabilities "ship" needs a field the file does not have
   yet; that is a separate, later slice.

## Motivation / Problem

The v0.1.9 review scored RA-RUSE-010 (requirement → design → code → test traceability,
[`docs/reviews/2026-11-08-v0.1.9.md:177`](../../reviews/2026-11-08-v0.1.9.md)) and RA-TEST-030 (requirement →
test traceability, [`:223`](../../reviews/2026-11-08-v0.1.9.md)) `partial`: done features name existing test files, but capabilities and
criteria carry no test links. Probing the code (2026-11-28, origin/main `b9c443e7`, re-checked at `88345206`) confirms the premise and
sharpens it.

### What is checked today

- `trace.verify` is checked only for `status: done` features, only that it is non-empty and that each path
  exists ([`tools/rusekit/spec_trace.py:19-31`](../../../tools/rusekit/spec_trace.py), run from
  [`tools/spec-validate.py:177-183`](../../../tools/spec-validate.py)). A `#fragment` or `::item` suffix is
  stripped and ignored ([`spec_trace.py:16`](../../../tools/rusekit/spec_trace.py)): the syntax for a finer
  link already parses, but nothing reads it.
- `gov capability_coverage` checks that every capability declares `prd:` and that each named feature exists
  ([`tools/rusekit/gov/capability_coverage.py:47-64`](../../../tools/rusekit/gov/capability_coverage.py)); it
  never reaches a test.
- `gov ratchet` already holds `cap_untraced: 18` (capabilities with `prd: []`) as a falling ceiling
  ([`spec/parity/ratchet.yaml`](../../../spec/parity/ratchet.yaml),
  [`tools/rusekit/gov/ratchet.py:54-67`](../../../tools/rusekit/gov/ratchet.py)). It is the mechanism this RFC
  reuses.

### Measurements

Counted with PyYAML over `spec/PRD.yaml` and `spec/capabilities.yaml`, and with `cargo test --workspace --
--list --format terse` (and `--ignored`) for the test inventory.

| Quantity | Count |
| --- | --- |
| PRD features | 32 (18 `done`, 14 `planned`) |
| Acceptance criteria (all plain strings, none with an id) | 125 (70 in done features, 55 in planned) |
| Criteria linked to a specific test | **0** |
| Features with a non-empty `trace.verify` | 32 of 32 |
| `trace.verify` entries | 100 (82 under a `tests/` dir, 18 `src/` files), 80 distinct files, 0 with `::` or `#` |
| `trace.verify` paths that do not exist | 29, all in planned features (placeholders such as `tests/unit/health-report`) |
| Done-feature verify entries that point at a file with no `#[test]` | 13: `tests/parity/vim/fixtures/corpus.yaml` ×10 (data, run by `apps/tui/tests/parity_compare.rs`), `crates/core/src/command/mod.rs`, `crates/core/src/workspace/mod.rs`, `apps/tui/src/ui/window_view.rs` |
| Rust test functions in the workspace | 2580 listed by libtest = 2580 `#[test]`/`#[tokio::test]` attributes by grep (a naive one-line `#[test] … fn name` regex finds 2575: the scanner must walk the attribute block, not match one pattern) |
| `#[ignore]` tests | 5 (two timing probes, `indent_tree_cost`, the live rust-analyzer smoke, the SSH smoke) |
| `.rs` files that contain tests | 277; the 50 `.rs` files named by done features hold 516 tests |
| Code and spec already citing a criterion by position | 39 positional refs, 25 with a feature id (20 files, `F-0NN acceptance #N`, e.g. `apps/tui/src/caps/ledger.rs:1`, `apps/tui/src/persist/atomic.rs:1`, `apps/tui/src/input/cmdline.rs:9`, `apps/tui/src/app/session/frame.rs:188`), plus variants such as `F-008 acceptance 1` (`spec/config-schema.yaml:173`); the other 14 are bare `acceptance #N` whose feature is only in the surrounding prose (e.g. 3 in `docs/rfc/accepted/RFC-0013-lang-arg-translation-stage.md`) |
| Tests named after a criterion | 11 `fn f0NN_…` in `crates/core/tests/vertical_slice.rs` (of its 13 tests; `snapshot_is_immutable_under_later_edits` and `golden_scenario_transcript` are not criterion tests); 173 `.rs` files mention an `F-0NN` id in prose |
| Capabilities | 45 (29 base, 8 official-pack, 8 third-party); no `test`/`verify` field, no `status` field |
| … linked to at least one **done** feature | 8 (EDIT-CORE, TRACE, VIM-PROFILE, COMMAND-PALETTE, SEARCH, RECOVERY, BACKUP, THEME) |
| … linked only to planned features | 19, several of which ship code with tests (e.g. CAP-HEALTHCHECK: `apps/tui/src/health.rs` has 5 tests while F-030's `trace.verify` names the missing `tests/unit/health-report`) |
| … with `prd: []` | 18; at least 3 ship code (CAP-FILE-PICKER `apps/tui/src/ui/file_picker.rs`, CAP-CONFIG `apps/tui/src/config/`, CAP-LSP-CODEC `apps/tui/src/lsp/codec.rs`), the rest are infra or deferred |
| Design requirements (context, not in scope) | 111 items, 194 criteria, 217 `evidence` paths (25 test files) |

The review's "no test cites a requirement id" (`:223`) is off: `vertical_slice.rs` cites them by name, and
code comments across `apps/tui/src` cite criteria by position ("F-010 acceptance #2"). That informal,
positional convention is what this RFC makes machine-readable; the id form keeps its numbers.

**Backfill size.** The work a full link costs is 70 criteria in done features (the gate's scope) and 55 more
as planned features finish. The 516 tests already sitting in done-linked files suggest most done criteria
have a proving test today and need a marker, not a new test; the criteria that turn out to have none are the
real finding the ratchet exposes.

### Costs measured

- A Python regex scan over every `.rs` file under `crates/` and `apps/`: 0.04 s.
- `cargo test --workspace --no-run` (warm deps): 19 s wall; `cargo test --workspace -- --list` afterwards:
  31 s wall (compiles doc-tests). A list-based check is a CI step, not a `gov check` step.

## Guide-level explanation

A contributor finishing a criterion writes the test and marks it:

```rust
// covers: F-001.AC1
#[test]
fn f001_undo_restores_byte_identical_original() { … }
```

`F-001.AC1` is the first criterion of F-001 in `spec/PRD.yaml`:

```yaml
acceptance:
  - { id: AC1, text: Undo after an edit restores the byte-identical original document. }
  - { id: AC2, text: Every change records an origin and a revision. }
```

`python3 tools/ruse.py gov spec_tests` (name provisional) prints, per done feature, which criteria are linked
and by which tests, and FAILS when a marker is dangling or sits on an ignored or non-test item. spec-validate
FAILS a done feature with a criterion that is neither linked nor waived, next to the existing
empty-`trace.verify` rule. `gov ratchet` FAILS when the number of `legacy-unlinked` marks rises.

Parity evidence, which is data rather than a Rust function, is linked from the spec side; a done criterion
not yet linked carries the grandfather mark:

```yaml
  - { id: AC3, text: …, verify: [../tests/parity/vim/fixtures/corpus.yaml#x_delete_char] }
  - { id: AC4, text: …, waiver: legacy-unlinked (#1321) }
```

## Reference-level explanation

### 1. Criterion ids

- `acceptance` items become maps `{id: AC<k>, text: <string>, verify?: [<fixture ref>], waiver?: <reason>}`.
  The full id is `<feature>.<id>`, e.g. `F-022.AC8`. k is the criterion's current 1-based position in the
  list at conversion (a mechanical change: 125 items), so `F-010 acceptance #2` becomes `F-010.AC2` with the
  same meaning. After conversion ids are never renumbered or reused; a removed criterion leaves a gap; a new
  one takes the next k.
- Slice 1 converts the existing positional prose refs (39 positional refs, 25 with a feature id in 20
  files, plus `F-0NN acceptance N` variants), not by blind rewrite:
  - each ref is checked against its criterion's text first, since a ref written before a criterion was
    inserted or reordered may already point at the wrong one; a mismatch is fixed to the right k;
  - a bare `acceptance #N` (no feature id) has its feature filled in by hand from the surrounding prose;
  - only code and living docs are rewritten. Historical records stay as written: the 4 refs in
    `spec/DECISIONS.md` and the 3 in accepted `RFC-0013` (the new id may be added next to the old ref).
- spec-validate rejects a duplicate `id` within a feature and a plain-string item (after slice 1 converts all).

### 2. The marker

- Syntax: a line `// covers: <ID>[, <ID>…]` (also `///`), where `<ID>` is `F-NNN.ACk` or `CAP-*`. It must sit in
  the contiguous comment/attribute block directly above a `fn`; the scanner binds it to that `fn`.
- The bound `fn` must carry `#[test]` or `#[tokio::test]` (a `proptest!` case is written as `#[test] fn …`
  inside the macro, so the same rule covers it), and no `#[ignore]` in the same block.
- A marker that binds to nothing, to a non-test, to an ignored test, or that cites an unknown id FAILS.
- Rust tests are linked only by the marker; a criterion never names a Rust test (`path::fn` is rejected on
  criteria), so there is one place for each kind of evidence.
- Spec-side `verify` refs on a criterion are for non-Rust evidence only: `path#fixture-name` into a parity
  corpus. The `#` part is checked, not stripped: the fixture `name` exists in the corpus and is not in
  `tests/parity/*/known-divergences.txt`.

### 3. The check

- **Static (slice 1, `gov check`, no build).** The rules in §2 plus §1, over the scanner's map. Cost measured
  at 0.04 s for the regex pass.
- **Runtime (later slice, CI).** Parse `cargo test --workspace -- --list --format terse` and `--ignored`,
  keyed by the `Running <target>` line on stderr, and require every linked test to appear in the runnable
  list on both CI platforms. This catches what a text scan cannot: `#[cfg(target_os = …)]` gating, a test
  module never declared with `mod`, a macro-generated name. Today the attribute grep and the libtest list
  agree (2580 = 2580), so this is insurance against drift, not a measured gap.
- **Pass/fail of the linked test** is not a new check: `cargo test --workspace` already fails the build.

### 4. Rollout

- **The done rule applies to every done feature from slice 1:** each criterion of a `status: done` feature
  is linked (a `covers:` marker or a fixture ref) or carries `waiver:`. No feature list lives in checker code.
- **Grandfathering is per criterion, in PRD.yaml.** Slice 1 marks every done criterion it does not link with
  `waiver: legacy-unlinked (#1321)`. A hand-reviewed criterion uses another reason (`waiver: manual — …`), if
  D-070 allows non-legacy waivers at all.
- **The bucket counts the marks.** `acceptance_unlinked` = the number of `legacy-unlinked` waivers, added to
  `current_buckets` ([`ratchet.py:54-67`](../../../tools/rusekit/gov/ratchet.py)) with a ceiling equal to its
  count when slice 1 lands: 70 minus the criteria the 11 `vertical_slice.rs` tests link (F-001, F-002 and F-005
  hold 12 criteria, so the ceiling lands between 58 and 69; slice 1 measures it). Because each mark names one criterion, a PR cannot link one criterion and quietly unlink
  another: unlinking a criterion without adding its mark FAILS the done rule, and adding the mark raises the
  bucket. Removing a mark (backfill) lowers it.
- A new `legacy-unlinked` mark is legal only on a criterion of a feature that was done on the base branch;
  a feature becoming done must link or carry a non-legacy waiver.

### 5. Capabilities

- Covered when any linked feature has a linked criterion, or any test carries `covers: CAP-<id>`.
- Requiring coverage needs to know which capabilities ship. `spec/capabilities.yaml` has no such field; the
  slice that adds one (`status: shipped | planned | deferred`) also adds a `cap_untested` ratchet bucket.

### 6. Files the slices change

`tools/rusekit/spec_tests.py` (new scanner) and `tools/rusekit/gov/spec_tests.py` (gate), their unit tests
under `tools/tests/`, `tools/rusekit/spec_trace.py` (criterion fixture refs resolved, not stripped),
`tools/rusekit/gov/ratchet.py` + `spec/parity/ratchet.yaml` (bucket), `spec/PRD.yaml` (criterion ids and
`legacy-unlinked` marks), `crates/core/tests/vertical_slice.rs` (first markers), the code and living docs with positional
`acceptance #N` refs (not DECISIONS.md or accepted RFCs), `docs/operations/spec-validate.md` (the rule), and later
`spec/capabilities.yaml` (status) and a CI workflow step.

## Reference Invariants

- No product invariant changes; no editor code changes. The governance invariant introduced: **a done
  criterion names a runnable test, or a stated waiver** (enforced from the slice that adds the rule).
- The new ids follow the registry rule already stated for CAP-* ids: never renumber, never reuse
  ([`spec/capabilities.yaml:11`](../../../spec/capabilities.yaml)). No INV-* is added or changed; the editor
  invariants in [reference-invariants](../../invariants/reference-invariants.md) are untouched.

## Failure modes & Recovery

- **A test is deleted or renamed.** With a test-side marker, a rename keeps the link; a delete leaves the
  criterion unlinked and unwaived, so the done rule FAILS with the criterion named (adding a
  `legacy-unlinked` mark instead raises the ratchet).
- **A marker on the wrong criterion.** Not machine-detectable; review catches it, as today with
  `trace.verify`. The coverage report makes it reviewable (criterion text next to test name).
- **One test claims many criteria.** Allowed; the report shows it, and a reviewer can ask for a split.
- **Ids drift from text** (a criterion reworded so the marker no longer fits). The PR that rewords the
  criterion shows the markers citing it (`grep covers: F-001.AC2`).

## Security impact

None. Tools-only; reads repository files.

## Performance impact

`gov check` grows by one text scan (0.04 s measured). The runtime cross-check costs a test build plus ~31 s
of listing and belongs in CI where the build already exists.

## Compatibility & Migration

`acceptance` items change from strings to maps once (slice 1). The only tool that reads `acceptance` today is
the DoD non-empty check ([`tools/spec-validate.py:190`](../../../tools/spec-validate.py)), which a map list
still satisfies; slice 1 re-greps `tools/` for readers and updates any in the same PR. No user-facing or on-disk format
changes.

## Observability

The gate prints linked / unlinked / waived per done feature, and the ratchet line. The coverage map can feed
the milestone review (RA-TEST-030) directly instead of a hand count.

## Alternatives

### Decision 1 — how a criterion is identified

| Option | Shape | For | Against |
| --- | --- | --- | --- |
| **1A (recommended)** stable id per criterion, k = current position frozen at conversion | `{id: AC1, text: …}`, cited `F-001.AC1` | survives reordering and insertion afterwards; greppable; keeps the meaning of the 39 existing positional refs (25 with a feature id, 20 files), each checked against its criterion's text at conversion; matches the CAP-* "never rename" rule | one mechanical 125-item change to PRD.yaml; readers of `acceptance` update |
| 1B live positional index (today's informal form) | `F-001#2` = whatever is second now | no schema change | silent re-pointing when a criterion is inserted or reordered; links rot without failing |
| 1C stay at feature level | tighten `trace.verify` to `path::fn` only | smallest | does not answer the issue: a feature with 8 criteria (F-022) is "covered" by one test |

### Decision 2 — where the link lives

| Option | Shape | For | Against |
| --- | --- | --- | --- |
| **2A (recommended)** test-side marker, scanner collects; spec side only for fixtures | `// covers: F-001.AC1` above the test | moves with the test on rename/split; the reviewer sees in the test what it proves; no PRD bloat; code already cites criteria informally (`vertical_slice.rs` names, 39 positional `acceptance #N` refs) | coverage truth is assembled, not read in one file (the gate prints it); a comment the compiler never sees |
| 2B spec-side refs on each criterion | `verify: [../crates/core/tests/x.rs::fn]` | one place to read; extends the existing `trace.verify` pattern | breaks on every test rename/move; ~125+ more paths in a 1338-line PRD.yaml |
| 2C a separate link file | `spec/test-links.yaml` id → tests | neither PRD nor tests change | a third place to keep in sync; the drift this issue is about |
| 2D a Rust attribute | `#[covers("F-001.AC1")]` via a proc-macro crate | compiler-visible, could feed `--list` | a new crate (D-034 + `spec/dependencies.yaml`), a dev-dependency in both crates, for a check a text scan does |

2A keeps the spec side (2B) only for non-Rust evidence (parity fixtures), where no test function exists to
carry a marker and the corpus is generated (`tools/parity/oracle.py`), so it cannot carry one either.

### Decision 3 — what the check proves

| Option | Proves | Cost |
| --- | --- | --- |
| 3A static scan only | marker → known id, bound to a `#[test]` fn, not `#[ignore]` | 0.04 s, in `gov check` |
| 3B static + libtest `--list` (recommended, as two slices) | also: the test is compiled and runnable on each CI platform | a test build + ~31 s, CI only |
| 3C plus per-test pass evidence (JUnit / nextest) | the linked test passed in this run | a new tool (nextest) or unstable libtest JSON; redundant with `cargo test` failing the build |

### Decision 4 — rollout

| Option | Shape | For | Against |
| --- | --- | --- | --- |
| **4A (recommended)** done rule for every done feature + per-criterion `legacy-unlinked` marks, the bucket counts the marks | each unlinked done criterion is named in PRD.yaml; ceiling falls only | reuses `gov ratchet`'s count-and-ceiling; no feature list in code; a link swapped for an unlink is visible (a mark appears, the count rises); the backfill is visible per criterion | 58-69 marks in PRD.yaml until burned down |
| 4A′ count-only bucket + a list of grandfathered features in the checker | `acceptance_unlinked` = unlinked done criteria | no PRD marks | one PR can link one criterion and unlink another at the same count; the 18-feature list lives in code, not spec |
| 4B backfill all at once | one PR links all 70 done criteria | done in one step | a large PR mixing 18 features; the criteria with no test need new tests, which are separate fixes |
| 4C ratchet over all 125 | planned criteria counted too | one number | planned criteria have no code yet (29 placeholder paths); the bucket would be mostly noise |

### Decision 5 — capabilities

| Option | Shape | For | Against |
| --- | --- | --- | --- |
| **5A (recommended)** derived through `prd:`, plus `covers: CAP-*` for `prd: []`; add a cap `status` later | no new cap field now | no duplication of the feature links | the 19 caps whose features are planned but code ships (CAP-HEALTHCHECK) stay unproven until their feature is done or a cap marker is added |
| 5B a `verify:` field on every capability | spec-side, like 2B | one place | duplicates features; same rot as 2B |
| 5C defer capabilities entirely | criteria only | smallest | leaves `prd: []` shipped caps (file picker, config, LSP codec) with no route at all |

## Rejected approaches

- **Coverage percentage (llvm-cov) as proof.** Line coverage says code ran, not that a criterion was asserted.
- **Inferring links from test names** (`fn f001_…`). Names carry the feature but not the criterion, and a
  rename silently changes coverage; the marker is explicit.
- **A count-only bucket with the grandfathered features listed in checker code** (4A′). It hides which
  criteria are debt and lets a swap of one link for one unlink pass unnoticed.
- **Counting `trace.verify` file existence as criterion coverage.** That is today's state; 13 done entries
  point at files with no test function.

## Trade-offs

Test-side markers trade a single readable spec column for links that survive refactors (the repo splits files
routinely: #1414, #1418). The gate's printed report replaces the column. Ids add a one-time mechanical diff
to PRD.yaml and a rule that ids are permanent.

## Re-evaluation conditions

- A proc-macro or test-harness crate is admitted for another reason (then 2D costs nothing extra).
- nextest or stable libtest JSON is adopted (3C becomes cheap).
- The PRD generator (D-022, #982) lands: the coverage map can be generated into the digest.
- Design requirements (DR-*) want the same links: the scanner accepts `covers: <DR id>` without a new design.

## Open questions

At acceptance (2026-11-28) the maintainer took every recommendation: question 1 is settled as `F-001.AC1`;
question 3 as recommended by 2A (a named fixture ref on a criterion is a link); question 4 as 5A (the
capability `status` field comes in slice 5, not now). Questions 2 and 5 had no recommendation and stay open
for the slice that needs them (2 before slice 3's backfill; 5 after #1705).

1. Id spelling: `F-001.AC1` (proposed) or `F-001/AC-1`, `AC-F001-1`.
2. Whether a non-legacy `waiver:` (a hand-reviewed criterion) is allowed at all, or every done criterion
   eventually needs a test.
3. Whether the 10 corpus links of done features count as criterion links once a fixture name is given, or
   whether the corpus is feature-level evidence only.
4. Whether the capability `status` field (5A, later) is wanted now, given D-033's axes.
5. Whether `spec/design-requirements.yaml` joins later. It does not parse as YAML today (PAR-003's `notes:`
   at line 199, ` #901)` starts a comment); that is filed separately as #1705 and is not part of this design.

## Slice plan

1. **Ids, scanner, gate, done rule, grandfather marks, ratchet, first markers.** Convert the 125 criteria
   to `{id, text}` with k = current position (mechanical); convert the positional refs (39, 25 with a feature id
   in 20 files, plus variants such as `spec/config-schema.yaml:173`) to `F-0NN.ACN` in code and living docs,
   each checked against its criterion's text first, bare `acceptance #N` given its feature by hand, and
   historical records (4 in `spec/DECISIONS.md`, 3 in accepted RFC-0013) left as written; update the `acceptance` readers; add the scanner + `gov spec_tests` with unit tests and the
   done rule in spec-validate; mark the 11 criterion tests in `crates/core/tests/vertical_slice.rs`
   (F-001/F-002/F-005; not `snapshot_is_immutable_under_later_edits` or `golden_scenario_transcript`); put
   `waiver: legacy-unlinked (#1321)` on every other done criterion; add `acceptance_unlinked` counting those
   marks at its measured ceiling. No new test is written.
2. **Fixture refs.** Criterion `path#fixture-name` refs checked instead of stripped (`spec_trace.py:16`),
   against the corpus and its known-divergence lists; `path::fn` on a criterion rejected.
3. **Backfill, one feature per PR**: add `covers:` markers, remove the marks, lower the ceiling; a criterion
   with no proving test gets an issue in the next milestone, not a marker.
4. **Runtime cross-check in CI** (3B) on macOS and Linux.
5. **Capabilities**: `status` field, `cap_untested` bucket (5A).
