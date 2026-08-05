---
doc: parity-remote
project: ruse
title: "Parity: Remote Development"
summary: >
  Remote-development parity, TUI-first: thin client + remote workspace runtime split, UI vs workspace
  extension placement, SSH/WSL/container transports, reconnect/session resume, remote file watching,
  client↔runtime version negotiation, typed path handling, and security. Grounded in the VS Code Remote
  reference model; ruse targets the architecture properties, not the wire format.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - ../architecture/architecture.md
  - ../anti-patterns/anti-patterns.md
---

# Parity: Remote Development

ruse is **TUI-first** but remote-first in architecture: the local client is thin, and a workspace runtime
runs next to the code. Remote is not "a remote filesystem" — a design where only files are remote while
LSP/build/debugger run locally fails (SSHFS-style latency on every keystroke). Parity target = the
architecture properties below (see [../architecture/architecture.md](../architecture/architecture.md) §5), at L1.

## REM-SPLIT — Thin client + remote workspace runtime
The local editor is a thin client (render/input/UI); a server binary hosts the workspace-side runtime.

| Component | Runs | Why |
| --- | --- | --- |
| Filesystem | remote | source of truth lives with the code |
| LSP / language servers | remote | need real files, deps, indexes |
| Build / tasks / terminal | remote | correct toolchain + env |
| Debugger | remote | attaches to remote processes |
| Workspace extensions | remote | operate on workspace content |
| UI / theme / keymap extensions | local | only touch the client UI |

| ID | Capability | Target |
| --- | --- | --- |
| REM-SPLIT-1 | Thin client + auto-installed remote runtime; only high-level RPC crosses the wire | L1 |
| REM-SPLIT-2 | UI vs Workspace extension placement (like VS Code `extensionKind`) | L1 |

Guards REMOTE-1/2 (remote ≠ remote FS; not "only files remote"). **Full agent/bootstrap/transport/per-service
design: [../design/remote-runtime.md](../design/remote-runtime.md).**

## REM-SERVICE — Workspace services (UI local / execution remote)
Every heavy service runs in the remote Workspace Agent; the client renders its UI. (Design:
[../design/remote-runtime.md](../design/remote-runtime.md).)

| ID | Service | Client (local) | Agent (remote) | Target |
| --- | --- | --- | --- | --- |
| REM-SERVICE-1 | File tree | tree view/keymap | filesystem, ignore model, **lazy** children, watcher | L1 |
| REM-SERVICE-2 | Search | results/preview UI | ripgrep/index | L1 |
| REM-SERVICE-3 | Git | diff/status UI | repository ops | post-MVP |
| REM-SERVICE-4 | LSP | completion/diagnostic UI | language server + normalized model (C-LSPHOST) | post-MVP |
| REM-SERVICE-5 | Debug | stack/variable/console UI | debugger + target (location model, C-DEBUG) | future |
| REM-SERVICE-6 | Terminal | terminal rendering | PTY/shell | L1 |
| REM-SERVICE-7 | Build/Task/Test | task/test UI | process execution / discovery | post-MVP |

Agent = **headless workspace-execution runtime** (supervisor of these services), auto-bootstrapped over SSH
stdio, no sudo (D-029/D-030/D-031). Capability negotiation gates UI; missing tools degrade, not fail.

## REM-TRANSPORT — SSH / WSL / Container
| ID | Transport | Runtime location | Lifecycle |
| --- | --- | --- | --- |
| REM-TRANSPORT-1 | SSH | remote host over tunnel | bound to host/session |
| REM-TRANSPORT-2 | WSL | Linux distro (`+<distro>` authority) | bound to the distro |
| REM-TRANSPORT-3 | Container/Dev Container | container from a manifest | **container lifecycle ≠ workspace lifecycle** |

- Container: lifecycle hooks (create→postCreate→postStart→postAttach); a failed hook skips later ones;
  editing the manifest does not auto-rebuild; rebuild resets container state (bind-mount persists).
- Guards REMOTE-21/22 (WSL ≠ Linux; container ≠ workspace lifecycle).

