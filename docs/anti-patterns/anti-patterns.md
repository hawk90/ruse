---
doc: anti-patterns
project: ruse
title: "ruse Design Anti-Patterns Catalog"
summary: >
  A categorized catalog of design anti-patterns to avoid when building ruse (a Rust,
  TUI-first editor with Vim/Neovim/Emacs parity). Each item has a stable ID for
  cross-referencing from design docs, PRDs, and code review. Use as a design-review
  checklist and as lint/gate seeds.
audience: [maintainers, contributors, llm-agents, reviewers]
status: draft
related:
  - ../architecture/architecture.md
  - ../parity/README.md
usage: >
  IDs are <CATEGORY>-<n>. Reference them in PRDs and PRs (e.g. "guards against CORE-2").
  The 15 highest-leverage items to fix before writing core code are listed in §Critical-15.
---

# ruse Design Anti-Patterns Catalog

> This editor couples an editing core, input modes, plugins, remote, and terminal rendering.
> A wrong early decision here is nearly impossible to unwind later. Establishing this catalog
> up front is worth it. Do **not** try to solve all ~300 items at once — lock the five boundaries
> first (see [architecture.md §12.1](../architecture/architecture.md)); most of the rest follow.

Legend: each category has a short code; items are numbered for stable reference (e.g. `CORE-3`).

---

## CORE — Core Architecture

1. [P1] Piling all state into a single `EditorState`.
2. [P0] Sharing `Arc<Mutex<EditorState>>` globally.
3. [P1] Bypassing the borrow checker with `Rc<RefCell<T>>`.
4. [P0] Mixing document, view, window, and cursor into one object.
5. [P1] Treating file and document as the same concept.
6. [P1] Letting the UI mutate document state directly.
7. [P1] Letting plugins reference core objects directly.
8. [P1] Making the core directly aware of TUI, LSP, Git, plugins.
9. [P2] Overusing global singleton services.
10. [P1] Creating crate cycles with no clear dependency direction.
11. [P2] Splitting modules only by technical layer, not by feature.
12. [P2] Over-splitting into too many crates from the start.
13. [P2] Conversely, dumping all code into one crate.
14. [P1] Storing long-lived references instead of stable IDs.
15. [P0] Exposing internal data structures as the public API.
16. [P1] Not defining mutation boundaries.
17. [P1] Applying async results with no revision concept.
18. [P1] Coupling core and frontend lifecycles.
19. [P0] Bolting on local vs remote execution models later.
20. [P1] Implementing the UI before a headless core exists.

## TEXT — Text Engine

1. [P0] Representing all coordinates as `usize`.
2. [P0] Mixing byte, char, grapheme, and UTF-16 column.
3. [P2] Storing only line/column and ignoring byte-offset conversion cost.
4. [P0] Storing raw offsets as long-lived positions.
5. [P1] Not accounting for anchor affinity.
6. [P2] Linearly updating every anchor per edit.
7. [P1] Designing the text buffer only for small files.
8. [P1] Exposing rope/piece-table implementation details across all layers.
9. [P0] Scattering text changes as direct insert/delete calls.
10. [P0] Bolting undo on without transactions.
11. [P1] Managing inverse-edit generation rules inconsistently.
12. [P2] Recording undo per keystroke.
13. [P1] Treating undo group and transaction as the same concept.
14. [P1] Not normalizing multiline edits.
15. [P1] Allowing overlapping edits with implicit application order.
16. [P1] Letting a background parser read the live buffer without a snapshot.
17. [P2] Not considering a large-file mode.
18. [P2] Forcing binary files through the text buffer.
19. [P2] Mixing line endings and encoding into document data.
20. [P2] Having no policy for invalid UTF-8.

## VIM — Vim Input Model

