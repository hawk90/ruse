---
doc: rfc
project: ruse
title: "RFC-0005: Terminal Capability"
summary: >
  Locks how ruse discovers and uses terminal capability. Capability is a per-capability LEDGER
  ({value, source: env|terminfo|probe|user, confidence}) — never a bare bool and never inferred from
  TERM alone. Active probing is fenced by DA1 (no arbitrary timeouts); a user override always wins; the
  compatibility-vs-enhanced render profile is pinned per client-view and renegotiated only on explicit
  events; unsupported features DEGRADE (image ladder: native → Unicode preview → placeholder → external
  open) rather than disappear. Covers ConPTY vs Unix PTY, grapheme width, and bracketed-paste / OSC-52
  security. Decision record ratifying D-015; detail lives in the linked parity/design docs.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../parity/terminal.md
  - ../../architecture/architecture.md
  - ../../design/render-and-frontends.md
  - ../../invariants/reference-invariants.md
  - ../../../spec/DECISIONS.md
  - RFC-0009-render-model.md
---

# RFC-0005: Terminal Capability

- **Status:** proposed
- **Author(s):** ruse maintainers
- **Created:** 2026-08-05
- **Decision link:** [D-015](../../../spec/DECISIONS.md) (terminal capability fallback + pinned render profile); relates to [D-014](../../../spec/DECISIONS.md) (View Model ↔ Render IR boundary) via [RFC-0009](RFC-0009-render-model.md), and to [D-012](../../../spec/DECISIONS.md) (multi-client, open)

<!-- RFCs are only for hard-to-reverse decisions. How ruse decides what a terminal can do is hard to
     reverse: every input path and every renderer reads the capability ledger, and getting it wrong
     (TERM-sniffing, bare bools, mid-session backend flips) is exactly the class of bug that made prior
     TUIs unstable. This RFC ratifies D-015 and the parity contract in docs/parity/terminal.md; it does
     not re-derive the escape sequences or detection methods, which live there by TERM-* ID. -->

## Summary

ruse is TUI-first, so the terminal *is* product surface. This RFC locks the discipline by which ruse
learns what a terminal can do and how it uses that knowledge. Capability is held as a **runtime ledger**,
one entry per capability `{value, source: env|terminfo|probe|user, confidence}` — **never a bare `bool`,
never inferred from `TERM` alone**. The ledger is populated by an ordered pipeline: environment scan →
terminfo prior → **active probes fenced by DA1** (no arbitrary timeouts) → user override, and the override
always wins. Each client-view reads the ledger to pin **one** render profile (compatibility | enhanced)
and holds it; unsupported capabilities **degrade** (the image ladder native → Unicode preview →
placeholder → external open) rather than vanish. This RFC ratifies [D-015](../../../spec/DECISIONS.md);
the full detection/fallback table lives in
[parity/terminal.md](../../parity/terminal.md) (TERM-* IDs) and the profile mechanism in
[RFC-0009](RFC-0009-render-model.md) / [render-and-frontends.md §4](../../design/render-and-frontends.md).

## Motivation / Problem

The governing rule ([architecture.md §6](../../architecture/architecture.md)) is blunt: **never identify a
terminal by `TERM` alone.** `TERM` only asserts that a terminfo entry *exists*, not what the terminal at
the other end of the PTY does *right now* — the user may be inside tmux, over SSH, in a terminal that lies,
or in one whose real capabilities the entry predates. The classic TUI failure is to treat capability as a
handful of booleans keyed off `TERM`, then either assume a feature works (garbage on screen) or assume it
doesn't (a permanently degraded experience the terminal could have supported). Both are unrecoverable from
the user's side.

Four things must be decided once, structurally, so they are not re-litigated per feature:

1. **How capability is represented** — richly enough to record *why* we believe it and how sure we are, so
   fallback and override are first-class, not afterthoughts.
2. **How capability is discovered** — actively and safely, without arbitrary timeouts that race real input
   ([architecture.md §6.1](../../architecture/architecture.md); TERM-PROBE).
