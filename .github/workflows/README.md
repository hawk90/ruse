# CI/CD Workflows

Implements [`docs/operations/ci-cd-and-release.md`](../../docs/operations/ci-cd-and-release.md). CI and CD
are separate; the release artifact is built once and reused by all channels; rollback re-publishes a prior
verified artifact.

| Workflow | Trigger | Purpose | Status |
| --- | --- | --- | --- |
| [`spec-check.yml`](spec-check.yml) | PR / push | **`spec validate`** — doc-system integrity (dangling refs, layer violations, broken links, enums, dup IDs) | **runnable now** |
| [`ci.yml`](ci.yml) | PR / push | Rust: fmt · clippy · test | no-op until `crates/` has real code |
| [`security.yml`](security.yml) | PR / push / weekly | `cargo-audit` advisory; CodeQL + secret-scan via repo settings | audit activates with `Cargo.lock` |
| [`labeler.yml`](labeler.yml) | PR | path-based `area/*` labels (advisory) via [`.github/labeler.yml`](../labeler.yml) | active |
| [`release.yml`](release.yml) | tag `v*` | build-only: all-platform binaries + SHA256SUMS + SBOM + provenance → draft | planned (needs real builds) |

Not yet as workflows (planned — see the ci-cd doc): `ci-full` (3-OS/integration/benchmarks), `compatibility`
(parity/plugin/protocol fixtures), `nightly` (WSL/tmux/SSH matrix, fuzzing, soak, fault-injection).

Notes:
- **Security (ci-cd §11):** pin `uses:` by commit SHA (TODO markers in each file), least-privilege
  `GITHUB_TOKEN`, block deploy secrets on fork PRs, never feed untrusted PR/issue text into an agent step
  wired to shell/deploy.
- **Gotcha:** GitHub `${{ … }}` expressions are invalid inside YAML flow mappings `{ }` — use block style.
- Merge gates to add as the repo matures: parity fixtures (§3), plugin-compat (§4), protocol fixtures (§5),
  performance budgets (§10).