1. [P0] Implementing Vim modes as a mere keymap preset.
2. [P1] Scattering `if mode == Normal` conditions throughout the code.
3. [P2] Registering all operator×motion combinations as individual commands.
4. [P2] Implementing `dw`, `de`, `d$` each separately.
5. [P1] Collapsing text object and motion into the same type.
6. [P1] Ignoring the inclusive/exclusive motion distinction.
7. [P1] Bolting on linewise/characterwise/blockwise later.
8. [P2] Handling count in a different place per command.
9. [P1] Baking register handling directly into delete/yank implementations.
10. [P1] Implementing macros as raw key replay only.
11. [P2] Not distinguishing dot-repeat from transaction replay.
12. [P2] Treating operator-pending as a special case rather than a transient state.
13. [P2] Treating command-line mode as just an ordinary text buffer.
14. [P2] Not stating the level of Vim compatibility.
15. [P2] Using the same command name as Neovim while behaving differently.

## EMACS — Emacs Input Model

1. [P0] Implementing Emacs mode only at the `Ctrl+A`/`Ctrl+E` level.
2. [P1] Treating prefix keys as merely consecutive key combos.
3. [P2] Special-casing the universal argument per command.
4. [P2] Treating mark and selection as entirely identical.
5. [P2] Simplifying the kill ring to plain clipboard history.
6. [P1] Ignoring major/minor mode keymap layering.
7. [P2] Not distinguishing transient map from prefix map.
8. [P2] Treating `M-x` as just a command-palette alias.
9. [P2] Mixing the scope of buffer-local variables and mode-local settings.
10. [P2] Cloning keys without Emacs command semantics.

## PROFILE — Profile & Keymap

1. [P1] Registering Vim, Emacs, and Native keys into the same global map.
2. [P1] Activating bindings unrelated to the current profile.
3. [P2] Letting the last-loaded plugin override keys.
4. [P2] Detecting conflicts by key string only.
5. [P2] Not checking context overlap.
6. [P2] Leaving priority rules implicit.
7. [P2] Letting plugins force global keys.
8. [P2] Letting plugin bindings win over user overrides.
9. [P2] Leaking special-view shortcuts into the global scope.
10. [P1] Not clearly separating terminal passthrough from editor commands.
11. [P1] Interpreting shortcuts during IME input.
12. [P2] Leaving the timeout rule for the same key combo unclear per profile.
13. [P3] Having no which-key / prefix discovery.
14. [P0] Having no keymap versioning.
15. [P2] Building Native Style as only a Vim+Emacs key mix.

## CMD — Command System

1. [P0] Coupling command implementation with key bindings.
2. [P1] Coupling command implementation with command-line strings.
3. [P1] Making `<Plug>`, Lua functions, command, and RPC separate entry points.
4. [P2] Creating multiple redundant semantic commands per feature.
5. [P2] Overusing string-based command dispatch.
6. [P0] Changing command IDs easily.
7. [P2] Using command IDs without a namespace.
8. [P2] Passing arguments as `Vec<String>` with no typed arguments.
9. [P2] Checking command availability only at execution time.
10. [P2] Exposing all commands in the palette regardless of context.
11. [P1] Not declaring command side effects in the manifest.
12. [P1] Not distinguishing local/client/remote execution location.
13. [P1] Not distinguishing undoable and non-undoable commands.
14. [P1] Modifying the UI directly during command execution.
15. [P1] Letting a command mutate arbitrary global state.
16. [P1] Leaving partial mutation after a command failure.
17. [P2] Equating command ID with the user-facing display name.
18. [P2] Having no deprecated-command alias policy.
19. [P2] Making macros depend on raw keys rather than commands.
20. [P2] Managing docs and autocompletion manually with no command metadata.

## PLUGIN — Plugin Architecture