3. **How capability is used** — pinned per client-view so the screen is stable, not flickering between
   tiers on probe noise ([render-and-frontends.md §4](../../design/render-and-frontends.md)).
4. **What happens when a capability is absent** — the feature degrades along a defined ladder; it does not
   disappear ([architecture.md §6.3](../../architecture/architecture.md); TERM-GFX).

## Guide-level explanation

Five commitments define ruse's terminal-capability model:

1. **Capability is a ledger, not a bool.** Every capability ruse cares about (truecolor, Kitty keyboard,
   bracketed paste, synchronized output, each graphics protocol, OSC-52, mouse/focus, grapheme-width
   model) has a ledger entry `{value, source: env|terminfo|probe|user, confidence}`. A renderer or input
   path asks the ledger, never `TERM`. `source` and `confidence` are load-bearing: they drive whether to
   probe further, and they make the decision *auditable* (`:debug capabilities`) instead of magic. This is
   the [parity/terminal.md](../../parity/terminal.md) Capability Ledger, TERM-PROBE-2.

2. **Probing is active and DA1-fenced.** Detection runs an ordered pipeline: env scan (`COLORTERM`,
   `TERM_PROGRAM`, `NO_COLOR`, …) → terminfo prior → **active queries** (DA1 `CSI c`, DA2, XTVERSION,
   DECRQM, graphics queries) → ledger update. Because a terminal that ignores a query simply says nothing,
   ruse never waits on a wall-clock timeout to decide "unsupported": it emits the query **followed by
   DA1**, whose universal reply is used as an **ordering fence** — if DA1 has answered and the feature
   query has not, the feature is absent (TERM-PROBE-1). No arbitrary timeouts, no lost input during startup
   probing.

3. **User override always wins.** A ledger entry sourced `user` is authoritative and cannot be overturned
   by any probe or terminfo result. This is the escape hatch for terminals that lie in either direction —
   the user can force a capability on or off, and ruse honours it for the session (TERM-PROBE-2).

4. **The render profile is pinned per client-view.** Each client-view probes once, decides
   **compatibility** (ANSI / Unicode / 256-color / legacy keyboard) vs **enhanced** (truecolor / Kitty
   keyboard / synchronized output / inline images), records it, and **freezes** it. It does *not* flip
   backends when a later probe wobbles. Renegotiation happens **only** on explicit events — resize to a new
   terminal, user override change, reconnect. With multiple client-views on one document, each pins its own
   tier and lowers the *shared* Render Tree accordingly ([render-and-frontends.md §4](../../design/render-and-frontends.md); RFC-0009; INV-RENDER-PROFILE).

5. **Absent capabilities degrade; they do not disappear.** An unsupported feature drops in quality along a
   defined ladder. For images ([architecture.md §6.3](../../architecture/architecture.md), TERM-GFX):

   ```
   native image → Unicode half-block preview (▀/▄) → metadata placeholder → external open
   ```

   The same principle governs color (truecolor → 256 nearest → 16 → mono, TERM-COLOR-1) and keyboard
   (Kitty → modifyOtherKeys → legacy + ESC-timeout, TERM-KBD). The feature is always *reachable*
   (INV-CAP-DEGRADE).

## Reference-level explanation

This RFC is a decision record; the wire-level detection methods, escape sequences, and fallback chains are
owned by [parity/terminal.md](../../parity/terminal.md) and cited here by TERM-* ID rather than duplicated.

- **Ledger entry (the contract, not a wire type).** Each capability key maps to
  `{ value, source ∈ {env, terminfo, probe, user}, confidence }`. Resolution precedence when sources
  disagree: **`user` > `probe` > `terminfo` > `env`** for authority, while `env`/`terminfo` seed the
  *prior* that probing then confirms or corrects. No consumer branches on `TERM`; consumers branch on
  ledger `value`. (TERM-PROBE-2; guards TERMOUT-15.)

