# Security Policy

## Reporting a vulnerability

**Do not open a public issue, PR, or Discussion for a security vulnerability.**

Report privately via one of:

- **GitHub private vulnerability reporting** — repo → *Security* tab → *Report a vulnerability*, or
- **Email** — `<security contact>`.

Please include: affected version/commit, reproduction steps or PoC, impact, and any suggested fix.
We aim to acknowledge within a few days and will keep you updated through resolution.

## Supported versions

| Version         | Supported |
| --------------- | --------- |
| `main` (dev)    | ✅        |
| latest release  | ✅        |
| older releases  | ❌        |

ruse is pre-1.0; only the development tip and the latest tagged release receive security fixes.

## Disclosure

**Coordinated disclosure.** We fix privately, release a patched version, then publish an advisory
crediting the reporter (unless anonymity is requested). Please give us reasonable time before public
disclosure. There is **no bounty program** initially.

## Security-sensitive scope

These subsystems are trust boundaries — reports touching them are high priority:

- **Plugin sandbox** — isolation of third-party plugins from core and the host.
- **Remote agent / remote protocol** — the workspace-over-a-connection runtime.
- **Terminal escape handling** — parsing/emitting control sequences (escape-sequence injection).
- **Credential & port forwarding** — secrets and forwarded ports over the remote channel.
- **AI review-before-apply** — the gate that requires human review before AI-proposed edits are applied.

Design context:
[`docs/design/stability-and-observability.md`](docs/design/stability-and-observability.md)
(error isolation boundaries / trust — §6, §9, security-relevant sections) and
[`docs/architecture/architecture.md` §10 "Security · Trust Model"](docs/architecture/architecture.md).