1. [P0] Using the Rust dynamic-library ABI as the official plugin ABI.
2. [P0] Handing plugins `&mut EditorState`.
3. [P0] Letting plugins use internal types like Rope or View directly.
4. [P1] Loading plugins into the main process with no isolation.
5. [P1] Letting a plugin panic terminate the whole editor.
6. [P1] Running all plugins with the same permissions.
7. [P1] Allowing filesystem/network/process permission by default.
8. [P0] Letting plugins print escape sequences to stdout directly.
9. [P1] Letting plugins modify TUI cells directly.
10. [P2] Defining plugin UI only as HTML/WebView.
11. [P0] Making authors write separate TUI and GUI plugins.
12. [P2] Registering commands only dynamically in activate code.
13. [P1] Making features knowable only after runtime execution, with no manifest.
14. [P1] Having no version negotiation in the plugin API.
15. [P1] Not distinguishing stable, experimental, and internal API.
16. [P2] Making plugins check host capability themselves.
17. [P2] Letting plugins branch on terminal type directly.
18. [P1] Letting the extension host block the main event loop.
19. [P1] Having no per-plugin memory/CPU limits.
20. [P1] Having no plugin shutdown, timeout, or cancellation model.
21. [P1] Letting a plugin update auto-destroy configuration.
22. [P2] Loading plugin dependencies arbitrarily with no dependency resolver.
23. [P2] Having no plugin lockfile.
24. [P2] Having no plugin compatibility CI.
25. [P1] Having no signature or checksum verification in the plugin store.
26. [P2] Confusing plugin sandbox with remote execution location.
27. [P2] Having no plugin state migration policy.
28. [P2] Making the API deprecation window too short.

## ECO — Ecosystem Stability

1. [P1] Changing the public API with every internal refactor.
2. [P0] Declaring semantic versioning but not honoring it.
3. [P1] Deleting command IDs in a minor release.
4. [P1] Renaming config keys outright.
5. [P1] Silently changing profile default keys.
6. [P1] Fully coupling the plugin API to the editor release cycle.
7. [P2] Providing no shim for old APIs.
8. [P2] Having no LTS support window.
9. [P0] Having no representative-plugin regression tests.
10. [P2] Docs diverging from the actual API.
11. [P2] Keeping only manual docs with no generated schema.
12. [P2] Blurring the nightly/stable API boundary.
13. [P2] Not warning at install time about outdated plugins.
14. [P1] Force-loading incompatible plugins.
15. [P3] Having no deprecated-API telemetry or usage tracking.
16. [P2] Making large API changes with no migration tool.
17. [P2] Implicitly tying config and keymap versions to the editor version.
18. [P2] Making installs non-reproducible if a plugin repository is deleted.
19. [P2] Making reproducible installs impossible.
20. [P2] Having no lockfile checksum.

## REMOTE — Remote Architecture

1. [P0] Viewing remote as merely a remote filesystem.
2. [P1] Running only files remotely while LSP, build, debugger run locally.
3. [P0] Not defining the client / workspace-runtime boundary.
4. [P1] Letting the remote server guess local terminal capabilities.
5. [P1] Letting a remote plugin access the local clipboard directly.
6. [P1] Handling Windows/WSL paths via string substitution.
7. [P1] Using the same type for local path and workspace path.
8. [P1] Having no document-state recovery model on SSH disconnect.
9. [P1] Not considering reconnect and session resume.
10. [P1] Having no remote-runtime version negotiation.
11. [P2] Requiring client and server to be the exact same version.
12. [P2] Trusting file-watcher events with no full-rescan fallback.
13. [P2] Re-transferring large remote files in full every time.
14. [P2] Stuffing images or binary blobs into RPC JSON directly.
15. [P2] Having no request cancellation.
16. [P2] Having per-command inconsistent timeout and retry policies.
17. [P1] Letting plugins arbitrarily decide a remote command's execution location.
18. [P1] Allowing credential forwarding by default.
19. [P1] Implicitly opening localhost port forwarding.
20. [P1] Not distinguishing remote and local extensions.
21. [P2] Equating WSL entirely with Linux.
22. [P2] Not tying container lifecycle to workspace lifecycle.

## TERMIN — Terminal Input