- **Probe ordering & the DA1 fence.** env → terminfo prior → emit feature queries **+ DA1** as a fence →
  on DA1 reply, unanswered features resolve to unsupported → write ledger with `source: probe`. DA1
  (`CSI c`) is universal and therefore the ordering primitive; DA2 / XTVERSION refine terminal identity;
  DECRQM confirms mode support (bracketed paste 2004, synchronized output 2026); graphics use
  transmit-and-query (Kitty `i=<id>`) and DA1's `4` (Sixel). Multiplexer passthrough (tmux/screen) wraps
  payloads per TERM-PROBE-3. (TERM-PROBE-1/3; [architecture.md §6.1](../../architecture/architecture.md).)

- **Profile selection & pinning (D-015).** probe → decide tier → record in ledger with user override →
  **freeze for the client-view**; re-evaluate only on explicit events. On any unsupported element or
  runtime lowering failure, the client-view is pinned to the **compatibility** path rather than flipping
  backends mid-frame. The mechanism and its multi-client (V-13 / [D-012](../../../spec/DECISIONS.md))
  behaviour are specified in [RFC-0009](RFC-0009-render-model.md); this RFC ratifies the *capability*
  half that feeds it. (INV-RENDER-PROFILE.)

- **Degradation ladders.** Images: native → Unicode preview → placeholder → external open (TERM-GFX,
  §6.3). Plugins never emit graphics/escape sequences directly — they emit a semantic `ImageNode` and the
  backend chooses the rung (host-mediated; INV-RENDER-IR, RFC-0009). Color and keyboard have their own
  chains (TERM-COLOR-1, TERM-KBD-1/3/4). The ladder is a property of the render lowering, so a capability
  gap never becomes a missing feature. (INV-CAP-DEGRADE.)

- **PTY backends — Unix vs ConPTY (TERM-PTY).** Unix PTYs are byte-transparent: resize is `TIOCSWINSZ` +
  `SIGWINCH`, raw mode via `termios`. Windows **ConPTY reinterprets the grid and re-emits VT**, reordering
  or dropping sequences — so ruse must **not assume round-trip fidelity on Windows**, must drive resize via
  `ResizePseudoConsole()` (not only `SIGWINCH`), and should prefer canonical SGR over exotic passthrough on
  that backend. Both are ledger-visible so lowering can adapt. (TERM-PTY-1/2.)

- **Grapheme width — the wcwidth problem (TERM-WIDTH).** There is no shared authoritative width function;
  terminals disagree, causing cursor desync and corruption. Width is computed over **grapheme clusters
  against a pinned Unicode version + correction tables**, never by char count; East-Asian-Ambiguous
  handling is configurable; cursor position is resynced via `CSI 6 n` and is **never equated with logical
  text position**. This ties to the text engine's typed byte/char/grapheme/cell coordinates
  ([D-023](../../../spec/DECISIONS.md), INV-POS-TYPED; [architecture.md §3.2](../../architecture/architecture.md)).
  (TERM-WIDTH-1/2/3; guards TERMOUT-3/4/5/6/7.)

- **Input & paste/clipboard security.** Bracketed paste (mode 2004) is detected/enabled and paste content
  is **distinguished from typed input, has keymaps *not* applied to it, and has embedded escape sequences
  stripped/neutralized** (TERM-PASTE-1/2; SEC-5; guards TERMIN-5/6). OSC-52 clipboard **write** is used
  (critical for remote/tmux) with an OS-clipboard fallback; **read** is silent-exfiltration-prone and is
  therefore **opt-in with explicit confirmation only** (TERM-OSC52-1/2; SEC-7). Mouse/focus modes are
  optional and **all such modes are disabled on exit** so the parent shell is not flooded with raw
  sequences (TERM-MOUSE; keyboard flags likewise popped/reset on exit, TERM-KBD-2).

## Reference Invariants

