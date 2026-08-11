---
doc: operations
project: ruse
title: "Manual test checklist — what automation structurally cannot cover"
summary: >
  The scenarios a human must verify at a real terminal because no automated test can reach them:
  live rendering, real key/IME decoding, terminal-capability probing against real emulators, the
  raw-mode/panic/signal lifecycle, real-crash recovery, and editing feel. Run the release smoke set
  before a tag; run the new-terminal set when adding support for a terminal emulator.
audience: [maintainers, contributors]
status: canonical
related:
  - testing-and-benchmarks.md
  - ../design/stability-and-observability.md
  - ../../spec/DECISIONS.md
---

# Manual test checklist

Automation covers the engine and core exhaustively (unit, property, full-stack keystroke fuzzer,
oracle differential, system `--replay`, cross-feature scenarios — see
[testing-and-benchmarks.md](testing-and-benchmarks.md) §1). What it **cannot** reach is the live
terminal: `--replay` bypasses the input engine, and the fuzzer/scenario tests drive the engine *below*
the terminal, so `main.rs::run`'s crossterm read loop, the screen flush, and every real-terminal / IME /
signal interaction are verified only by hand. This file lists those scenarios.

**When to run:**
- **Release smoke** (§1–§5): before cutting a tag.
- **New-terminal** (§3): when claiming support for a terminal emulator (add a row to the matrix).
- **After input/render/lifecycle changes**: the affected section.

Each item names *why* it is manual — the structural reason an automated test can't stand in.

## 1. Live rendering (F-006 cell-grid diff, D-015 pinned profile)
*Why manual: automated tests check the `Screen` grid **logic**; only a real terminal shows the pixels.*
- [ ] Cursor sits on the **correct cell** for wide/CJK/emoji/ZWJ graphemes (not one cell off).
- [ ] Editing repaints **only changed cells** — no full-screen flicker (synchronized output `?2026h/l`).
- [ ] Cursor shape/colour and truecolour render as intended; degrade (not vanish) on a poor terminal.
- [ ] `:split` / `:vsplit` tile correctly with `─`/`│` separators; the focused pane is distinguishable.
- [ ] Long lines and the `scrolloff` viewport scroll smoothly; no torn frames.

## 2. Real key input decoding (crossterm)
*Why manual: tests synthesise `KeyEvent`s directly; only a real terminal proves the bytes decode.*
- [ ] `CTRL-^` (Lang-Arg toggle, F-027), `CTRL-V` (blockwise), `CTRL-O` (insert one-shot), `CTRL-W`
      (window prefix) all arrive and act.
- [ ] Arrow / Home / End / PageUp-Down / function keys decode.
- [ ] **Bracketed paste**: pasted text is inserted verbatim, does **not** trigger keymaps or `lmap`.
- [ ] **IME composition** (F-027 acceptance #3): typing non-Latin text via a terminal IME composes on
      the no-keymap path and is **not** double-applied with `:lmap` — the one interaction only a human
      with a real IME can confirm (the disjointness is defined in code, not testable headless).

## 3. Terminal-capability probe (F-010) — per-emulator matrix
*Why manual: automated tests feed canned DA1 reply streams; real emulators reply differently.*

| Terminal | DA1 probe detects caps | Truecolour | Sync output | Notes |
|---|---|---|---|---|
| iTerm2 | ☐ | ☐ | ☐ | |
| Alacritty | ☐ | ☐ | ☐ | |
| kitty | ☐ | ☐ | ☐ | kitty keyboard protocol |
| tmux | ☐ | ☐ | ☐ | passthrough / caps clamping |
| Terminal.app | ☐ | ☐ | ☐ | limited caps — must degrade cleanly |
| Windows Terminal | ☐ | ☐ | ☐ | |

- [ ] A terminal that never replies to a probe is marked **unsupported** (not hung) — the probe is
      DA1-fenced, so verify it does not block startup on any of the above.

## 4. Terminal lifecycle / recovery (D-040)
*Why manual: raw-mode, signals, and panic-time terminal restoration need a real tty.*
- [ ] Enter and exit are clean — after quit the shell prompt is intact (no stuck raw mode, no colour bleed).
- [ ] **Panic recovery**: force a panic mid-edit → `<file>.ruse-recovered` is written **and** the terminal
      is restored to a usable state (the panic hook, not a garbled screen).
- [ ] Terminal **resize** (SIGWINCH) reflows the layout; the cursor stays correct.
- [ ] `CTRL-Z` suspend then `fg` resume restores the screen; `CTRL-C` behaves as configured.

## 5. Real file I/O & crash recovery (F-008)
*Why manual: automated tests exercise the byte logic on temp files; a real crash can't be faked in-proc.*
- [ ] **Kill the process** (`kill -9`) mid-edit with unsaved work → reopen → the 3-way recovery prompt
      appears and never auto-overwrites the on-disk file; accepting recovery restores the edit.
- [ ] Open real-world files: CRLF, a UTF-8 BOM, a read-only file, a symlink — line endings / encoding /
      permissions are preserved on save.
- [ ] Save is atomic under a real filesystem (no zero-length file if interrupted).

## 6. Editing feel / dogfood (subjective — human only)
*Why manual: latency and "feel" are not assertions.*
- [ ] Edit a large file (≥1 MB): keystroke latency stays imperceptible; no visible lag or flicker.
      (If it lags, that is the [D-042](../../spec/DECISIONS.md) storage-rewrite trigger — file a benchmark.)
- [ ] Use ruse to edit the ruse repo for a real task; note anything that feels wrong that no test caught.

---

*A PTY-driven golden harness (testing-and-benchmarks.md §1.7) could eventually automate parts of §1 and
§4; §2's IME, §3's real emulators, and §6's feel are structurally manual. Keep this list in sync when a
manual scenario becomes automatable — move it, don't duplicate it.*
