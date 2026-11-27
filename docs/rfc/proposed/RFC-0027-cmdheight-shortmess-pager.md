---
doc: rfc
project: ruse
title: "RFC-0027: The message area: 'cmdheight', 'shortmess' flags and one pager for listing output"
summary: >
  ruse's last row holds the status bar, the message and the command line at once. 'cmdheight' and 'shortmess'
  are only names, `:!` output is joined into one row with " | ", view-only listings open an overlay instead of
  nvim's pager, and there is no `g<` or 'more' (#1275). This RFC defines the bottom of the screen as a bar row
  plus 'cmdheight' message rows, with nvim's meaning for every value. 0 is today's shared row. It also ships the
  'shortmess' flags that change a message ruse gives, and routes all multi-row output through the one
  `-- More --` pager with `g<` and 'more'.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../design/render-and-frontends.md
  - ../../../spec/DECISIONS.md
  - ../../../spec/PRD.yaml
  - ../../../spec/config-schema.yaml
  - ../accepted/RFC-0009-render-model.md
  - ../accepted/RFC-0017-better-than-vanilla-defaults.md
---

# RFC-0027: The message area: 'cmdheight', 'shortmess' flags and one pager for listing output

- **Status:** proposed: D-068 open, waiting for the maintainer
- **Author(s):** Claude Opus 5.5, for maintainer review
- **Created:** 2026-11-28
- **Decision link:** D-068

<!-- Hard-to-reverse: the screen's bottom-row layout every frame, PTY test and user sees, and the meaning of
     the 'cmdheight' values that user configs will start to carry. -->

## Summary

The bottom of the screen has two parts: the **bar** (ruse's one global status line: mode, name, flags,
diagnostics) and the **message area**, which is `'cmdheight'` rows below it. Each value means what it means in
nvim v0.12.6:

- **`'cmdheight'` N ≥ 1:** the bar has its own row, and N rows below it hold messages and the command line.
  The windows get the rows that are left, the same count nvim gives them.
- **`'cmdheight'` 0:** today's layout. The bar is the last row and also shows a message after its prefix. The
  command line opens over it.

The default is the open question D-068 asks the maintainer (§1.3). The recommendation is 1, which is nvim's
layout and also Emacs's mode line plus echo area.

All output with more than one row goes through the one pager that #1274 added. That covers real multi-row
`:!` output, `:ls` with one row per buffer, and the view-only listings that open an overlay today. The pager
adds `g<`, `'more'` and the typed command line on the first row. The `'shortmess'` flags that change a message
ruse gives today work. The other nvim flags are accepted and checked, and are listed as having no effect
because ruse gives none of the messages they control. The Emacs profile's echo area is a separate design.

## Motivation / Problem

Seven gaps listed in #1275 (from the #1274 PR body), each confirmed against the code at `origin/main` f76bd2d3:

| Gap | Where in the code |
| --- | --- |
| The message shares the last row with the bar. A long message loses the prefix's cells, and a file message gets the `CutRow` special case (#1235) | `ui/render.rs:106` `text_rows = rows - 1`, `:197` one `put_str` on the last row; `ui/render/status.rs:22` `status_text` builds `"{prefix}{status}"` |
| `'cmdheight'`, `'shortmess'`, `'more'` exist only as names. `:set ch?` gives `option not supported` | `input/nvim_options.rs:13,47`; probed in a PTY |
| `:!` output is joined into one row | `app/session/ex_exec/shell.rs:150` `replace('\n', " \| ")`. `:w !cmd` does the same (`app/file_cmds.rs:416`) |
| `:ls` is one row joined with two spaces | `app/dispatch/labels.rs:21` `buffer_list_line` ("compact form for MVP") |
| View-only listings open an `Overlay::Listing`, not the pager | `app/ex_more.rs:55` `show_rows`: `:p :nu :l :# :z :g//p :set :set all [I ]I :checkhealth :inspect config` |
| No `g<`, no `'more'`, no last-output store | `app/messages.rs:128` `Status` holds only the current text |
| `'messagesopt'` `wait:` and `progress:` are checked and then ignored | `input/set_cmd/table.rs:279` |
| The pager's first page has no typed command line | `ui/render/status.rs:86` paints only the message rows |

