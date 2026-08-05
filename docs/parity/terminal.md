---
doc: parity-terminal
project: ruse
title: "Parity: Terminal Capability (Input & Rendering)"
summary: >
  Terminal capability parity for a TUI-first editor: keyboard protocols (Kitty vs legacy
  modifyOtherKeys + ESC/Alt timeout), bracketed paste, synchronized output, truecolor detection,
  graphics protocols (Kitty/Sixel/iTerm) with degradation ladder, OSC 52, mouse/focus, TERM vs
  active probing + tmux passthrough, PTY vs ConPTY, and the grapheme-width problem. Escape
  sequences and detection/fallback cited from primary specs.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - ../architecture/architecture.md
  - ../anti-patterns/anti-patterns.md
notation: "ESC=\\x1b; CSI=ESC [; OSC=ESC ]; DCS=ESC P; ST=ESC \\; DECSET/RST = CSI ? Pn h/l"
---

# Parity: Terminal Capability (Input & Rendering)

ruse is TUI-first, so terminal capability *is* product surface. The governing rule (architecture §6):
**never identify a terminal by `TERM` alone; actively probe, keep a per-capability confidence ledger,
degrade safely, and always allow user override.** Every item below has a detection method and a fallback;
these map directly to anti-patterns [TERMIN-*](../anti-patterns/anti-patterns.md) and
[TERMOUT-*](../anti-patterns/anti-patterns.md).

## TERM-KBD — Keyboard protocol
Source: kitty keyboard-protocol; xterm modifyOtherKeys; fixterms.

| ID | Capability | Detection | Fallback | Target |
| --- | --- | --- | --- | --- |
| TERM-KBD-1 | **Kitty keyboard protocol** (disambiguate esc, report event types/all-keys/text; modifiers = `1+bitmask`) | `CSI ? u` → `CSI ? <flags> u`, fenced by DA1 | modifyOtherKeys, else legacy + ESC timeout | L1 |
| TERM-KBD-2 | Push/pop flags on enter/exit (`CSI > <flags> u` / `CSI < u`) | — | reset on exit (avoid corrupting parent shell) | L1 |
| TERM-KBD-3 | Legacy `modifyOtherKeys` (`CSI > 4 ; 2 m`, `CSI 27;<mod>;<key>~`) | terminfo | legacy encoding | L1 |
| TERM-KBD-4 | **ESC/Alt ambiguity + timeout** (`Alt+C` ≡ `ESC c`) resolved by ~25–50 ms timeout | — | Kitty disambiguate eliminates the timeout | L2 |

**⚠️ TERMIN-2/3/4**: disambiguate `Ctrl+I`/Tab, `Ctrl+M`/Enter, `Ctrl+[`/Esc; report release/repeat. This
enables ruse's rich profile bindings (architecture §1) without the legacy timeout footgun. Always
pop/reset flags on exit (guards a common real-world corruption bug).

## TERM-PASTE — Bracketed paste (mode 2004)
Source: xterm paste64; cirw.in.
- Enable `CSI ? 2004 h`; content wrapped `ESC[200~`…`ESC[201~`. Detect via `DECRQM` (`CSI ? 2004 $ p`) or set-and-assume.

| ID | Capability | Target |
| --- | --- | --- |
| TERM-PASTE-1 | Distinguish paste from typed input | L1 |
| TERM-PASTE-2 | **Strip/neutralize escape sequences inside the paste payload** | L1 (security, SEC-5) |

**⚠️ TERMIN-5/6**: do not treat paste as key input; do not apply keymaps to paste content (guards a
security + correctness bug — some terminals leak `ESC` through the brackets).

## TERM-SYNC — Synchronized output (mode 2026)
Source: contour vt-extensions.
- BSU `CSI ? 2026 h`, ESU `CSI ? 2026 l`. Detect via `DECRQM` (`CSI ? 2026 $ p`; ps 1/2 = supported, 0/no-reply = unsupported).

| ID | Capability | Target |
| --- | --- | --- |
| TERM-SYNC-1 | Atomic (tear-free) frame updates for large redraws | L1 |

Gate behind detection so it never leaks as visible text (guards TERMOUT-9).

## TERM-COLOR — Truecolor detection
Source: termstandard/colors; terminfo.dev.
- 24-bit SGR `CSI 38;2;r;g;b m`. `COLORTERM=truecolor|24bit` → certain; else `TERM` floor (`-256color`, `-direct`/`RGB`/`Tc`). Respect `NO_COLOR`.

| ID | Capability | Fallback chain | Target |
| --- | --- | --- | --- |
| TERM-COLOR-1 | Truecolor with detection | truecolor → 256 (nearest-color) → 16 → mono | L1 |

**⚠️ TERMOUT-8**: never assume true color always supported.

## TERM-GFX — Graphics protocols & degradation
Source: kitty graphics; sixel; iTerm2 1337.

| ID | Protocol | Transport | Detection | Target |
| --- | --- | --- | --- | --- |
| TERM-GFX-1 | Kitty graphics | `APC G <k=v>;<base64> ST` | transmit+query `i=<id>` → `Gi=<id>;OK` | L1 |
| TERM-GFX-2 | Sixel | `DCS q … ST` | DA1 reply contains `4` | L1 |
| TERM-GFX-3 | iTerm2 inline | `OSC 1337;File=…:<base64> BEL` | terminal identity (XTVERSION / `TERM_PROGRAM`) | L1 |
| TERM-GFX-4 | Pixel↔cell sizing | — | `CSI 14 t` (text-area px), `CSI 16 t` (cell px) | L1 |

