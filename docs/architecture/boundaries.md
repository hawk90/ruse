---
doc: architecture-boundaries
project: ruse
title: "ruse Boundaries — State, Authority, Process, Trust, Restart"
summary: >
  One lookup table of the boundaries the other documents describe piecemeal: who owns each piece of
  state, who may change it, which external processes ruse runs and behind which trust gate, and which
  state survives a restart. Describes the code as it is; each row cites the file or D-*/INV-* record it
  is read from, and anything not settled is marked unknown.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - architecture.md
  - ../invariants/reference-invariants.md
  - ../design/buffer-arena-and-global-state.md
  - ../design/persistence-and-recovery.md
  - ../design/config-model.md
  - ../design/lsp.md
---

# ruse Boundaries

This page is a digest. The normative sources are the invariants
([reference-invariants.md](../invariants/reference-invariants.md)), the decisions
([spec/DECISIONS.md](../../spec/DECISIONS.md)) and the code; when this page disagrees with them, they win
and this page is the bug. Status words: **yes** / **no** = observed in the code at the cited path;
**unknown** = not settled by code or a decision.

## 1. State ownership

| State | Owner (type) | Scope | Code | Refs |
|---|---|---|---|---|
| Text, revision, undo tree, anchor store | `Document` | per buffer | `crates/core/src/document.rs` | D-003, INV-DOC-VIEW, INV-TXN, INV-UNDO, INV-ANCHOR |
| Buffer arena: `Document`s and `View`s by id, `buffer_order`, alternate buffer `#` | `Workspace` | editor | `crates/core/src/workspace/mod.rs` | D-057, INV-HANDLE |
| Registers (unnamed, `a`-`z`, numbered ring, `"#`); macros live in them | `Workspace` (lent to the focused `View` per command) | editor | `crates/core/src/workspace/mod.rs` (`registers`, `lend`), `crates/core/src/workspace/registers.rs` | D-026, D-055, D-057 |
| `"+` / `"*` clipboard | the OS, reached through an injected provider | outside ruse | `crates/core/src/clipboard.rs` (seam), `apps/tui/src/clipboard.rs` (shell-out provider) | — |
| Cursor, mode, scroll (`top`, `leftcol`, `skipcol`), window-local options | `View` | per window | `crates/core/src/editor/mod.rs` (`View`) | D-003, INV-DOC-VIEW |
| Marks `a`-`z`, `A`-`Z`, `0`-`9`, jumplist, change list | `View` | per window (uppercase marks kept with their buffer) | `crates/core/src/editor/mod.rs` (`named_marks`, `jumps`, `changes`) | D-027, D-057 (cross-buffer global marks: F-032, not done) |
| Command-line and search history rings | input engine | session | `apps/tui/src/input/history.rs` | — |
| Folds | frontend | per window | `apps/tui/src/folds.rs`, `apps/tui/src/app/fold_cmds.rs` | AGENTS.md "Folds are frontend-only state" |
| Terminal (PTY) grid | frontend `Terminal` | per terminal | `apps/tui/src/term_buffer.rs`, `apps/tui/src/term_grid.rs` | INV-BUFFER-KIND |
| File shape (`FileFormat`: line ending, `'bomb'`, `'endofline'`, `'endoffile'`, `'binary'`, encoding) | frontend `Files` registry (`BufferFile.fmt`; `WriteOptions.nofile_fmt` for a buffer without a file); `Document` holds only the decoded text | per buffer | `apps/tui/src/persist/encoding.rs` (`FileFormat`), `apps/tui/src/app/dispatch/files.rs` (`BufferFile`), `apps/tui/src/app/write_opts.rs` (`nofile_fmt`) | F-008, D-066 |
| Encoding codecs and name table (`FileEncoding`); the only user of `encoding_rs` | frontend persist boundary; always the default (UTF-8, invalid bytes kept) until `'fileencoding'` lands | read / write / journal | `apps/tui/src/persist/codec.rs` | D-066, RFC-0025, `DEP-ENCODING` |
| LSP diagnostics | frontend LSP coordinator | per buffer | `apps/tui/src/lsp/model.rs`, `apps/tui/src/app/lsp_coordinator.rs` | F-014 |
| Effective config (layers merged) | frontend config loader | session | `apps/tui/src/config/` | D-062, `spec/config-schema.yaml` |
| Workspace trust decision | frontend (`LspCoordinator.trusted`, config `TrustStore`) | session / state dir | `apps/tui/src/trust.rs`, `apps/tui/src/app/lsp_coordinator.rs` | D-058, D-062, INV-TRUST-1 |

`EditorState` is not an owner: it is the transient `(Document, View)` pair plus the lent register store
for one command, built by `Workspace::lend` and taken apart again (`crates/core/src/workspace/mod.rs`).

## 2. Mutation authority