1. [P2] Identifying the terminal by `TERM` value alone.
2. [P1] Ignoring ESC/Alt key ambiguity.
3. [P2] Assuming all terminals support the Kitty keyboard protocol.
4. [P1] Having no timeout policy in the legacy escape parser.
5. [P1] Treating bracketed paste like normal key input.
6. [P1] Applying keymaps to paste content.
7. [P1] Triggering state transitions during IME composition.
8. [P2] Ignoring that modifier combos differ per terminal.
9. [P2] Confusing focus events and key events.
10. [P2] Making mouse input a required feature.
11. [P1] Fully allowing terminal passthrough escapes.
12. [P2] Having no escape route for editor prefix keys inside a nested terminal buffer.
13. [P2] Hardcoding tmux/screen passthrough.
14. [P1] Having a stdin parser that cannot handle partial sequences.
15. [P1] Having the parser stall on a malformed escape sequence.
16. [P1] Failing to distinguish terminal query responses from user input.
17. [P2] Losing input during startup probing.
18. [P2] Confusing key repeat with command repeat.

## TERMOUT — Terminal Rendering

1. [P2] Using full-screen redraw only.
2. [P2] Re-emitting every cell each frame.
3. [P1] Computing grapheme width by char count.
4. [P1] Ignoring East Asian Width and emoji width differences.
5. [P1] Treating combining marks as independent cells.
6. [P2] Ignoring per-terminal width-behavior differences.
7. [P1] Equating cursor position with logical text position.
8. [P2] Assuming true color is always supported.
9. [P2] Emitting large frames with no synchronized output.
10. [P0] Letting image plugins emit Kitty/SIXEL sequences directly.
11. [P2] Disabling the whole feature when images are unsupported.
12. [P2] Confusing pixel size and cell size.
13. [P2] Rendering stale layout during resize.
14. [P2] Ignoring that a terminal multiplexer filters escapes.
15. [P2] Representing capability as just a few bools.
16. [P1] Having no safe fallback on active-probe failure.
17. [P2] Providing no user override.
18. [P2] Not managing image delete/reposition protocols.
19. [P2] Updating animated images without limit.
20. [P1] Handing a remote image blob to the renderer as a file path directly.

## UI — UI & Workspace

1. [P2] Cloning the VSCode layout onto the TUI verbatim.
2. [P2] Always showing the file tree and side panel.
3. [P2] Removing features in a narrow terminal.
4. [P2] Forcing everything into a text buffer.
5. [P2] Conversely, building a fully custom UI per view.
6. [P0] Treating buffer and view as the same object.
7. [P1] Making the same buffer un-openable in multiple views.
8. [P1] Storing view-local state in the document.
9. [P1] Allowing a per-plugin layout engine.
10. [P1] Providing only a cell grid with no semantic view model.
11. [P0] Using different command systems across TUI/GUI/Web.
12. [P1] Merging panel focus and editor mode into one state.
13. [P2] Breaking modal input in popups and dialogs.
14. [P2] Making the command palette a flat list only.
15. [P2] Having no context action discovery.
16. [P2] Making users expect text-editor behavior in special views.
17. [P2] Not showing the current mode/prefix in the status line.
18. [P2] Having no priority-based degradation when the screen is narrow.

## ASYNC — Async & Event

1. [P1] Making every function `async fn`.
2. [P1] Making even editing commands async.
3. [P1] Handling sync and async flows through a single event path.
4. [P2] Broadcasting every event on a global event bus.
5. [P1] Having no event-ordering contract.
6. [P1] Letting event handlers mutate documents directly.
7. [P1] Having no request ID on async responses.
8. [P0] Having no document revision on async responses.
9. [P0] Applying stale results as-is.
10. [P2] Having no cancellation token.
11. [P2] Letting background work block editor shutdown.
12. [P2] Having no debounce on watcher event storms.
13. [P2] Accumulating unbounded parse requests for the same document.
14. [P1] Applying both the latest and prior request results.
15. [P1] Letting a plugin event callback cause re-entrant mutation.
16. [P1] Letting an event-handler failure abort the whole dispatch.
17. [P1] Processing external events mid-transaction.
18. [P2] Managing event names as strings only.

