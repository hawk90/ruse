---
doc: rfc
project: ruse
title: "RFC-0013: The Lang-Arg translation stage (resolving CONCEPT-LANG-ARG)"
summary: >
  Vim's Lang namespace (`lmap`, `map_mode` Lang, targeted as nvim.mapmode.lang) is the 8th keymap
  namespace, but unlike the other seven it does not BIND a key — it TRANSLATES the key through the
  active language map and re-dispatches the result, for non-Latin input in Insert, Command-line, and
  single-character command arguments. ruse's layer router (D-045) resolves a key in ONE lookup and has
  no re-dispatch stage, so promising the namespace without deciding the stage is unimplementable — the
  tension recorded as CONCEPT-LANG-ARG (blocks C-INPUT / F-027). This RFC resolves it: model Lang-Arg as
  a PRE-DISPATCH TRANSLATION STAGE, active only in the three Lang-Arg contexts, that applies EXACTLY ONE
  substitution (the translated result is dispatched literally, never re-translated) so resolution stays
  TOTAL and BOUNDED. The terminal-side IME composes TEXT that arrives on the paste/IME path and bypasses
  lmap, so the two never double-apply. Unblocks F-027; the layer stack's `resolve` stays total.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/parity/concepts/irreconcilable.yaml
  - ../../../spec/parity/contracts/keymap-layers.yaml
  - ../../../spec/parity/contracts/vim-style.yaml
  - ../../design/input-engine.md
  - ../../../spec/PRD.yaml
  - ../../../spec/config-schema.yaml
---

# RFC-0013: The Lang-Arg translation stage (resolving CONCEPT-LANG-ARG)

- **Status:** proposed
- **Decision link:** D-048 (proposed by this RFC; not yet recorded)
- **Resolves:** CONCEPT-LANG-ARG (`spec/parity/concepts/irreconcilable.yaml`), KL-Q-LANG-ARG
  (`spec/parity/contracts/keymap-layers.yaml`), D-045's stated re-evaluation trigger.
- **Unblocks:** F-027 (Lang-Arg namespace), C-INPUT.

## Summary

Model Vim's Lang-Arg (`lmap`) as a **translation stage that runs BEFORE keymap resolution**, not as a
resolution layer in the stack. When the active context is Lang-Arg-eligible — Insert, Command-line, or a
command reading a single character — an incoming decoded key is looked up in the active language map; if
it maps, the mapped key(s) **replace** the input and are dispatched **as literal input, exactly once**
(never re-translated). Everywhere else (Normal, Visual, Operator-pending, …) the stage is inert, so `d`
stays `d`. Terminal-side IME composition is a **different mechanism**: it produces composed TEXT that
arrives on the bracketed-paste / IME path (where "no keymaps apply mid-compose", input-engine.md §IME),
so lmap and the terminal IME never double-apply. This keeps the layer router's `resolve` **total** (the
translate layer never yields a partial resolution) and the work **bounded** (one map lookup, one
substitution — INV-FAIL-BOUNDED).

## Motivation / Problem

The keymap layer router (D-045, RFC anticipated by `keymap-layers.yaml`) says a key is resolved by ONE
walk of the active layer stack until a layer BINDS it. Seven of Vim's eight namespaces fit: each binds
(or declines with its `unmatched_key` policy). The **eighth, Lang**, does not fit, and this is not a
vocabulary gap — it is a mechanism gap:

- `lmap` does not map a key to a COMMAND; it maps a key to **another key**, then the editor proceeds as
  if that other key had been typed. It is a *rewrite-and-yield*, not a *bind*.
- A layer that rewrites the event and yields (rather than resolving it) makes the router's `resolve`
  **non-total** — there is no longer a guaranteed "this layer resolved it" outcome — which is exactly
  what D-045 flagged as its re-evaluation trigger (KL-Q-LANG-ARG).
- The terminal may ALREADY apply an input method (IME). Two independent translation mechanisms over the
  same keystream, undefined against each other, is a correctness hazard.

`CONCEPT-LANG-ARG` records this as `resolution: pending, blocks: [C-INPUT]`. `nvim.mapmode.lang` is
targeted by product decision, and `keymap.lang` already exists in `config-schema.yaml` marked blocked.
The namespace cannot ship until the STAGE it lives in is decided. This RFC decides it.

## Guide-level explanation

A user typing a non-Latin script maps their Latin keys to the target script **only where text is being
entered**:

```
:lmap a б            " while typing (Insert) or on the command line, 'a' produces 'б'
```

- In **Insert** and on the **Command-line**, `a` now inserts `б`. A key with no `lmap` entry is
  unchanged.