| Actor | May produce | May mutate a `Document`? | Path | Refs |
|---|---|---|---|---|
| Input engines (`vim`, `emacs`, `native`, `cmdline`, `repeat`) | semantic `Command`s | no — commands only | `apps/tui/src/input/`, `crates/core/src/command/mod.rs` (`Command`) | INV-CMD-SEMANTIC, D-006 |
| Editor core (command execution) | `Transaction`s | yes, only through `Document::apply` (stale `base_revision` refused) | `crates/core/src/document.rs` (`apply`), `crates/core/src/transaction.rs` | D-001, INV-TXN, INV-ORIGIN |
| Ex commands | resolved semantic commands (`ExCmd`) | through the same transaction path | `crates/core/src/editor/ex_cmd/` | D-061 |
| LSP coordinator (rename, code action, format) | `ExCmd::Edits` with `TransactionOrigin::Lsp` | yes, through a transaction | `apps/tui/src/app/lsp_coordinator.rs` | INV-ORIGIN, INV-TRUST-1 |
| File re-read from disk (`'autoread'`) | a transaction with its own origin | yes, through a transaction | `crates/core/src/transaction.rs` (`TransactionOrigin`) | INV-ORIGIN |
| LSP / PTY raw I/O | status, diagnostics, grid updates | no; not recorded as `Command`s; `--replay` ignores it | `apps/tui/src/lsp/mod.rs`, `apps/tui/src/term_buffer.rs` (module docs) | F-022 |
| Terminal (PTY) | grid updates only | no; the grid lives outside any `Document`, so the INV-BUFFER-KIND exemption is never exercised (there is no buffer-kind type) | `apps/tui/src/term_buffer.rs` | INV-BUFFER-KIND (not yet enforced) |
| Plugins | — | no plugin host exists | — | D-004 (deferred by D-039), D-009, INV-PLUGIN-NO-CORE |
| Frontend | view-only state (scroll, folds, rendering) | no; `Document` never depends on `View` | `apps/tui/src/` | INV-DOC-VIEW |

`TransactionOrigin` (`crates/core/src/transaction.rs`) has `UserInput`, `Macro`, `Plugin`, `Lsp`, `AiAgent`,
`RemotePeer` and a disk re-read origin; `Plugin`, `AiAgent` and `RemotePeer` have no producer today.

## 3. External processes