## PERF — Performance

1. [P1] Cloning the whole document per command.
2. [P1] Implementing snapshots as deep copies.
3. [P2] Full-parsing syntax on every keystroke.
4. [P2] Rendering the visible region and the whole document identically.
5. [P2] Fully merging decorations every frame.
6. [P2] Anchor updates that are O(anchors × edits).
7. [P3] Fully re-indexing the command palette on every call.
8. [P2] Making plugin IPC excessively chatty.
9. [P2] Sending per-cell RPC.
10. [P2] Issuing thousands of small reads over remote.
11. [P2] Iterative access that ignores rope chunk boundaries.
12. [P2] Running UTF-16 conversion over the whole document on every LSP request.
13. [P2] Implementing every feature with trait objects and heap allocation.
14. [P3] Using `Box<dyn Trait>` even for small enum states.
15. [P2] Duplicating command implementations per profile.
16. [P2] Running duplicate per-plugin LSP processes.
17. [P2] Rebuilding the workspace index per plugin.
18. [P2] Assembling the whole string with no TUI render diff.

## SEC — Security

1. [P0] Granting all permissions immediately after plugin install.
2. [P0] Executing plugin code just from opening a workspace.
3. [P1] Letting project settings arbitrarily override user settings.
4. [P0] Running remote-workspace code with local-client permissions.
5. [P1] Not filtering terminal escape injection.
6. [P1] Treating plugin output as trusted UI markup.
7. [P1] Unconditionally allowing OSC 52 clipboard.
8. [P1] Executing URL-open requests without confirmation.
9. [P1] Auto-allowing remote port forwarding.
10. [P0] Having no plugin-update signature verification.
11. [P1] Having no marketplace package checksum.
12. [P1] Applying plugin capability changes silently.
13. [P1] Having no workspace trust model.
14. [P1] Granting an AI agent full filesystem access by default.
15. [P1] Applying AI changes directly and skipping review.
16. [P1] Storing long-lived credentials in the remote runtime.

## TEST — Testing & Operations

1. [P1] Having only UI tests and no headless command tests.
2. [P2] Having no differential tests for Vim behavior.
3. [P2] Having no golden tests for Emacs key sequences.
4. [P1] Having no terminal-parser fuzzing.
5. [P1] Having no transaction property tests.
6. [P1] Having no undo/redo round-trip tests.
7. [P2] Having no anchor-transformation property tests.
8. [P1] Having no crash-recovery tests.
9. [P2] Having no SSH-disconnect tests.
10. [P2] Having no WSL path-translation tests.
11. [P2] Having no tmux passthrough tests.
12. [P2] Having no representative terminal integration tests.
13. [P0] Having no representative plugin compatibility tests.
14. [P2] Having no old-API plugin fixtures.
15. [P2] Having no plugin-upgrade migration tests.
16. [P2] Having no benchmark baseline.
17. [P2] Having no large-file benchmark.
18. [P2] Having no startup regression test.
19. [P2] Having no latency budget.
20. [P2] Solving flaky async tests with sleep.
21. [P2] Having no real command-sequence replay corpus.
22. [P2] Having no user-config migration tests.

---

## Long-Horizon Categories

> These bite 2–5 years out, not at first implementation. Each category is the "don't" mirror of a domain
> in [../architecture/design-requirements.md](../architecture/design-requirements.md) (same code).

## STAB — Stability & Observability

1. [P2] Using `Err(String)` for internal errors.
2. [P0] Swallowing an assert/invariant failure as an ordinary error and continuing on corrupted state.
3. [P3] Logging the same error at multiple layers (log-and-rethrow at every layer).
4. [P2] Unstructured log sentences ("Something went wrong", "Plugin failed").
5. [P1] Blanket `panic=abort` for the whole program (kills crash reports/recovery).
6. [P1] `catch_unwind` swallowing every panic (hides corruption).
7. [P2] A single global `is_ok: bool` for the whole editor.
8. [P2] The UI owning/managing status directly instead of rendering a subscribed Health Registry.
9. [P2] Status as a string instead of a per-component state machine.
10. [P2] Changing the status enum without actually verifying the component is Ready.
11. [P1] No transaction/correlation IDs; no origin on mutations.
12. [P1] An infinite crash loop with no supervisor/backoff/disable threshold.
13. [P0] A diagnostic bundle leaking document contents, paths, tokens, or env vars.
14. [P3] Logging all failures at `error` level; logging normal cancellation as an error.
15. [P3] Equating log level with user importance.
16. [P2] Recording context needed in production only in debug builds.

