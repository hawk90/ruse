---
doc: size-ceilings
project: ruse
title: "ruse — Function and File Size Ceilings"
summary: >
  The size rule for non-test Rust code (RA-STRUCT-005): aim for functions under 100 lines and files under
  800 non-test lines. `gov ratchet` holds two buckets, functions over 150 lines and files over 1,000
  non-test lines, at their committed ceilings, so the counts can fall but never rise unexplained.
  Exhaustive dispatches whose length is their variant list, and data tables, are allowed above the target.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - ../../spec/parity/ratchet.yaml
  - ../../spec/review-axes.yaml
  - ../reviews/2026-11-16-v0.2.1.md
---

# ruse — Function and File Size Ceilings

## Targets

| Unit | Target | Ratcheted above |
| --- | --- | --- |
| Function (from its `fn` line to its closing brace) | under 100 lines | 150 lines (`fn_over_150`) |
| File (non-test lines) | under 800 lines | 1,000 lines (`file_over_1000`) |

The target is what new and refactored code aims for. The ratchet is what the gate enforces.

## The ratchet

- `python3 tools/ruse.py gov ratchet` (part of `gov check` and `verify`) counts both buckets and fails
  when either rises above its ceiling in [`spec/parity/ratchet.yaml`](../../spec/parity/ratchet.yaml).
- On failure it lists every function or file in the bucket (`path:line name size`), so you can see what grew.
- When a split brings a count down, lower the ceiling in the same PR. The gate warns until you do.
- Raising a ceiling needs a new reason in `raised.<bucket>`, the same as every other ratchet bucket.

## What is measured

- Non-test `.rs` files under `crates/` and `apps/`. Files under a `tests/` or `benches/` directory, and
  files named `tests.rs`, `test.rs` or `*_tests.rs`, are skipped.
- Lines of a `#[cfg(test)]` module do not count toward the file, and its functions are not measured.
- Comments, strings and char literals are blanked first, so a brace inside them does not move a function's
  end.
- The scanner is `tools/rusekit/arch/size_scan.py`.

## Allowed above the target

- **Exhaustive dispatches** whose length is their variant list: one `match` over an enum with one short
  arm per variant. Splitting it would hide the exhaustiveness check, not reduce complexity. It still
  counts in the bucket. Say why in the PR that adds or grows it.
- **Data tables** are exempt by name, with a reason, in `EXEMPT_FILES` / `EXEMPT_FNS` of the scanner:
  `crates/core/src/case_tables.rs` (generated case tables), `apps/tui/src/input/set_cmd/table.rs` (the
  `:set` option table), `crates/core/src/editor/ex_cmd/samples.rs` (the Ex sample table) and
  `EmacsProfile::new` in `apps/tui/src/input/emacs.rs` (the Emacs binding table). A new exemption is
  reviewed like a raised ceiling.
