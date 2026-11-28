---
doc: terminal
project: ruse
title: "Terminal buffer (F-011) — design"
summary: >
  Design of the terminal buffer (F-011, CAP-TERMINAL): the PTY (DEP-PTY) and VT parser (DEP-TERM-PARSER)
  layers, and the landed and planned slices.
audience: [maintainers, contributors, llm-agents]
status: draft
---

# Terminal buffer (F-011) — design

Status: **slices 1–2 landed** (unix VT grid). Slices 2b/3/4 planned. Owner capability: `CAP-TERMINAL`,
dependencies `DEP-PTY` (libc/forkpty) + `DEP-TERM-PARSER` (the `vte` crate). Spec: `spec/PRD.yaml` F-011.

## Why

`ruse` needs a shell inside the editor. The hard part is not spawning a process — it is that a PTY child
emits output **with no keypress**, while the editor's event loop historically blocked on `event::read()` and
rendered once per key. F-011 is therefore architecture-defining: it introduces the first asynchronous input
source and the loop multiplexing that renders it. The feature is intentionally sliced so each PR is shippable.

## The async seam (slice 1)

```
 forkpty (libc)          reader std::thread            session (main loop)
 ┌──────────┐  master fd  ┌───────────────┐  Vec<u8>   ┌──────────────────────────┐
 │  $SHELL  │◀───write────│  read()→Sender │──mpsc────▶│ terminals: DocumentId →  │
 │  (child) │────output──▶│                │           │   Terminal (drain →      │
 └──────────┘             └───────────────┘            │   vte → Grid)            │
                                                        └──────────────────────────┘
```