This RFC **depends on and restates** the following IDs from
[reference-invariants.md](../../invariants/reference-invariants.md). It introduces no new INV IDs (new
invariants are minted only in that registry, per [D-022](../../../spec/DECISIONS.md)).

- **INV-CAP-DEGRADE** — An unsupported capability degrades (lower quality / fewer features), it does not
  disappear; capability is a **confidence ledger with user override, never a bare bool, and never inferred
  from `TERM` alone**. *(Guards TERMOUT-11/15/17, TERMIN-1.)* — this is the core of §Guide commitments
  1, 3, 5 and the degradation ladders.
- **INV-RENDER-PROFILE** — A render profile (compatibility | enhanced) is pinned per **client-view** and
  not switched mid-session on probe noise; with multiple clients, each client-view lowers the shared Render
  Tree at its own tier. *(Guards RENDER-1/3.)* — the pinning rule (§Guide commitment 4), whose render
  mechanism is owned by [RFC-0009](RFC-0009-render-model.md).

Reaffirmed (owned by other RFCs): **INV-RENDER-IR** (plugins emit semantic nodes, never escapes — the
host mediates graphics; RFC-0009), **INV-POS-TYPED** (typed byte/char/grapheme/cell coordinates behind the
width model; D-023), **INV-TRUST-1** (terminal-output is a distinct, low-trust principal — relevant to
paste-neutralization and OSC-52 read).

## Failure modes & Recovery

- **Terminal ignores a probe.** *Recovery:* the DA1 fence resolves it as unsupported (`source: probe`,
  low positive confidence); the feature falls to its degradation rung. No timeout race, no hang
  (TERM-PROBE-1).
- **Terminal lies (claims/denies a capability wrongly).** *Recovery:* the user override (`source: user`)
  is authoritative and permanent for the session; it beats any probe/terminfo result (TERM-PROBE-2).
- **Probe noise / capability wobble mid-session.** *Recovery:* per-client-view pinning absorbs it — the
  profile does not flip; the ledger only re-reads on explicit renegotiation events (resize/override/
  reconnect) (INV-RENDER-PROFILE).
- **Runtime lowering failure (e.g. a graphics payload rejected mid-frame).** *Recovery:* the client-view
  is pinned to the compatibility path and the affected node degrades along the ladder; it never tears the
  screen by switching backends (INV-CAP-DEGRADE, INV-RENDER-PROFILE).
- **ConPTY drops/reorders sequences.** *Recovery:* ruse does not assume round-trip fidelity on Windows and
  drives resize via the platform API; canonical SGR is preferred so a dropped exotic sequence is not
  load-bearing (TERM-PTY-2).
- **Escape leaked through bracketed paste.** *Recovery:* paste payload is neutralized before it can reach
  the input/command path; keymaps are never applied to it (TERM-PASTE-2; guards TERMIN-5/6).
- **Startup probing eats user keystrokes.** *Recovery:* the parser distinguishes query responses from user
  input and buffers rather than dropping during the probe window ([architecture.md §6.1](../../architecture/architecture.md)).

## Security impact

Two terminal-native exfiltration/injection vectors are closed by policy, not luck. **Bracketed paste** is
neutralized — embedded escape sequences in a paste payload are stripped so a hostile clipboard cannot drive
the editor, and keymaps are never applied to paste content (TERM-PASTE-2, SEC-5). **OSC-52 clipboard read**
is silent-exfiltration-prone (any program can read your clipboard over the stream), so it is **opt-in with
explicit confirmation only**, while write uses an OS-clipboard fallback (TERM-OSC52-2, SEC-7). Both fit the
trust model: terminal output is a distinct, low-trust principal (INV-TRUST-1), and — per
[RFC-0009](RFC-0009-render-model.md) — plugins cannot smuggle control sequences because they emit semantic
nodes, never bytes (INV-RENDER-IR). The capability ledger being auditable (`:debug capabilities`) means a
surprising clipboard/graphics behaviour is a queryable fact, not a mystery.

## Performance impact