**Degradation ladder (feature stays, quality drops)** — architecture §6.3:
```
native image → Unicode half-block preview (▀/▄) → ASCII mosaic → metadata placeholder → external open
```
**⚠️ TERMOUT-10/11/12/20**: plugins must NOT emit graphics sequences directly (host-mediated); detect via
DA1 + identity before emitting any binary payload (else garbage on screen); don't confuse pixel/cell size;
don't disable the whole feature when unsupported.

## TERM-OSC52 — Clipboard over the stream
Source: microsoft/terminal; vtdn.dev; tmux-yank-osc52.
- Set `OSC 52 ; c ; <base64> ST` (c=clipboard, p=primary). Get `OSC 52 ; c ; ? ST`.

| ID | Capability | Target |
| --- | --- | --- |
| TERM-OSC52-1 | Clipboard **write** over terminal (critical for remote/tmux) | L1 |
| TERM-OSC52-2 | Clipboard **read** — SECURITY: silent exfiltration; most terminals disable it | opt-in only, L2 |

**⚠️ SEC-7 / TERMIN**: never assume read works; write→OS-clipboard fallback; read requires explicit
confirmation/opt-in (architecture §10). This is Neovim's OSC-52 clipboard provider ([neovim.md](neovim.md) NVIM-PROV).

## TERM-MOUSE — Mouse & focus
Source: xterm ctlseqs.
- Tracking `1000/1002/1003`; **SGR 1006** preferred (`CSI < btn;col;row M/m`); focus events mode `1004` (`CSI I`/`CSI O`).

| ID | Capability | Target |
| --- | --- | --- |
| TERM-MOUSE-1 | SGR-1006 mouse (optional feature, not required) | L1 |
| TERM-MOUSE-2 | Focus events | L1 |

**⚠️ TERMIN-9/10 + hygiene**: distinguish focus vs key events; mouse is optional; **disable all
mouse/focus modes on exit** (else the shell is flooded with raw sequences — common bug).

## TERM-PROBE — TERM vs active probing & multiplexer passthrough
Source: terminfo.dev; xterm ctlseqs; tmux passthrough.
- `TERM` only asserts a terminfo entry exists, not runtime truth. Probes: **DA1** (`CSI c`, universal → used
  as an ordering **fence**), **DA2** (`CSI > c`), **XTVERSION** (`CSI > 0 q` → name+version), **DECRQM**/`DECRQSS`.
- Recommended: env scan → prior; fire queries + DA1 fence; update per-capability confidence ledger.
- tmux/screen passthrough: wrap `ESC P tmux ; <payload, every ESC doubled> ESC \`; needs `allow-passthrough on`.

| ID | Capability | Target |
| --- | --- | --- |
| TERM-PROBE-1 | Active capability probing with a DA1 fence (no arbitrary timeouts) | L1 |
| TERM-PROBE-2 | Confidence ledger + user override per capability | L1 |
| TERM-PROBE-3 | Multiplexer (tmux/screen) passthrough handling | L2 |

**⚠️ TERMIN-1/13, TERMOUT-14/15/16/17**: don't judge by `TERM`; don't hardcode tmux; capability is not a
few bools; provide safe fallback + user override.

## TERM-PTY — Unix PTY vs Windows ConPTY
Source: warp.dev; microsoft/terminal; deepwiki ConPTY.

| Aspect | Unix PTY | Windows ConPTY | ruse note |
| --- | --- | --- | --- |
| Fidelity | byte-transparent | **reinterprets grid then re-emits VT** (reorders/drops sequences) | don't assume round-trip fidelity on Windows |
| Resize | `TIOCSWINSZ` + `SIGWINCH` | `ResizePseudoConsole()` | drive resize via platform API, not just SIGWINCH |
| Raw mode | `termios` | console mode flags (`ENABLE_VIRTUAL_TERMINAL_*`) | — |

| ID | Capability | Target |
| --- | --- | --- |
| TERM-PTY-1 | Unix PTY backend | L1 |
| TERM-PTY-2 | Windows ConPTY backend (prefer canonical SGR; don't rely on exotic sequence passthrough) | L1 |

## TERM-WIDTH — Grapheme width (the wcwidth problem)
Source: jquast/wcwidth; UAX #11/#29; unicode L2/16027.
- No shared authoritative width function; terminals disagree → cursor desync + corruption.
- EAW F/W = 2 cols; **Ambiguous (A)** = 1 or 2 (locale/terminal); emoji/ZWJ clusters render 2/4/N; VS16 flips
  width inconsistently; regional indicators double-width despite Neutral class; combining marks = width 0.

| ID | Capability | Target |
| --- | --- | --- |
| TERM-WIDTH-1 | Grapheme-cluster width model matching the terminal (pinned Unicode version + correction tables) | L1 |
| TERM-WIDTH-2 | Configurable East-Asian-Ambiguous handling; cursor-position resync (`CSI 6 n`) | L2 |
| TERM-WIDTH-3 | OSC 66 / grapheme-width negotiation where available | L2 |

**⚠️ TERMOUT-3/4/5/6/7**: never compute width by char count; handle EAW/emoji/combining marks; don't equate
cursor position with logical text position. This ties to the text engine's byte/char/grapheme distinction
([../architecture/architecture.md](../architecture/architecture.md) §3.2; guards TEXT-2).

---

## Capability Ledger (summary)
ruse maintains a runtime capability ledger, each entry `{value, source: env|terminfo|probe|user, confidence}`,
with a **user override** always winning. Never a bare `bool` (guards TERMOUT-15). Probe order: env → terminfo
prior → active queries fenced by DA1 → update ledger. Every renderer/input path reads the ledger, not `TERM`.
