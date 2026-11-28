---
doc: rfc
project: ruse
title: "RFC-0025: File encodings at the persist boundary ('fileencoding', ++enc, C-x RET f)"
summary: >
  ruse reads and writes only UTF-8 (with its BOM) and raw bytes, so a Latin-1, Shift_JIS, EUC-KR or UTF-16
  file shows as invalid bytes (#1292). This RFC adds file encodings at the frontend's read/write boundary
  (apps/tui persist), with encoding_rs for the legacy codecs and ruse's own name table, so the core keeps its
  UTF-8 byte text. It decides the detection order, what happens to undecodable bytes and unencodable
  characters, the Vim and Emacs commands, the journal field and the config keys.
audience: [maintainers, contributors, llm-agents]
status: accepted
related:
  - ../../design/persistence-and-recovery.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/dependencies.yaml
  - ../accepted/RFC-0008-document-model.md
  - ../accepted/RFC-0017-better-than-vanilla-defaults.md
  - ../accepted/RFC-0023-per-transaction-journal.md
  - ../accepted/RFC-0024-line-terminated-text.md
---

# RFC-0025: File encodings at the persist boundary ('fileencoding', ++enc, C-x RET f)

- **Status:** proposed
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-28
- **Decision link:** D-066

<!-- Hard-to-reverse: a new dependency (D-034), an RJ01 field (D-064), and the bytes every non-UTF-8 file is
     written back as. -->

## Summary

A file's encoding becomes part of the shape ruse remembers for each file, beside its line ending, BOM,
`'endofline'` and `'endoffile'` (`FileFormat` in `apps/tui/src/persist/encoding.rs`). A read decodes the file
into UTF-8 before anything else looks at it, and a write encodes the UTF-8 text back after the line endings
are restored. The core is not changed: its text stays UTF-8 bytes in which an invalid byte is allowed and
shown as `<xx>`, as it is today. `'fileencodings'` is tried in order, as nvim does. The default is
`ucs-bom,utf-8`, not nvim's `ucs-bom,utf-8,default,latin1` (the maintainer's decision). When every entry fails,
the file reads as UTF-8 with its invalid bytes kept and shown `<xx>`, as today, so a UTF-8 file with one stray
byte is not turned into Latin-1 mojibake. Latin-1 and legacy files are opened by listing their encoding or with
`++enc`. Legacy codecs come from `encoding_rs`, behind ruse's own name table, which maps
`latin1` to true ISO-8859-1, not to windows-1252 as `encoding_rs` does. Two more defaults that differ from
nvim are proposed and still need the maintainer's approval (Open questions). Both make sure a file never
changes by being read and written back: an undecodable byte in a forced `++enc` read is kept
(`++bad=keep`), not replaced by `?`, and a character the file's encoding cannot hold makes the write fail
(E513) for every encoding, where nvim writes a substitute byte for some encodings.

## Motivation / Problem