- For a **single-character command argument** — `r{char}` (replace), `f{char}` (find), `t`, `F`, `T` —
  the argument character is translated too (Vim's Lang-Arg applies here), so `rб`-worth of input can be
  typed as `ra`.
- In **Normal / Visual / Operator-pending**, `lmap` does **nothing**: `d` is delete, `a` is append —
  the operators and motions are never translated. This is the whole point of a SEPARATE Lang namespace:
  translation is scoped to text/argument entry, not to the command grammar.

`CTRL-^` (Vim `i_CTRL-^`) toggles the language map on/off within Insert; a full toggle UI is post-MVP,
but the map being active-or-not is a single boolean the stage reads.

## Reference-level explanation

### Where the stage sits (INPUT-LANGXLATE)

The input pipeline gains one stage at the **very top of dispatch**, above the layer-stack resolution:

```
decoded KeyEvent
    │
    ├─ bracketed paste / IME-composed text ──► inserted verbatim (NO keymaps, NO lmap)   [existing]
    │
    ▼
  Lang-Arg translation stage  ◄── active iff context ∈ {Insert, Command-line, single-char-arg}
    │        (look up key in the active language map; if mapped, SUBSTITUTE once)
    ▼
  layer-stack resolution (D-045)  ── resolve the (possibly translated) key in the active namespace
```

- **Eligibility** is a property of the active context, read from the same mode/await state the router
  already has: the Insert namespace, the Command-line namespace, and the "reading a single character"
  await (`r`/`f`/`t`/`F`/`T`). Nothing else. (Acceptance #2 — "and to nothing else".)
- **One substitution, then literal.** The mapped right-hand side is fed to the layer resolution **as
  input, not back through the translation stage** — so a map like `a → b`, `b → a` cannot loop, and
  resolution never re-enters translation. This is what keeps the router `resolve` **total** (the
  translate layer is not a resolution layer; it is a preprocessor that always yields a concrete key) and
  the work **bounded** (INV-FAIL-BOUNDED: one lookup, one substitution, no fixpoint).
- **Multi-key RHS** (`:lmap x abc`) is expressible as a small pending queue of already-translated keys
  drained before the next raw key is read; each queued key is literal (not re-translated). MVP may
  restrict the RHS to a single key and defer multi-key to a follow-up without changing the model.

### Config surface

`keymap.lang` (already in `config-schema.yaml`, `derives_from: [F-027]`) is the language map:
`{ "a": "б", … }`. It is a `deep-merge` user-scoped map; the stage reads the resolved map. No new schema
key is introduced by this RFC.

### IME interaction (acceptance #3)

The two translation mechanisms are **disjoint by construction**, so their interaction is defined, not
left to chance:

- **Terminal-side IME** composes one or more keystrokes into a finished CHARACTER and delivers it as
  TEXT. ruse already treats composed text and bracketed paste as a no-keymap path (input-engine.md §IME:
  "no transitions mid-compose"). That path **does not enter the Lang-Arg stage** — composed text is
  inserted verbatim.
- **ruse `lmap`** operates only on **decoded single KeyEvents** that are NOT part of a composition.

Therefore a given unit of input is translated by **at most one** mechanism: the terminal IME (for
composed input) or lmap (for raw keys), never both. If a user has both configured, the terminal IME owns
composed input and lmap owns the remaining raw keys — a stable, explainable split. ruse does not attempt
to drive the terminal's IME.

### What this does NOT change

- The layer stack (`crates/core/src/keymap.rs`) is untouched: `resolve` stays a total walk. Lang is
  `UnmatchedKey::Translate` as a DECLARED policy (already present) but the translation itself happens in
  the pre-dispatch stage, not inside `resolve`. The "representable but not resolvable" note in keymap.rs
  becomes "representable as a declared policy; realised by the pre-dispatch stage (RFC-0013)".
- The other seven namespaces, operator/count/awaiting state (KL-OBL-4), and the return-address stack
  (KL-OBL-5) are unaffected.

## Reference Invariants

This RFC depends on and enforces (single-registry rule — no new `INV-*` minted):

- **INV-FAIL-BOUNDED** — translation is a single bounded substitution; a map that would loop cannot,
  because the RHS is literal. No hang, no unbounded re-dispatch.
- **INV-CMD-SEMANTIC** — translation happens over KEYS before resolution; the command a key ultimately
  resolves to is unchanged in identity.
- **INV-ORIGIN** — a translated key carries the origin of the raw key it replaced (UserInput / Macro /
  …); translation does not launder provenance.
- **INV-ADDITIVE** — `keymap.lang` and the stage evolve additively; a multi-key RHS is an additive
  extension of a single-key RHS.

## Failure modes & Recovery

- **A map that would cycle** (`a→b`, `b→a`). *Recovery:* impossible to loop — the RHS is dispatched
  literally, never re-translated. At most one substitution per raw key.
- **Lang stage leaking into Normal** (translating `d`). *Recovery:* the stage is gated on the three
  Lang-Arg contexts only; a test asserts Normal `d` is never translated (acceptance #2).
- **Double translation with a terminal IME.** *Recovery:* composed text bypasses the stage entirely
  (the IME path is no-keymap); the two mechanisms are disjoint by construction (acceptance #3).

## Drawbacks & Alternatives

- **Alternative: a resolution layer that yields.** Model Lang as a real stack layer whose `resolve` can
  return "translated to K, re-resolve K". *Rejected:* it makes `resolve` non-total and invites unbounded
  re-dispatch — the exact hazard D-045 flagged. A pre-dispatch stage keeps `resolve` total.
- **Alternative: defer to the terminal IME entirely** (no `lmap`). *Rejected:* `lmap` is a targeted
  parity namespace (nvim.mapmode.lang) and works without a terminal IME (e.g. over SSH, in tests); it is
  a first-class Vim feature, not an IME shim.

## Unresolved questions

- The `CTRL-^` toggle UX and per-buffer `iminsert`/`imsearch` state (which contexts start with the map
  active) — the MVP reads a single "map active" boolean; the full per-context toggle model is a
  follow-up that does not change this stage's shape.
- Multi-key RHS ordering with counts (`3ra` where `a→б`) — the argument is read once and translated
  once; count interaction is the ordinary single-char-arg path, verified by fixtures.
