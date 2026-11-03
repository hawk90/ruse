---
doc: rfc
project: ruse
title: "RFC-0021: The workspace config boundary (keymaps, .ruse/ ownership, content-bound trust)"
summary: >
  Three linked rules for what a workspace (a repository) may contribute to ruse and how that is trusted.
  (1) A workspace never adds or changes active key bindings: `keymap.*` stay `scope: user`; a project
  contributes commands and may declare suggested bindings that act only after the user opts in.
  (2) `.ruse/` at a workspace root belongs to the product and holds only committed project configuration;
  ruse never writes local or per-user state into a user's repository, so the ruse repo's own development
  tooling moves out of `.ruse/`. (3) Workspace trust is content-bound: the user-side trust record holds a
  hash of the workspace's `.ruse/` contents, and a change after the grant makes the workspace config stale
  and unloaded until the user re-confirms.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/DECISIONS.md
  - ../../design/config-model.md
  - ../../../spec/config-schema.yaml
  - ../../operations/change-workflow.md
  - ../../architecture/architecture.md
---

# RFC-0021: The workspace config boundary (keymaps, .ruse/ ownership, content-bound trust)

- **Status:** proposed
- **Author(s):** Claude Opus 5.5 (design agent), recording the maintainer's decision
- **Created:** 2026-11-03
- **Decision link:** D-062
- **Issues:** #1026 (workspace keymaps), #987 (`.ruse/` ownership); tooling follow-up #1137

<!-- Hard-to-reverse: the workspace directory name and what it may contain become a public, on-disk contract
     the day a config loader ships, and the workspace's power over key bindings is a trust boundary
     (INV-TRUST-1). Loosening either later breaks users' expectations of what opening a repo can do. -->

## Summary

C-CONFIG (`docs/design/config-model.md`) defines a workspace layer at `.ruse/` in the workspace root, inert
until trusted. Three questions about that layer were open or in conflict:

