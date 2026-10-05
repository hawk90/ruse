# AGENTS.md — working notes for coding agents

Tool-agnostic. Rules here are operational shortcuts; the normative sources are `spec/` (state, YAML) and
`docs/` (prose). When this file disagrees with them, they win — fix this file.

## Orientation

- Spec-first Rust TUI editor targeting Vim/Neovim/Emacs parity. Workspace: `crates/core` (kernel: document,
  transaction, undo, anchors, registers, editing grammar) and `apps/tui` (the `ruse` binary: input engines,
  rendering, terminal, LSP, remote).
- Process: [CONTRIBUTING.md](CONTRIBUTING.md) → [docs/operations/development-model.md](docs/operations/development-model.md).
  AI policy: [docs/contributing/ai-assisted-development.md](docs/contributing/ai-assisted-development.md).
- `spec/CONTEXT.md` is hand-maintained and lags reality; trust `spec/PRD.yaml`, `spec/capabilities.yaml`,
  `spec/phases.yaml`, `spec/DECISIONS.md` over it.
- Invariants: [docs/invariants/reference-invariants.md](docs/invariants/reference-invariants.md).
  Anti-patterns: [docs/anti-patterns/anti-patterns.md](docs/anti-patterns/anti-patterns.md).

## Verify before pushing

```sh
python3 tools/spec-validate.py
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # local `ruse verify` clippy is lenient; CI is not
cargo test --workspace
python3 tools/ruse.py verify --full                     # what pre-push (lefthook) runs
```

`python3 tools/ruse.py --help` lists the rest (classify, impact, pr check, bench, gov check).

## Change rules

- Squash-merge only. Branch `feat|fix|spec|rfc|spike/<issue>-<name>`; commit/PR-title type from
  `spec rfc feat fix refactor test bench docs build chore` (enforced by `tools/lint-commit-msg.py`).
- One feature per PR. Design-first for anything touching scope or a hard-to-reverse boundary: RFC +
  `D-*` record in `spec/DECISIONS.md`, landing before or with the first implementation PR.
- A user-visible feature updates `spec/PRD.yaml`, `spec/capabilities.yaml`, and `spec/config-schema.yaml`
  in the same PR when they are affected — not only the design doc. Drift here has bitten before.
- New crate → D-034 policy + `spec/dependencies.yaml` entry first.
- Docs are written in English and structured for agents to parse, whatever language the chat is in.

## Architecture boundaries to preserve

- Input → semantic `Command` → editor. Input engines (`vim`, `emacs`, `cmdline`, `repeat`) produce
  commands; they never mutate the document directly.
- All document mutation goes through transactions (revision + origin); Document never depends on View.
- Folds are frontend-only state.

## Testing and parity

- Behavior parity is checked against real Neovim/Emacs via oracle fixtures; when fixing a divergence,
  capture the oracle ground truth first, then fix (see `tests/parity/` and `tools/parity/`).
- Property tests guard core invariants; a proptest regression file is evidence — commit it.
- Platform hazard: `forkpty` signatures differ between macOS and Linux; clippy-clean on one is not proof for
  the other — check CI on both.