Probing adds a bounded startup cost: a batch of queries terminated by one DA1 fence, resolved in a single
round-trip window rather than N wall-clock timeouts — strictly cheaper and more deterministic than
timeout-per-feature detection, and it does not stall on partial/malformed sequences. At steady state the
ledger is read, not recomputed, and the pinned profile means no per-frame capability branching. Rendering
uses **synchronized output** (mode 2026) at lowering time for tear-free large redraws (TERM-SYNC) and a
render diff (changed cells only), both of which are render-side concerns governed by RFC-0009 and the
latency budgets under [D-019](../../../spec/DECISIONS.md)/ENG-PERF-001, not set here.

## Compatibility & Migration

No prior public capability contract exists; nothing to migrate. Forward-compatibility is structural: new
capabilities are **added to the ledger** (each with its own detection + fallback rung) without changing how
consumers read it, and unknown/absent capabilities degrade by default (INV-CAP-DEGRADE, INV-ADDITIVE). New
terminal features (a new graphics protocol, a new keyboard flag) land as additional ledger keys and ladder
rungs, not as a schema break. TUI-first does not foreclose GUI/Web: those are additional client-views with
their own profiles, per RFC-0009.

## Observability

The capability ledger and the pinned profile are inspectable via `:debug capabilities` — every entry shows
`{value, source, confidence}`, making "why did truecolor turn off?" or "why is this the compatibility
tier?" a queryable fact rather than "the terminal is weird". This is the capability+lowering stage of the
dumpable pipeline ([render-and-frontends.md §5](../../design/render-and-frontends.md); RFC-0009):

```
… → Semantic Render Tree (:debug render-tree) → Terminal output (:debug capabilities + lowering)
```

Debug surfaces are product features, not ad-hoc logging.

## Alternatives

- **A1 — Identify the terminal by `TERM` alone.** Rejected (see below).
- **A2 — Capability as a few bools.** Rejected (see below).
- **A3 — Disable the whole feature when unsupported.** Rejected (see below).
- **A4 — Timeout-per-feature detection (no DA1 fence).** Rejected: racy against real input, non-deterministic,
   and prone to losing keystrokes during startup probing ([architecture.md §6.1](../../architecture/architecture.md));
   the DA1 fence gives the same answer without a wall clock. → active queries + DA1 fence (TERM-PROBE-1).
- **A5 — Flip render backend mid-session on probe result.** Rejected (see below).
- **A6 — Let plugins emit escape sequences directly.** Rejected (see below).
- **A7 — Trust `COLORTERM`/terminfo without probing.** Rejected as *insufficient*, not wrong: env/terminfo
   are used as the **prior**, then confirmed by probe; they seed the ledger but do not get the final say
   (except where the user overrides). This is the pipeline, not an alternative to it.

## Rejected approaches

- **Identify the terminal by `TERM` alone.** Rejected: `TERM` asserts only that a terminfo entry exists,
  not runtime truth (wrong under tmux/SSH, on lying terminals, or when the entry predates the terminal's
  real capabilities). This is the root TUI-instability bug. Violates INV-CAP-DEGRADE. → env prior + active
  DA1-fenced probe + per-capability ledger. *Recorded so "just check `$TERM` / `$COLORTERM`" is not
  re-proposed as sufficient.*
- **Capability as a few bools.** Rejected: a bare `bool` cannot record *why* we believe it or how sure we
  are, so there is nowhere to hang fallback or a user override, and disagreeing sources silently clobber
  each other. Violates INV-CAP-DEGRADE (guards TERMOUT-15). → `{value, source, confidence}` ledger with an
  explicit precedence and a winning user override.
- **Disable the whole feature when unsupported.** Rejected: it turns a *quality* gap into a *capability*
  gap — an image becomes nothing instead of a preview/placeholder/external-open, color becomes an error
  instead of a nearest-color fallback. Users on modest terminals lose features they could partially have.
  Violates INV-CAP-DEGRADE (guards TERMOUT-11/17, TERM-GFX). → degradation ladders.