## REM-RESUME — Reconnect / session resume
- The runtime **persists on the remote across a dropped tunnel**, so terminals, running tasks, and
  language-server state survive; the client re-attaches.
- Distinguish **transient tunnel loss** (auto-reattach to the live runtime) from **runtime death** (respawn).
- Keepalives to avoid idle drops; a stale cache / version mismatch forces a clean re-provision.

| ID | Capability | Target |
| --- | --- | --- |
| REM-RESUME-1 | Runtime survives client disconnect; client re-attaches | L1 |
| REM-RESUME-2 | Distinguish tunnel loss vs runtime death; document-state recovery | L1 |

Guards REMOTE-8/9. Offline/reconnect editing policy is an open decision (DECISIONS D-013).

## REM-WATCH — Remote file watching
- The watcher runs on the remote FS (e.g. `inotify`); has a fixed watch limit → exclude large trees
  (`node_modules`, `.git/objects`, build dirs); **polling fallback** when unavailable; **full rescan**
  reconciles after gaps/reconnect.

| ID | Capability | Target |
| --- | --- | --- |
| REM-WATCH-1 | Remote-side watching with exclusions + polling fallback | L1 |
| REM-WATCH-2 | Full-rescan reconciliation after watcher gaps | L1 |

Guards REMOTE-12 (trust watcher without full-rescan fallback). Don't re-transfer large files in full
(REMOTE-13); don't stuff blobs into RPC JSON (REMOTE-14).

## REM-VERSION — Client ↔ runtime version negotiation
- Do **not** require byte-identical client/runtime builds. Negotiate a compatible protocol version; either
  bundle/offline-cache compatible runtime builds or negotiate down.

| ID | Capability | Target |
| --- | --- | --- |
| REM-VERSION-1 | Protocol version negotiation (not commit-pinned) | L1 |

Guards REMOTE-10/11; uses the additive-protocol policy ([../protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md)).

## REM-PATH — Typed path handling
- Resources are identified by **URI + remote authority** (`ruse-remote://ssh+host/…`, `+wsl+<distro>/…`),
  never bare OS paths across the boundary.
- Local path ≠ workspace path (distinct types); WSL translation via the platform tool, not string replace.

| ID | Capability | Target |
| --- | --- | --- |
| REM-PATH-1 | Typed ClientPath / WorkspacePath / RemoteUri | L1 |
| REM-PATH-2 | Correct WSL/UNC/case-sensitivity handling (no `/mnt/c` string substitution) | L1 |

Guards REMOTE-6/7, XPLAT-2/4. Aligns with the cross-platform path model (design-requirements §7, §13).

## REM-SEC — Security
- Runtime auth: random per-session key on the remote, presented on every connect.
- Port forwarding defaults to `localhost` on a random port, tunneled (not public); prefer a user-locked
  Unix socket where possible.
- Credential/agent forwarding is a hazard on untrusted hosts — off by default.
- **Connecting executes remote code at the workspace's trust level** — treat any remote as a code-execution
  boundary (workspace trust, [../architecture/architecture.md](../architecture/architecture.md) §10).

| ID | Capability | Target |
| --- | --- | --- |
| REM-SEC-1 | Per-session runtime auth; no public port exposure by default | L1 |
| REM-SEC-2 | Credential/port forwarding off by default; workspace-trust before executing remote code | L1 |

Guards REMOTE-18/19, TRUST-2/7, SEC-4.

## Execution-location model
Every command has an execution location; leaving it implicit causes WSL/SSH bugs.

| Action | Location |
| --- | --- |
| UI command, clipboard, open-browser | client |
| file search, Git, LSP, build, debug | workspace runtime |
| image decode | either (capability-dependent) |

Plugins do not arbitrarily decide execution location (REMOTE-17); remote vs local extensions are
distinguished (REMOTE-20). See design-requirements §25.

## Reference Invariants
- **INV-REMOTE-FIRST** ([../invariants/reference-invariants.md](../invariants/reference-invariants.md)) —
  the client/runtime boundary and typed paths exist from the start, not bolted on; versions are negotiated.