## RENDER — Render Path

1. [P2] Flipping the render backend mid-session on capability-probe noise.
2. [P1] Each view/plugin emitting terminal escapes directly (see also TERMOUT-10).
3. [P2] Dynamically picking the "best" renderer per element, producing visual instability.

## RIR — Render IR

1. [P2] The Render IR expanding without limit like an HTML DOM.
2. [P1] Plugins specifying pixel/cell coordinates directly.
3. [P1] Leaking TUI constraints into the semantic model.
4. [P2] Indiscriminately adding nodes the TUI can't handle just to match GUI.
5. [P1] Including executable callbacks or Rust closures in the IR.
6. [P1] A render node continuing to reference a deleted resource.
7. [P1] Letting the IR become the union of all backends instead of backend-neutral.

## SPEC — Specification vs Implementation

1. [P1] Treating the current code as the spec.
2. [P2] Documenting Rust's ownership structure as the project's essential architecture.
3. [P2] Treating every test-passing behavior as official compatible behavior.
4. [P2] Turning an implementation bug into a permanent ABI just because users started depending on it.
5. [P2] Over-specifying every behavior, removing room for later improvement.
6. [P2] Substituting an external project as the spec ("same as Neovim").

## PAR — Parity Meaning

1. [P2] Calling it Vim-compatible because `hjkl` works.
2. [P2] Calling it Emacs Style with only `Ctrl+A`/`Ctrl+E`.
3. [P2] Treating Vim and Neovim as identical parity targets.
4. [P1] Blindly mixing conflicting semantics into Native Style.
5. [P2] Adding many shallow implementations to inflate feature count.
6. [P1] Letting a "100% compatible" marketing goal destroy the new architecture.

## PERSIST — Persistence & Crash Consistency

1. [P0] Implementing autosave as an overwrite of the original file.
2. [P1] Crash recovery serializing the editor's in-memory layout directly.
3. [P1] Judging "document saved" in the UI by write-call success only.
4. [P0] Replaying a corrupted journal in full unconditionally.
5. [P1] Treating external-change-detection failure as "no user change".
6. [P1] Showing "saved" in the UI after a save failure.
7. [P1] A recovery file permanently storing sensitive content in plaintext.

## DET — Determinism & Replay

1. [P2] Reusing debug logs as the replay log.
2. [P2] Recording only the raw keyboard byte stream, not semantic commands.
3. [P1] A structure where results depend on async completion order.
4. [P2] Increasing `sleep()` to reproduce test failures.
5. [P1] Creation time or pointer address affecting command results.
6. [P1] Applying a plugin's nondeterministic result to core state without validation.

## SCHED — Background Scheduler & Resources

1. [P2] Each plugin creating its own thread pool.
2. [P1] Spawning unbounded Tokio tasks.
3. [P2] Running all background tasks at the same priority.
4. [P2] A cancelled task discarding results but continuing computation.
5. [P2] Status line / git indicator high-frequency timer polling.
6. [P2] An indexer monopolizing CPU while the user types.
7. [P2] Background tasks staying alive after plugin suspend.
8. [P2] Duplicating the same Git/Tree-sitter/LSP analysis in one workspace.

## CACHE — Cache

1. [P1] Making the cache effectively the source of truth.
2. [P2] Depending implicitly on event names for cache invalidation.
3. [P1] Caching position info without a document revision.
4. [P1] Plugins directly modifying core cache objects.
5. [P2] Trying to keep a cache format permanently like a public API.
6. [P1] Silently allowing a wrong cache hit.
7. [P2] Syncing a cache that must not be shared across platforms.

