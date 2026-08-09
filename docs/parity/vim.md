---
doc: parity-vim
project: ruse
title: "Parity: Vim Editing Language"
summary: >
  Feature-parity target set for classic Vim (8.2+/9.x): modes, operator+motion grammar,
  text objects, counts, registers, marks/jumplist, macros/dot-repeat, search/substitute,
  undo tree, Ex commands, autocommands, folds, windows, terminal/jobs, mappings, completion.
  Grounded in vimhelp.org; edge cases a compatibility layer must get exactly right are flagged.
audience: [maintainers, contributors, llm-agents]
status: draft
source_of_truth: false
verified_against_upstream: false
sources_root: https://vimhelp.org/
related:
  - README.md
  - neovim.md
  - ../architecture/architecture.md
  - ../anti-patterns/anti-patterns.md
---

# Parity: Vim Editing Language

> **⚠️ NOT THE SOURCE OF TRUTH (D-043).** This file is hand-authored and has never been checked
> against a pinned upstream revision. The parity source is the machine-derived census in
> [`spec/parity/inventory/`](../../spec/parity/inventory/), generated from the SHA pins in
> [`spec/parity/upstreams.yaml`](../../spec/parity/upstreams.yaml). These tables survive as *human
> annotation* — reading, grouping and intent — and are migrating onto census IDs. Do not add rows
> here to record a newly discovered upstream feature: humans classify, they do not enumerate.