| Process | Spawned by | When | Trust gate | Runs workspace code? | Code | Refs |
|---|---|---|---|---|---|---|
| Language server (rust-analyzer, pyright, …) | `LspCoordinator` | focusing a buffer whose extension maps to a server | yes: none spawned while the workspace is untrusted | yes (build scripts, proc-macros, project config) | `apps/tui/src/lsp/mod.rs` (`server_for_ext`), `apps/tui/src/app/lsp_coordinator.rs` | D-058, INV-TRUST-1 |
| Shell in a terminal buffer (PTY) | the user (`:terminal`) | on request | no gate: user principal | the user's shell | `apps/tui/src/pty.rs` (Unix `forkpty`) | F-011, INV-BUFFER-KIND |
| `sh -c` for `:!`, `:r !`, `:{range}!` | the user's typed Ex command | on request, blocking | no gate: user principal | what the user typed | `apps/tui/src/shell.rs` | — |
| `editor.formatprg` / `editor.equalprg` filter | `gq` / `=` | when the option is set | key is `security_locked`, `scope: user`: a workspace layer cannot set it | the configured program | `spec/config-schema.yaml` | INV-TRUST-1 |
| Clipboard tool (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip`) | the `"+`/`"*` provider | on clipboard read/write | none | no | `apps/tui/src/clipboard.rs` | — |
| Remote agent (`ruse agent`, locally or `ssh <host> ruse agent`) | `ruse ssh [host]` CLI only | on request; handshake and print, not wired into an editing session | none implemented: `remote.trust_prompt` exists only in the schema | serves `fs.readFile`, `fs.writeFile`, `fs.stat`, `fs.list`; no path confinement in `agent.rs` | `apps/tui/src/remote/`, `apps/tui/src/app/cli.rs` | D-029, D-030, D-031, D-039 |

## 4. Trust principals

| Principal | Can | Cannot | Refs |
|---|---|---|---|
| User | grant trust (`:trust`, `workspace.trusted_roots`, `RUSE_TRUSTED_WORKSPACES`); set every config key in the user layer; run shell commands | — | D-058, D-062 |
| Machine config (`/etc/ruse/config.toml`) | pin keys above the user | — | `apps/tui/src/config/paths.rs` |
| Workspace repository (`<root>/.ruse/config.toml`) | set `scope: workspace` keys, once trusted and while its `.ruse/` hash matches the trust record | set `keymap.*`, `security_locked` keys, or any key before trust; start a language server by itself | D-062, `apps/tui/src/config/load.rs`, `apps/tui/src/trust.rs` |
| Language server | edit documents through `TransactionOrigin::Lsp` transactions | run before the workspace is trusted | D-058 |
| Remote agent | answer fs requests | unknown: no remote trust posture is implemented | D-030, `spec/config-schema.yaml` (`remote.trust_prompt`) |
| Plugin, AI agent | — (no host exists) | — | D-039, INV-PLUGIN-NO-CORE, INV-TRUST-1 |
| Terminal output | draw into the terminal grid | mutate a `Document` | `apps/tui/src/term_buffer.rs`, INV-TRUST-1 |

## 5. What survives a restart

State directory = `${XDG_STATE_HOME:-~/.local/state}/ruse/`. Without `XDG_STATE_HOME` or `HOME` there is no
state directory and unsaved work is not journaled (`apps/tui/src/health.rs`).

| State | Survives? | Where | Code | Refs |
|---|---|---|---|---|
| Saved file text | yes | the file (atomic temp + rename) | `apps/tui/src/persist/atomic.rs` | CONTRACT-PERSISTENCE |
| Unsaved edits | yes, as an offer: the recovery prompt restores the disk text plus one undoable change | `<state>/journal/<basename>.<path hash>.rj` (RJ01) | `apps/tui/src/persist/journal.rs`, `apps/tui/src/persist/rj01.rs` | D-064, `spec/contracts/persistence-format.yaml` |
| File shape of a journaled buffer (`FileFormat`, encoding since RJ01 1.2) | yes, in the journal header and file-format meta records; a record naming an encoding this build lacks is refused | `<state>/journal/` | `apps/tui/src/persist/rj01.rs` (`decode_encoding`) | D-064, D-066 |
| Discarded journals | yes, archived 14 days / 20 per path | `<state>/journal/archive/` | `apps/tui/src/persist/retention.rs` | D-064 |
| Legacy `RJS0` sidecar (`<file>.ruse-journal`) | read once, then archived; never written | next to the file | `apps/tui/src/persist/rjs0.rs` | D-064 |
| Undo history | no (no undofile; recovery does not restore the pre-crash undo tree) | — | — | D-064 ("Re-evaluate if: persistent undo") |
| Backups | only with `files.backup` (default off); `files.writebackup` (default on) removes its backup after a successful write | `files.backupdir`, default `.,$XDG_STATE_HOME/ruse/backup//` | `apps/tui/src/persist/backup.rs` | `spec/config-schema.yaml` |
| Config | yes (user-authored files) | `/etc/ruse/config.toml`, `${XDG_CONFIG_HOME:-~/.config}/ruse/config.toml`, `<root>/.ruse/config.toml` | `apps/tui/src/config/paths.rs` | D-062 |
| Config trust record (`.ruse/` hash) | yes, until `:untrust` | `<state>/trust/<sha256 of root>` | `apps/tui/src/trust.rs` (`TrustStore`) | D-062 |
| Language-server trust | only via `workspace.trusted_roots` or `RUSE_TRUSTED_WORKSPACES`; a `:trust` grant is not re-read for the LSP gate at the next start | config / environment | `apps/tui/src/app/session/event_loop.rs` (startup trust), `apps/tui/src/trust.rs` (`workspace_trusted`) | D-058 |
| Crash-loop counter (safe mode) | yes, until the first frame or `:config-reload` clears it | `<state>/config-boot` | `apps/tui/src/config/boot.rs` | CFG-SAFE-001 |
| Registers and macros | no (in memory) | — | `crates/core/src/workspace/mod.rs` | D-057 ("Re-evaluate if: session persistence lands") |
| Marks (all, including `A`-`Z` and `0`-`9`) | no; numbered marks are not filled from a shada file | — | `crates/core/src/editor/mod.rs` (`named_marks`) | D-057 |
| Jumplist, change list | no | — | `crates/core/src/editor/mod.rs` | D-027 |
| Command-line and search history | no (in-memory rings) | — | `apps/tui/src/input/history.rs` | PERSIST-008 (todo) |
| Options set with `:set` | no; only config-file values return | — | `apps/tui/src/app/set_exec.rs` | — |
| Folds, window layout, buffer list, session | no | — | — | PERSIST-008 (todo: no mksession/shada equivalent) |
| Command trace | only when the user runs `:savetrace <path>` (includes register and clipboard text) | the given path | `apps/tui/src/app/session/ex_exec/files.rs` | F-022, D-061 |
| Log | only when `RUSE_LOG=<file>` is set | that file | `apps/tui/src/log.rs` | D-017 (PII policy open) |
| `"+` / `"*` clipboard | yes, owned by the OS | OS clipboard | `apps/tui/src/clipboard.rs` | — |

## 6. Unknown or not decided

- Remote runtime trust posture, path confinement of the agent's fs services, and version-skew policy
  (D-024 open; `remote.trust_prompt` has no consumer).
- Whether `:trust` should persist the language-server grant (D-058 "Re-evaluate if").
- Serialized format for registers, marks and history if session persistence lands (D-057, PERSIST-008).
- Persistent undo and its effect on journal compaction (D-064).
- Plugin and AI-agent principals: designed (INV-PLUGIN-NO-CORE, INV-TRUST-1) but no host exists (D-039).
- Multiple clients per workspace and the register/mark client-scope split (D-012 open).
- Log PII redaction (D-017 open).
