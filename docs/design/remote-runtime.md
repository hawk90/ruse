---
doc: remote-runtime
project: ruse
title: "ruse Remote Runtime & Agent Architecture"
summary: >
  Remote development is a built-in capability, not a plugin. The local client is thin (UI/input/render);
  a headless "Workspace Agent" runs on the remote as a supervisor of workspace-local services — filesystem/
  watch, search/index, Git, LSP, debug (DAP/GDB), PTY, port-forward, toolchain discovery. UI is local,
  execution is remote. Covers the SSH-stdio transport, agent bootstrap/install policies (no sudo, versioned
  side-by-side, offline-first upload), per-service protocol, capability negotiation, and the debug
  location model. This is the RFC-0006 design; parity targets live in parity/remote.md.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - ../parity/remote.md
  - ../../spec/DECISIONS.md
  - ../protocols/versioning-and-evolution.md
---

# ruse Remote Runtime & Agent Architecture

## Core principle

> Remote is **not "files are remote."** Every computation that must be near the data/toolchain runs in the
> **Workspace Runtime** on the remote; the local client renders UI and captures input. The Agent is a
> **headless execution runtime that manages the workspace's filesystem, tools, processes, language servers,
> and debuggers** — not a file transfer daemon.

- **File Tree** is local UI over a remote filesystem.
- **LSP** UI is local; the language server runs remotely (real toolchain, deps, indexes, sysroot).
- **Debug** UI is local; the debugger and target each have their own location.
- **Terminal** renders locally; the PTY runs remotely.

## Not like VS Code's local Remote extension

There is **no required local Remote package**. The SSH connector is a **Built-in Service**; the user just
runs `editor ssh host`. Making remote a plugin would cause: hard-to-discover feature, plugin↔core version
coupling, connection breaking on plugin-load failure, remote unusable in safe/recovery mode, and the
client/runtime protocol becoming hostage to a third-party extension API. (DECISIONS **D-029**.)

Classification:

| Concern | Layer |
| --- | --- |
| SSH connect + Agent bootstrap | **Built-in Service** |
| SSH host list / connection picker / status | Bundled Extension or built-in UI |
| AWS Session Manager, Kubernetes, special VPN providers | Third-party Plugin |

## Architecture

```
Local TUI Client                         Workspace Runtime (remote)
├─ File Tree View                        ├─ FileSystem Service + Watcher
├─ Editor View                           ├─ Search / Index Service
├─ Diagnostics View                      ├─ Git Service
├─ Debug View                            ├─ Process / Task Service
├─ Terminal View          ── Remote ──▶  ├─ PTY Service
└─ Command / Input / Profile   Protocol  ├─ Language Service Host
                                         ├─ Debug Service
                                         ├─ Port Forwarding
                                         ├─ Toolchain Discovery
                                         └─ Health / Recovery
                                                    │
                                         Debug Target (remote process / container / board / QEMU)
```

The Agent is a **supervisor of workspace-local services**, not a single daemon. `editor-client` has no
business understanding remote files directly; `editor-agent` has no terminal rendering / clipboard / input
profile / status-bar / theme / command-palette UI.

## Bootstrap & install (auto, no sudo)

User does one thing: `editor ssh build-server` (or `editor ssh user@host:/workspace/project`). Internally:

```
1 SSH connect → 2 detect OS/arch → 3 check for a compatible agent →
4 if absent, client UPLOADS the matching agent binary → 5 verify checksum →
6 atomic install under $HOME → 7 run agent --stdio → 8 protocol handshake → 9 workspace connect
```

Install layout (versioned, side-by-side):

```
~/.local/share/ruse/agents/   executables (side-by-side by protocol+version+arch)
~/.cache/ruse/                regenerable cache
~/.local/state/ruse/          logs / state / recovery
```

**Install principles (D-030):** no sudo; install under `$HOME`; never overwrite a running agent in place;
side-by-side per version; checksum/signature verify; upload to a temp file then atomic rename; never execute
from a world-writable path; auto-prune old versions; provide removal.

```
editor remote status <host>     editor remote reinstall <host>     editor remote logs <host>
editor remote remove <host>     editor remote prune <host>
```

