---
doc: rfc
project: ruse
title: "RFC-0028: Exact free text and a result hash in the trace"
summary: >
  Amends RFC-0020. The trace line form loses a trailing space or tab in a search pattern, an `\=` expression or
  a completion, and a newline in one splits the line, so a saved trace can replay to a different document
  (#721). `ruse --replay` also cannot tell when its result differs from the session's, because the trace
  records no result (#1264). This RFC writes a line whose free text the plain form cannot hold as one hex
  `line_hex` record, and adds a `result_hash=` header token that `:trace save` fills and `--replay` checks.
  Both are additive in `v1`.
audience: [maintainers, contributors, llm-agents]
status: accepted
related:
  - ../../../spec/DECISIONS.md
  - ../../../spec/PRD.yaml
  - ../../../spec/design-requirements.yaml
  - ../accepted/RFC-0020-ex-edits-in-the-trace.md
  - ../accepted/RFC-0012-collapse-to-two-crate-editor.md
  - ../../design/command-engine.md
---

# RFC-0028: Exact free text and a result hash in the trace

- **Status:** accepted: D-069 decided (design), approved by the maintainer 2026-11-28 with every recommendation
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-28
- **Decision link:** D-069 (decided; amends D-061 / RFC-0020 R5 and its *Reference Invariants*)

<!-- Hard-to-reverse: it changes the trace file format (RFC-0012's product pillar). Traces are files users keep
     and share. A record or header token, once written, has to be read by every later build. -->

## Summary

Two gaps break F-022's promise that a trace round-trips through its text form and replays to the same
document. This RFC closes both. Neither change bumps the format version.

1. **Exact free text (#721).** Most lines keep today's form. A line whose free text the plain form cannot hold
   is written as `line_hex <hex of the line>`. That covers trailing whitespace, a `\n` and a `\r`. A reader
   decodes the hex and parses the line inside without trimming it.
2. **Result hash (#1264).** The header gets `result_hash=<16 hex digits>`. It is the FNV-1a hash of the
   buffer text that the save-time replay produced (the same function as `doc_hash`). `ruse --replay`
   compares it with its own result and exits 1 on a mismatch. A trace without the token replays as today.

## Motivation / Problem

### The line form drops text (#721), probed

Probe: a scratch test in `crates/core/tests/`, run on main at b9c443e7 and then deleted. Each command went
through `Command::to_line` → `Command::from_line`, and through `Trace::to_text` → `Trace::from_text`:

| Command | Line written | Read back |
| --- | --- | --- |
| `SearchNext("foo ")` | `search_next foo ` | `SearchNext("foo")` |
| `SearchNext("foo\t")`, `SearchNext("foo\u{a0}")` | as is | `SearchNext("foo")` |
| `SearchNext("a\r")` | `search_next a\r` | `SearchNext("a")` |
| `SearchNext("a\nb")` | two lines | `from_text`: `BadCommand { line: 3, UnknownVerb("b") }` |
| `InsertEval("'a' ")`, `SetExprRegister("1 ")`, `GotoFirstMatch("x ")` | as is | trailing space lost |
| `SearchNext(" foo")`, `SearchNext("a\\nb")`, `SearchNext("a\rb")`, `SearchNext("")` | as is | exact |

Where the text is lost:

- `Trace::from_text` trims every line (`crates/core/src/trace.rs:149`). `Command::from_line` trims it again
  (`crates/core/src/command/line_form.rs:314`). The free-text verbs read `raw`, the rest of that trimmed line
  after the first space (`line_form.rs:323`). Leading spaces survive, because only the first space separates
  the verb. Trailing whitespace does not: `str::trim` removes every Unicode `White_Space` character, so a
  no-break space goes too. The trimmed `arg` (`:319`) is not used by these verbs.
- `str::lines` (`trace.rs:138`) splits at `\n` and drops a `\r` before it, so a `\n` breaks the line and a
  trailing `\r` is lost.
- The encoder writes the free text raw. Eight sites do this: `insert_eval`, `complete_word`
  (`crates/core/src/command/line_form/encode.rs:307,316`), `set_expr_register` (`:557`), `search_next`,
  `search_prev` (`:599,600`), `goto_first_match` (`:627`), and the pattern tail of `search` and
  `search_object` (`:642,654`). Free text elsewhere is already hex: `insert_paste`, `replace_paste`,
  `emacs_paste` (`encode.rs:309-311`) and every `ex_*` text and byte field (`editor/ex_cmd/codec.rs:18`).

**The save-time check does not catch it, for two reasons.** `:trace save` replays the in-memory `Trace`
(`apps/tui/src/app/trace_check.rs:125`), not the text it writes (`app/session/ex_exec/files.rs:33-40`). And it
compares only the text, `replayed.bytes() == doc.bytes()` (`trace_check.rs:126`), so a loss that moves only the
cursor would pass even a text replay. Take a session that runs `/foo ` on `foo bar\nfoox foo y\n`. The
in-memory replay ends at byte 13, and the text replay ends at byte 8 (probed), while the text is the same. The
save goes through, and the file replays to another place. `Refusal` (`trace_check.rs:92-100`) has no variant
for a trace whose text does not parse back to itself. So
#1264's premise ("within one binary a saved trace always reproduces the session", RFC-0020 *Implementation
notes (#690)*) does not hold for these commands.

**Why the tests miss it.** `round_trips_every_variant` (`crates/core/src/command/tests.rs:814`) uses fixed
samples with no trailing whitespace (`SearchNext("foo bar")`, `:593`). The hostile-argument fuzz
(`trace.rs:607-629`) parses a line first and then re-encodes it, so the text it checks has already been
trimmed.

### The trace records no result (#1264)

`Trace` holds `format_version`, `doc_hash` and `commands` (`trace.rs:31-35`). `ruse --replay` checks
`doc_hash` before it replays (`trace.rs:191-197`). After the replay it prints the result and exits 0
(`apps/tui/src/app/cli.rs:312-314`). So a later build whose commands behave differently replays an old trace
to a different document and reports success. DET-003 ("same initial document + same trace ⇒ same final state,
… at the process boundary") is checked only inside one binary.

## Guide-level explanation

```text
# ruse-trace v1 doc_hash=58100c163f98d177 result_hash=9a2c0e71b4f3d5a8
ex_set tabstop 8
search_next foo bar
line_hex 7365617263685f6e65787420666f6f20
delete 1 word_fwd
```

- The hash values are illustrative. `search_next foo bar` is written plainly, as before.
- `line_hex …` is `search_next foo ` (with the trailing space). The plain form cannot hold that space, so the
  whole line is written as hex.
- `ruse --replay t.trace f` exits 1 when the replay gives another document. It writes nothing to stdout, as
  for a `doc_hash` mismatch. The message names both hashes and says how to replay anyway: delete the
  `result_hash=` token, since a later build may behave differently on purpose (a parity fix).
- A trace saved before this change has no `result_hash` and replays exactly as it does today.

## Reference-level explanation

### T1. Exact free text: the `line_hex` record

- **The plain-form rule (semantic).** `Command::to_line` writes the plain line `L` if and only if `L` has no
  `\n` and `Command::from_line(&L) == Ok(self)`; otherwise it writes `line_hex`. The rule is the round trip
  itself, not a guess about which characters are lost, so it keeps every line that round-trips today plain.
  Two such lines end in a space on purpose: an empty pattern writes `search_next ` (`encode.rs:599`) and an
  empty completion writes `complete_word {back} ` (`encode.rs:316`). Both read back exactly, because the trim
  leaves the empty remainder. A `trim_end` test would wrongly hex them. Leading whitespace in the free text
  and backslashes (the plain form has no escapes) also round-trip, as the probe table shows. The `\n` test
  stays, because `from_line` parses one line and so cannot see that `Trace::from_text` will split it.
- **Encoder.** If the plain line fails the rule, `to_line` writes `line_hex <h>` instead, where `h` is the
  lowercase hex of the plain line's UTF-8 bytes (`hex_lower`, `encode.rs:109`). Only the eight free-text sites
  can fail the rule, because every other token is a number, a fixed word or hex. The rule is the round trip of
  the whole line, so a free-text verb added later is covered without a change. Cost: one `from_line` per
  written command, at save time only.
- **Decoder.** `line_hex` decodes `h` (`hex_decode`, `line_form/args.rs:122`, which already rejects odd length,
  bad digits and invalid UTF-8). It then parses the inner line with an **exact** parse: the same decoders
  with no `trim` and with `raw` taken from the untrimmed line. The plain path keeps its trims, so a plain
  line decodes as it does today.
- **Refused:** a `line_hex` inside a `line_hex` (`BadArgument`), and an inner line with an empty or unknown
  verb (the usual errors). The size is bounded by the line itself. The hex doubles a line, and a line is
  already in memory, so no new limit is needed (INV-FAIL-BOUNDED). The inner line is subject to the same
  limits as a plain one (`TooLarge`). At save time, any decode error of the written text is a
  `Refusal::NoReplay`, not a `NotExact` (T2). Treat a `line_hex` whose inner line would have been written
  plain as valid, so that hand-written traces work.
- **Not covered: non-UTF-8 text.** Every free-text field is a `String` (`crates/core/src/command/mod.rs:314-765`),
  so it is UTF-8 when it is built. A trace file is read with `read_to_string` (`cli.rs:285`). A pattern that
  has to match non-UTF-8 buffer bytes is a `Command` type change. It is not a line-form change, so it is out
  of scope here.

### T2. The result hash

- **What is hashed:** the replay result's buffer text, `EditorState::bytes()`. That is the LF,
  BOM-less, line-terminated text the editor holds, which is also what `doc_hash` hashes at the start (#690)
  and what the save-time check compares (`trace_check.rs:126`). The cursor, registers, marks, undo tree and
  file format are not hashed.
- **Algorithm:** 64-bit FNV-1a, the existing `trace::doc_hash` (`trace.rs:66`), written as 16 lowercase hex
  digits like `doc_hash`. It adds no dependency to `core`.
- **Where it is recorded:** a header token, `# ruse-trace v1 doc_hash=<h> result_hash=<h>`. `Trace` gains
  `result_hash: Option<u64>`. `Trace::record` and `record_with_options` leave it `None`. `:trace save` sets it
  from the state that `trace_check::check` already replays, after the check passes, so no second replay is
  needed. Because the check compares that state with the live buffer first, the recorded hash is the live
  document's hash. The headless `Headless::trace` (`apps/tui/src/headless.rs:465`) leaves it `None`. The save
  path's check is the one place that has a verified result.
- **What `--replay` does:** `Trace::replay` checks the hash after the last command. If they differ it returns
  `TraceError::ResultMismatch { expected, actual }`. `--replay` handles that as it handles `HashMismatch`:
  stderr names both hashes, stdout gets nothing, and the exit code is 1 (`cli.rs:316-319`). `replay_until`
  (the fuzz bound) skips the check when it stops early. A trace without the token is not checked. The
  pre-RFC-0024 fallback replay (`cli.rs:303-310`) runs on any `HashMismatch` when the file lacks a final
  newline. It checks the token the same way, so a fallback replay that ends somewhere else also fails.
- **The message names the opt-out.** Later parity fixes will change what some commands do, so an old trace
  will mismatch on purpose. The error says so and how to replay anyway:
  `ruse: replay result differs from the recorded one (result_hash expected …, got …); a later build may
  behave differently on purpose: delete the result_hash= token from the header to replay without the check`.
- **The save-time check is structural.** Before its replay, `trace_check::check` checks that the text it
  will write parses back to the same trace: `Trace::from_text(&trace.to_text()) == Ok(trace)` (`Trace` derives
  `Eq`, `trace.rs:30`). It runs before `result_hash` is set. The two failures stay distinct:
  - `from_text` returns `Err(e)`: the written text would not parse at all. One case is an `ex_register` past
    `MAX_REGISTER_BYTES` (`TraceError::TooLarge`). The save is refused with the existing
    `Refusal::NoReplay(e)` (`trace_check.rs:96`), the same refusal a replay error gives today.
  - `from_text` returns `Ok(t2)` with `t2 != trace`: the new `Refusal::NotExact { line }`, naming the first
    line whose command reads back differently (`trace save refused: line 12 does not read back as written`).

  This catches a loss that changes only the cursor, which the text comparison cannot, and any future
  line-form gap. It costs one encode and one parse per save, and a save is not on a keystroke path.

### T3. Versioning

The format stays `v1`. RFC-0020's *Reference Invariants* line "the trace format grows by verbs only" becomes
**"the trace format grows by verbs, and by header tokens that a `v1` reader ignores"**. INV-ADDITIVE
(`docs/invariants/reference-invariants.md:157`) already allows unknown fields that readers ignore. Probed:
`parse_header` (`trace.rs:228-244`) loops over every whitespace-separated token. It acts only on a token that
starts with `v` or `doc_hash=` and ignores the rest. So an older build reads a trace that has `result_hash=`
and replays it without the check. It refuses a trace that has a `line_hex` line with
`BadCommand { UnknownVerb("line_hex") }`, which is R5's honest failure. A constraint follows: no later header
token may start with `v`, because the old reader would take it as the version. Slice 3 writes that rule as a
doc comment on `parse_header` and adds a forward-compatibility test: a `v1` header with an unknown token
(`# ruse-trace v1 doc_hash=… future_token=x`) parses and replays.

## Reference Invariants

- **INV-ADDITIVE** (depends; wording of RFC-0020's use amended, T3): one new verb and one new header token,
  and every existing trace stays valid.
- **INV-FAIL-BOUNDED** (depends): a malformed `line_hex` gives a typed `BadArgument`. A result mismatch gives a
  typed `ResultMismatch`, never a panic.
- **INV-CMD-SEMANTIC** (depends): free text stays a field of the semantic `Command`, and only its spelling in
  the file changes.
- Core stays IO-free (D-039): hashing and the check are pure functions over bytes.

## Failure modes & Recovery

- **Mismatch:** exit 1 and a typed error that names both hashes and the opt-out (delete the token). The
  user can bisect with an older build, because both builds read the same `v1` trace.
- **Save-time text check:** if the text would not parse, the save is refused with `NoReplay(e)`, which names
  the `TraceError`, for example `TooLarge { line }`. If it parses to a different trace, the save is refused
  with `NotExact { line }`. Nothing is written in either case.
- **An older binary given a `line_hex` trace:** `BadCommand` names the line. The replay refuses to start.
- **A trace edited by hand after saving:** `result_hash` no longer matches. That failure is the intent. The
  user deletes the token to replay without the check.
- **FNV-1a collision:** probability about 2^-64 per replay for an accidental divergence. The hash is a
  divergence detector, not tamper evidence (see Security impact).

## Security impact

None new. The hash is not a signature. Anyone who can edit the trace can edit the hash, so no trust decision
may rest on it. `line_hex` hides the text of the line it encodes from a casual reader, which matters for
review. The line holds what the plain form held, so it carries no new data. RFC-0020's caution about
traces carrying document, register and clipboard data still applies.

## Performance impact

- Encode: one `contains` and one `from_line` per line. Hex only for the rare lines that need it.
- Save: one extra `to_text` plus `from_text` (T2), and one FNV-1a pass over the result. FNV-1a is the same
  pass `doc_hash` already makes over the start text.
- Replay: one FNV-1a pass over the result.

None of this runs per keystroke.

## Compatibility & Migration

Measured on main at b9c443e7:

- **Committed trace files: 0.** `git ls-files` has no `*.trace`. The trace tests write temporary files.
- **Inline trace texts in tests: 7** in `crates/core/src/trace.rs` tests (`:386, :438, :450, :504, :575, :597,
  :614`), plus the header assertion `crates/core/tests/editor_spine.rs:185` (`starts_with("# ruse-trace v1
  doc_hash=")`). None of them has a free-text argument that ends in whitespace or holds a line break, and
  neither recommended change alters a plain line or the header prefix. So **0 fixtures change** under T1 + T3.
  Under the rejected v2 options, the header assertion and the RFC-0020 example change, and the inline v1 texts
  still parse.
- **Old traces:** they replay exactly as before, because the plain decoder keeps its trims. A v1 trace whose
  pattern had a trailing space has already lost it. That loss happened when the file was written, so this RFC
  cannot recover it, and it does not try. It cannot tell such a space from one an editor added.
- **API:** `Trace` gains `result_hash: Option<u64>`, and `TraceError` gains `ResultMismatch`. Both are
  in-workspace types, so this is not a published-API break.

## Observability

`--replay` failure messages name the hashes. `:trace save` keeps its message.

## Alternatives

### Decision A: how free text is written exactly

- **A1 (recommended): a conditional `line_hex` wrapper.** It adds one verb, keeps today's readable form for
  almost every line, and covers any free-text verb added later.
- **A2: conditional hex twins per verb** (`search_next_hex`, …, eight verbs). It is exact and additive like A1,
  but it adds eight verbs, and each future free-text verb needs its own twin.
- **A3: always hex in a `v2` format** for all free text. It is consistent with `ex_*` and the pastes. But the
  most common trace lines (searches) become unreadable, every older build refuses every new trace, and the
  reader has to keep the v1 decoder anyway.
- **A4: backslash escapes in a `v2` format** (`\\`, `\n`, `\r`, `\s` for trailing whitespace). It stays
  readable, but every backslash in a Vim pattern (`\<foo\>`) doubles. It also needs a version gate, because
  v1 patterns hold literal backslashes.

### Decision B: what the result hash covers

- **B1 (recommended): buffer text only.** It is symmetric with `doc_hash` and with the save-time check, and it
  is what F-022 and DET-003 promise.
- **B2: text plus a `result_cursor=<byte>` token.** It catches a divergence that only moves the cursor (a
  search that behaves differently, with no edit after it). The value comes from the save-time replay, so it
  cannot refuse a save, but the contract it adds is wider than F-022's. It can be added later as another
  ignored token.
- **B3: a full state digest** (text, cursor, registers, marks, undo). The digest's definition would change
  whenever `EditorState` gains a field. Then every trace saved earlier would fail, or the digest would need
  its own version.

### Decision C: hash algorithm

- **C1 (recommended): FNV-1a 64, the existing `doc_hash`.** It is already in `core`, it is deterministic
  across machines, and there is no adversary to resist.
- **C2: SHA-256 via `sha2`.** The crate is in the tree (`apps/tui/Cargo.toml:52`, DEP-CRYPTO-HASH), but its
  `allowed_layers: []` keeps it out of `core` (`spec/dependencies.yaml:30`). So the check would have to live
  in the frontend, or D-034 would have to change. It gives no real tamper evidence, since the hash sits next
  to what it hashes.
- **C3: a new 128-bit non-cryptographic hash.** It needs a new dependency, or the hand-rolled code that
  DEP-CRYPTO-HASH warns against, to fix a collision rate that is not a problem.

### Decision D: where the result is recorded

- **D1 (recommended): a header token.** Older readers ignore it (probed, T3). It sits next to `doc_hash`, and
  `to_text` writes the whole file at save time, when the result is known.
- **D2: a trailing `# result_hash=…` comment.** Older readers skip it (`trace.rs:150`), and it would suit a
  streaming recorder. But a truncated file silently loses the check, and a comment that carries meaning is
  easy to delete by accident.
- **D3: a verb (`ex_result <h>`).** It would allow checkpoints in the middle of a trace, but older builds
  refuse the whole trace, and a hash is not an edit.

### Decision E: what `--replay` does on a mismatch

- **E1 (recommended): fail like `HashMismatch`.** Exit 1, the typed error on stderr naming the opt-out
  (delete `result_hash=`; later parity fixes make old traces mismatch on purpose), nothing on stdout. A pipe
  (`ruse --replay t f > out`) never gets a result that is known to be wrong.
- **E2: print the result and exit 1.** The user can diff it, but a script that ignores the exit status keeps
  bad output.
- **E3: warn on stderr and exit 0.** This repeats the silent-success bug #1264 reports.

### Decision F: versioning

- **F1 (recommended): stay `v1`**, and amend "grows by verbs only" as in T3. This follows R5 and INV-ADDITIVE.
- **F2: bump to `v2`** for traces that use either feature. An older build then says `UnsupportedVersion` rather
  than `UnknownVerb("line_hex")`, which is a clearer message. But it also refuses traces that only gained the
  ignorable hash, and D-061 makes a `v2` the point to fold all verbs into a new format.

## Rejected approaches

- **Stop trimming** (drop `trim` at `line_form.rs:314` and `trace.rs:149`). It needs no format change and fixes
  the trailing ASCII space. But it does not fix `\n` or a trailing `\r`, and it makes whitespace that editors
  and `git diff --check` strip meaningful. It would also change what a hand-edited v1 trace with a stray
  trailing space replays to.
- **Recording the result hash from the live document without the save-time replay.** It would record a hash
  of a document the trace does not reproduce whenever the check would have refused the save. T2 takes the
  hash only after the check passes.

## Trade-offs

- `line_hex` lines cannot be read in the file. They are rare (searches that end in whitespace or hold a line
  break), and they are exact.
- A result hash makes a trace brittle to hand edits. That is the purpose of the check, and deleting the token
  opts out.
- B1 does not detect a divergence that only moves the cursor. Accepted for now, and B2 is additive if it is
  wanted later.

## Re-evaluation conditions

- A trace `v2` or session persistence lands (D-061): fold `line_hex` into that format's quoting, and make the
  result hash mandatory there.
- `Command` free text becomes bytes (non-UTF-8 patterns): `line_hex` already carries bytes, so only its UTF-8
  check changes.
- Traces become a trust input (a shared corpus that is run on review): the hash must then become a signature
  (C2 or stronger).

## Slice plan

1. **Slice 1 (small, no format change): a structural save check.** `trace_check::check` refuses a trace
   unless `Trace::from_text(&trace.to_text()) == Ok(trace)` (T2, last bullet). A parse error maps to the
   existing `Refusal::NoReplay(e)`. A trace that parses to something else gets the new
   `Refusal::NotExact { line }`, naming the first line that reads back differently. Tests cover both. Add a property test: for arbitrary
   `String`s in each of the eight free-text fields, `from_text(to_text)` gives back the same command. It is
   the failing half of #721 until slice 2.
2. **Slice 2 (#721): `line_hex`.** Add the encoder with the semantic plain-form rule, and the exact decoder
   (T1). The slice-1 property passes. F-022's round-trip acceptance gets the evidence. `Closes #721`.

   **Land slices 1 and 2 together** (one PR, or back to back with no release between). Slice 1 alone newly
   refuses saves (`/foo `) that today write a trace silently wrong. That is more honest, but it is a
   regression for the user in between. The slices stay separable: slice 1 is a frontend check and a test, and
   slice 2 is the core codec, so either can be reverted alone.
3. **Slice 3 (#1264): `result_hash`.** Add the `Trace` field, header token, `TraceError::ResultMismatch` with
   the opt-out message, `:trace save` filling it, and the `--replay` exit (T2, T3). Tests: tamper with one
   command of a saved trace, then `--replay` exits 1 with nothing on stdout; a trace without the token
   replays; a `v1` header with an unknown token parses (forward compatibility, T3), with the "no token may
   start with `v`" rule documented at `parse_header`. Update the RFC-0020 *Reference Invariants* wording and
   add evidence to DET-003. `Closes #1264`.

## Open questions

1. B2: should the cursor be recorded now as `result_cursor=`, or left until a cursor-only divergence is seen?
2. Should a `line_hex` whose inner line would have been written plain be refused, to keep one spelling per
   command? The proposal accepts it, so that hand-written traces work.
