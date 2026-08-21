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
  - ../rfc/proposed/RFC-0004-input-profiles.md
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
(D-055). The frontend needs to WRITE a named register, which the core does not expose today
(`EditorState::registers()` is read-only) — so a small accessor `EditorState::set_register_raw(name, bytes)`
is added (writes a charwise `Register`), with a `Workspace` passthrough. Reads use the existing
`registers().get(name)`.

## 3. The key codec (`apps/tui/src/keys.rs`)

Recording serializes each `KeyEvent` to bytes; replay parses them back. No round-trippable codec exists
(`apps/tui/src/pty.rs::encode_key` is one-way and lossy), so this module is new.

The byte alphabet mirrors what a terminal actually delivers, so a recorded macro is also legible when pasted:

| KeyEvent                | bytes            |
|-------------------------|------------------|
| printable char `c`      | its UTF-8 bytes  |
| `Esc`                   | `0x1b`           |
| `Enter` (CR)            | `0x0d`           |
| `Tab`                   | `0x09`           |
| `Backspace`             | `0x7f`           |
| `Ctrl`-`a`..`z`         | `0x01`..`0x1a`   |

`decode` is **tolerant**: an unrecognized byte is skipped (never a panic), so a hand-edited or partial register
still runs as far as it can. Arrows, Fn, Alt, and Shift-specials are **deferred** (PR C): they will encode to a
reserved multi-byte escape that current `decode` already skips, so extending the alphabet stays
backward-compatible.

## 4. Frontend flow (session)

All recording/replay lives in the session key loop (`apps/tui/src/app/session.rs`), mirroring the existing
`pending_z` / `pending_window` prefix intercepts and adding one queue:

- **State:** `recording: Option<(char, Vec<KeyEvent>)>`, `replay_queue: VecDeque<KeyEvent>`.
- **Read step:** if `replay_queue` is non-empty, pop from it (source = *replay*), else `event::read()` (source =
  *typed*). Only **typed** keys are appended to a live recording — Vim records `@x` literally, not its
  expansion.
- **`q` (Normal, not in cmdline/insert):** not recording ⇒ arm a `pending_q` prefix; the next key `a`–`z`
  starts recording into that register. `q` again while recording ⇒ stop: `encode` the buffer and
  `set_register_raw`.
- **`@` (Normal):** arm `pending_at`; the next key `a`–`z` reads that register, `decode`s it, and pushes the
  keys onto `replay_queue`.
- **Recursion guard:** a cap on total replayed keys per top-level `@` (e.g. 100k) so a self-invoking macro
  terminates with a status line instead of hanging.

## 5. Scope

**Slice 1 (D-055 + this doc + PR B impl):** `q{a-z}`/`q`, `@{a-z}`, the codec above, the core accessor, the
recursion guard.

**Deferred (PR C):** `@@` (repeat last macro), `{count}@{reg}`, `q{A-Z}` append-record, arrow/Fn/Alt keys,
`:reg`/`:registers` display, running a macro inside `:g`, and cross-session persistence (which would promote
the byte encoding to a persisted-format contract — see D-055 *Re-evaluate if*).