ruse has `'fileformat'`, `'bomb'`, `'endofline'`, `'endoffile'` and `'binary'` (#619), but no encoding. Every
file is taken as UTF-8: a Latin-1 `café` shows as `caf<e9>`, a Shift_JIS or EUC-KR file as a row of `<xx>`
cells, and a UTF-16 file as text with a NUL after each letter. The bytes survive a save, because ruse
never converts them, but the text cannot be read or edited. nvim and Emacs both decode these files by default
(the evidence below), and #1292 gathers the census items `nvim.opt.fileencoding` and `nvim.opt.fileencodings`
(moved from #619).

RFC-0008 §6 already says that the encoding is metadata, not data. The core holds decoded text, and the
encoding lives beside the Document. This RFC builds that for encodings, as #619 and RFC-0024 did for line
endings and the final newline.

### Evidence

Probed against nvim v0.12.6 (`nvim --clean --headless`, `:set shm=` so the read and write messages show)
and GNU Emacs 31.1 (`emacs -Q --batch`, `LANG=en_US.UTF-8`) on 2026-11-28. Files were made with Python's
codecs; written files were checked with `xxd`. "Probed" means the line was observed; anything else is marked.

| # | Case | nvim v0.12.6 | Emacs 31.1 |
|---|---|---|---|
| 1 | `café\nnaïve\n` in Latin-1, default options | `fenc=latin1`, `"…" [converted] 2 lines, 13 bytes` (13 is the UTF-8 count; the file has 11) | `iso-latin-1-unix` |
| 2 | `price \x80 5\n` (cp1252 €), default | `fenc=latin1`, the line holds U+0080 | `raw-text-unix` (0x80 is not Latin-1 text to Emacs's detector) |
| 3 | same, `fencs=cp1252,latin1` | `fenc=cp1252`, `€` | n/a |
| 4 | Shift_JIS `日本語`, default | `fenc=latin1`, mojibake (no guessing) | `japanese-shift-jis-unix`: Emacs's coding-category detection finds it |
| 5 | EUC-KR `한국어`, default | `fenc=latin1` | `iso-latin-1-unix`; with `(prefer-coding-system 'euc-kr)`: `korean-iso-8bit-unix`, and the default `buffer-file-coding-system` becomes `korean-iso-8bit` |
| 6 | EUC-KR, `fencs=ucs-bom,utf-8,cp949` | `fenc=cp949`, `한국어` | n/a |
| 7 | UTF-8 with BOM | `fenc=utf-8`, `bomb` | `utf-8-with-signature-unix` |
| 8 | UTF-16LE with BOM | `fenc=utf-16le`, `bomb`, `[converted]`; `:w` writes the BOM back, byte-identical | `utf-16le-with-signature-unix` |
| 9 | UTF-16BE with BOM | `fenc=utf-16` (Vim's `utf-16` is big-endian) | `utf-16be-with-signature-unix` |
| 10 | UTF-32LE with BOM (`FF FE 00 00`) | `fenc=ucs-4le`, `bomb` | `utf-16le-with-signature-unix`, wrong text (Emacs does not sniff UTF-32) |
| 11 | UTF-16LE, no BOM | `fenc=latin1`, NULs in the text | `no-conversion` |
| 12 | ASCII only | `fenc=utf-8` | `undecided-unix` |
| 13 | new file | `fenc=` (empty: `'encoding'`, UTF-8) | default `utf-8-unix` |
| 14 | valid UTF-8 with one stray `\xE9` byte | `fenc=latin1`: the whole file, the valid UTF-8 lines too, reads as mojibake | `iso-latin-1-unix`, same |
| 15 | 14 with `:e ++enc=utf-8` | `[ILLEGAL BYTE in line 2]`, the byte becomes `?`; `:w` writes `?` | n/a |
| 16 | 15 with `++bad=keep` / `drop` / `X` | the byte kept / removed / `X` | n/a |
| 17 | 14 with `fencs=utf-8` or `fencs=` | `fenc=` (empty), `[ILLEGAL BYTE in line 2]`, `?` | n/a |
| 18 | `ok \x81\x20 x` with `fencs=sjis` | every entry fails: `fenc=` (empty), read as UTF-8, `[ILLEGAL BYTE in line 1]`, `?` | n/a |
| 19 | 18 with `:e ++enc=sjis` | `fenc=sjis`, `[converted][CONVERSION ERROR in line 1]`, `?` | n/a |
| 20 | `:e ++enc=bogus` | `[NOT converted][ILLEGAL BYTE in line 1]`, bytes as they are | n/a |
| 21 | Latin-1 buffer, `한` added, `:w` / `:w!` | written: `한` becomes `\xBF`; `CONVERSION ERROR in line 1;` in the write message; the buffer stays modified | silently saved as `utf-8-unix`: the whole file becomes UTF-8 (`select-safe-coding-system` takes the preferred coding system) |
| 22 | same after `C-x RET f iso-latin-1-unix` | n/a | prompts `Select coding system (default utf-8):`; the answer is used |
| 23 | UTF-8 `é 한`, `:set fenc=latin1`, `:w` (existing file) | written with `\xBF`, `CONVERSION ERROR in line 1;`, still modified; also with `'nowritebackup'` | n/a |
| 24 | same with `fenc=sjis` or `cp1252` (iconv) | `E513: Write error, conversion failed (make 'fenc' empty to override)`, `WARNING: Original file may be lost or damaged`; the file is unchanged (the write backup restored it) | n/a |
| 25 | `:w ++enc=latin1 ++bad=keep` | as 21: `++bad` has no effect on a write | n/a |
| 26 | UTF-8 buffer, `:w ++enc=utf-16le new` | the file is UTF-16LE without BOM; `'fenc'` stays `utf-8` | n/a |
| 27 | Latin-1 buffer, `:w ++enc=utf-8` (its own file) | the file becomes UTF-8, `'fenc'` stays `latin1`, the buffer is unmodified | n/a |
| 28 | `:set fenc=utf-16le`, `:w` | `[converted]`, UTF-16LE without BOM (`'bomb'` off), unmodified after | `C-x RET f utf-16le-with-signature-unix` + save: BOM + UTF-16LE |
| 29 | `:set fenc=latin1` | modifies the buffer; `:set fenc=utf-8` back makes it unmodified again | `C-x RET f` modifies; setting it back keeps it modified |
| 30 | `:e ++enc=sjis f`, then `:e!` or `:e` or `:bd` + `:e f` | detected again (`latin1`): `++enc` does not stick | `C-x RET r euc-kr` (`revert-buffer-with-coding-system`) re-reads in that coding, unmodified |
| 31 | `:set fenc=latin1`, `:e!` | detected again (`utf-8`) | n/a |
| 32 | `:e ++bin` of the UTF-16 file | no conversion, `fenc=` (empty) | n/a |
| 33 | Latin-1 buffer, `:w !xxd`, `:1!xxd` | the command gets UTF-8 (the buffer's text), not Latin-1 | n/a |
| 34 | `:r latin1.txt` into a UTF-8 buffer; `:r ++enc=cp1252 f` | each read file detected or forced and converted to the buffer's text; the buffer's `'fenc'` stays `utf-8` | n/a |
| 35 | `:set fenc=` names | `LATIN1`, `iso-8859-1` → `latin1`; `utf8` → `utf-8`; `shift_jis` → `sjis`; `ucs2` → `ucs-2`; `UTF-16LE` → `utf-16le`; `euc-kr`, `windows-1252`, `8bit-cp1252`, `foo` kept as given | `latin-1`, `iso-8859-1` → `iso-latin-1`; `cp1252` → `windows-1252` |
| 36 | `C-x RET f`, `C-x RET c`, `C-x RET r` | n/a | `set-buffer-file-coding-system`, `universal-coding-system-argument`, `revert-buffer-with-coding-system` |
| 37 | `C-x RET f utf-8-with-signature-dos` + save | n/a | `EF BB BF … 0D 0A`: the suffix sets the line ending too |
| 38 | EUC-KR `한국어`, `fencs=ucs-bom,utf-8,cp949` | `fenc=cp949`, `[converted]` (case 6) | n/a |
| 39 | cp949 with UHC-only syllables `갂똠 한국어`, same list | `fenc=cp949`, read correctly | n/a |
| 40 | same list, UTF-8 with one stray byte: `bad \xE9 byte`; Korean text + a `\xFF` line; `한국\x80어 텍스트`; `안녕하세요 caf\xE9 세계`; `caf\xE9s ok` | cp949 fails on every one; all entries fail: `fenc=` (empty), `[ILLEGAL BYTE in line N]`, the byte becomes `?` | n/a |
| 41 | same list, `\xE9` + UTF-8 `마바사바다` (a stray byte that realigns the UTF-8 bytes into valid cp949 pairs; found by searching 2-word Korean strings, 169 of them decode) | cp949 accepts it: `fenc=cp949`, `[converted]`, the text is `雨쭏諛붿궗諛붾떎`, garbage with no warning; `:w` writes the bytes back identically | n/a |
| 42 | EUC-KR file, `fencs=ucs-bom,utf-8` | all entries fail: `fenc=` (empty), `[ILLEGAL BYTE in line 1]`, `?` for each invalid byte | n/a |

`encoding_rs` 0.8.44 (`cargo info`; probed with a scratch program on 2026-11-28): license
`(Apache-2.0 OR MIT) AND BSD-3-Clause`, `rust-version` 1.88 (the workspace's is 1.97), dependencies `cfg-if`,
`scopeguard`, `multiversion_no_op`, `simdutf8` (the `simd-accel`, `serde`, `any_all_workaround`, `rustversion` features off).
Its labels are the WHATWG Encoding Standard's:

| Label | `Encoding::for_label` gives | What Vim means |
|---|---|---|
| `latin1`, `iso-8859-1`, `l1` | windows-1252 | ISO-8859-1 |
| `utf-16` | UTF-16LE | UTF-16BE |
| `ucs-2` | UTF-16LE | UCS-2BE, its own encoding: no surrogate pairs |
| `ucs-4`, `utf-32` | none | UTF-32BE |
| `cp932`, `cp949`, `cp936`, `cp950`, `uhc`, `cp437` | none | iconv names |
| `latin9` | none | `iso-8859-15` |
| `euc-kr`, `windows-949`, `ks_c_5601-1987` | EUC-KR (decodes the UHC/cp949 extension too: `81 41` is `갂`) | `euc-kr` |

Other facts: `windows-1252` decodes `0x80` as `€` and `0x81` as U+0081, and cannot encode U+0080
(`Unmappable`). The UTF-16 encodings' output encoding is UTF-8, so `encoding_rs` cannot write UTF-16.
`Encoding::for_bom` reads `FF FE 00 00` as UTF-16LE. `decode_to_utf8_without_replacement` reports
`Malformed(bad_len, consumed_after)` with its position, and `encode_from_utf8_without_replacement` reports
`Unmappable(char)`. ISO-2022-JP encodes.

## Guide-level explanation

A UTF-8 file, with or without a BOM, opens as today. A UTF-16 or UTF-32 file with a BOM opens as text and
saves in the same encoding with the same BOM. A file that is not UTF-8 opens as it does today, with its
invalid bytes shown `<xx>` and written back unchanged, until its encoding is listed
(`files.fileencodings = "ucs-bom,utf-8,cp949"`, or `…,latin1`) or given for one read (`:e ++enc=latin1`).
Then a Latin-1 file shows `café`, the read message says `"f" [converted] 2 lines, 13 bytes` as nvim's does,
and saving it, modified or not, writes Latin-1. The status line shows the encoding when it is not UTF-8:
`f.txt [latin1]`.

`:set fenc=utf-16le` changes the encoding the next `:w` writes, and modifies the buffer. `:w ++enc=utf-8`
writes one file in UTF-8 and leaves `'fenc'` as it was. As proposed (§4.1, open), typing `한` into a Latin-1 buffer and saving fails
with `E513: Write error, conversion failed in line 1 (make 'fenc' empty to override)`. The file on disk is
not touched and the buffer stays modified; `:set fenc=` (UTF-8) or `:set fenc=utf-8` lets the write go
through.

In the Emacs profile, `C-x RET f` sets the buffer's coding system (`utf-8-dos` sets the line ending too),
`C-x RET r` reads the file again in a coding system, and `C-x RET c` gives a coding system to the next
command (`C-x RET c sjis RET C-x C-f`). Saving a character the coding system cannot hold switches the buffer
to `emacs.prefer_coding_system` without asking, unless the coding system was chosen with `C-x RET f` or
`C-x RET c`; then it asks `Select coding system (default utf-8):`, as Emacs does.

## Reference-level explanation

### 1. Where the encoding lives

The encoding is a field of `FileFormat` (the per-file shape, `apps/tui/src/persist/encoding.rs`), next to
`eol`, `bom`, `final_newline`, `eof` and `binary`. Only that module and a new codec module beside it
(`persist/codec.rs`, slice 1) know encodings. Every `encoding_rs` type stays inside the codec module (D-034
Adapter): the rest of ruse names an encoding by its canonical name, a string from §2's table. The empty name
means "UTF-8, invalid bytes kept as bytes": the only encoding ruse has today, and the meaning of a journal
or a format with no encoding (§7).

The core is unchanged. Its text is UTF-8 bytes in which a byte that is not part of a valid UTF-8 sequence is
allowed (rendered `<xx>` by `cells.rs`, counted as one cell, written back as it is). That is the text after
decoding, so D-042 (core stays dependency-free), D-065 (line-terminated text) and INV-TEXT-TERMINATED hold
as they are. The two existing pipelines get one more step at each end:

- **Read:** raw bytes → the BOM, if the list allows it (§3) → decode to UTF-8 (§3) → the existing line-ending,
  `'endoffile'` and `'endofline'` detection on the decoded text (`FileFormat::detect_with`,
  `FileFormat::reading`) → `to_buffer`. Line endings are found after decoding, because in UTF-16 or UTF-32 a
  newline is not the byte `0A`.
- **Write:** buffer text → `to_disk` without the BOM (line endings, `'endofline'`, CTRL-Z) → encode (§4) →
  the encoding's BOM when `'bomb'` is set. `WriteOpts::encode` stays the one encoder for `:w`, `:w >>`,
  `:up`, `:wq`, `:sav`; `:w !cmd`, filters and `'formatprg'` get the UTF-8 text without encoding, as nvim
  (evidence 33).

`'binary'` (`++bin`, `ruse -b`, `find-file-literally`) does no conversion either way, and its `'fenc'` is
empty (evidence 32).

### 2. Names and codecs

ruse has one name table, in the codec module. Each row has a canonical name (what `:set fenc?` shows),
the Vim aliases nvim folds to it (evidence 35), the Emacs coding-system names (§6), and a codec. Lookup is
case-insensitive, with `_` the same as `-`. A name nvim keeps as given (`l1`, `windows-1252`, `cp932`, `uhc`,
`macintosh`) is a canonical name of its own that shares its row's codec, so `:set fenc?` shows it as given.

| Canonical | Aliases (Vim / Emacs) | Codec |
|---|---|---|
| `utf-8` | `utf8` / `utf-8`, `mule-utf-8` | std (validation only) |
| `latin1`, `l1` | `iso-8859-1`, `latin-1`, `ansi` / `iso-latin-1`, `iso-8859-1`, `latin-1` | own: byte = code point (ISO-8859-1) |
| `cp1252`, `windows-1252` | / `windows-1252`, `cp1252` | `encoding_rs` windows-1252 |
| `utf-16`, `utf-16le` | `utf-16be`, `utf16`, `utf16le` / `utf-16be`, `utf-16le` (+ `-with-signature`) | own, both directions |
| `ucs-2`, `ucs-2le` | `ucs2`, `ucs-2be`, `unicode`, `ucs2le` / | own, both directions (the UTF-16 code without surrogate pairs) |
| `ucs-4`, `ucs-4le` | `utf-32`, `utf-32le` / `utf-32be`, `utf-32le` | own, both directions |
| `sjis`, `cp932` | `shift_jis`, `pck` / `japanese-shift-jis`, `shift_jis`, `cp932` | `encoding_rs` Shift_JIS |
| `euc-jp`, `iso-2022-jp` | / `japanese-iso-8bit`, `iso-2022-jp` | `encoding_rs` |
| `cp949`, `euc-kr`, `uhc` | / `korean-iso-8bit`, `euc-kr`, `cp949` | `encoding_rs` EUC-KR (§2.1) |
| `cp936`, `gb18030`, `big5`, `cp950` | `gbk` / `chinese-gbk`, `gbk`, `chinese-gb18030`, `big5`, `cp950` | `encoding_rs` GBK, gb18030, Big5 |
| the other WHATWG single-byte sets: `iso-8859-N`, `koi8-r`, `koi8-u`, `cp125N`, `windows-125N`, `macroman`, `macintosh`, … | `latin2` … `latin9` (`latin9` is `iso-8859-15`), `mac`, … / | `encoding_rs` |

**`latin1` is ISO-8859-1** (the maintainer's decision on #1292). `encoding_rs` maps the `latin1`,
`iso-8859-1` and `l1` labels to windows-1252, because that is what the Web does. ruse never passes a user's
name to `Encoding::for_label`: the table is looked up first, and its `latin1` row is ruse's own codec, in
which every byte is the code point of the same value. So `latin1` decodes every file without an error, and
`0x80` reads as U+0080, as in nvim (evidence 2). `cp1252` (`windows-1252`) is a separate row, which reads it
as `€` (evidence 3). An encoding is chosen only by its row, never by `for_label` alone, so the
WHATWG-only traps (`utf-16` is little-endian there, `ucs-2` too) cannot reach the user either.

The hand-written codecs are about 60 lines: ISO-8859-1 both ways, UTF-16BE/LE both ways (with surrogate
pairs; UCS-2 shares the code without them: a `ucs-2` row refuses a character above U+FFFF as unencodable and
does not decode a surrogate in the file), and UTF-32BE/LE both ways. `encoding_rs` does no UTF-16. Each is tested
against Python's codecs on the evidence files.

**2.1 Names that are approximations.** These are recorded divergences:

- `cp949`, `euc-kr`: `encoding_rs`'s EUC-KR is windows-949. It reads both. A write in `euc-kr` can produce a UHC
  (cp949) byte pair for a hangul syllable that strict EUC-KR lacks, where iconv's `euc-kr` would refuse it.
- `sjis` is windows-31j (cp932), and `gbk` is cp936, as the WHATWG defines them. They are supersets of the
  strict sets.
- ISO-2022 variants other than ISO-2022-JP, EBCDIC, and the iconv-only names (`cp437`) are not offered.
  `latin9` works: nvim folds it to `iso-8859-15`, and so does ruse.

**2.2 Unknown names.** nvim accepts any `'fenc'` and passes it to iconv; an unknown one reads and writes
`[NOT converted]` (evidence 20). ruse has no iconv, so an unknown name is refused where it is given:
`:set fenc=foo` and `:e ++enc=foo` fail with `E474: Invalid argument: fenc=foo`, and the file is not read. An
unknown entry in `'fileencodings'` is skipped on read, and `:set fencs=` and the config loader warn about it
(a config warning, D-062 provenance).

### 3. Reading

`'fileencodings'` (global; config `files.fileencodings`) is tried in order. Each entry:

- `ucs-bom`: if the file starts with a BOM (UTF-32 BOMs checked before UTF-16, so `FF FE 00 00` is `ucs-4le`,
  evidence 10; then UTF-16BE/LE, then UTF-8), decode in that encoding, strip the BOM and set `'bomb'`.
- `utf-8`: accept the file when it is valid UTF-8. The decoded text is the file's bytes, so nothing is copied.
- `default`: the same as `utf-8`, since ruse's `'encoding'` is always UTF-8.
- any other name: accept when the codec decodes the whole file with no malformed sequence.

The first entry that accepts sets `'fenc'` (`utf-8`, or the name; a pure-ASCII file is `utf-8`, evidence 12).
When every entry fails, the file is read as UTF-8 with `'fenc'` empty, as in nvim (evidence 17, 18, 42).
Its invalid bytes are kept as bytes, shown `<xx>` and written back as they are. That is ruse's behaviour
today, and it differs from nvim, which turns them into `?`. This was decided by the maintainer together with
the default below; §3.1's `++bad` default for forced reads is a separate open point.

**Default: `ucs-bom,utf-8`.** This is a deliberate divergence from nvim's `ucs-bom,utf-8,default,latin1`,
decided by the maintainer. The reason: with a final `latin1`, any file that is not strictly valid UTF-8 is
decoded as Latin-1 as a whole, so one stray byte in a UTF-8 file turns all of its text into mojibake without
an error. For Korean text every syllable becomes three Latin-1 characters (evidence 14 shows it for `é`).
nvim and Emacs both do this. Under `ucs-bom,utf-8` the same file shows its text correctly and the stray byte
as one `<xx>` cell. A Latin-1 or legacy file is opened by listing its encoding in `files.fileencodings` or by
`:e ++enc={name}`.

The round trip holds under this default too. F-008 says an unmodified buffer writes back its exact bytes.
Every file is accepted (a BOM encoding, valid UTF-8, or the all-fail UTF-8 read). In the all-fail read every
byte that is not part of a valid UTF-8 sequence is kept in the buffer and written as it is (§4), and the
valid sequences are the file's own bytes, so read then write gives the file back. This is today's read
path. It is checked by the existing property test `an_unmodified_buffer_writes_back_its_bytes`, whose
alphabet includes `\xFF`, NUL and the BOM bytes (the 12 `persist::encoding` tests pass on this branch), and
slice 2 adds the encodings to it.

With a listed encoding the round trip holds because an entry accepts a file only when the decode is exact:
no malformed sequence, and, for a multi-byte set, the decoded text encodes back to the file's bytes
(`FileEncoding::decode_exact`, slice 1). A clean decode alone is not enough. `encoding_rs`'s multi-byte
decoders accept sequences their encoders do not give back; counting two-byte sequences over all byte pairs
(`encoding_rs` 0.8.44): Shift_JIS 2278 (for example `87 90` → `≒` → `81 E0`), Big5 3941, gb18030 130,
EUC-JP 10 plus every JIS X 0212 triple, GBK 2, and some ISO-2022-JP escape forms. EUC-KR (cp949) and every
single-byte set round-trip exactly. Such a file fails its `'fileencodings'` entry and falls through to the
next one. What a forced `++enc` read of it does (keep the decode and warn, or refuse) is open for slice 2
(Open questions).

It does not guess. A list with `sjis` or `cp949` would read some files of another encoding as CJK, because both
accept many high-byte sequences. The result then depends on the content, so such an encoding is listed only by
the user who has those files.

**For legacy Korean files: `files.fileencodings = "ucs-bom,utf-8,cp949"`** (`cp949` also reads EUC-KR, which
it contains, §2.1). Probed with nvim v0.12.6 under that list (evidence 38–41): an EUC-KR file and a cp949
file with UHC-only syllables are detected as `cp949` and read correctly. A UTF-8 file with one stray byte
usually still falls through to the UTF-8 read: cp949 rejected each of five such files (a stray byte at the
end, in the middle of Korean, next to ASCII), because UTF-8's three-byte syllables seldom pair up into valid
cp949 two-byte codes. But not always. A stray byte that realigns the UTF-8 bytes into valid pairs is accepted:
`\xE9` before UTF-8 `마바사바다` reads as `cp949` `雨쭏諛붿궗諛붾떎`, garbage with no warning except the
`[cp949]` status flag and `[converted]` in the read message. A search over two-word Korean strings with one
stray byte found 169 that cp949 accepts. The bytes still write back identically (probed), so nothing is lost,
but the text is wrong until `:e ++enc=utf-8` reads it as UTF-8 with the stray byte kept. The list is
recommended only for trees that hold legacy Korean files.

**`++enc={name}`** (`:e`, `:e!`, `:r`, `:sp`, `:new`, `:vs`, `:tabe`) forces one encoding for that read: no BOM sniffing other than that encoding's own BOM, no fallback.
It does not stick: a later `:e`, `:e!` or `'autoread'` reload detects again (evidence 30), and so does `:e!`
after `:set fenc=` (evidence 31). `:r` and `:r ++enc=` decode the read file the same way, and the target
buffer's `'fenc'` does not change (evidence 34). `ruse --replay` and `headless` read with the default list.

**3.1 Undecodable bytes.** A byte sequence the chosen encoding cannot decode is a conversion error (forced
`++enc`, or the all-fail UTF-8 read). `++bad=` says what it becomes:

- `++bad=keep`: the bytes go into the buffer as they are (invalid UTF-8, shown `<xx>`) and are written back as
  they are (§4). **Proposed as ruse's default** (open point, needs the maintainer's approval); it differs
  from nvim's.
- `++bad=drop`: removed. `++bad={c}` (one single-byte character): replaced by `{c}`. These are as in nvim
  (evidence 16).

nvim's default is `?` (evidence 15, 17, 18), so a read followed by a `:w` replaces the byte in the file without
saying so at the write. ruse keeps it, so the write gives back what was read; this is the RFC-0017 rule (a
better default where nvim's only cost is silent data loss), applied in the frontend. The read message is
nvim's: `[ILLEGAL BYTE in line N]` for invalid UTF-8, `[converted][CONVERSION ERROR in line N]` for another
encoding (evidence 15, 19), N the first such line. `Malformed`'s position gives the byte offset, and the line
is counted from the decoded text.

### 4. Writing

The text is encoded in `'fenc'` (empty: UTF-8). A byte of the text that is not valid UTF-8 (a kept byte, §3.1)
is written as it is, in every encoding. The write message adds `[converted]` when `'fenc'` is neither empty
nor `utf-8`, before `[New]`, and the byte count is the buffer's, as nvim's (evidence 1, 8). `'bomb'` writes
the BOM of `'fenc'` when it has one (UTF-8, UTF-16, UTF-32), and nothing for the others.

**4.1 Unencodable characters: the write fails** (proposed, open point needing the maintainer's approval). If the text holds a character `'fenc'` cannot encode, nothing
is written. The write reports `E513: Write error, conversion failed in line N (make 'fenc' empty to
override)` (nvim's message, with the line of the first such character) and returns before the backup, the
temporary file and the rename (`persist/atomic.rs`). The file is not touched, and the buffer stays modified.
ruse encodes into memory before it opens the file, so there is no "original file may be lost" case. This
covers `:w`, `:w!`, `:w ++enc=`, `:sav`, `:up`, `:wq`, `:x`, `:wa` (which reports per buffer and goes on),
and the journal's save points do not move.

nvim does this only for its iconv encodings (evidence 24). For latin1, UTF-16 and UCS-4, its own converters,
it writes `\xBF` or `?` in place of the character and reports `CONVERSION ERROR in line N;` (evidence 21,
23). That is an accident of two code paths, not a choice a user makes, and the result is a file that has lost
characters while the buffer still shows them. ruse refuses in every encoding. `:w!` does not override it,
because nvim's `:w!` does not either (evidence 21, 23 are the same with and without `!`). The override is
nvim's own advice: `:set fenc=` or another encoding.

**4.2 The options.**

- `'fileencoding'` / `'fenc'` (buffer-local, string): the encoding of the buffer's file, set by every read
  (§3). `:set fenc={name}` takes a §2 name (E474 otherwise), stores the canonical name, and modifies the
  buffer. The buffer is unmodified again when `'fenc'` returns to the value it was read or last written with
  (evidence 29, nvim's `b_start_fenc`), so `'fenc'` joins `'fileformat'`, `'bomb'`, `'endofline'` and
  `'endoffile'` in the "file shape differs" check, which `Document::mark_unsaved` serves today. A successful
  write makes the written value the new start value. In a `'nomodifiable'` buffer `:set fenc=` fails with E21,
  as `:set ff=` does. `:setglobal fenc=` (config `files.fileencoding`) is the encoding of new buffers; empty
  is UTF-8, as nvim's.
- `'fileencodings'` / `'fencs'` (global, comma list, config `files.fileencodings`): §3.
- `:w ++enc={name} [file]` writes in that encoding and leaves `'fenc'` unchanged, also when it writes the
  buffer's own file, which then becomes unmodified (evidence 26, 27, as nvim). `++bad` on a write is accepted
  and ignored (evidence 25).
- `:e ++enc`, `:w ++enc` and `:set fenc` use one parser with `++ff` and `++bin`
  (`apps/tui/src/app/file_cmds.rs`).

### 5. Status line and messages

The status-line flags (`status_flags`, after `[RO]`) gain `[{fenc}]` when `'fenc'` is set and is not `utf-8`:
`[latin1]`, `[utf-16le]`, `[sjis]`. A UTF-8 file shows nothing, a BOM shows nothing. nvim's default status line
shows no encoding, and Emacs shows a one-letter mnemonic (`U`, `1`, `S`, `-` for undecided) on every buffer.
The flag is ruse's, shown only for the case that changes what a save writes. The Emacs profile shows the
same flag with the Emacs name (`[iso-latin-1]`). `CTRL-G` / `:file` do not show the encoding, as in nvim (it
showed none in the probe). The read and write messages carry `[converted]`, `[ILLEGAL BYTE in line N]`,
`[CONVERSION ERROR in line N]` as in §3.1 and §4.

### 6. The Emacs profile

The Emacs profile uses the same codecs, the same `FileFormat` field and the same write refusal underneath;
these are its commands and its one config key.

- **`emacs.prefer_coding_system`** (config, default `utf-8`): `prefer-coding-system`. The Emacs profile reads
  with the list `ucs-bom, {prefer}, utf-8` (BOM first, then the preferred system, then UTF-8; when all fail,
  the UTF-8 read of §3 with the bytes kept, the same divergence as the Vim default; Emacs would read such a
  file as Latin-1, evidence 14) instead of `files.fileencodings`, and new buffers get `{prefer}` (evidence 5: preferring `euc-kr` made the
  EUC-KR file read as `korean-iso-8bit` and the default `buffer-file-coding-system` follow).
- **`C-x RET f`** (`set-buffer-file-coding-system`): reads a coding system in the minibuffer (§2's Emacs names,
  with completion) and sets it for the next save; it modifies the buffer, and setting it back does not unmodify
  it (evidence 29, unlike nvim). An `-unix`, `-dos` or `-mac` suffix sets `'fileformat'` too, and
  `-with-signature` sets the BOM (evidence 37).
- **`C-x RET c`** (`universal-coding-system-argument`): reads a coding system, then a key sequence, and runs
  that command with the coding system for its reads and writes: `C-x C-f` reads with it as `:e ++enc=` does,
  `C-x C-s` and `C-x C-w` write with it and keep it (Emacs's `coding-system-for-write`; the keeping is
  not probed, see Open questions).
- **`C-x RET r`** (`revert-buffer-with-coding-system`): reads the file again in that coding system, as
  `:e! ++enc=` (evidence 30). It asks `Revert buffer from file …? (yes or no)` first, as `revert-buffer`
  does (probed: Emacs asked even for an unmodified buffer).
- **Saving an unencodable character:** Emacs's `select-safe-coding-system` (`mule-cmds.el`, read for this
  RFC) tries the buffer's coding system, the default one and the preferred one, and takes the first that can
  encode the text without asking (evidence 21: the Latin-1 file was saved as UTF-8). It asks
  `Select coding system (default utf-8):` when the coding system was chosen explicitly
  (`buffer-file-coding-system-explicit`, set by `C-x RET f` and `C-x RET c`) (evidence 22). The Emacs
  profile does the same: without an explicit choice the buffer switches to `emacs.prefer_coding_system` and
  names it in the save message (`(utf-8-unix)`, a ruse addition; the batch probe printed nothing,
  and Emacs's interactive message was not probed); with one it asks in the
  minibuffer, `C-g` cancels the save, and an answer that still cannot encode the text gives §4.1's error.
  The silent switch is Emacs's default. ruse keeps it in the Emacs profile, because a user of that profile
  expects it.

Recorded divergences: Emacs's coding-category detection is not modelled (Emacs finds Shift_JIS by itself,
evidence 4, and reads cp1252 as `raw-text`, NULs as `no-conversion`, evidence 2 and 11); the `-*- coding: -*-`
cookie and `coding:` in local variables, `file-coding-system-alist`, `auto-coding-alist`, `C-x RET F`
(file names), `C-x RET t`/`k` (terminal and keyboard: ruse's are UTF-8) and the ISO-2022 variants beyond
ISO-2022-JP are not offered; `raw-text` and `no-conversion` map to `'binary'`.

### 7. The journal (RJ01 schema 1.2)

The journal's text is the buffer's text (UTF-8), so its Edit and Checkpoint records do not change. What it
must record is the encoding, or a recovered Latin-1 buffer would be saved as UTF-8. This amends RFC-0023 the
way its Amendment 1 did, strictly by appending (INV-ADDITIVE):

- **Header** and **Meta** file-format payloads: after `format_ext u8`, an `enc bytes` field (`u32` length and
  the canonical name, §2; empty for UTF-8 with kept bytes). `schema_ver` becomes 1.2. The `bom` bit of the
  1.0 `format` byte now means the BOM of that encoding.
- A `:set fenc=` appends a Meta format record, as `:set ff=` does.
- **Recovery** takes the encoding the journal last recorded over the disk file's (RFC-0023 Amendment 1): a
  buffer recovered from a Latin-1 file writes Latin-1, whatever the file now detects as.
- **`DiskBase`** (above 32 MiB) hashes the disk file's text decoded with the Header's format, which now
  includes the encoding, so the check compares like with like.

**Compatibility.** A 1.2 reader reads a 1.1 or 1.0 record with an empty encoding. That is what those builds
did, raw bytes as UTF-8 with invalid bytes kept, so the recovered buffer writes back the same bytes. A
1.1 reader skips the appended field (newer minor, `payload_len`) and recovers the text but saves it as UTF-8,
which breaks only after a downgrade with an unsaved non-UTF-8 buffer. That is accepted and written in the
1.2 amendment. A `DiskBase` journal written by a build before slice 2 for a large non-UTF-8 file hashed the
raw bytes; after the upgrade a file whose encoding is now listed decodes differently, the hash differs, and recovery reports the journal
unreplayable and keeps it (RFC-0023 §5): safe, and it needs a file over 32 MiB, not UTF-8, with unsaved
changes, across an upgrade.

### 8. Spec changes the slices make

- **`spec/dependencies.yaml`:** `DEP-ENCODING` (this PR, D-034 "entry first"): `encoding_rs`, wrapped, tier 2,
  frontend only (`allowed_layers: []`), no public exposure. Slice 1 adds the crate with
  `default-features = false, features = ["alloc"]` and adds `BSD-3-Clause` (the WHATWG index data) to
  `deny.toml`'s allow list, which an MIT binary can carry with its notice. The notice goes into the
  third-party license list that release packaging ships.
- **`spec/PRD.yaml` F-008:** a v0.2.4 note for each slice: `'fileencoding'`, `'fileencodings'`, `++enc`,
  `++bad`, the messages, E513, the status flag, the Emacs commands and `emacs.prefer_coding_system`, the
  RJ01 1.2 field, and the divergences of §2.1, §2.2, §3.1, §4.1 and §6.
- **`spec/capabilities.yaml`:** a new `CAP-FILE-ENCODING` (base, service, `implementation: wrapped`,
  `requires: [CAP-FS]`, `prd: [F-008]`).
- **`spec/config-schema.yaml`:** `files.fileencodings` (string, default `ucs-bom,utf-8`, a recorded divergence from
  nvim, user scope like `files.fileformats`, runtime `:set fencs`); `files.fileencoding` (string, default empty, the
  encoding of new buffers, runtime `:setglobal fenc`); `emacs.prefer_coding_system` (string, default `utf-8`,
  Emacs profile only). All `derives_from: [F-008]`. A name outside §2's table is a config warning, as an
  unknown `files.fileformats` value is.
- **Census:** `nvim.opt.fileencoding`, `nvim.opt.fileencodings` and `emacs.command.set-buffer-file-coding-system`,
  `universal-coding-system-argument`, `revert-buffer-with-coding-system`, `prefer-coding-system` are classified
  when the slice that adds each one lands. Each slice's PTY tests assert the probed bytes and messages above.
- **RFC-0023** gets "Amendment 2 — schema 1.2" in slice 1; `docs/design/persistence-and-recovery.md` §4's
  "write encoded bytes" step points here.

### 9. Slices

1. **Codec and journal, no user-visible change** — slice 1 (#1292): the dependency, `deny.toml`, the name
   table and codecs (`persist/codec.rs`) with tests against the evidence files, the `FileFormat` field, the
   read and write steps of §1 with the field always empty, and RJ01 1.2 (§7). Every file still reads as today,
   so a build of this slice cannot write a file in a different encoding than it read it. Journals carry the
   field before any buffer can have a non-empty one.
2. **The Vim surface** — slice 2 (#1292): `'fenc'`, `'fencs'` and their config keys, detection (§3) with
   the default list, `++enc` / `++bad` on the read commands, `:w ++enc`, §4.1's refusal, the messages, the
   status flag, `CAP-FILE-ENCODING`, the PRD note, the two `nvim.opt.*` census items.
3. **The Emacs profile** — slice 3 (#1292): `C-x RET f` / `c` / `r`, `emacs.prefer_coding_system`, §6's save
   behaviour and the Emacs names, the `emacs.command.*` census items.

Slice 2 depends on slice 1, and slice 3 on slice 2.

## Reference Invariants

- **F-008 byte-preserve:** an unmodified buffer writes back its exact bytes. Under the default list every
  file is accepted (BOM, valid UTF-8, or the all-fail UTF-8 read with its invalid bytes kept), and kept bytes
  are written as they are (§3, §4), so the property holds for every file;
  the round-trip property test gains the encodings. A listed encoding accepts a file only when it decodes
  exactly (§3); a forced `++enc` read in a multi-byte set that does not is open for slice 2.
- **INV-TEXT-TERMINATED (D-065):** unchanged; line endings are found after decoding.
- **INV-ADDITIVE (RFC-0023):** RJ01 1.2 only appends a field.
- **INV-TXN / INV-UNDO:** `:set fenc=` is file-shape metadata, like `:set ff=`: not a transaction, and it
  modifies the buffer through the same path.
- **D-034 Adapter:** no `encoding_rs` type leaves `persist/codec.rs`.
- **D-042:** the core gains no dependency and no code.

## Failure modes & Recovery

- **A character the encoding cannot hold:** the write fails before any file is opened (§4.1); the buffer and
  its journal keep the text.
- **Undecodable bytes:** kept and written back (§3.1), with the read message naming the first line.
- **Kept bytes that form valid UTF-8 together:** two adjacent kept bytes can, in principle, form a valid UTF-8
  sequence (a malformed pair in a forced `++enc` read), and a write then encodes that character instead of
  passing the bytes through. nvim's `++bad=keep` has the same limit; it needs a forced `++enc` that does not
  match the file. Not mitigated.
- **A wrong forced encoding** (`:e ++enc=sjis` of a Latin-1 file) reads mojibake with conversion errors,
  kept; `:e!` reads it again with detection (evidence 30).
- **Downgrade with an unsaved non-UTF-8 buffer:** §7, the text recovers and saves as UTF-8.

## Security impact

Decoding is of the same untrusted file bytes ruse already reads, bounded by the existing size checks (the
large-file profile, Emacs `large-file-warning-threshold`). `encoding_rs` is fuzzed upstream and used by
Firefox; ruse calls only its safe API. No name from a file chooses an encoding (no `coding:` cookie, §6),
so a file cannot switch how it or another file is written. The supply-chain cost is one crate and four
small dependencies (`cfg-if` and `scopeguard` are already in the tree; `multiversion_no_op` and `simdutf8`
0.1.5, MIT OR Apache-2.0, are new).

## Performance impact

A UTF-8 file costs one validation pass (`std::str::from_utf8`), and its bytes are used as they are. A legacy file costs a decode on read and an encode on write,
linear in its size. `encoding_rs` without `simd-accel` decodes at hundreds of MB/s, which is not
measured here and is to be checked with the existing `persist` benchmarks in slice 1. No cost per keystroke.

## Compatibility & Migration

- Files: with the default list nothing changes for a UTF-8 file or a file that is not UTF-8 (it opens as today,
  `<xx>` bytes kept); a UTF-16/UTF-32 file with a BOM now opens as text. Listing an encoding is opt-in.
- RJ01: §7. No record is rewritten.
- Traces: a trace's start-text hash recorded before slice 2 on a non-UTF-8 file hashed the raw bytes.
  `ruse --replay` tries the decoded text, then the raw text, as it tries the pre-RFC-0024 text today.
- LSP: `didOpen` / `didChange` already send the buffer's UTF-8 text, which for a legacy file is now the decoded
  text the server can read; kept invalid bytes still go lossy, as today.
- Config: three new keys; `editor.line_endings` is untouched.

## Observability

The status flag and the read/write messages (§5). `:checkhealth` lists the supported encodings and whether
any `files.fileencodings` entry was unknown. The `RUSE_LOG` trace logs the detected encoding per read at
`debug`.

## Alternatives

- **System iconv** (nvim's way): names and error modes differ per platform, it is absent on Windows, and
  every call is unsafe FFI. Its one advantage is more encodings.
- **The `encoding` crate:** unmaintained since 2016.
- **Guessing (`chardetng`), or Emacs's coding-category detection:** neither nvim nor ruse guesses; Emacs's
  detector reads EUC-KR as Latin-1 and cp1252 as raw text even so (evidence 2, 5). Detection by a list the user
  controls is predictable. Guessing can be an explicit command later.
- **Decoding in the core** (a text type that knows its encoding): the core would need a dependency (D-042)
  and every position would be in a second unit; RFC-0008 §6 already put encodings outside.
- **nvim's write behaviour** (a substitute byte for its own encodings, E513 for iconv's): rejected in §4.1.
- **nvim's `++bad` default `?`:** rejected in §3.1 (open point).
- **nvim's default `ucs-bom,utf-8,default,latin1`:** rejected by the maintainer (§3): a stray byte turns a
  UTF-8 file into Latin-1 mojibake.

## Rejected approaches

- Passing user names to `encoding_rs::Encoding::for_label`: `latin1` would silently be windows-1252 and
  `utf-16` little-endian (Evidence, label table).
- Recording the encoding only in the journal's Meta records and not the Header: a journal must say its
  encoding before its first Edit, so recovery never decodes with a guess.

## Trade-offs

- **The default is not nvim's** (`ucs-bom,utf-8` vs `ucs-bom,utf-8,default,latin1`, maintainer's
  decision): a UTF-8 file with a stray byte keeps its text, at the price that a Latin-1 or legacy file shows
  `<xx>` until its encoding is listed or given with `++enc`. A user moving from nvim who relied on the
  Latin-1 fallback adds `latin1` to the list.
- **A listed legacy encoding can accept a broken UTF-8 file** (evidence 41 for cp949): the text reads as
  garbage, the bytes stay exact.
- **The write refusal** is stricter than nvim for latin1/UTF-16/UCS-4 and stricter than Emacs's silent
  switch in the Vim profile; it never loses a character.
- **`euc-kr` writes can contain cp949 pairs** (§2.1).
- **The Emacs profile keeps Emacs's silent switch to UTF-8**, which changes the file's encoding without
  asking; ruse adds the coding system to the save message.

## Re-evaluation conditions

- A user needs an encoding `encoding_rs` lacks (EBCDIC, ISO-2022-KR, a strict EUC-KR writer): add a hand-written
  codec row, or reconsider iconv as an optional backend.
- Detection by guessing is asked for: an explicit command (`:Detect`) first, never the default.
- A second frontend or remote peer (F-018, D-013) that reads files itself: the codec module moves to a
  frontend-shared crate, still out of the core.
- Measured decode cost on large files contradicts §Performance.

## Open questions

Open points that need the maintainer's approval (proposed above, not decided):

- **The `++bad` default for a forced `++enc` read** (§3.1): `keep` as proposed, or nvim's `?`. The all-fail
  read under the default list keeps the bytes: that part is decided.
- **E513 for every encoding** (§4.1), where nvim writes a substitute byte for latin1, UTF-16 and UCS-4.

Questions for the slices:

- Slice 3: whether `C-x RET c` before `C-x C-s` keeps the coding system for later saves, and whether it makes the coding system "explicit" for later saves in ruse as
  in Emacs (`buffer-file-coding-system-explicit`'s cdr). To be probed before slice 3.
- Slice 2: what a forced `++enc` read of a file that a multi-byte set decodes but does not encode back to
  the same bytes does (§3): keep the decode and warn that a write changes those bytes, or refuse the read as
  a conversion error. `'fileencodings'` entries already refuse such a file (slice 1).
- Slice 2: nvim's exact message when `:set fenc=` is given in a `'nomodifiable'` buffer (§4.2 assumes E21,
  as for `:set ff=`; not probed).