Scope: **classic Vim** (Neovim's additions are in [neovim.md](neovim.md)). Vim is ruse's model for the
**editing language** of the Vim Style profile (and Native Style's text layer). We target L1 broadly and
**L2 for the core operator/motion/register/undo/search machinery** — the parts users' muscle memory and
plugins depend on. We do **not** target L3 (running Vimscript / actual Vim plugins); see
[../architecture/architecture.md](../architecture/architecture.md) §2.

> **⚠️ markers** below flag behaviors most often gotten wrong in Vim-emulation layers. These are L2
> obligations and belong in the differential test corpus (see [../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md) TEST-2).

## VIM-MODE — Modes & Transitions
Source: `intro.txt`, `visual.txt`, `terminal.txt`.

> **MIGRATED TO THE CENSUS (D-043).** The eight Vim keymap *namespaces* — Normal, Insert, Command-line,
> Visual, Select, Operator-pending, Terminal, Lang-Arg — are now enumerated from upstream's own
> `runtime/doc/map.txt` map-table as the [`map_mode`](../../spec/parity/inventory/neovim/map_mode.yaml)
> census surface (`nvim.mapmode.*`), and what ruse promises for them lives in the
> [`vim-style.yaml`](../../spec/parity/contracts/vim-style.yaml) contract. The old transition-table rows
> (VIM-MODE-1/2/4/5/6/7/9) have been dropped: they are superseded by census IDs the PRD now cites directly.
> Two rows survive here because they are **not** map_mode namespaces and have no census ID of their own:

| ID | Mode | Enter | Exit | Target | Compat | Weight |
| --- | --- | --- | --- | --- | --- | --- |
| VIM-MODE-3 | Replace / Virtual Replace | `R` / `gR gr` | `<Esc>`; `<BS>` restores | L1 | Equivalent | low |
| VIM-MODE-8 | Ex mode | `gQ` | `:visual` | L1 | Adapted | low |

- **VIM-MODE-3** (Replace / Virtual Replace) is not a keymap namespace: it is the *overwrite* unmatched-key
  policy *inside* the Insert namespace (`nvim.mapmode.ins`), so it stays as a human annotation on F-024
  rather than becoming its own census ID.
- **VIM-MODE-8** (Ex mode) is entered by `gQ` only — at the pinned Neovim revision `Q` is *replay-last-register*,
  not an Ex-mode key (see `vim-style.yaml` census_corrections). Ex mode is not one of the eight map_mode
  namespaces, so it too is kept as a legacy annotation (on F-026) rather than forced onto a census ID.
- `C-o` in Insert runs **one** Normal command then returns; `gi` re-enters Insert at `` `^ `` — now an
  obligation of the Insert namespace (VS-OBL-2, one-shot return), see the contract.
- **⚠️** Operator-pending is a real namespace with an *abort* unmatched-key policy, not a transient flag on
  Normal — modelled as `nvim.mapmode.opr` / the `operator_pending` contract namespace (was VIM-MODE-6).

## VIM-OP — Operators & Operator+Motion Grammar
Source: `change.txt`, `motion.txt`. Grammar: `[count1] operator [count2] {motion|text-object}`; effective repeat = `count1 × count2`. Doubling → linewise (`dd yy >> == cc guu`).

_Partially retired to the census (D-043): the IMPLEMENTED operator grammar — `d c y` (VIM-OP-1) + the
normal aliases + `<<` (part of VIM-OP-2) — migrated to `nvim.key.normal.*` (surface `mode_key.normal`),
screened FAM-EDIT-VIM-GRAMMAR/targeted and cited by PRD F-023. NOT repointed (un-implemented or on other
census sub-surfaces — screen when feature-motivated): `>`/`>{motion}` (census-extraction gap: no upstream
row, only `<` present), `=` reindent (VIM-OP-3), `!` filter (VIM-OP-5), the extended operators
`gu gU g~ gq gw g? g@` (VIM-OP-4/6/7/9, surface `mode_key.normal.g`) and `zf` (VIM-OP-8, `mode_key.normal.z`)._

Normal aliases: `x=dl X=dh D=d$ C=c$ Y=yy s=cl S=cc`.
- **⚠️ VIM-OP-CW**: `cw`/`cW` behaves like `ce`/`cE` on a non-blank (no trailing whitespace) — preserve this Vi wart.
- `tildeop` makes `~` an operator; `startofline` affects post-command column.

## VIM-MOT — Motions (types & inclusive/exclusive)
Source: `motion.txt`. Three types: **characterwise** (inclusive/exclusive), **linewise**, **blockwise**.

_Partially retired to the census (D-043): the IMPLEMENTED motions — `h l j k 0 $` (VIM-MOT-1 subset),
`f t F T ; ,` (VIM-MOT-2), `w W b B e E` (VIM-MOT-3 subset), `G` (VIM-MOT-4 subset), `( )`/`{ }` para
(VIM-MOT-6) and `%` match (VIM-MOT-8) — migrated to `nvim.key.normal.*`, screened FAM-EDIT-VIM-GRAMMAR/
targeted and cited by PRD F-023. NOT repointed (un-implemented or elsewhere): `^ _ + - | H M L {n}%`,
`ge gE` (rest of VIM-MOT-1/3/4); display-line `gj gk g0 g^ g$` (VIM-MOT-5, `mode_key.normal.g`); section
`[[ ]] [] ][` (VIM-MOT-7, `mode_key.normal.bracket`); search `/ ? n N * #` (VIM-MOT-9 → PRD F-009)._

**⚠️ VIM-MOT-PROMOTE** — replicate exactly (governs `d}`, `d/pat`):
1. exclusive→inclusive: exclusive motion ending in column 1 → end moves to prev line's end, becomes inclusive.
2. exclusive→linewise: if additionally it started at/before first non-blank → becomes linewise.
3. Forced motion after operator: `v` force charwise (toggles incl/excl), `V` force linewise, `C-v` force blockwise.

## VIM-TOBJ — Text Objects
Source: `motion.txt`. `i`=inner, `a`=around.

_Retired to the census (D-043): VIM-TOBJ-1..5 migrated to the 34 `nvim.key.text_object.*` items
(surface `mode_key.text_object`), screened FAM-EDIT-VIM-GRAMMAR/targeted and cited by PRD F-028._

- **⚠️** `aw` trailing-vs-leading whitespace rule; quote objects single-line + "next pair on line" + `a"` space handling; `it`/`at` nesting with counts; `ap`/`ip` blank-line handling.

## VIM-CNT — Counts
- Count multiplies across operator and motion: `2d3w` = 6 words. Count on `G`/`gg`/`%` = line/percent.
- **⚠️ VIM-CNT-INS**: count on Insert repeats inserted text (`3ohello<Esc>`); count on `.` overrides original.

## VIM-REG — Registers & Put
Source: `change.txt`.

_Partially retired to the census (D-043): the IMPLEMENTED register surface — the `"{register}` selection
prefix (`nvim.key.normal.x22x7bregisterx7d`, named `"a`-`"z`/`"A`-`"Z` append = VIM-REG-5; unnamed `""` =
VIM-REG-1; yank-only `"0` = VIM-REG-2, a store SEMANTIC of the same key) plus put `p`/`P` — migrated to
`nvim.key.normal.*`, screened FAM-EDIT-VIM-GRAMMAR/targeted and cited by PRD F-029. NOT modelled yet
(carve-outs, screen when feature-motivated): numbered ring `"1`-`"9` (VIM-REG-3), small-delete `"-`
(VIM-REG-4), black-hole `"_` (VIM-REG-6), expression `"=` (VIM-REG-7), clipboard `"+`/`"*` (VIM-REG-8),
special `"~ "% "# ". ": "/` (VIM-REG-9); `gp gP ]p [p`._
- **⚠️ VIM-REG-TYPE**: register **type** (char/line/blockwise) is stored and governs paste geometry.
- **⚠️ VIM-REG-RING**: numbered-ring shifting rules; small deletes → `"-`; Visual-`p` swaps replaced text into unnamed.
- ruse maps this to a **unified register/kill-ring model** shared with Emacs (see [emacs.md](emacs.md) EMACS-KILL, [../architecture/architecture.md](../architecture/architecture.md)); the Vim *surface* must still reproduce the above semantics.

## VIM-MARK — Marks, Jumplist, Changelist
Source: `motion.txt`.
- Set `m{a-z}` (buffer), `m{A-Z}` (global, persist). Jump `` `{mark} `` (exact, charwise/excl) vs `'{mark}` (first non-blank, **linewise**).
- Special marks: `` `` '' `" `. `^ `[ `] `< `> `( `) `{ `} ``.
- Jumplist: `C-o` older / `C-i`/`<Tab>` newer (`:jumps`). Changelist: `g;` / `g,` (`:changes`).
- **⚠️ VIM-MARK-1**: backtick vs apostrophe semantics; which commands are "jumps" (`n` yes, `j` no); `` `[ ``/`` `] `` heavily used by plugins.

## VIM-REPEAT — Macros & Dot-repeat
Source: `repeat.txt`, `change.txt`.
- Record `q{a-z}` … `q`, append `q{A-Z}`; play `@{a-z}`, `@@`, `[count]@a`, `@:`, `@=`.
- `.` repeats last text-changing command (not motions/`:`/macros/yanks), honoring a new count.
- `:normal[!] {keys}` runs Normal keys programmatically.
- **⚠️ VIM-REPEAT-DOT**: `.` captures the full last change incl. inserted text; `g@`/`operatorfunc` is dot-repeatable (plugins hook this). Distinguish dot-repeat from transaction replay (anti-pattern VIM-11). _Dot-repeat retired to the census (D-043): `.` = `nvim.key.normal..`, screened FAM-EDIT-VIM-GRAMMAR/targeted, cited by PRD F-023. Macros `q`/`@` (VIM-REPEAT record/replay) stay legacy — not yet feature-motivated._

## VIM-SEARCH — Search & Substitute
Source: `pattern.txt`, `change.txt`.
- Search `/{pat}[/offset]`, `?`, `n N * # g* g# gd gD`; offsets `/e /e-1 /s+2 /+3`.
- Options: `ignorecase smartcase incsearch hlsearch wrapscan gdefault`.
- Magic levels: `\v` very-magic, `\V` very-nomagic, `\m \M`.
- Atoms: `\< \> \zs \ze \( \) \1 \%( \) * \+ \= \? \{n,m} \{-} \d \w \s \%[...] ^ $ \%V \%23l \@= \@! \@<= \@<! \c \C \|`.
- `:substitute`: `:[range]s/{pat}/{repl}/[flags] [count]`, flags `g c e i n & p # l r`; repeat `& g& :&& :~`.
- Replacement items: `& \0 \1-\9 ~ \u \U \l \L \e \E \r \n \t \=expr` (`submatch()`).
- Two engines (backtracking + NFA), `regexpengine`, `\%#=1/2`.
- **⚠️ VIM-SEARCH-1**: `gdefault` inverts `g`; Vim regex ≠ PCRE (`+ ? { } ( ) |` literal in default magic); `\zs`/`\ze`/`\@<=` are unique; `:s///\=` runs script per match. ruse needs its own regex mapping layer or a Vim-dialect mode.

## VIM-UNDO — Undo, Undo-tree, Persistent Undo
Source: `undo.txt`.
- `u C-r U`; branching tree: new change after undo **branches** (no history loss). `g-`/`g+` traverse **chronological** states; `:undolist`, `:earlier`/`:later {N|5m|3f}`.
- `:undojoin`, Insert `C-g u` (undo break). `undofile`/`undodir` persistent; `undolevels`.
- **⚠️ VIM-UNDO-1**: chronological `g-`/`g+`, line-level re-doable `U`, `:earlier Nf`. Map onto ruse's transaction/undo model (TEXT-12/13). This validates the design's "undo by logical unit" decision.

## VIM-EX — Ex Command System
Source: `cmdline.txt`, `change.txt`, `map.txt`.
- Ranges: `N . $ % 'a '< '> /pat/ ?pat? +N -N ;`. `:d :y :m :t :j :put :read :w :s :sort :!`.
- `:global` `:[range]g/pat/cmd` (default `:p`), `:g!`/`:v`. `:normal[!]`.
- User commands: `:command[!] -nargs= -range= -complete= Name {repl}` with `<args> <line1> <count> <q-args> <f-args> <bang>`.
- Cmdline editing: history, `q:` window, `wildmenu`/`wildmode`, `C-r`.
- **⚠️ VIM-EX-GLOBAL**: `:g` is **two-pass** (mark matching lines first, then run) — this makes deletes/moves predictable; per-command default ranges differ; support address `0` and `;`.

## VIM-AU — Autocommands & Events
Source: `autocmd.txt`.
- `:autocmd [group] {event} {pat} [++once] [++nested] {cmd}`; `augroup`; `:doautocmd`.
- Events: `BufReadPre/Post BufWritePre/Post BufEnter/Leave FileType WinEnter TabEnter InsertEnter/Leave TextChanged(I) CursorMoved(I) CursorHold VimEnter/Leave FocusGained/Lost CmdlineEnter/Leave QuickFixCmdPre/Post User` …
- **⚠️ VIM-AU-1**: `++nested` (re-entrant events), `++once`, `<amatch>/<afile>/<abuf>`, and **event ordering** matter for parity. Maps to ruse's typed event model (ASYNC-5, no re-entrant mutation ASYNC-15).

## VIM-FOLD — Folds
Source: `fold.txt`.
- `foldmethod`: manual / indent / expr / marker / syntax / diff.
- Keys: `zf zd zo zc za zv zm zM zr zR zj zk [z ]z zn zN zi`; `:fold :foldopen`.
- **⚠️ VIM-FOLD-1**: operators/motions over a **closed fold** act on the whole fold (`dd` deletes all its lines); manual folds are volatile without `:mkview`.

## VIM-QF — Quickfix & Location Lists
Source: `quickfix.txt`. Quickfix = global stack; location list = per-window (every `:c…` has `:l…` twin).
- Populate `:make :grep :vimgrep :cexpr :cbuffer`; navigate `:cc :cnext :cprev :cfirst :clast :cabove :cbelow`; window `:copen :cclose :cwindow`; batch `:cdo :cfdo`; stack `:colder :cnewer :chistory`.
- **⚠️ VIM-QF-1**: `errorformat` scanf-mini-language; loclist bound to its window. In ruse these are **workspace result buffers** (see [workspace.md](workspace.md)).

## VIM-WIN — Windows, Tabs, Buffers
Source: `windows.txt`, `tabpage.txt`.
- Buffers `:e :bn :bp :b :ls :bd :bufdo C-^` (active/hidden/unlisted, `hidden`).
- Windows `:split :vsplit C-w {s v w hjkl q c o HJKL r x = _ | }` `:windo`.
- Tabs `:tabnew :tabclose gt gT :tabmove :tabdo`.
- **⚠️ VIM-WIN-1**: Vim tabs are **window layouts**, not buffers-as-tabs — matches ruse's buffer≠view≠window model (UI-6, CORE-4).

## VIM-JOB — Terminal, Jobs/Channels, Timers
Source: `terminal.txt`, `channel.txt` (Vim 8+).
- `:terminal [++opts] [cmd]`; `job_start/status/stop`; channels `ch_open/sendraw/sendexpr/read` (raw/nl/json/js); `timer_start/stop/pause`.
- **⚠️ VIM-JOB-1**: async single-threaded callbacks driven by the main loop — aligns with ruse's deterministic-executor model ([../architecture/architecture.md](../architecture/architecture.md) §8). Neovim's `jobstart` differs (see [neovim.md](neovim.md)).

## VIM-MAP — Mappings, Abbreviations, Timeouts
Source: `map.txt`.
- Per-mode `:map/:noremap nmap imap xmap smap omap cmap tmap lmap`; args `<buffer> <silent> <expr> <nowait> <unique>`; notation `<CR> <C-x> <A-x> <Leader> <Plug> <SID>`.
- Abbreviations `:iabbrev :cabbrev`. Timeouts `timeout/timeoutlen` (mappings) vs `ttimeout/ttimeoutlen` (key codes).
- **⚠️ VIM-MAP-1**: `vmap` covers Visual **and** Select (use `xmap` for Visual only); mapping-vs-keycode timeout; `<expr>`/`<nowait>`. In ruse, `<Plug>`-style indirection is replaced by **semantic command IDs** (CMD, [../architecture/architecture.md](../architecture/architecture.md) §2).

## VIM-INS — Insert Completion, Digraphs, Spell
Source: `insert.txt`, `digraph.txt`, `spell.txt`.
- Completion `C-n/C-p` + `C-x` submodes (`C-x C-l/C-n/C-k/C-f/C-]/C-o/C-u/C-v/s`), popup `C-y/C-e`, `completeopt`.
- Insert keys `C-r{reg} C-a C-w C-u C-t C-d C-o C-v C-k C-e C-y C-g u`.
- Digraphs `C-k {c1}{c2}`; Spell `spell/spelllang ]s [s z= zg zw`.
- Target L1; completion-popup key semantics L2 where feasible.

## VIM-STATE — Sessions, viminfo, Encoding, Large Files
Source: `starting.txt`, `mbyte.txt`.
- `:mksession` / `:mkview`; viminfo persists registers, marks `A-Z 0-9`, histories, jumplist.
- `encoding`/`fileencoding(s)`, BOM `bomb`, `fileformat` (unix/dos/mac), `++enc=`/`++ff=`.
- Large files: `swapfile`, `binary`/`-b`, syntax-off patterns.
- **⚠️ VIM-STATE-1**: uppercase/numbered marks & registers **persist across sessions**; encoding detection order + BOM; `dos` fileformat hides `^M`. ruse must separate encoding/line-endings from document data (TEXT-19).

## VIM-SCRIPT — Vimscript (noted, non-goal)
Source: `eval.txt`. Legacy Vimscript + Vim9script. **ruse does not target running Vimscript.** A parity layer must at minimum model `:set` options and expression-register/`\=` evaluation enough for the editing surface — nothing more. See [../architecture/architecture.md](../architecture/architecture.md) §0.3.

---

## Compatibility "Get-It-Exactly-Right" Checklist (L2 / test-corpus)
These are the Vim behaviors most often wrong in emulators — required differential tests (TEST-2):

1. Exclusive→inclusive→linewise motion promotion (VIM-MOT-PROMOTE).
2. `cw` acting like `ce` on non-blank (VIM-OP-CW).
3. Register model: numbered-ring shifting, `"0` yank-only, `"-` small-delete, register-type paste geometry, Visual-`p` swap (VIM-REG-*).
4. Dot-repeat capturing full inserted text + new count; `operatorfunc` repeatability (VIM-REPEAT-DOT).
5. Count multiplication and count-on-insert repetition (VIM-CNT-INS).
6. Backtick vs apostrophe marks; jumplist membership; changelist (VIM-MARK-1).
7. `:global` two-pass semantics; per-command default ranges; address `0`, `;` (VIM-EX-GLOBAL).
8. Undo-tree `g-`/`g+` chronological traversal; `U`; `:earlier Nf` (VIM-UNDO-1).
9. Vim regex dialect + `gdefault` (VIM-SEARCH-1).
10. `vmap` covering Select mode; `timeoutlen` vs `ttimeoutlen` (VIM-MAP-1).
11. Fold-aware operators; manual-fold volatility (VIM-FOLD-1).
12. viminfo/session statefulness (VIM-STATE-1).