How listings show today (`origin/main`):

| Command | Today | nvim |
| --- | --- | --- |
| `:messages` | pager (#1274) | pager |
| `:!cmd`, `:w !cmd` | one row joined with ` \| ` | rows, then hit-enter |
| `:ls` `:buffers` `:files` | one row joined with two spaces | one row per buffer |
| `:p` `:nu` `:l` `:#` `:z` `:g//p` `:set` `:set all` `[I` `]I` `:checkhealth` `:inspect config` | 1 row: status; 2 or more: view-only `Overlay::Listing` | pager (`:checkhealth`/`:inspect` are ruse-only) |
| `:reg` `:di` `:marks` `:jumps` `:changes` `:dig` `:diagnostics` | interactive picker (Enter acts) | pager (`:diagnostics` is ruse-only) |
| `:hi` `:map` `:undolist` `:history` `:autocmd` `:command` `:abbreviate` `:tabs` `:version` `:scriptnames` `:oldfiles` `:clist` | not implemented (E492) | pager |

## Evidence

Every behaviour below was probed with `nvim --clean` v0.12.6, either headless (`redir`, `&ch`, `try`) or in a
real PTY at 80x24 (tmux on a private socket, `capture-pane`). The Emacs rows used `emacs -Q` 31.1, in `--batch`
for values and `-nw` in tmux for the screen. "ruse" rows were probed on the current debug build in the same
PTY. Rows are 1-based screen rows.

| # | Probe | Result |
| --- | --- | --- |
| E1 | defaults (headless) | `shortmess=ltToOCF cmdheight=1 more messagesopt=hit-enter,history:500,progress:c laststatus=2` |
| E2 | `:set ch=-1` / `ch=0` / `ch=30` / `ch=1000` (headless, 24 rows) | `E487: Argument must be positive` / ok / clamped to 22 / clamped to 22 (rows − 2) |
| E3 | `:set shm=z` | `E539: Illegal character <z>`; `shm+=I` is ok |
| E4 | default layout (ch=1, ls=2) | row 23 is the status line, row 24 the command line, so 22 window rows |
| E5 | `:set ch=2`, `:echo winheight(0)` | 21: status line on row 22, message rows 23–24 |
| E6 | ch=2: `:echo "a\nb"`; `:echo repeat("x",100)` | shown on rows 23–24, no prompt |
| E7 | ch=2: `:echo "a\nb\nc"` | rows 21–23, then hit-enter on 24 |
| E8 | ch=3: `:echo repeat("x",150)` | rows 22–23, no prompt |
| E9 | last-row width (`v:echospace`=68 with `'showcmd'`) | `"a\n"` plus 68 `y`: fits; plus 69: hit-enter. With `'noshowcmd'`: 79 fits, 80 gives hit-enter. With ch=1 the same edge (68/69) |
| E10 | ch=2, `:` | the command line opens on row 23, the first message row; the status line stays |
| E11 | ch=3, `:ls` with 1 buffer / 2 buffers | the typed `:ls` row plus the rows fit in 3 rows: no prompt |
| E12 | ch=0 idle | status line on row 24, 23 window rows (`winheight` 23) |
| E13 | ch=0: `:echo "hi"`; `:echo winheight(0)` | hit-enter for each, even a one-row message |
| E14 | ch=0: `u` after an edit; `i` | no undo message; no `-- INSERT --` |
| E15 | ch=0, `:` | the command line replaces the status line on row 24 |
| E16 | ch=1: `:!printf "a\nb\nc\n"` | rows 1–17 unchanged (not scrolled), 18 blank, 19 `:!printf "a\nb\nc\n"`, 20–22 `a b c`, 23 blank (the trailing newline), 24 hit-enter |
| E17 | `:!printf a`; `:!true` | `:!printf a`, `a`, hit-enter / `:!true`, blank, hit-enter |
| E18 | `:!seq 1 60` | **not paged on the way out:** rows 1–22 show `39`…`60`, 23 blank, 24 hit-enter. `b` there scrolls back (`16`…`38`) with `-- More --` |
| E19 | `:reg` | 17 `:reg`, 18 `Type Name Content`, rows, hit-enter: the typed line starts the output |
| E20 | `:echo join(range(1,60),"\n")` | `1`…`23` with `-- More --` (the whole screen above the prompt) |
| E21 | E20, then `q`, then `g<` | `1`…`23` again with hit-enter: g< keeps the rows that were shown, and `q` drops the rest |
| E22 | `:echo "zz"` (one row, no prompt), `k`, `g<` | `:echo "zz"`, `zz`, hit-enter: a motion in between does not clear it |
| E23 | `:!…`, `<CR>`, `k j k` or `x`, `g<` | the `:!` rows again (with `[No write since last change]` after the edit) |
| E24 | `g<`, `<CR>`, `g<` | the second `g<` shows nothing (also after `q`) |
| E25 | `:set nomore`, E20 | no `-- More --`: rows `38`…`60` and hit-enter |
| E26 | `:set nomore`, output, `g<` | nothing (`:help g<`: "only works when 'more' is set") |
| E27 | `:set mopt=wait:2000,history:500`, `:echo "a\nb\nc"` | rows 22–24 with no prompt; after 2.9 s cleared |
| E28 | `:w` with `shm+=W` | `"f.txt" 3L, 12B` (no `written`) |
| E29 | `shm-=l` `:w`; `shm+=w` `:w` | `3 lines, 11 bytes written`; `3 lines, 11 bytes [w]` |
| E30 | CTRL-G modified, `shm+=m` | `"f.txt" [+] 3 lines --33%--` (default: `[Modified]`) |
| E31 | `:e g.txt`, `:e #` (default `F`) | no file message; the row keeps the typed `:e g.txt`. `shm-=F`: `"g.txt" [New] --No lines in buffer--` |
| E32 | `nvim --clean f.txt` | last row empty (no file info, `F`) |
| ruse-1 | startup `ruse f.txt` | `NORMAL  f.txt  ruse — :q to quit` |
| ruse-2 | `:e g.txt` | `NORMAL  g.txt  "g.txt" 0 bytes`, a file message nvim's default `F` drops |
| ruse-3 | `/foo` wrapping past the end; `qa` | no wrap message, no search count, no `recording @a` |
| ruse-4 | `:!printf "a\nb\n"`; `:ls` | `NORMAL  f.txt  a \| b`; `NORMAL  f.txt  1% f.txt` |
| ruse-5 | `:set shm?` `ch?` `more?` `ls?` | `option not supported: …` |
| ruse-6 | 2 errors, `:messages` | painted over window rows 22–23, hit-enter on 24, no typed `:messages` row |
| Emacs-1 | `resize-mini-windows`, `max-mini-window-height` | `grow-only`, `0.25` |
| Emacs-2 | `-nw`, 24 rows, idle | the mode line is row 23 and the echo area row 24: the ch=1 layout |
| Emacs-3 | `M-!` `printf "a\nb\nc\n"` | the echo area grows to rows 22–24 and the mode line moves up. No prompt; the next key shrinks it and runs |
| Emacs-4 | `M-!` `seq 1 7` (more than 6 rows = 25 %) or `seq 1 60` | output goes to `*Shell Command Output*` in a split window, the echo area stays one row. No pager |

## Guide-level explanation

```
'cmdheight' 1 (recommended default)       'cmdheight' 0 (today's layout)
┌──────────────────────────────┐          ┌──────────────────────────────┐
│ windows: rows − 2            │          │ windows: rows − 1            │
├──────────────────────────────┤          │                              │
│ NORMAL  f.txt [+]  [E:1 W:0] │ bar      │                              │
│ "f.txt" 3L, 12B written      │ message  │ NORMAL  f.txt  "f.txt" 3L, … │ bar + message
└──────────────────────────────┘          └──────────────────────────────┘
```

- `:set ch=3` gives three message rows. A message of up to three rows shows with no prompt. A longer one
  scrolls up over the windows to a hit-enter prompt, as nvim's does (E6–E8).
- `:!ls` shows the typed command and then the output rows, as nvim's does (E16). Output taller than the
  screen shows its end, and `b` scrolls back (E18).
- `:ls`, `:p`, `:g/x/p`, `:set all` and `[I` show through the same pager. `:reg`, `:marks`, `:jumps`,
  `:changes` and `:dig` stay pickers (§4.3).
- `g<` shows the last output again, once (E21–E24). `:set nomore` turns paging off (E25).
- `:set shm+=W` drops `written` from the write message. `:set shm-=F` brings back the file info on `:e`.

## Reference-level explanation

### 1. The bottom layout (`'cmdheight'`)

#### 1.1 Geometry

There is one function, `BottomLayout::new(rows, cmdheight) -> { text_rows, bar_row, msg_rows }`, in
`ui/layout.rs`. It replaces the places that assume `rows - 1` today: `ui/render.rs:106` and `:197`,
`app/session/frame.rs:207`, `ui/render/caret.rs:118`/`:125` (the command-line caret row) and
`ui/recovery.rs:604` (the recovery prompt's page).

| `'cmdheight'` | window rows | bar | message area |
| --- | --- | --- | --- |
| 0 | rows − 1 | the last row, which also shows the message after its prefix (today) | none: the message is on the bar, and the command line opens over the bar (E15) |
| N ≥ 1 | rows − 1 − N | its own row, above the message area | the last N rows: messages and the command line, which opens on the first of them (E10) |

- The bar is ruse's one status line, which nvim calls `laststatus=3`. ruse has no window status lines, so no
  `'laststatus'` is added.
- `'cmdheight'` is global (nvim makes it tabpage-local, and ruse has no tabpages). Its type is int and its
  default is §1.3.
- A negative value gives `E487: Argument must be positive` (E2).
- A value that leaves less than one window row is clamped silently to `rows − 2`, as nvim clamps it (E2).
  ruse clamps every frame, so a resize to a smaller screen keeps the option's value and narrows only the
  layout. Whether nvim does the same on resize was not probed.

#### 1.2 What fits without a prompt (N ≥ 1)

- **Fits:** a message is shown on the message rows, starting at the first row (E6, E8), when it wraps to at
  most N rows and its last row takes at most `cols − 1` cells. That is nvim's `v:echospace` with
  `'noshowcmd'` (E9), and ruse has no `'showcmd'`.
- **Does not fit:**
  - The message area grows up over the windows. The bar's row and the window rows above are painted over, and
    the rows above them stay where they are, with no scroll (E16). The last row becomes the hit-enter prompt,
    or `-- More --` when the output is taller than the screen (§4).
  - The `CutRow` special case of #1235 and the prefix arithmetic in `fit()` apply only at 0. At N ≥ 1 the
    message has the whole row, as nvim's does.
- **At 0:** today's rules stay as they are: `fit(prefix, …)`, `Cut`, `CutRow` and `More`.

#### 1.3 The default (open in D-068)

| Choice | For | Against |
| --- | --- | --- |
| **1 (recommended)** | nvim's default value and layout (E1, E4), and Emacs's (Emacs-2: mode line plus echo area). Messages get the whole row, so the prefix no longer eats their cells. `:set ch?` reads as nvim's, and screen oracles compare the bottom rows one to one | One window row fewer than today. Every user sees the change. PTY tests that assert the shared row move (6 lines in `tests/pty_messages.rs` name the bar prefix; no other test file does) |
| 0 | Today's look and window count. RFC-0017 allows a frontend default that is better than nvim's. ruse's 0 already improves on nvim's own `ch=0`, which asks for `<CR>` after every message (E13) and drops the mode and undo messages (E14) | The default value differs from nvim's, so `:set ch&` gives 0. An nvim user's `set ch=1` changes the look |

#### 1.4 What is not copied

- **nvim's `ch=0` message policy**, a hit-enter for every message (E13). ruse's 0 keeps today's bar message,
  which is the reason for having 0 at all. nvim marks `ch=0` as experimental, and its help says it "works
  better with ui2".
- **The `'showcmd'` reserve** of 11 columns (E9). ruse has no `'showcmd'`.

### 2. `'shortmess'`

- **Value:** a string, default `ltToOCF` (E1). Every nvim flag is accepted: `l m r w a o O s t T W A I c q F
  S C`. Any other character gives `E539: Illegal character <x>` (E3).
- **Rule (D-068):** a flag takes effect for every message ruse gives that the flag names. A feature that later
  adds such a message honours its flag in the same PR.

| Flag | Effect in ruse | Evidence |
| --- | --- | --- |
| `l` | file messages say `3L, 12B` (set) or `3 lines, 12 bytes` (unset) | E29 |
| `m` | `[+]` for `[Modified]` in CTRL-G and file messages | E30 |
| `r` | `[RO]` for `[readonly]` in file messages. ruse's file messages carry no `[readonly]` today; the one place it is shown is the recovery prompt's read-only note (`ui/recovery.rs:316`), and `r` applies there | `:help shm-r` (not probed) |
| `w` | `[w]` for `written`, `[a]` for `appended` | E29 |
| `a` | all of `l m r w` | `:help shm-a` |
| `W` | no `written` / `[w]` in the write message | E28 |
| `F` | no file info when a file is edited (`:e` and startup were probed; `:b` and the others follow `:help shm-F` and were not probed). **Default on, so the `:e` message of ruse-2 goes away.** With `F` unset, the edit message uses nvim's wording (`"g.txt" [New] --No lines in buffer--`, `"f.txt" 3L, 12B`) and not ruse's `"g.txt" 0 bytes` (`app/dispatch/open.rs:132`) | E31, E32, ruse-2 |
| `t` | the existing `<` cut of a too-long file message (#1222). Unset: the message goes to the hit-enter prompt instead | `:help shm-t`. The unset case was not probed |
| `I` | no startup line (`ruse — :q to quit`). This is a ruse mapping: nvim's `I` hides its intro screen, and ruse's startup line plays that part | ruse-1 |
| `o O s S A c C q T` | **no effect**: ruse gives none of these messages (ruse-3: no search-wrap message, no search count, no `recording @a`, no completion messages, no swap-file `ATTENTION`). A new message replaces the old one on the row, so `o`/`O` already hold. Which messages nvim's `T` cuts in the middle was not probed | ruse-3 |

`'shortmess'` changes only how messages are worded and whether they are shown. The `:messages` history keeps
what it kept before. For `W`/`F`, a message that is not given is not kept, which matches nvim's `:silent`-like
wording in `:help shm-F`. Whether nvim keeps a `W`-shortened write message in `:messages` was not probed, and
it is probed in slice 2.

### 3. Output as rows

- **Output:** a message given with `Status::output(cmdline, rows)` carries the typed command line and its rows.
  `Status::echo(text)` stays for one-row replies. `Kind` is unchanged: output is `Echo` and is not kept in
  `:messages` (#1235).
- **`:!cmd` and `:w !cmd`:** rows are the command's output lines. A trailing newline adds one blank row, as
  E16 and E17 show. No output gives the typed row and one blank row (E17). The shell failure note
  (`with_shell_failure`) is its own last row. nvim's exact wording of `shell returned N` in this position was
  not probed, and it is probed in slice 3.
- **`:ls`:** one row per buffer in nvim's columns: `  1 %a   "f.txt"                        line 1` (E11).
- **The typed command line:** it is the first row of any output that scrolls (E16, E19). It counts toward
  what fits in N rows (E11).
- **The blank row above it** (E16 row 18, E19 row 16) is copied as nvim shows it. Why nvim leaves it was not
  checked in the source.

### 4. One pager

#### 4.1 Paging

- The `ui/message.rs` `Pager` of #1274 pages every output, with its keys and steps unchanged.
- Output that arrives as a whole (a listing) starts at its top with `-- More --` (E20).
- Shell output shows its end, as nvim's does (E18: nvim does not page `:!` output on the way out). The
  hit-enter prompt then scrolls back into it.
- **`'more'`** is a bool, default on. When it is off there is no `-- More --`: the last page shows above the
  hit-enter prompt (E25).

#### 4.2 `g<`

- When an output or a message is dismissed, `Status` keeps the rows that were shown as **last output**. After
  `q` at `-- More --`, the rest of the rows are dropped (E21).
- `g<` (a frontend Normal-mode key, like `ga`) shows the last output again at the hit-enter prompt, and the
  store is then emptied. A second `g<` shows nothing (E24).
- Motions and edits in between leave the store alone (E22, E23).
- With `'nomore'`, `g<` does nothing (E26).
- The store is one `Vec<String>` per session, frontend-only, and never kept across sessions.

#### 4.3 Which listings go through the pager

- **Through the pager:** every view-only listing. That is all `show_rows` callers (`:p :nu :l :# :z :g//p
  :set :set all [I ]I :checkhealth :inspect config`), `:ls`, `:!`, `:w !` and `:messages`. A view-only
  overlay holds nothing the pager loses. `Overlay::Listing` and `keyword_lines_picker::open_rows` then have no
  callers and are removed.
- **Interactive pickers stay** (open in D-068, recommended): `:reg`/`:di`, `:marks`, `:jumps`, `:changes`,
  `:dig`, `:diagnostics`. Enter there acts (jumps or inserts), which nvim's printed list cannot do. These are
  ruse's better-than-vanilla surfaces (RFC-0017). Their rows are not stored as last output. The alternative is
  to print them as nvim does (E19) and give up the pickers.

### 5. `'messagesopt'` `wait:` and `progress:`

- **`wait:{n}`** (used only without `hit-enter`): output that would ask for `<CR>` is shown with no prompt for
  `n` ms and then cleared (E27). `wait:0` clears it at once. The event loop already waits until a deadline
  (autosave). nvim marks `wait:` as deprecated with ui2, so it is implemented only because an accepted value
  that does nothing misleads users (open in D-068).
- **`progress:`** stays checked and has **no effect**: ruse gives no progress messages (`lsp/client.rs:199`
  drops `$/progress`). The rule of §2 applies: the feature that first shows progress honours it.

### 6. The Emacs profile

- **What Emacs does:**
  - The echo area grows to fit a message, up to 25 % of the frame (Emacs-1, Emacs-3), with no prompt. The
    next key shrinks it and runs.
  - Output beyond that goes to a buffer (`*Shell Command Output*`, Emacs-4).
  - The history is the `*Messages*` buffer.
- **ruse's Emacs profile today:** it has no `M-!` and no `*Messages*`, and shares the Vim bar and hit-enter
  prompt.
- **This RFC:**
  - It changes neither profile's key bindings. The layout of §1 is a frontend setting, so with the default
    of 1 the Emacs profile gets the mode line plus echo area rows that Emacs has (Emacs-2).
  - The echo area's growth policy (`resize-mini-windows`, `max-mini-window-height`), its overflow buffer and
    `*Messages*` are a separate Emacs-profile design. The pager is written behind one `MessageArea` policy
    (prompt, or grow then overflow) so that design can drop in.

### 7. Regression guard

- **Screen tests:** each slice adds PTY tests at 80x24 (`tests/support` `Editor`, as `tests/pty_messages.rs`
  does) that assert whole bottom rows copied from the captures above: E5–E11, E16–E18, E20–E26 and E28–E31.
- **Layout and pager:** `BottomLayout` gets unit tests for every value, clamping and resizes. The pager's
  existing step table (#1235) stays as it is.
- **Default 0:** if the maintainer keeps 0, no existing test changes in slice 1.
- **Default 1:** the shared-row asserts in `tests/pty_messages.rs` (6 lines) are rewritten to the two-row layout, and
  one test pins `ch=0` to keep today's layout covered.
- **Unchanged:** the diff renderer (`flush_diff`) and the window layout (`window_rects`) take `text_rows` as
  they do now.

## Reference Invariants

- **INV-DOC-VIEW / "folds are frontend-only":** the layout, the pager, the last-output store and
  `'shortmess'` are frontend state. The Document and core are untouched.
- **INV-CMD-SEMANTIC:** `g<` is a frontend key handled as the pager's keys are, the way #1274 handles them.
  It produces no document `Command`.
- **INV-RENDER-PROFILE:** unaffected. The bottom layout is the same on both render tiers.
- **INV-PROFILE-ISOLATION:** no key is added to the Emacs key space (§6).
- No new invariant.

## Failure modes & Recovery

- **A screen too small for the asked height:** the height is clamped every frame (§1.1). A screen with fewer
  than 3 rows falls back to 0.
- **An answer prompt:** the recovery prompt (`ui/recovery.rs:604`) and the questions of `fit_question` use the
  message area's first row. A question is never paged (#619), and that does not change.
- **Large output:** output rows are capped at the `:messages` cap (10 000) and the rest is dropped with a
  `(N more lines)` row. nvim has no such cap. This guards against a runaway `:!` filling memory.

## Security impact

None. Shell output was shown before. It is still transcribed (`^X`, `<xx>`) by `ui/message.rs` `cells` and
`screen::transcribe` and never reaches the terminal raw (#1181).

## Performance impact

The bottom layout is computed once per frame. Output rows are wrapped once per frame, as `Shown::More` already
does. The last-output store copies the rows that were shown when output is dismissed.

## Compatibility & Migration

- No stored format changes. `'cmdheight'`, `'shortmess'` and `'more'` are new `:set` options.
- **Config keys** (`spec/config-schema.yaml`, in the `render.*` group like `render.wrap`):
  - `render.cmdheight` (int, min 0, default per D-068)
  - `render.shortmess` (string, default `ltToOCF`, E539 rule)
  - `render.more` (bool, true)
  - Each has `derives_from: [F-006]`.
  - `'showmode'`, `'report'` and `'messagesopt'` have no config key today. Whether these three get keys is
    the maintainer's call (§Open questions).
- **PRD F-006:** a note per slice. No new F-id.
- **`spec/capabilities.yaml`:** no change. No CAP is added, and F-006 already depends on C-RENDER.
- **The `F` default:** it removes the `:e` file message ruse shows today (ruse-2). Users who want it set
  `shm-=F`.

## Observability

- `:set ch? shm? more?` report the values.
- `:messages` is unchanged.
- The startup line is the only message `I` hides, and `RUSE_LOG` still logs errors and warnings when they are
  given (#966).

## Alternatives

- **N ≥ 2 adds rows under today's shared row** (1 = today, 2 = bar plus 1 message row). This keeps the default
  value 1 and today's look, but no value then gives nvim's `ch=1` layout, which is what the issue asks for, and
  N would count one row fewer than nvim's.
- **Always a separate bar row, with no 0.** This is simpler, but it removes today's layout with no way back.
- **Route every listing through the pager, pickers too.** This matches nvim most closely (E19), but it loses
  Enter-to-jump in `:marks`, `:jumps` and `:changes`, and insertion from `:dig`.
- **The Emacs model for both profiles:** an echo area that grows, with overflow into a buffer. Vim users
  expect `-- More --` and `g<`, and the pager already exists (#1274).

## Rejected approaches

- **nvim's exact `ch=0`** (hit-enter for every message, no mode or undo messages; E13, E14). It is worse than
  what ruse has, and nvim itself calls it experimental.
- **Accepting every `'shortmess'` flag and claiming it works.** The flags with no message to act on are listed
  as having no effect (§2) instead.
- **Copying nvim's duplicated `:!cmd` row in `g<`.** In one probe `g<` showed the typed `:!` line twice. That
  is an artefact of the scrollback, not behaviour anyone relies on.

## Trade-offs

- With default 1, the screen loses a window row in exchange for nvim's and Emacs's layout. With default 0, it
  keeps the row and the bar loses message width.
- The pickers stay as a known divergence from nvim's printed lists (§4.3).
- `I` hides ruse's startup line, which is not nvim's intro screen.

## Re-evaluation conditions

- **A tabpage feature:** `'cmdheight'` becomes tabpage-local.
- **nvim's ui2 replaces the legacy message grid upstream** (nvim marks `wait:` and the hit-enter prompt
  deprecated with ui2): re-probe against the new default and revisit §4.
- **The Emacs-profile echo-area design (§6):** it may want the layout of §1 per profile.
- **A `'showcmd'` feature:** the last-row width of §1.2 becomes `v:echospace`.

## Open questions

These are listed in D-068 for the maintainer:

1. **The default `'cmdheight'`:** 1 (nvim and Emacs layout, recommended) or 0 (today's look).
2. **The `F` default:** follow nvim and drop the `:e` file message (recommended), or ship `ltToOC` without `F`
   as a better-than-vanilla default.
3. **Pickers or the pager** for `:reg :marks :jumps :changes :dig`: keep the pickers (recommended).
4. **`wait:`:** implement it (recommended, small), or leave it checked with no effect and document it.
5. **Config keys** for `'cmdheight'`, `'shortmess'` and `'more'` (recommended), and whether `'showmode'`,
   `'report'` and `'messagesopt'` get theirs in the same slice. Recommended: no, they get theirs separately.
6. **Splitting the issue:** keep slices 1–2 (layout, `'shortmess'`) on #1275 and file slices 3–4 (pager) as
   their own issue. Recommended, because the layout and the pager touch different code and land separately.

## Slices

1. **slice 1 (#1275): the bottom layout.** Adds `BottomLayout` (§1.1) and `'cmdheight'` (0..rows−2, E487,
   clamping), the message rows and the fit rule (§1.2), the command line on the first message row, and the
   default from D-068. It also adds the `render.cmdheight` key, an F-006 note and PTY tests E4–E10 and E12,
   E15.
2. **slice 2 (#1275): `'shortmess'`.** Adds the option and its E539 check, the effective flags `l m r w a W F
   t I` (§2), and the no-effect list in the option's help text. It also adds the `render.shortmess` key, an
   F-006 note and PTY tests E28–E32 and ruse-1/2.
3. **slice 3 (#1275): output rows and the pager for listings.** Adds `Status::output` (§3), multi-row `:!` and
   `:w !`, the `:ls` columns, the typed command row and the blank row, `show_rows` routed through the pager,
   and the removal of `Overlay::Listing`. It also adds `g<` and the last-output store, `'more'` and the
   `render.more` key (§4), and PTY tests E11, E16–E26.
4. **slice 4 (#1275): `'messagesopt'` `wait:`** (§5), if D-068 says to implement it, with PTY test E27.

Each slice keeps `cargo test --workspace` and the parity census green. No slice touches `crates/core`.