1. May a trusted workspace bind keys? `config-model.md` said yes (a workspace provenance tier), and
   `spec/config-schema.yaml` said no (`keymap.*` are `scope: user`) (#1026).
2. Who owns `.ruse/`? The product's workspace directory and the ruse repository's own development tooling
   (`.ruse/work/`, `.ruse/cache/`) used the same name, and the tooling writes per-user, uncommitted state
   there (#987).
3. Once a user trusts a workspace, does a later change to its config inherit that trust?

This RFC answers all three. The compact record is D-062.

## Motivation / Problem

**Keymaps.** A key binding invokes a command. A workspace that can rebind common keys (`dd`, `:w`, `C-x C-s`)
can make a user run a command they did not mean to run, with their own authority: a confused-deputy risk.
Workspace trust (D-058) means "this repo's tools may run", not "this repo may rewire my keys". Keys are
muscle memory and are per-profile (INV-PROFILE-ISOLATION): a project cannot know whether its user types Vim
or Emacs keys, so any binding it ships is wrong for some of its users.

**`.ruse/`.** Once ruse ships a config loader, any repository may contain `.ruse/config.toml`, and that file
is committed project configuration. The ruse repo itself writes per-change contracts, evidence and an
upstream-source cache to `.ruse/` and gitignores the whole directory. Product and tooling would then share a
name with opposite rules (committed versus never committed), and the ruse repo could not dogfood its own
project config.

**Trust.** A root-only trust record (`workspace.trusted_roots`, D-058) trusts whatever the workspace contains
later. A `git pull`, a branch switch or a merged pull request can change `.ruse/config.toml` after the user
looked at it, and the new content would load with the old grant. Helix met the same problem and bound trust
to content (see References).

## Guide-level explanation

- A project can ship commands in `.ruse/` (project commands are #647) and can say "we suggest binding
  `<leader>t` to `project.test`". That suggestion shows in which-key and the command palette as a hint. It
  does nothing until the user accepts it, and an accepted binding is stored in the user's own config.
- `.ruse/` in any repository holds only files a project commits. ruse never creates or writes files there.
- After `:trust`, or with the root in `workspace.trusted_roots`, ruse remembers what `.ruse/` contained. If it
  changes (new commit, other branch), ruse shows the workspace config as **stale**, does not load it, and asks
  the user to look and re-confirm.

## Reference-level explanation

### R1. A workspace adds no active bindings

- `keymap.*` keys stay `scope: user` in `spec/config-schema.yaml`. A workspace layer that sets one is inert
  for that key and the entry is dropped with a diagnostic (CFG-LAYER-001), even when the workspace is trusted.
- Projects contribute **commands**, which are trust-gated and capability-gated like any workspace code
  (INV-TRUST-1; project commands are #647).
- A project may declare **suggested bindings**. They enter the existing plugin-suggested, opt-in provenance
  tier (architecture §1.4, tier 7). They appear only as hints in which-key and the palette; they take effect
  only when the user opts in, and the accepted binding is written to the user layer, with its provenance.
- The workspace provenance tier (architecture §1.4, tier 4) therefore holds no active bindings.
- Relaxing this (letting a workspace bind keys) needs a new `D-*`.

### R2. `.ruse/` belongs to the product

- `.ruse/` at a workspace root is the product's project-configuration directory (for example
  `.ruse/config.toml`). It holds only files meant to be committed.
- ruse (the editor) never writes local or per-user state into a user's repository: no caches, sessions,
  trust records, undo files or swap files under `.ruse/` or elsewhere in the repo. Such state goes to the
  user's state or cache directories.
- The ruse repository's development tooling stops using `.ruse/`:
  - per-change contracts (`change.yaml`, `plan.md`, `evidence.json`, …) go to
    `$(git rev-parse --git-path ruse/work)/<id>/`, which is inside `.git`, per worktree, and never committed;
  - the upstream-source cache (`ruse parity fetch`) goes to `${XDG_CACHE_HOME:-~/.cache}/ruse-dev/`, shared
    across worktrees.
- The tooling migration (tools, `.gitignore`) is a separate change, tracked in #1137. Until it lands the
  tools still write the old paths.

### R3. Workspace trust is content-bound

- Trust is stored on the user side, never in the repository (R2).
- A trust record holds the trusted root and a hash of the workspace's `.ruse/` contents at the time of the
  grant. An absent `.ruse/` hashes as empty, so adding one later also counts as a change.
- On load, ruse recomputes the hash. If it differs, the workspace config is **stale**: it is treated as
  untrusted, not loaded (inert, shown in `:inspect config` as `stale`), and the user is told. Re-confirming
  (for example `:trust` again) records the new hash.
- This binds the workspace **config layer**. The D-058 language-server gate stays root-bound: a language
  server's risk is the workspace's code, not its `.ruse/` contents, and the executables it runs are
  `security_locked` keys a workspace can never set.
- Where the trust record lives on the user side, the hash algorithm and the exact set of hashed files are
  implementation choices for the config loader (C-CONFIG); a collision-resistant hash is required.

## Reference Invariants

- **INV-TRUST-1** (depends) — R1 keeps bindings out of the workspace principal's reach; R3 makes a trust
  decision apply only to the content it was made about.
- **INV-PROFILE-ISOLATION** (depends) — bindings are per-profile; only the user knows their profile.
- **INV-PRIORITY** (depends) — suggested bindings use the existing opt-in tier; no new tier is added.
- **INV-FAIL-BOUNDED** (depends) — a stale workspace degrades to an inert layer; startup continues.

## Failure modes & Recovery

- **Workspace sets `keymap.*`** → entry dropped, diagnostic; user and profile bindings unaffected.
- **`.ruse/` changed after trust** → layer stale and inert; the status line and `:inspect config` say why;
  the user re-confirms to load it.
- **Trust record unreadable** → the workspace is untrusted (default-deny), never trusted.

## Security impact

Positive. A repository can no longer change what a user's keys do, and a trust grant can no longer be
carried over to config the user never saw. No per-user state is written into repositories, so nothing a
user's editor records can be committed by accident.

## Performance impact

Hashing `.ruse/` at startup and on reload. The directory is small (a few config files); the cost is bounded
by its size and is not on any per-keystroke path.

## Compatibility & Migration

No config loader has shipped, so no user config or workspace changes meaning. The ruse repo's contributors
lose nothing: #1137 moves the tooling paths; existing local `.ruse/work/` contents can be moved by hand or
discarded (they are per-change scratch).

## Observability

`:inspect config` shows a workspace layer as `untrusted`, `stale` or trusted, and names dropped `keymap.*`
entries. Suggested bindings show their source project in which-key and the palette.

## Alternatives

1. **A trusted workspace may bind keys at the workspace tier** (the earlier config-model text). Rejected:
   confused-deputy risk, and trust is about running tools, not about input.
2. **A workspace may bind keys only inside its own views or for its own commands.** Not rejected forever, but
   deferred: it needs project commands (#647) and views to exist first; it would need a new `D-*`.
3. **Keep `.ruse/` shared with dev tooling under subdirectories.** Rejected: one directory would hold both
   committed and never-committed files, and per-user state in a repo is what R2 forbids.
4. **Root-only trust (D-058 as written).** Rejected for the config layer: a later commit inherits trust.

## Rejected approaches

Workspace-scoped active keymaps (Alternative 1). Precedent agrees: VS Code keybindings are application-scoped
(user file only, no workspace keybindings), and Zed limits project settings to editor behaviour and language
tooling and keeps `vim_mode` user-only.

## Trade-offs

- Projects cannot ship a working key layout; their users must opt in once. Accepted.
- Every change to `.ruse/` asks for re-confirmation, including harmless ones. Accepted: the file is small and
  changes rarely, and a silent grant is the failure being prevented.

## Re-evaluation conditions

- Project commands and project views exist and users ask for view-local project bindings (Alternative 2).
- The re-confirmation prompt proves noisy in practice (consider per-file or per-key granularity).
- A workspace starts contributing executable content outside `.ruse/` that the config loader reads (it must
  be hashed too).

## References

- Helix workspace trust documentation (workspace config is loaded only from trusted workspaces, with trust
  bound to the config content), and helix-editor/helix issue #9514 ("Stop silently executing arbitrary
  code from the current directory by default"), which motivated it.
- VS Code: keybindings are user-level only (`keybindings.json`); there is no workspace keybindings file.
- Zed: project settings (`.zed/settings.json`) cover editor behaviour and language tooling; `vim_mode` is a
  user setting.
- Helix `.helix/`, Zed `.zed/`: project directories that hold committed project configuration.

## Open questions

1. Where the user-side trust record lives once the config loader exists (`workspace.trusted_roots` gains a
   hash per root, or a separate user state file keyed by root).
2. Whether re-confirmation should show a diff of `.ruse/` since the last grant.
