---
doc: rfc-0006-remote-runtime
project: ruse
title: "RFC-0006: Remote Runtime & Agent"
summary: >
  Remote development in ruse is a built-in capability, not a plugin. A thin local client (UI/input/render)
  talks to a headless Workspace Agent that supervises workspace-local services (filesystem/watch, search,
  Git, LSP, debug, PTY, port-forward, toolchain discovery). The Agent auto-bootstraps over SSH stdio with no
  sudo, offline-first. This RFC records the load-bearing decisions (D-011/D-024/D-029/D-030/D-031/D-032) and
  their rejected alternatives; the full architecture lives in docs/design/remote-runtime.md.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../design/remote-runtime.md
  - ../../parity/remote.md
  - ../../../spec/DECISIONS.md
  - ../../invariants/reference-invariants.md
  - ../../protocols/versioning-and-evolution.md
---

# RFC-0006: Remote Runtime & Agent

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-05
- **Decision link:** D-011, D-024, D-029, D-030, D-031, D-032

> This RFC is a decision record. The full architecture — bootstrap sequence, install layout, per-service
> design, protocol shape, capability schema — is in
> [docs/design/remote-runtime.md](../../design/remote-runtime.md); parity targets are in
> [docs/parity/remote.md](../../parity/remote.md). This document states *what is being decided and why*, and
> pins the alternatives so they are not re-litigated. It does not restate the design in full.

## Summary

Remote development is a **first-class, built-in** part of ruse, not an add-on. The local client is thin
(render/input/UI); a headless **Workspace Agent** runs next to the code and supervises the heavy services —
filesystem + watcher, search/index, Git, LSP, debug, PTY, port-forwarding, toolchain discovery. **UI is
local, execution is remote.** The Agent auto-bootstraps over **SSH stdio** with a per-service multiplexed
protocol, installs under `$HOME` with **no sudo**, and is **offline-first** (the client ships and uploads the
matching agent bundle rather than having the remote reach the internet). Client and Agent **negotiate a
protocol version**; they never assume identical builds. Capabilities are negotiated per host and missing
tools **degrade rather than fail**. Fixing the "Agent = headless workspace-execution runtime" definition now
lets WSL/Docker/Kubernetes/QEMU/boards reuse the same model later — SSH is just the first transport.

## Motivation / Problem

Remote is the classic retrofit failure: if the client/runtime boundary is not present from the start, it
cannot be added cleanly later ([D-011](../../../spec/DECISIONS.md)). Two naive designs fail concretely:

- **"Files are remote."** Mounting the remote FS (SSHFS-style) and running LSP/build/debug locally breaks on
  missing toolchain, mismatched dependency and `compile_commands.json` paths, sysroot/SDK and container
  include paths, and per-keystroke latency. The computation must be *near the data and toolchain*.
- **"Remote is a plugin."** Making the connector an extension (the VS Code shape) makes the feature
  hard to discover, couples connection liveness to plugin-load success, makes remote unusable in
  safe/recovery mode, and hands ownership of the client↔runtime protocol to a third-party extension API.

ruse is TUI-first but **remote-first in architecture**. This RFC locks the boundary, the Agent's role, the
transport, the install/version policy, and the debug model — the parts that are expensive to change later.

## Guide-level explanation

The user does one thing: `editor ssh build-server` (or `editor ssh user@host:/workspace/project`). There is
**no local Remote package to install**. On first connect the client detects OS/arch, checks for a compatible
agent, and if absent **uploads** the version-matched bundle over the existing SSH channel, verifies its
checksum, installs it atomically under `$HOME`, and runs it with `--stdio`. From then on the file tree, LSP,
debugger, terminal, search, and Git all render locally while executing remotely.

What runs where is not implicit — every action has an execution location:

| Concern | Location |
| --- | --- |
| UI, keymap, clipboard, open-browser, status bar, theme, command palette | **client** |
| filesystem + watcher, search, Git, LSP, build/task/test, debugger, PTY | **workspace runtime (remote)** |
| SSH connect + Agent bootstrap | **Built-in Service** (not a plugin) |
| host picker / connection status UI | bundled UI / extension |
| AWS SSM, Kubernetes, special VPN providers | third-party plugin |

If agent execution is forbidden on a host, the client says so and offers a **Basic SSH mode** (open/save,
simple command execution, limited features stated explicitly) or another install path — features degrade with
a clear choice, they do not silently vanish. The MVP implements **Agent mode only**; on install failure it
tells the user to use plain SSH rather than polishing Basic mode first.

## Reference-level explanation

The load-bearing decisions this RFC records:

1. **Built-in, not a plugin (D-029).** The SSH connector and agent bootstrap are Built-in Services. Host
   list/picker/status may be bundled UI; only exotic providers (SSM, K8s, VPN) are third-party plugins.