## ID — IDs, Generations, Time

1. [P2] Making all IDs UUIDs, hiding meaning and cost.
2. [P1] Conversely making all IDs `u32`, ignoring reuse.
3. [P1] Using timestamps as an ordering guarantee.
4. [P1] Treating an old handle as valid again after workspace reconnect.
5. [P2] Encoding type and state into ID strings.

## MULTI — Multi-Client & Concurrency

1. [P1] A global cursor assuming a single user.
2. [P2] Applying the last-connected client's terminal capability to the whole workspace.
3. [P1] A slow remote client blocking document-change processing.
4. [P1] Multiple clients generating the same command sequence number.
5. [P1] Force-extending a raw-offset model after collaborative editing is required.
6. [P2] Treating connection loss the same as client termination.

## GOV — Plugin Ecosystem Governance

1. [P3] Using download count as a quality metric.
2. [P2] Keeping only the latest version in the Marketplace.
3. [P2] Making a deleted plugin unrecoverable even from the lockfile.
4. [P1] Letting official plugins use private internal APIs.
5. [P1] Plugins directly reading another plugin's private state.
6. [P2] A dependency activating dependencies without limit.
7. [P2] Showing permission requests only as a long string on the install screen.
8. [P1] Multiple plugins claiming the same command ID.

## CFG — Config, Profile, Feature Pack

1. [P2] Accepting all settings as free-form JSON/TOML values.
2. [P1] Config merge depending on load order.
3. [P1] Plugins arbitrarily changing user settings at runtime.
4. [P1] A profile monkey-patching all core behavior.
5. [P1] One config error preventing the whole editor from starting.
6. [P2] Evaluating all unused plugins' config at startup.
7. [P2] Silently applying default-value changes in a minor release.

## TRUST — Security & Trust Boundary

1. [P1] Assuming the local user is always trusted.
2. [P1] Using paths received from a remote server directly as local filesystem paths.
3. [P1] Passing terminal-output hyperlink/clipboard escapes through unconditionally.
4. [P0] Auto-executing an executable via workspace settings.
5. [P1] Auto-approving new permissions added after a plugin update.
6. [P1] Auto-applying AI results identically to a normal plugin transaction.
7. [P1] A remote agent storing local credentials long-term.

## XPLAT — Cross-Platform Semantics

1. [P2] Scattering `cfg(target_os)` branches throughout the code.
2. [P1] Representing paths only as a UTF-8 `String`.
3. [P2] Treating Windows and Unix process termination the same.
4. [P2] Ending WSL path translation at `/mnt/c` string substitution.
5. [P2] Assuming the file watcher reports every change accurately.
6. [P2] Dismissing platform-specific test failures as flaky.

## TUX — Terminal UX

1. [P2] Enabling advanced features whenever capability is high.
2. [P1] Input entirely breaking after an advanced-keyboard-protocol failure.
3. [P2] A popup silently changing modal state.
4. [P2] Editor shortcuts intercepting process input in a terminal buffer.
5. [P2] The status line overtaking document content on a narrow screen.
6. [P2] Ending image fallback at a bare `[image]` string, losing context.

## APIX — API-Stability Paradox

1. [P2] Adding a Stable API immediately just because a user requested it.
2. [P1] Exposing an internal impl function as a public API with only a wrapper.
3. [P2] Promising to never remove an API for the sake of stability.
4. [P2] Adding every plugin request to the core API.
5. [P3] Judging ecosystem maturity by API count.
6. [P2] Deprecated APIs living forever, complicating the core path.

## PERFS — Performance Stability

1. [P2] Judging speed by startup time alone.
2. [P2] Hiding cost with lazy loading and ignoring first-use latency.
3. [P2] Presenting benchmark results as a single average.
4. [P2] Removing error context and observability for optimization.
5. [P2] Measuring Rope/render performance only on small fixtures.
6. [P2] Debug features vanishing entirely in release builds, making incident analysis impossible.

