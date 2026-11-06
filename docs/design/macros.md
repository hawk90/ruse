---
doc: macros
project: ruse
title: "ruse Vim Macros — record (q) and replay (@)"
summary: >
  How ruse records and replays Vim keyboard macros. Recording captures the RAW keystroke stream (not resolved
  commands) and stores it as raw bytes in the shared a-z registers; replay decodes those bytes back to key
  events and re-feeds the input engine. This is the faithful unit for macros — it replays mode changes, counts,
  inserted text, and a macro containing `.` verbatim — and it is deliberately distinct from dot-repeat and the
  command trace, which record resolved commands. Ratified as D-055 under RFC-0004 / CAP-VIM-PROFILE (F-003).
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - ../rfc/accepted/RFC-0004-input-profiles.md
  - view-window-workspace.md
---

# Vim Macros

## 1. Why keystrokes, not commands (D-055)

A macro must replay *exactly what the user typed*: entering Insert and typing text, a `{count}` prefix, an
operator that is completed several keys later, a mode change, even a `.` (dot-repeat) whose meaning depends on
the last change **at replay time**. The only representation that preserves all of this is the **raw keystroke
stream**.

ruse already has two recorders, and neither fits:

- **Dot-repeat** records a `ChangeIntent` → `Feed::Replay(Vec<Command>)` (`apps/tui/src/input/mod.rs`): a
  RESOLVED, re-parameterizable *change*. It is scoped to one change, not a free key sequence.
- **The F-022 command trace** records `recorded: Vec<Command>` (`apps/tui/src/app/session.rs`): resolved
  commands for deterministic replay of a whole session.

Both are the wrong granularity. Macros are a **third, separate mechanism**: capture `KeyEvent`s, store the
bytes, re-feed the engine. A macro containing `.` re-feeds the literal `.` key, so it dot-repeats against the
last change present when the macro runs — Vim's behaviour, and the resolution of the D-025 open concern
("ChangeIntent serialization when a macro contains `.`"): there is nothing to serialize.

## 2. Storage: the shared a-z registers

A macro lives in the SAME named register as yank/paste (`crates/core/src/register.rs`), stored as **raw
bytes** — Vim's model, where a register is one thing:

- `"ap` pastes register `a`'s bytes as text (so you can see/edit a recorded macro), and
- text yanked into `a` can be run as a macro with `@a`.

A private, frontend-only macro store would be simpler but would break this identity, so it is rejected
(D-055). The frontend writes a named register through `EditorState::set_register_raw(name, bytes)` (a charwise
`Register`), with a `Workspace` passthrough. Reads use `registers().get(name)`.

## 3. The key codec (`apps/tui/src/keys.rs`)

Recording serializes each `KeyEvent` to bytes; replay parses them back. (`apps/tui/src/pty.rs::encode_key` is
one-way and lossy, so it is not usable here.)

The byte alphabet mirrors what a terminal actually delivers, so a recorded macro is also legible when pasted:

| KeyEvent                | bytes            |
|-------------------------|------------------|
| printable char `c`      | its UTF-8 bytes  |
| `Esc`                   | `0x1b`           |
| `Enter` (CR)            | `0x0d`           |
| `Tab`                   | `0x09`           |
| `Backspace`             | `0x7f`           |
| `Ctrl`-`a`..`z`         | `0x01`..`0x1a`   |
| arrows, Home/End, PageUp/PageDown, Delete, Insert, BackTab | `0x80` + a tag byte |

`decode` is **tolerant**: an unrecognized byte is skipped (never a panic), so a hand-edited or partial register
still runs as far as it can. `0x80` is an invalid UTF-8 lead, so an unknown tag is skipped too and the alphabet
stays backward-compatible. Fn, Alt, and Shift-specials have no encoding yet (#812).

## 4. Frontend flow (session)

The state machine is `keys::MacroState` (`apps/tui/src/keys.rs`), so the record→stop→replay cycle is unit-testable
off the terminal. The session key loop (`apps/tui/src/app/session.rs`) calls it for every key and keeps the
register I/O, which lives in the core `Workspace`:

- **State:** the recording buffer (`Option<(char, Vec<KeyEvent>)>`), the `q`/`@` prefix flags, the replay queue,
  the last played and last recorded registers, and the recursion budget.
- **Read step:** if the replay queue is non-empty, pop from it (source = *replay*), else `event::read()` (source =
  *typed*). Only **typed** keys are appended to a live recording — Vim records `@x` literally, not its
  expansion.
- **`q` (Normal, not in cmdline/insert):** not recording ⇒ arm a `pending_q` prefix; the next key `a`–`z`
  starts recording into that register, and `A`–`Z` appends to it on stop. `q` again while recording ⇒ stop:
  `encode` the buffer and `set_register_raw`. `q:`, `q/` and `q?` open the command-line window instead.
- **`@` (Normal):** arm `pending_at`; the next key `a`–`z` reads that register, `decode`s it, and pushes the
  keys onto the replay queue. `{count}@{reg}` repeats, `@@` replays the last register, `@:` re-drives the last
  Ex line, and `Q` replays the last recorded register. The prefix keys match an unchorded key with SHIFT
  allowed, since a terminal reports `Q` as SHIFT+`Q` (#681); a chord is never a register name.
- **Recursion guard:** a cap of 100,000 replayed keys for everything one TYPED key expands to (its `@`, the
  `@`s and `:normal`s that replay reaches), so a self-invoking macro terminates with a status line instead of
  hanging. The budget is reseeded only by a typed key, never by the queue running dry: a body whose length
  divides the cap empties the queue exactly as the budget runs out, and reseeding there looped forever
  (#787).
- **Interrupt:** CTRL-C (`C-g` under Emacs) stops a replay between two keys, as nvim's `got_int` does: the
  session polls the terminal while replay keys remain (`breakcheck.rs` →
  `ruse_core::interrupt`), drops the keys still queued, closes the replay's undo step, and shows
  `Interrupted`. The CTRL-C itself is then read as a key (nvim's `process_ctrl_c`), so an interrupted Insert
  ends. While a replay runs the screen repaints at most every 100 ms, not once per key.

## 5. Scope

**Shipped:** §4, the codec above, the core accessor, the recursion guard, and `:reg`/`:registers`, except for the
open bug #787.

**Deferred:** Fn/Alt/Shift keys (#812), running a macro inside `:g`, and cross-session persistence (which would promote
the byte encoding to a persisted-format contract — see D-055 *Re-evaluate if*).