**Offline-first:** the client ships agent bundles for common targets and uploads over SSH rather than having
the remote download from the internet (works in air-gapped nets, no curl/wget needed, exact version match,
simpler supply chain). Resolution order: (1) local agent cache → (2) client downloads from official server
→ (3) remote direct download → (4) user-specified internal mirror.

```
release package
├─ editor client
└─ agent bundles: linux-x86_64 · linux-aarch64 · macos-x86_64 · macos-aarch64 · windows-x86_64
```

Three install policies: **managed** (default; auto-install/update on connect), **pre-installed**
(`editor-agent install --system` by an admin; client detects it), **no-install** (basic mode / limited).

## Transport: SSH stdio first

Start with SSH stdio + a multiplexed framed protocol; no listening ports/tunnels initially.

```
ssh host ~/.local/share/ruse/agents/<ver>/editor-agent --stdio
   └─ multiplexed protocol: control · filesystem · process · terminal · language · debug · git · events · logs
```

Benefits: no listening port, minimal firewall config, auth delegated to SSH, clean process/connection
lifecycle, simpler call graph and debugging. **Rule: stdout is protocol-only** — agent logs go via protocol
messages or stderr, never raw stdout (would corrupt framing). Evolve to a **persistent socket** later for
fast reconnect, multiple clients, task survival across disconnect, session sharing, long background indexing.
(D-031.)

## Two modes

- **Basic SSH mode** — no agent install: open/read/save files, simple command execution; limited features
  stated explicitly. Useful for a quick server edit or agent-execution-forbidden environments.
- **Workspace Agent mode** — persistent connection: watch/search/Git/LSP/debug/task/PTY, status/recovery/
  cancellation; the full remote experience.

On agent-exec failure, fall back with a clear choice:

```
Remote host does not allow agent execution.
[Use basic mode]  [Choose another install path]  [Show diagnostics]
```

**MVP:** implement Agent mode only; on install failure, tell the user to use plain SSH. Don't polish Basic
mode first.

## Per-service design

### File Tree
UI is local; tree data comes from the remote FileSystem Service. **Lazy loading** (never download the whole
tree): open → root entries only → expand a dir → request its children → cache → refresh on watcher events.
Entries carry `ResourceId, name, kind, workspace-relative path, permissions, symlink info, version/token` —
not bare path strings. Considerations: unified `.gitignore`/editor-ignore/user-exclude; symlink + cycle
detection; pagination for huge dirs; keep open-Document identity across rename; re-sync on missed watcher
events; permission errors; coalesce duplicate requests under latency; keep a tree snapshot during
disconnect; the client cache is **not** the source of truth. (File Tree *view* = Bundled Extension; Remote
FileSystem API / Watcher / Ignore-Resource model = Built-in Services.)

### Language Service (LSP)
The server runs remotely (running rust-analyzer locally with only files fetched breaks: missing toolchain,
different build/dependency/`compile_commands.json` paths, sysroot/SDK, container include paths, and constant
file streaming). Runtime owns: LSP process start/stop/restart, stdio transport, workspace-root resolution,
env/toolchain, per-server config, **document-revision validation**, result delivery, crash-loop prevention.
Client owns: completion popup, hover, diagnostics display, code-action picker, symbol UI, command input.
**Don't expose raw LSP protocol to the client UI** — normalize into a **Language Service model** so
Tree-sitter, compiler diagnostics, and a native indexer can merge into the same model:

```
LanguageService: diagnostics · completion · symbols · navigation · hover · rename · code-actions
```

### Debug (GDB / DAP) — location model
Debug has ≥4 actors: Debug UI, Debug Session Coordinator, debugger process, debug target. The debugger and
target locations are **separate** (remote program: gdb on the runtime; embedded board: gdb+OpenOCD on a
build server driving a target board; container: gdb→gdbserver→container).

```
DebugSession:
  ui_location: client
  adapter_location: workspace
  debugger_location: workspace
  target_location: workspace | external
  source_map · executable · symbols · transport
```

Backend choice: **DAP first** (common Debug UI model, many debuggers behind one interface, standardized
breakpoint/stack/variables) with a **GDB/MI native backend added later** for fine control and embedded/
FPGA/firmware scenarios. Don't build both at MVP. (D-032.) Remote-side: executable+symbol access, launch/
attach, run gdb/lldb, PTY, environment, core dump, source-path mapping, port forwarding, target connection,
signals, privilege requests. Client-side: breakpoint/stack/variable/watch/register/memory UI, thread
selection, source navigation, debug console.