- **PTY** (`apps/tui/src/pty.rs`, unix-only): `UnixPty::spawn` uses `libc::forkpty` — the shell `CString` is
  built in the parent so nothing between fork and exec allocates (async-signal-safety; the editor may already
  be multi-threaded). Before `execvp` the child puts every signal back to its default action and empties
  its signal mask (#1796), as libuv does for an nvim job: an ignored signal and the mask survive exec, and
  the Rust runtime ignores SIGPIPE, so `yes | head` would otherwise never kill its producer. The shell-outs
  and the clipboard tools spawn through `std::process::Command`, which resets SIGPIPE and the mask itself.
  A dedicated reader thread streams master-fd output over an `mpsc` channel; the channel
  disconnects on EOF (child exit). `Drop` wakes and joins the reader, closes the master, sends `SIGHUP`, and
  hands the child to one process-wide reaper thread (`apps/tui/src/reap.rs`, shared with the shell-outs and
  the clipboard tools, #1774), so closing a terminal (`:bd!`) never waits on the child (#1775). The reaper SIGKILLs a child still there after 0.5 s, reaps it (`waitpid`), and gives up after 2 s
  more (an unkillable zombie leaks rather than holding the reaper). At exit the editor waits for the reaper
  (`pty::finish_reaping`) so no child outlives it. A failure after `forkpty` (the master clone, the reader
  thread spawn) drops the half-built PTY, so the child is still hung up and reaped (#1754).
  `Pty` is a trait so a future Windows ConPTY impl (slice 3) slots in without touching the session.
- **Terminal state** (`apps/tui/src/term_buffer.rs`): a session-side `Terminal` owns the PTY, the receiver,
  and a VT `Grid` with a **bounded** scrollback. `drain()` feeds pending output to the `vte` parser, which drives the grid.
- **Loop multiplex** (`apps/tui/src/app/session/event_loop.rs`; the poll timeout in `session/frame.rs`): gated so the common case is unchanged. With **no**
  terminal and no live language server, the loop keeps the pure blocking `event::read()` (no spin). With a terminal live, it `drain`s each
  frame and uses `event::poll(TERM_TICK_MS ≈ 33ms)` — a key is handled if ready, otherwise the loop re-renders
  the freshly drained output. An LSP-only session polls too, more slowly (`LSP_TICK_MS`).

## VT grid model (slice 2)

Terminal output lands in a **VT screen grid** (`apps/tui/src/term_grid.rs`): a `rows × cols` matrix of styled cells with a cursor, implementing
`vte::Perform` so `Terminal::drain` feeds raw PTY bytes straight into the `vte` parser. Coverage: printable +
wrap + CR/LF/BS/TAB; cursor `CUP`/`CUU`/`CUD`/`CUF`/`CUB`/`CHA`/`VPA` + save/restore; `SGR` (16 + bright
colors, truecolor, bold/underline/italic/reverse); erase `ED`/`EL`; **alt-screen** (`?1049h/l` — stash/restore
the main screen) so vim/htop don't corrupt it; `RI`/index scrolling; `?25` cursor visibility; settable tab
stops (`HTS`, `TBC` 0/3, `CHT`, `CBT`; every 8 columns after `RIS`; a resize keeps the stops on kept columns
and gives new columns the default stops, as libvterm does). Off-screen rows accumulate in a bounded
scrollback (paging UI = slice 2b), readable oldest first through `Grid::scrollback` / `scrollback_row`.

`screen::Cell` gained a `CellStyle` (bg + bold/underline/italic on top of fg/reverse); `flush_diff` emits the
extra SGR lazily. The renderer paints a terminal window from its grid (`paint_grid`) and places the terminal's
cursor at the grid cursor (hidden when the app hides it, e.g. vim). Resize (`Terminal::resize`) reallocates the
grid **and** `ioctl(TIOCSWINSZ)`s the PTY so the child reflows.

The terminal is a first-class workspace buffer: `:terminal` creates an empty placeholder `Document`, focuses
it, enters `Mode::Terminal`. Modes are per-view (VS-OBL-1): `Mode::Terminal` forwards keys to the PTY;
`CTRL-\ CTRL-N` drops to `Mode::TerminalNormal` (the normal grammar over the empty doc — `:q`, `C-w`, window
switch); `i`/`a`/`A` resume Terminal unless they are the argument of a pending command (`"a`, `fa`).
`CTRL-\ CTRL-O` (#1733) drops to Terminal-Normal for one command: the engine records a
`Suspended { resume: Terminal }` return address (the `i_CTRL-O` construct) and keeps the count, register and
operator alive between keys, and the session resumes Terminal once the engine is back at a clean boundary
with no session prefix, command line or selection open. So `yy`, `3yk`, `"ayy`, `<Esc>`, `:cmd<CR>` and
`v…<Esc>` each return to Terminal mode, as in nvim 0.12.6 (mode `ntT`, then `t`). `CTRL-\ CTRL-N` in
Terminal-Normal stays there and ends a one-shot without resuming; focus leaving the terminal ends it too.

**Slice 2b (partly landed):** scroll regions (`DECSTBM`) + insert/delete line (`IL`/`DL`) + insert/delete/erase
char (`ICH`/`DCH`/`ECH`) are in, so region-scrolling apps (less, vim, git log, tmux) render correctly. Still
deferred: the scrollback **paging UI**, mouse reporting, sixel. A bracketed paste in Terminal or
Terminal-Normal mode goes to the child (#600): ESC and C1 controls are dropped, and it is wrapped in
`CSI 200 ~`/`CSI 201 ~` only when the child enabled mode 2004 (tracked by the grid).

## Determinism boundary (F-022)

Terminal input and output are **external and non-deterministic**: terminal keystrokes are not recorded as
`Command`s and never mutate a `Document`, and `--replay` ignores terminals. The trace/replay contract covers
editor edits only — a terminal buffer's placeholder document stays empty.

## Slicing

- **Slice 1 (landed, #290):** line-mode, unix, no new deps. `:terminal` + async output + Terminal/Terminal-Normal.
- **Slice 2 (landed):** VT grid — `screen::Cell` gained bg + bold/underline/italic (+ `flush_diff` SGR); the
  `term_grid::Grid` model + the `vte` parser (`DEP-TERM-PARSER`) + `paint_grid`; resize via `TIOCSWINSZ`.
  Unlocks full-screen TUIs (vim/htop).
- **Slice 2b (partly landed):** scroll regions + IL/DL/ICH/DCH/ECH in `term_grid.rs`; remaining = scrollback
  paging UI, mouse.
- **Slice 3:** Windows ConPTY behind the `Pty` trait; grid re-emission without byte round-trip assumptions.
- **Slice 4:** fold the reader into the `C-SCHEDULER` deterministic executor (`docs/design/scheduler.md`),
  honoring INV-SCHED-1 / INV-ASYNC-ORDER.