- **Flip render backend mid-session on a probe result.** Rejected: mixing tiers within one screen gives
  visual instability, and a single probe wobble would visibly flip elements; the terminal-side symptom of
  the same disease RFC-0009 rejects render-side. Violates INV-RENDER-PROFILE. → pin one profile per
  client-view; re-evaluate only on explicit events (resize/override/reconnect).
- **Let plugins emit escape sequences (Kitty/SIXEL/iTerm/OSC) directly.** Rejected: competing writers
  corrupt cursor/screen state, make capability degradation impossible (the host no longer controls the
  rung), and open a terminal-injection vector. Violates INV-RENDER-IR (guards TERMOUT-10). → plugins emit
  semantic nodes; the host lowers them per the ledger (RFC-0009).

## Trade-offs

- **A ledger is more machinery than a bool.** Accepted: it is the substrate for safe fallback, user
  override, auditability, and additive growth — the point, not overhead (INV-CAP-DEGRADE).
- **Active probing adds a startup round-trip.** Accepted: it is bounded by a single DA1 fence and is more
  deterministic and cheaper than timeout-per-feature; it also avoids the "assumed support → garbage on
  screen" failure entirely (TERM-PROBE-1).
- **Pinning can leave a client on a lower tier after the environment improves.** Accepted: screen stability
  beats opportunistic upgrades; the explicit renegotiation events are the escape hatch and user override
  always wins (INV-RENDER-PROFILE).
- **OSC-52 read gated behind opt-in confirmation is less seamless.** Accepted: silent clipboard
  exfiltration is not an acceptable default (SEC-7).
- **A pinned Unicode-version width table can disagree with a given terminal.** Accepted and mitigated:
  configurable East-Asian-Ambiguous handling plus `CSI 6 n` cursor resync bound the damage; there is no
  authoritative width function to defer to (TERM-WIDTH).

## Re-evaluation conditions

- **D-015 (fallback + pinning)** — revisit on the defined renegotiation events (resize/reconnect/override)
  as *behaviour*, not policy; the policy reopens only if pinning demonstrably causes worse UX than
  controlled mid-session upgrades on real terminals.
- **New detection primitive** — if a widely-supported, reliable capability-report supersedes the DA1-fence
  approach (e.g. a universal machine-readable capability query), the probe pipeline is re-examined; the
  *ledger* representation is not.
- **Grapheme-width negotiation (OSC 66 / mode-based)** — if terminals converge on an authoritative
  negotiated width, TERM-WIDTH's correction-table approach is revisited (TERM-WIDTH-3).
- **D-012 (multi-client)** — when multi-client attach is decided (before F-017 hardening), confirm
  per-client-view ledger + pinning still expresses the chosen sequencing (shared with RFC-0009).
- Superseding this RFC requires superseding [D-015](../../../spec/DECISIONS.md) in the same change (never
  editing this RFC alone) and updating [parity/terminal.md](../../parity/terminal.md) and
  [architecture.md §6](../../architecture/architecture.md) to match.

## Open questions

- **Exact ledger key set and `confidence` scale** — the concrete capability enumeration and how confidence
  is quantified/combined across sources are an implementation concern of ENG-RENDER-001, not frozen here;
  this RFC fixes the *shape* `{value, source, confidence}` and the precedence, not the numbers.
- **Grapheme-width correction tables & pinned Unicode version** — which corrections ship and how per-user
  East-Asian-Ambiguous config is expressed (TERM-WIDTH-1/2) — validated against real terminals during F-003.
- **Multiplexer passthrough breadth** — how far to auto-detect vs require `allow-passthrough` for
  tmux/screen graphics (TERM-PROBE-3) without hardcoding a multiplexer.
- **ConPTY enhanced-tier scope** — how much of the enhanced path (inline images, Kitty keyboard) is
  reachable through ConPTY's reinterpreting layer, or whether Windows is compatibility-tier by default
  until measured (TERM-PTY-2).
