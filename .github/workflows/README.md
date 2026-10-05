# CI/CD Workflows

Implements [`docs/operations/ci-cd-and-release.md`](../../docs/operations/ci-cd-and-release.md). CI and CD
are separate; the release artifact is built once and reused by all channels; rollback re-publishes a prior
verified artifact.

| Workflow | Trigger | Purpose | Required check |
| --- | --- | --- | --- |
| [`spec-check.yml`](spec-check.yml) | PR / push | `spec validate`, docs hygiene, toolchain unit tests, **actionlint** over these workflows | `spec-validate` |
| [`change-policy.yml`](change-policy.yml) | PR | merge gate: re-derives change kind + blast radius from the diff vs the `ruse-gate:v1` block in the PR body (`dependabot[bot]` / `renovate[bot]` are auto-declared) | `gate` |
| [`ci.yml`](ci.yml) | PR / push | Rust: fmt · clippy `-D warnings` · test · `arch deps`; heavy steps self-skip on PRs with no Rust-relevant change. `rust-macos` (advisory) repeats clippy + test on macOS | `rust` |
| [`security.yml`](security.yml) | PR / push / weekly | `rustsec` advisory scan (PRs only when a Cargo manifest changed); CodeQL + secret scanning live in repo settings | — |
| [`labeler.yml`](labeler.yml) | PR | path-based `area/*` labels via [`.github/labeler.yml`](../labeler.yml) | — |
| [`perf.yml`](perf.yml) | nightly / manual | benches vs `spec/perf-baseline.yaml`; trend + warn only, never fails on regressions (D-019) | — |
| [`release.yml`](release.yml) | tag `v*` | **stub** — build-once release (binaries + SHA256SUMS + SBOM + provenance) not implemented yet | — |

Not yet as workflows (planned — see the ci-cd doc): `ci-full` (3-OS/integration/benchmarks), `compatibility`
(parity/plugin/protocol fixtures), `nightly` (WSL/tmux/SSH matrix, fuzzing, soak, fault-injection).

Notes:
- **Security (ci-cd §11):** every `uses:` is pinned by commit SHA with a `# vN` comment (Dependabot's
  `github-actions` ecosystem bumps both); least-privilege
  `GITHUB_TOKEN`, block deploy secrets on fork PRs, never feed untrusted PR/issue text into an agent step
  wired to shell/deploy.
- **Gotcha:** GitHub `${{ … }}` expressions are invalid inside YAML flow mappings `{ }` — use block style.
- Merge gates to add as the repo matures: parity fixtures (§3), plugin-compat (§4), protocol fixtures (§5),
  performance budgets (§10).
- **Gotcha:** `hashFiles()` is only valid in step-level fields, not a job-level `if:` — the whole run fails
  before any step. `actionlint` in `spec-check` now catches this class.
- **Toolchain:** `actions-rust-lang/setup-rust-toolchain` is used WITHOUT a `toolchain:` input so it reads the
  repo's `rust-toolchain.toml` — local rustup and CI then run the same pinned version. Pass `cache: false`
  (Swatinem/rust-cache is the cache) and `build-warnings: allow` (the action defaults to `deny`).