2. **Agent = headless workspace-execution runtime (D-030).** A *supervisor of workspace-local services*
   (fs/watch, search/index, Git, process/task, PTY, Language Service Host, Debug Service, port-forward,
   toolchain discovery, health/recovery) — **not** a file-transfer daemon. `editor-client` never understands
   remote files directly; `editor-agent` has no rendering/clipboard/input/theme/command UI. Install is
   auto-bootstrapped: **no sudo**, under `$HOME`, versioned side-by-side, never overwrite a running agent,
   upload-to-temp then atomic rename, checksum/signature verified, never executed from a world-writable path.
   **Offline-first**: the client ships bundles for common targets and uploads over SSH (resolution order:
   local cache → client-downloads-from-official → remote direct download → user mirror). Three install
   policies: managed (default), pre-installed (admin `--system`), no-install (basic mode). External tools are
   **discovered, not bundled**; a missing compiler/LSP/GDB must not fail agent install — partial operation is
   required.

3. **Transport: SSH stdio first, persistent socket later (D-031).** Start with SSH stdio + a per-service
   multiplexed **framed** protocol; **no listening ports/tunnels**, auth delegated to SSH, clean lifecycle.
   **stdout is protocol-only** — agent logs go via protocol messages or stderr, never raw stdout (it would
   corrupt framing). Evolve to a persistent socket later for fast reconnect, multiple clients, task survival
   across disconnect, session sharing, and long background indexing.

4. **Client/runtime boundary + typed paths + version negotiation (D-011, D-024).** The boundary is a
   first-class type distinction from the start (local path ≠ workspace path; URI + remote authority, never
   bare OS path strings). Resources carry `ResourceId, kind, workspace-relative path, permissions, version/
   token`, not path strings. The protocol is **split per service** (control · resource · process · terminal
   · language · debug · git · events · logs) over one transport — **not** one giant `RemoteRequest` enum —
   and is **versioned + additive**; client and agent **negotiate a compatible version** and never require
   byte-identical builds ([versioning-and-evolution.md](../../protocols/versioning-and-evolution.md)).

5. **Debug location model; DAP first, GDB/MI later (D-032).** A debug session has ≥4 actors with independent
   locations: `DebugSession{ui_location, adapter_location, debugger_location, target_location, source_map,
   executable, symbols, transport}` — debugger and target locations are **separate** (remote process;
   container → gdbserver; board → gdb+OpenOCD). Backend: a common Debug Service over **DAP first**, adding a
   **GDB/MI** native backend later for embedded/FPGA/firmware. Language results normalize into a **Language
   Service model** (diagnostics · completion · symbols · navigation · hover · rename · code-actions) so
   Tree-sitter, compiler diagnostics, and a native indexer can merge — **no raw-LSP passthrough to the UI**.

6. **Capability negotiation.** After connect the Agent advertises capabilities (filesystem watch/atomic-
   rename/symlink, process pty/signals/privilege, language lsp, debug dap/gdb_mi/attach/core_dump,
   networking port_forward, platform os/arch). The client enables UI or provides fallbacks accordingly
   (GDB missing → disable Debug commands + show install path; watcher missing → polling + degraded status).

For every field-level detail see [docs/design/remote-runtime.md](../../design/remote-runtime.md).

## Reference Invariants

This RFC depends on and enforces (from
[reference-invariants.md](../../invariants/reference-invariants.md)):

- **INV-REMOTE-FIRST** — the client/runtime boundary and typed paths (local path ≠ workspace path) exist from
  the start, not bolted on; client and runtime negotiate versions and never assume identical builds.
- **INV-CAP-DEGRADE** — an unsupported capability degrades (fewer features / fallback), it never disappears;
  capability is a negotiated ledger, not a bare bool inferred from environment.
- **INV-ADDITIVE** — protocol evolution is additive; readers tolerate unknown variants/fields/capabilities;
  breaking changes require a major bump.
- **INV-TRUST-1** — connecting executes remote code at the workspace's trust level; no code runs before a
  workspace-trust decision, remote is a distinct trust principal, and credential/port forwarding is off by
  default.

## Failure modes & Recovery

- **Agent execution forbidden** → offer Basic SSH mode / another install path / diagnostics (never a dead
  end).
- **Missing toolchain (LSP/GDB/compiler)** → agent install still succeeds; the specific feature is marked
  Missing with an install path; unrelated services stay Ready.
- **Watcher unavailable / watch-limit exhausted** → polling fallback + full-rescan reconciliation after gaps.
- **Tunnel loss vs runtime death** — must be distinguished: transient loss auto-reattaches to the live
  runtime (terminals/tasks/LSP state survive once the persistent socket lands); runtime death respawns. The
  client cache is **not** the source of truth; a stale/version-mismatched cache forces clean re-provision.
- **Framing corruption** — prevented by the stdout-is-protocol-only rule; violations are a protocol invariant
  failure, not a recoverable error.

## Security impact

