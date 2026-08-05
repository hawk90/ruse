---
doc: ci-cd-and-release
project: ruse
title: "ruse CI/CD & Release Governance"
summary: >
  Operational pipeline for a project with many artifacts and a plugin/remote ecosystem: separated CI/CD,
  layered tests, feature-parity/plugin/protocol compatibility gates, release build-vs-publish separation,
  signed artifacts + SBOM + provenance, release channels, performance-budget gates, security ops,
  fault-injection, and branch/release model. Goal: not "an editor that works now" but one whose ecosystem
  keeps not breaking across repeated releases. ruse adds parity, plugin API, remote protocol, terminal
  matrix, and performance budgets as gates.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../architecture/architecture.md
  - ../protocols/versioning-and-evolution.md
  - ../parity/README.md
  - ../design/stability-and-observability.md
---

# ruse CI/CD & Release Governance

> ruse's release flow: build verification → multi-artifact publish → documented fallback → release-asset
> verification, with per-platform binaries + SHA-256 checksums. Because ruse has many artifacts and a
> plugin/remote ecosystem, it is strict and adds **parity, plugin API, remote protocol, terminal matrix,
> and performance budgets** as merge/release gates.

## 1. Separate CI from CD

- **CI** — verify correctness of every change; the condition to merge a PR.
- **CD** — publish already-verified artifacts; requires a tag or approval.

Never test-and-immediately-deploy in one giant `ci.yml`.

```
.github/workflows/
├── ci-fast.yml          # every PR, 5–10 min
├── ci-full.yml          # main / merge
├── compatibility.yml    # parity + plugin + protocol
├── security.yml
├── release-build.yml
├── release-publish.yml
└── nightly.yml
```

> Skeletons live in [`.github/workflows/`](../../.github/workflows/README.md). `ci-fast.yml`'s
> `spec-validate` job is runnable today (`python3 tools/spec-validate.py`); Rust jobs no-op until `crates/`
> exists; the rest are planned stubs.

### Fast PR CI (5–10 min)
`format → lint → compile → unit tests → core property tests → changed-component tests`.
Base platforms: **Linux x86_64, Windows x86_64, macOS ARM64**. WSL is hard to reproduce on hosted runners —
run it on self-hosted or scheduled integration, not every PR.

## 2. Layered Tests

`cargo test` alone is insufficient.

| Level | Scope | Runs on |
| --- | --- | --- |
| **L1 Unit** | Document, Transaction, Anchor, key parser, command resolver | PR |
| **L2 Property** | edit→undo→original restored; anchor-transform invariants; transaction atomicity; serialization round-trip | PR |
| **L3 Compatibility** | Vim command corpus; Emacs key sequences; plugin API fixtures; remote protocol versions | PR (core subset) |
| **L4 Platform** | Unix PTY, Windows ConPTY, tmux, WSL, SSH | main / nightly |
| **L5 End-to-end** | launch → edit → save → restart → recover | main / nightly |

PR runs L1–L3 core; main/nightly run the full set.

## 3. Feature Parity as a CI Gate

Do not manage Vim/Neovim/Emacs parity in doc tables only — they rot. Encode as executable fixtures:

```
tests/parity/
├── vim/    operator_motion.yaml, registers.yaml, dot_repeat.yaml
├── emacs/  kill_ring.yaml, universal_argument.yaml, prefix_keys.yaml
└── expected/
```

```yaml
name: vim-delete-inner-word
initial: { text: "hello world", cursor: 1 }
input:   { profile: vim, keys: ["d", "i", "w"] }
expected: { text: " world", mode: normal, register: "hello" }
```

Fixtures assert not just final text but **cursor, register/kill ring, mode, selection shape, undo grouping,
and error timing** (see parity semantic-parity requirements, [../parity/README.md](../parity/README.md)).
A "Vim Style parity" status can only rise when these pass.

## 4. Plugin Compatibility CI

Core ecosystem-stability gate:

```
plugin-compat/
├── api-v1-minimal
├── api-v1-git-view
├── api-v1-language-service
├── api-v1-remote-provider
└── api-v1-media-view
```

On every core PR: build with the current SDK **and** run WASM built with the **previous** SDK, verifying
command registration, event receipt, and transaction requests. New API features may be added, but if an
existing fixture breaks, **merge is blocked**.

## 5. Protocol Compatibility Tests

Because of the client/runtime split, remote/plugin/render protocols, the diagnostic schema, and command
descriptors are long-lived contracts.

```
protocol-fixtures/
├── v1.0/
├── v1.1/
└── malformed/
```

Verify new versions can read old fixtures, with explicit tests for **unknown enum variant**, **missing
optional field**, and **ignored new capability** (the additive rules in
[../protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md)).

## 6. Release = Build separated from Publish

```
Tag → Release Build → all-platform binaries → test + sign + checksum → Draft Release → review → Publish
```

