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
 └──────────┘             └───────────────┘            │   AnsiStrip → scrollback)│
                                                        └──────────────────────────┘
```

- **PTY** (`apps/tui/src/pty.rs`, unix-only): `UnixPty::spawn` uses `libc::forkpty` — the shell `CString` is
  built in the parent so nothing between fork and exec allocates (async-signal-safety; the editor may already
  be multi-threaded). A dedicated reader thread streams master-fd output over an `mpsc` channel; the channel
  disconnects on EOF (child exit). `Drop` hangs up (`SIGHUP`), reaps (`waitpid`), and joins the reader.
  `Pty` is a trait so a future Windows ConPTY impl (slice 3) slots in without touching the session.
- **Terminal state** (`apps/tui/src/term_buffer.rs`): a session-side `Terminal` owns the PTY, the receiver,
  and a **bounded, sanitized** scrollback. `drain()` pulls pending output through `AnsiStrip` and appends it.
- **Loop multiplex** (`apps/tui/src/app/session.rs`): gated so the common case is unchanged. With **no**
  terminal, the loop keeps the pure blocking `event::read()` (no spin). With a terminal live, it `drain`s each
  frame and uses `event::poll(TERM_TICK_MS ≈ 33ms)` — a key is handled if ready, otherwise the loop re-renders
  the freshly drained output. Only the terminal-active path polls.

## VT grid model (slice 2)

Slice 1 shipped a line-mode stand-in (`AnsiStrip` → sanitized text). Slice 2 replaces it with a real **VT
screen grid** (`apps/tui/src/term_grid.rs`): a `rows × cols` matrix of styled cells with a cursor, implementing
`vte::Perform` so `Terminal::drain` feeds raw PTY bytes straight into the `vte` parser. Coverage: printable +
wrap + CR/LF/BS/TAB; cursor `CUP`/`CUU`/`CUD`/`CUF`/`CUB`/`CHA`/`VPA` + save/restore; `SGR` (16 + bright
colors, truecolor, bold/underline/italic/reverse); erase `ED`/`EL`; **alt-screen** (`?1049h/l` — stash/restore
the main screen) so vim/htop don't corrupt it; `RI`/index scrolling; `?25` cursor visibility. Off-screen rows
accumulate in a bounded scrollback (paging UI = slice 2b).

`screen::Cell` gained a `CellStyle` (bg + bold/underline/italic on top of fg/reverse); `flush_diff` emits the
extra SGR lazily. The renderer paints a terminal window from its grid (`paint_grid`) and places the terminal's
cursor at the grid cursor (hidden when the app hides it, e.g. vim). Resize (`Terminal::resize`) reallocates the
grid **and** `ioctl(TIOCSWINSZ)`s the PTY so the child reflows.

The terminal is a first-class workspace buffer: `:terminal` creates an empty placeholder `Document`, focuses
it, enters `Mode::Terminal`. Modes are per-view (VS-OBL-1): `Mode::Terminal` forwards keys to the PTY;
`CTRL-\ CTRL-N` drops to `Mode::TerminalNormal` (the normal grammar over the empty doc — `:q`, `C-w`, window
switch); `i`/`a`/`A` resume Terminal.

**Slice 2b (partly landed):** scroll regions (`DECSTBM`) + insert/delete line (`IL`/`DL`) + insert/delete/erase
char (`ICH`/`DCH`/`ECH`) are in, so region-scrolling apps (less, vim, git log, tmux) render correctly. Still
deferred: the scrollback **paging UI**, mouse reporting, bracketed paste into the child, sixel.

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
  paging UI, mouse, bracketed paste.
- **Slice 3:** Windows ConPTY behind the `Pty` trait; grid re-emission without byte round-trip assumptions.
- **Slice 4:** fold the reader into the `C-SCHEDULER` deterministic executor (`docs/design/scheduler.md`),
  honoring INV-SCHED-1 / INV-ASYNC-ORDER.