Connecting **executes remote code at the workspace's trust level** — a remote is a code-execution boundary
gated by workspace trust (INV-TRUST-1; [architecture §10]). No sudo; install under `$HOME`; never execute
from a world-writable path; checksum/signature verification before running an uploaded bundle. Per-session
runtime auth. Port forwarding defaults to `localhost` on a random port (prefer a user-locked Unix socket),
never public by default. Credential/agent forwarding is off by default on untrusted hosts. Offline-first
upload keeps the supply chain simple and avoids trusting remote-side downloads. See
[parity/remote.md §REM-SEC](../../parity/remote.md).

## Performance impact

The design exists to *avoid* the SSHFS per-keystroke-latency failure: heavy computation runs next to the
data, and only high-level RPC crosses the wire. Lazy file-tree loading avoids downloading whole trees; large
files/blobs are never stuffed into RPC JSON or re-transferred in full. Requests are coalesced under latency.
The persistent-socket phase adds long-lived background indexing and fast reconnect. Multiplexing keeps a
single transport for all services.

## Compatibility & Migration

Additive, versioned protocol (INV-ADDITIVE): client and agent negotiate a compatible version rather than
pinning to a commit ([D-024](../../../spec/DECISIONS.md)). Side-by-side versioned installs plus auto-prune
mean a client upgrade does not strand older agents; `editor remote {status,reinstall,logs,remove,prune}`
manage the lifecycle. The exact supported skew range and downgrade behavior are open (D-024). Because the
Agent role is fixed now, WSL/Docker/K8s/QEMU/boards can be added later as new transports/providers under the
same Workspace Runtime model without reworking the boundary.

## Observability

Agent logs travel via protocol log messages or stderr (never stdout). Per-host status is inspectable
(`editor remote status/logs`). Toolchain discovery reports each tool's path/version/capability/health, and
the connect surface shows per-service readiness (e.g. `Agent: Ready  Git: Ready  Rust LSP: Missing  GDB:
Missing  PTY: Ready`). Capability negotiation output is the ledger the UI subscribes to.

## Alternatives

- **Remote as a local plugin (VS Code Remote shape).** Considered and rejected — see below.
- **Files-only remote (SSHFS + local tools).** Considered and rejected — see below.
- **Remote self-downloads its agent/tools.** Considered as the default; rejected for air-gapped/proxied
  networks — kept only as a fallback step in resolution order.
- **One giant `RemoteRequest` enum / raw-LSP passthrough to the UI.** Considered for simplicity; rejected —
  see below.
- **Persistent socket / listening port from day one.** Deferred, not rejected: SSH stdio ships first;
  the socket is the planned evolution (D-031).
- **GDB/MI backend at MVP.** Deferred; DAP first, GDB/MI later (D-032), pulled forward only if the core
  audience is systems/firmware/embedded.

## Rejected approaches

- **Remote as a local plugin (VS Code style).** Hurts discoverability, couples connection liveness to
  plugin-load success, breaks safe/recovery mode, and makes the client↔runtime protocol hostage to a
  third-party extension API. The SSH connector is a **Built-in Service** instead (D-029).
- **Files-only remote (SSHFS + local tools).** LSP/build/debug break on toolchain/dependency-path/sysroot/
  container-include mismatch, plus per-keystroke latency. Execution must be remote (D-030).
- **Remote downloads its own agent/tools by default.** Fails in air-gapped/proxied networks and requires
  curl/wget and exact-version luck. The client **uploads** the version-matched bundle over SSH instead
  (offline-first, D-030).
- **One giant `RemoteRequest` enum / raw-LSP passthrough to UI.** Unmaintainable and blocks merging
  Tree-sitter + compiler + native-indexer results into one model. Replaced by a **per-service** protocol and
  a normalized **Language Service model** (D-032).

## Trade-offs

- Shipping agent bundles enlarges the local package. **Accepted** for offline-first; selective download +
  local cache to be added later (D-030 re-evaluation).
- DAP-first may miss GDB-specific control for embedded/firmware. **Accepted** for MVP; a GDB/MI backend
  comes later (D-032).
- SSH-stdio forgoes reconnect/multi-client/task-survival initially. **Accepted**; the persistent socket is
  the planned next phase (D-031).

## Re-evaluation conditions

- **D-029/D-030/D-031** are decided; the SSH path is not expected to reopen. The persistent-socket evolution
  triggers when reconnect / multi-client / background-indexing demand it (D-031); selective-download +
  cache when the offline-first path is proven (D-030).
- **D-032** (backend detail) reopens before F-021 (debugger) — specifically whether GDB/MI is pulled forward
  for a systems/firmware audience.
- **D-024** (version-skew range/downgrade) and **D-012/D-013** (multi-client, offline/reconnect editing)
  must close before F-017 hardening.

## Open questions

- **D-012 — Multiple clients per workspace.** Whether v1.x enables multi-client at all, and optimistic vs
  authoritative sequencing (cursor/viewport/mode are already client/view-local by design).
- **D-013 — Offline / reconnect editing policy.** On disconnect: continue editing, go read-only, or local
  journal + conflict resolution.
- **D-024 — Version-skew policy.** The supported skew range and downgrade behavior.
- **D-032 — Debug backend.** Whether GDB/MI is pulled forward ahead of a general DAP editor.