## OPS — CI/CD & Operations

1. [P2] Solving flaky tests with retry only.
2. [P1] Attempting the first full-platform build at the release tag.
3. [P1] Building the release artifact from a different source than the PR that passed.
4. [P2] Ignoring persistent nightly failures as "unrelated to stable".
5. [P2] Updating the benchmark baseline from a developer's personal machine.
6. [P1] Leaving long-lived secrets and residual workspaces on self-hosted runners.
7. [P2] Testing and deploying in one combined workflow (no CI/CD separation).
8. [P2] Managing feature parity only in doc tables (they rot; encode as fixtures).

## CONTRIB — Contributor Sustainability

1. [P2] Leaving all design decisions only in Discord/chat.
2. [P2] Equating code style with architectural quality.
3. [P2] Requiring the founder to personally approve every PR.
4. [P2] Dismissing complex setup as "expected for an advanced project".
5. [P2] A structure where no one can fix a module after its owner leaves.
6. [P2] Too many docs but no way to tell which is current.

## SCOPE — Product Scope & Strategy

1. [P2] Trying to complete Vim/Emacs/Native all in the first version.
2. [P2] Developing TUI, GUI, and Web simultaneously.
3. [P2] Platformizing Editor/IDE/OS-shell/notebook/analyzer at once.
4. [P3] Building a Marketplace before there are any users.
5. [P2] Completing a Plugin SDK with no real plugin to validate the API.
6. [P2] Over-distributing into a distributed system before remote is needed.
7. [P2] Generalizing every feature until simple file editing becomes complex.
8. [P2] Deferring the MVP forever in the name of "sustainability".
9. [P3] Using none of the current language's strengths for hypothetical future portability.
10. [P2] Perfecting the architecture without getting real user feedback.

---

## Critical-15 — Must Prevent Early {#critical-15}

All items matter, but these must be locked before writing core code. Each maps to catalog IDs.

| # | Anti-pattern | Catalog refs |
| --- | --- | --- |
| 1 | Globalizing `Arc<Mutex<EditorState>>` | CORE-2 |
| 2 | Coupling Document and View | CORE-4, UI-6 |
| 3 | Representing all coordinates as `usize` | TEXT-1, TEXT-2 |
| 4 | Modifying documents directly without transactions | TEXT-9, TEXT-10 |
| 5 | Storing cursor/diagnostics as raw offsets | TEXT-4 |
| 6 | Implementing Vim/Emacs as mere keymaps | VIM-1, EMACS-1 |
| 7 | Coupling command and key binding | CMD-1 |
| 8 | Exposing internal Rust types to plugins | PLUGIN-2, PLUGIN-3, CORE-15 |
| 9 | Using the Rust dynamic ABI as the plugin ABI | PLUGIN-1 |
| 10 | Adding remote later | CORE-19, REMOTE-1, REMOTE-3 |
| 11 | Plugins printing terminal escapes directly | PLUGIN-8, TERMOUT-10 |
| 12 | Splitting TUI and GUI plugin APIs | PLUGIN-11, UI-11 |
| 13 | No versioning of API / command IDs / profiles | CMD-6, ECO-2, PROFILE-14 |
| 14 | Unconditionally applying stale async results | ASYNC-9, ASYNC-8 |
| 15 | No representative plugin compatibility CI | ECO-9, TEST-13 |

## Priorities — Five Axes to Fix First

Rather than tackling all ~300 at once, lock these five boundaries in docs first; more than half of the
remaining anti-patterns are then prevented naturally:

1. **Core State & Transaction** (CORE, TEXT, ASYNC)
2. **Input Profile & Command** (VIM, EMACS, PROFILE, CMD)
3. **Plugin Stable API** (PLUGIN, ECO)
4. **Client/Remote Boundary** (REMOTE)
5. **Terminal Capability & Rendering** (TERMIN, TERMOUT)
