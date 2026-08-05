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
sources_root: https://vimhelp.org/
related:
  - README.md
  - neovim.md
  - ../architecture/architecture.md
  - ../anti-patterns/anti-patterns.md
---

# Parity: Vim Editing Language

Scope: **classic Vim** (Neovim's additions are in [neovim.md](neovim.md)). Vim is ruse's model for the
**editing language** of the Vim Style profile (and Native Style's text layer). We target L1 broadly and
**L2 for the core operator/motion/register/undo/search machinery** — the parts users' muscle memory and
plugins depend on. We do **not** target L3 (running Vimscript / actual Vim plugins); see
[../architecture/architecture.md](../architecture/architecture.md) §2.

> **⚠️ markers** below flag behaviors most often gotten wrong in Vim-emulation layers. These are L2
> obligations and belong in the differential test corpus (see [../anti-patterns/anti-patterns.md](../anti-patterns/anti-patterns.md) TEST-2).

## VIM-MODE — Modes & Transitions
Source: `intro.txt`, `visual.txt`, `terminal.txt`.

| ID | Mode | Enter | Exit | Target | Compat | Weight |
| --- | --- | --- | --- | --- | --- | --- |
| VIM-MODE-1 | Normal | `<Esc>` | — | L2 | Exact | high |
| VIM-MODE-2 | Insert | `i I a A o O gi gI` | `<Esc> C-c C-[` | L2 | Exact | high |
| VIM-MODE-3 | Replace / Virtual Replace | `R` / `gR gr` | `<Esc>`; `<BS>` restores | L1 | Equivalent | low |
| VIM-MODE-4 | Visual char / line / block | `v` / `V` / `C-v` | `<Esc>` / toggle | L2 | Exact | high |
| VIM-MODE-5 | Select mode | `gh gH g C-h` | printable char → Insert | L1 | Equivalent | low |
| VIM-MODE-6 | Operator-pending | after operator | motion completes / `<Esc>` | L2 | Exact | high |
| VIM-MODE-7 | Command-line | `: / ? ! q:` | `<CR>` / `<Esc> C-c` | L2 | Equivalent | high |
| VIM-MODE-8 | Ex mode | `Q` / `gQ` | `:visual` | L1 | Adapted | low |
| VIM-MODE-9 | Terminal-Job / Terminal-Normal | `:terminal` / `C-w N`, `C-\ C-n` | `C-w N` / any key | L1 | Equivalent | med |

- `C-o` in Insert runs **one** Normal command then returns; `gi` re-enters Insert at `` `^ ``.
- **⚠️ VIM-MODE-6**: model operator-pending as a *transient state*, not a special case (VIM-1, anti-pattern VIM-12).
- **⚠️ VIM-MODE-5/8**: Select mode and Ex mode are real, commonly-omitted modes.

## VIM-OP — Operators & Operator+Motion Grammar
Source: `change.txt`, `motion.txt`. Grammar: `[count1] operator [count2] {motion|text-object}`; effective repeat = `count1 × count2`. Doubling → linewise (`dd yy >> == cc guu`).

| ID | Operator | Action | Target | Compat | Weight |
| --- | --- | --- | --- | --- | --- |
| VIM-OP-1 | `d c y` | delete / change / yank | L2 | Exact | high |
| VIM-OP-2 | `> <` | shift by `shiftwidth` | L1 | Exact | high |
| VIM-OP-3 | `=` | reindent (`equalprg`/`indentexpr`) | L1 | Equivalent | med |
| VIM-OP-4 | `gu gU g~` | lower / upper / toggle case | L1 | Exact | med |
| VIM-OP-5 | `!` | filter lines through external program | L1 | Equivalent | low |
| VIM-OP-6 | `gq gw` | format text (`gw` keeps cursor) | L1 | Equivalent | med |
| VIM-OP-7 | `g?` | ROT13 | L2 | Exact | low |
| VIM-OP-8 | `zf` | create fold over motion | L1 | Equivalent | low |
| VIM-OP-9 | `g@` | call `operatorfunc` (user operator) | L2 | Equivalent | med |

Normal aliases: `x=dl X=dh D=d$ C=c$ Y=yy s=cl S=cc`.
- **⚠️ VIM-OP-CW**: `cw`/`cW` behaves like `ce`/`cE` on a non-blank (no trailing whitespace) — preserve this Vi wart.
- `tildeop` makes `~` an operator; `startofline` affects post-command column.

## VIM-MOT — Motions (types & inclusive/exclusive)
Source: `motion.txt`. Three types: **characterwise** (inclusive/exclusive), **linewise**, **blockwise**.

| ID | Class | Motions | Target | Compat | Weight |
| --- | --- | --- | --- | --- | --- |
| VIM-MOT-1 | char excl. | `h l 0 ^ <Space> <BS> \|` | L2 | Exact | high |
| VIM-MOT-2 | char incl. | `$ g_`; `f t` incl., `F T` excl.; `; ,` | L2 | Exact | high |
| VIM-MOT-3 | word | `w W b B` (excl.), `e E ge gE` (incl.) | L2 | Exact | high |
| VIM-MOT-4 | linewise | `j k + - _ G gg H M L {n}%` | L2 | Exact | high |
| VIM-MOT-5 | display line | `gj gk g0 g^ g$` | L1 | Exact | med |
| VIM-MOT-6 | sentence/para | `( ) { }` (excl.) | L2 | Exact | med |
| VIM-MOT-7 | section | `[[ ]] [] ][` | L1 | Equivalent | low |
| VIM-MOT-8 | match | `%` (incl.), `[( ]) [{ ]} [m ]m` | L1 | Equivalent | med |
| VIM-MOT-9 | search | `/ ? n N * # g* g#` | L2 | Exact | high |

**⚠️ VIM-MOT-PROMOTE** — replicate exactly (governs `d}`, `d/pat`):
1. exclusive→inclusive: exclusive motion ending in column 1 → end moves to prev line's end, becomes inclusive.
2. exclusive→linewise: if additionally it started at/before first non-blank → becomes linewise.
3. Forced motion after operator: `v` force charwise (toggles incl/excl), `V` force linewise, `C-v` force blockwise.

## VIM-TOBJ — Text Objects
Source: `motion.txt`. `i`=inner, `a`=around.

| ID | Objects | Target | Compat | Weight |
| --- | --- | --- | --- | --- |
| VIM-TOBJ-1 | `iw aw iW aW` | L2 | Exact | high |
| VIM-TOBJ-2 | `is as ip ap` | L2 | Exact | med |
| VIM-TOBJ-3 | `i( a( ib i{ a{ iB i[ a[ i< a<` | L2 | Exact | high |
| VIM-TOBJ-4 | `it at` (tag) | L1 | Equivalent | med |
| VIM-TOBJ-5 | `i" a" i' a'` `` i` a` `` | L2 | Exact | high |

- **⚠️** `aw` trailing-vs-leading whitespace rule; quote objects single-line + "next pair on line" + `a"` space handling; `it`/`at` nesting with counts; `ap`/`ip` blank-line handling.

## VIM-CNT — Counts
- Count multiplies across operator and motion: `2d3w` = 6 words. Count on `G`/`gg`/`%` = line/percent.
- **⚠️ VIM-CNT-INS**: count on Insert repeats inserted text (`3ohello<Esc>`); count on `.` overrides original.

## VIM-REG — Registers & Put
Source: `change.txt`.

| ID | Register | Contents | Target | Compat | Weight |
| --- | --- | --- | --- | --- | --- |
| VIM-REG-1 | `""` | unnamed; last delete/change/yank | L2 | Exact | high |
| VIM-REG-2 | `"0` | last **yank only** | L2 | Exact | med |
| VIM-REG-3 | `"1`–`"9` | delete/change ring (≥1 line) | L2 | Exact | med |
| VIM-REG-4 | `"-` | small-delete (<1 line) | L2 | Exact | low |
| VIM-REG-5 | `"a`–`"z`, `"A`–`"Z` append | named | L2 | Exact | med |
| VIM-REG-6 | `"_` | black hole | L1 | Exact | med |
| VIM-REG-7 | `"=` | expression register | L1 | Adapted | low |
| VIM-REG-8 | `"+ "*` | clipboard / primary selection | L1 | Equivalent | high |
| VIM-REG-9 | `"~ "% "# ". ": "/` | drop/file/alt/insert/cmd/search | L1 | Equivalent | low |

Put: `p P gp gP ]p [p`.
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
- **⚠️ VIM-REPEAT-DOT**: `.` captures the full last change incl. inserted text; `g@`/`operatorfunc` is dot-repeatable (plugins hook this). Distinguish dot-repeat from transaction replay (anti-pattern VIM-11).

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