### Terminal / PTY
Terminal UI is local; the remote Agent creates a real **PTY** (not a plain stdout pipe): resize, signal
forwarding, shell detection, environment, reconnect policy, exit status, binary-safe stream, escape
handling, clipboard/OSC permission control.

### Search / Git / Build / Test — same rule
UI local; computation remote (ripgrep/index, repository ops, process execution, discovery/run). General rule:
**every computation that must be near the data or toolchain runs in the Workspace Runtime.**

## Capability negotiation

Not every host supports everything; after connect the Agent sends capabilities; the client enables UI or
provides fallbacks accordingly.

```yaml
capabilities:
  filesystem: { watch: true, atomic_rename: true, symlink: true }
  process:    { pty: true, signals: posix, privilege_escalation: false }
  language:   { lsp: true }
  debug:      { dap: true, gdb_mi: false, attach: true, core_dump: true }
  networking: { port_forward: true }
  platform:   { os: linux, arch: x86_64 }
```

Fallbacks: GDB unavailable → disable Debug commands + show install path; watcher unavailable → polling +
degraded status. This is the remote analogue of the terminal capability ledger
([../parity/terminal.md](../parity/terminal.md)); features **degrade, not disappear** (INV-CAP-DEGRADE).

## Toolchain discovery (layered install target)

The Agent is a small base runtime; external tools live on the remote and are **discovered**, not bundled:

```
toolchain discovery: executable path · version · capability · environment · health
```

Missing tools must not fail agent install — partial operation is required:

```
Agent: Ready   Git: Ready   Search: Ready   Rust LSP: Missing   GDB: Missing   PTY: Ready
```

Auto-install policy is layered: **Agent itself** may auto-install; **language servers / debug adapters**
install only after explicit consent; **compilers / gdb / system packages** are user/admin provided (avoid
security/size/policy problems from installing packages on someone's server).

## Protocol shape (per-service, one transport)

Split by service (avoid one giant `RemoteRequest` enum), multiplexed over one transport:

```
control (handshake, capability, health) · resource (read/write/list/watch) · process (spawn/cancel/signal)
· terminal (create/input/resize) · language (completion/diagnostics/navigation) · debug (launch/breakpoint/
continue/variables) · git
```

Versioned + additive ([../protocols/versioning-and-evolution.md](../protocols/versioning-and-evolution.md));
client↔agent negotiate version (no exact-build pinning — D-024).

## Remote MVP scope

For it to be called "remote development":

- **Must:** agent bootstrap · remote filesystem · lazy file tree · read/write · watcher (or explicit
  refresh) · remote search · remote process/task · remote PTY · reconnect + health.
- **Next:** Git · LSP · port forwarding.
- **Then:** DAP/GDB · container/WSL provider · remote plugins · persistent session.

If the product's core target is systems/firmware/embedded developers, GDB may be pulled forward ahead of a
general editor.

## Extensibility

Fixing this "Agent = headless workspace execution runtime" definition early lets **WSL, Docker, Kubernetes,
QEMU, and dev boards** all fit under the same Workspace Runtime model later — SSH is just the first transport.

## Reference Invariants
- **INV-REMOTE-FIRST**, **INV-CAP-DEGRADE**, **INV-ADDITIVE**, **INV-TRUST-1** (see
  [../invariants/reference-invariants.md](../invariants/reference-invariants.md)).

## Alternatives / Rejected / Trade-offs
- **Rejected: Remote as a local plugin (VS Code style).** Discoverability, version coupling, safe-mode, and
  protocol-ownership problems (D-029).
- **Rejected: files-only remote (SSHFS + local tools).** LSP/build/debug break on toolchain/path/sysroot
  mismatch; per-keystroke latency.
- **Rejected: remote downloads its own agent/tools by default.** Fails in air-gapped/proxied nets → client
  upload first.
- **Rejected: one giant request enum / raw LSP passthrough to UI.** Unmaintainable; blocks merging TS/
  compiler/indexer results → per-service protocol + Language Service model.
- **Trade-off:** shipping agent bundles enlarges the local package. Accepted for offline-first; add selective
  download + local cache later.
- **Trade-off:** DAP-first may lose GDB-specific features. Accepted for MVP; add a GDB/MI backend later for
  embedded.