- The release artifact is built **once** in CI; **all channels use the same artifact**.
- **Rollback = re-publish a previous verified artifact**, never a fresh build.
- **Reproducible builds** verified on platforms that support them.

Recommended assets:

```
editor-linux-x86_64.tar.gz      editor-linux-aarch64.tar.gz
editor-macos-x86_64.tar.gz      editor-macos-aarch64.tar.gz
editor-windows-x86_64.zip
editor-remote-agent-*.tar.gz
editor-plugin-sdk.wasm
SHA256SUMS
SBOM.spdx.json
provenance.json
```

Checksums are the baseline; because ruse has plugin + remote execution, ship **SBOM and provenance** too.

## 7. Release Channels

| Channel | Contents |
| --- | --- |
| **stable** | long-term compatibility; verified Plugin API |
| **beta** | next release candidate; migration warnings |
| **nightly** | experimental API; debug features |

```toml
[update]
channel = "stable"
```

Plugin manifests may require a minimum channel: `editor_channel = "stable"`, `api = "^1.4"`.

## 8. Manual Deploy Fallback (documented, not default)

Automated deploy is the norm; document manual procedures for failures (keep tokens out of shell history/logs):

```
docs/runbooks/
├── release-failure.md
├── marketplace-outage.md
├── signing-failure.md
├── rollback.md
└── security-revocation.md
```

Manual release constraints: **2 approvers**, reuse the same release artifact (no local rebuild), audit-log
after publish.

## 9. Ops Status ↔ CI Status (computed support, not claimed)

Each release auto-generates a compatibility report — support is a **test result**, not a doc claim:

```
Compatibility report
- Plugin API v1:     PASS
- Remote protocol v1: PASS
- Vim parity:         82%
- Emacs parity:       54%
- Terminal (compatibility):  PASS   # ANSI/Unicode/256/legacy — the "compatibility" render profile
- Terminal (enhanced):       PASS   # truecolor/Kitty-kbd/sync-output/images — the "enhanced" profile
- Windows ConPTY:     PASS
- WSL image rendering: LIMITED
```

Parity % is **weighted by usage frequency and importance**, not raw feature count
([../parity/README.md](../parity/README.md)).

## 10. Performance Regression as a Merge Gate

To avoid the Neovim "slows down as plugins accumulate" problem, put performance in CI.

Benchmark budgets: cold startup, empty-buffer latency, 10 MB file open, insert latency, scroll frame time,
plugin activation, remote round-trip, memory after 100 files. Manage **p95/p99**, not averages.

```
startup p95 > baseline + 10%    → warn / block
input-to-render p99 > 16 ms     → fail
idle CPU > threshold            → fail
```

Benchmarks are noisy: **PR = trend comparison + warning; main/nightly = gate on a fixed machine** (never
update the baseline from a developer's personal machine).

## 11. Security Ops

Plugins + remote workspaces widen the supply-chain surface. Required:

- Dependabot/Renovate; CodeQL (or equivalent static analysis); Rust advisory (`cargo-audit`); license check;
  secret scan.
- Plugin package signing; GitHub Actions pinned by SHA; least-privilege `GITHUB_TOKEN`; deploy secrets
  blocked on fork PRs.
- **AI-based workflows:** never feed untrusted text (PR/issue bodies) into an agent and then wire its output
  directly into shell or deploy steps — a known GitHub-Actions agent attack surface.

## 12. Fault-Injection CI (from long-horizon additions)

Inject failures and assert graceful behavior: **disk full, permission loss, process crash, packet loss,
truncated journal**. This validates the stability model ([../design/stability-and-observability.md](../design/stability-and-observability.md)) and crash-consistency (persistence) design.

## 13. Branch / Release Model

```
main       always releasable; stable API
next       next minor; experimental integration
feature/*  short-lived
release/x.y  stabilization + security fixes only
```

Avoid a long-lived `develop`; for a small team, `main + short-lived branches + release branches` is simpler.
Use a **merge queue** to verify combined changes. Test **quarantine entries carry an expiry + owner**.
Observe CI's own performance and stability.

## Final Pipeline

```
PR
 → Fast CI (fmt, lint, unit, parity, protocol, affected plugins)
Merge to main
 → Full CI (3 OS, integration, compatibility, security, benchmarks)
Nightly
 → WSL/tmux/SSH matrix, fuzzing, large corpus, plugin ecosystem, soak test, fault injection
Version tag
 → Reproducible build → Sign + checksum + SBOM → Draft release → Smoke test → Stable publish
```

## Anti-Patterns Guarded
See [../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md) categories **OPS** and **TEST**:
first full-platform build at release tag; release artifact from different source than the PR that passed;
solving flaky tests with retry only; updating baselines on personal machines; leaving long-lived secrets +
residual workspaces on self-hosted runners; ignoring persistent nightly failures as "unrelated to stable".
