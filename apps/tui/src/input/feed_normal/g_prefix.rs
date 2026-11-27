//! The `g{char}` second key (#1477): `g_command` routes each key to one decoder per `g` family —
//! status info, mark / change-list jumps, Select entry, the Visual forms, motions, the operators and
//! the remaining edit commands. Every guard stays on the top-level arm, so a key whose guards all fail
//! still reaches the operator-pending `unmatched` policy; each family decoder only sees the chars its
//! arms route to and ends in `unreachable!`.

use crossterm::event::{KeyCode, KeyEvent};
use ruse_core::{Command, Mode, Motion, SelectKind, VisualOp, WordCase};

use super::super::{Awaiting, Feed, InputEngine, Ns, Op, ScreenMotion};

impl InputEngine {
    /// The `g{char}` command itself, once the chord and operator-pending checks let the key through.
    pub(super) fn g_command(&mut self, key: KeyEvent, mode: Mode, ctrl: bool) -> Feed {
        let visual = matches!(mode, Mode::Visual { .. });
        match key.code {
            // `g CTRL-G` is checked before the bare `g` (=`gg`) motion arm, since that arm ignores the
            // ctrl modifier. `!ctrl` keeps `ga` distinct from the Visual `g CTRL-A` arm below.
            KeyCode::Char('g') if ctrl => self.g_info('g'),
            KeyCode::Char(c @ ('a' | '8')) if !ctrl => self.g_info(c),
            KeyCode::Char(c @ ('`' | '\'')) if !ctrl => self.g_mark_jump(c),
            KeyCode::Char(c @ (';' | ',')) => self.g_mark_jump(c),
            KeyCode::Char(c @ ('h' | 'H' | 'V'))
                if mode == Mode::Normal && self.normal.op.is_none() =>
            {
                self.g_select(c, ctrl)
            }
            // The Visual forms come before their Normal twins (`gJ`, `gq`, `gc`, `gu`, ...).
            KeyCode::Char(c @ ('J' | 'q' | 'c' | 'u' | 'U' | '~' | '?')) if visual => {
                self.g_visual(c)
            }
            KeyCode::Char(c @ ('a' | 'x')) if ctrl && visual => self.g_visual(c),
            KeyCode::Char(
                c @ ('g' | 'e' | 'E' | '_' | 'j' | 'k' | '0' | '$' | '^' | 'm' | 'o' | 'M' | '*'
                | '#' | 'd' | 'D' | 'n' | 'N'),
            ) => self.g_motion(c),
            KeyCode::Char(c @ ('u' | 'U' | '~' | '?')) if self.normal.op.is_none() => {
                self.g_operator(c)
            }
            KeyCode::Char(c @ ('q' | 'w')) if matches!(mode, Mode::Normal) => self.g_operator(c),
            KeyCode::Char('c') if matches!(mode, Mode::Normal) && self.normal.op.is_none() => {
                self.g_operator('c')
            }
            KeyCode::Char(
                c @ ('J' | 'i' | 'I' | 'r' | 'p' | 'P' | '&' | 'v' | 'R' | '-' | '+' | 'Q'),
            ) => self.g_edit(c, mode),
            // A pending construct is in flight, so this is `closed/abort` — the policy
            // that distinguishes operator-pending from Normal (VS-OBL-3).
            _ => self.unmatched(Ns::OperatorPending, key),
        }
    }

    /// `g CTRL-G` / `ga` / `g8` — frontend-resolved, status-surfaced info; no buffer mutation.
    fn g_info(&mut self, c: char) -> Feed {
        match c {
            // `g CTRL-G` — cursor position / buffer counts (Col/Line/Word/[Char]/Byte).
            'g' => self.action(Command::CursorInfo),
            // `ga` — the numeric value of the character under the cursor (`:ascii`/`:as` synonym).
            'a' => self.action(Command::AsciiInfo),
            // `g8` — the UTF-8 bytes of the character under the cursor.
            '8' => self.action(Command::Utf8Info),
            _ => unreachable!("g_info routed {c:?}"),
        }
    }

    /// `` g` `` / `g'` / `g;` / `g,` — jumps that do not touch the jumplist.
    fn g_mark_jump(&mut self, c: char) -> Feed {
        match c {
            // `` g`{mark} `` / `g'{mark}` — a mark jump that leaves the jumplist alone (#1175); under an
            // operator the same motion as `` `{mark} ``/`'{mark}`. The count and operator stay armed.
            '`' => self.arm_awaiting(Awaiting::MarkJump { keep_jumps: true }),
            '\'' => self.arm_awaiting(Awaiting::MarkJumpLine { keep_jumps: true }),
            // `g;` / `g,` — walk the change list to older / newer change positions (Vim).
            ';' => self.action(Command::GotoOlderChange),
            ',' => self.action(Command::GotoNewerChange),
            _ => unreachable!("g_mark_jump routed {c:?}"),
        }
    }

    /// `gh` / `gH` / `g CTRL-H` / `gV` — Select entry, from Normal with no operator pending.
    fn g_select(&mut self, c: char, ctrl: bool) -> Feed {
        match c {
            // `gh` / `gH` / `g CTRL-H` — start Select charwise / linewise / blockwise (`:help gh`).
            'h' | 'H' => self.enter_select(if ctrl {
                SelectKind::Blockwise
            } else if c == 'H' {
                SelectKind::Linewise
            } else {
                SelectKind::Charwise
            }),
            // `gV` only matters after a mapping that reselects in Select mode; from Normal it does
            // nothing (nvim v0.12.6).
            'V' => {
                self.reset();
                Feed::Ignored
            }
            _ => unreachable!("g_select routed {c:?}"),
        }
    }

    /// The Visual `g` forms: they act on the selection at once instead of arming an operator.
    fn g_visual(&mut self, c: char) -> Feed {
        match c {
            // Visual `gJ` / `gq` — join the selected lines without spaces / reflow them.
            'J' => self.action(Command::VisualOp(VisualOp::Join { spaces: false })),
            'q' => self.action(Command::VisualOp(VisualOp::Format)),
            // Visual `gc` — comment / uncomment the selected lines (nvim's default `gc`, #618).
            'c' => self.action(Command::VisualOp(VisualOp::Comment)),
            // In Visual, `gu`/`gU`/`g~` recase the selection immediately (same as bare `u`/`U`/`~`).
            'u' => self.action(Command::CaseSelection(WordCase::Downcase)),
            'U' => self.action(Command::CaseSelection(WordCase::Upcase)),
            '~' => self.action(Command::CaseSelection(WordCase::Toggle)),
            '?' => self.action(Command::CaseSelection(WordCase::Rot13)),
            // Visual `g CTRL-A` / `g CTRL-X` — increment the selected lines as a SEQUENCE: the first
            // numbered line gets ±count, the next ±2·count, and so on (make a column of 1s into 1,2,3…).
            'a' => self.action(Command::IncrementSelection {
                delta: i64::from(self.mcount()),
                sequential: true,
            }),
            'x' => self.action(Command::IncrementSelection {
                delta: -i64::from(self.mcount()),
                sequential: true,
            }),
            _ => unreachable!("g_visual routed {c:?}"),
        }
    }

    /// The `g` motions — every one operator-aware, so they also finish a pending operator.
    fn g_motion(&mut self, c: char) -> Feed {
        match c {
            'g' => self.motion(Motion::GotoLine),
            // `ge` / `gE` — backward to the end of the previous word / WORD (operator-aware via
            // `motion`, so `dge` deletes back through the previous word-end).
            'e' => self.motion(Motion::WordEndBack),
            'E' => self.motion(Motion::BigWordEndBack),
            // `g_` — to the last non-blank char of the line (`{count}g_` = count-1 lines down).
            '_' => self.motion(Motion::LineLastNonBlank),
            // Screen-line motions `gj`/`gk`/`g0`/`g^`/`gm`/`g$` (#615): they count in the window's
            // screen rows ('wrap', 'showbreak', the gutter, `leftcol`), which the engine does not
            // know, so the frontend resolves them (the operator, count and register stay armed) and
            // finishes them with `screen_motion_to` / `screen_motion_lines` / `screen_motion_fail`.
            'j' => self.screen_motion(ScreenMotion::Down),
            'k' => self.screen_motion(ScreenMotion::Up),
            '0' => self.screen_motion(ScreenMotion::Start),
            '$' => self.screen_motion(ScreenMotion::End),
            '^' => self.screen_motion(ScreenMotion::FirstNonBlank),
            'm' => self.screen_motion(ScreenMotion::Middle),
            // `[count]go` — go to the count-th byte of the buffer (operator-aware: `dgo`).
            'o' => self.motion(Motion::GotoByte),
            // `gM` — go to `count`% of the line by CHARACTER count (bare `gM` = the middle char,
            // i.e. 50%). The count is a PERCENTAGE, not a repeat, so seed it into `count` (default 50)
            // and let `motion` carry it through — exactly the count-as-percentage shape `{count}%`
            // (`GotoPercent`) uses. Operator-aware (`dgM`); `op_over_motion` clears the count after.
            'M' => {
                if self.normal.typed() == 0 {
                    self.normal.count = 50; // bare `gM` = 50% of the line
                }
                self.motion(Motion::MidLine)
            }
            // `g*` / `g#` — like `*`/`#` but match the word ANYWHERE (no `\<…\>` boundaries).
            '*' => self.word_search(true, false),
            '#' => self.word_search(false, false),
            // `gd` / `gD` — go to the local / global declaration of the keyword under the cursor via
            // Vim's TEXT heuristic (NOT LSP; ruse's LSP goto is separate). The engine has no buffer,
            // so — exactly like `*`/`#` — the frontend reads the word and rewrites this to a concrete
            // whole-file first-match jump. Both land on the first whole-word match from the top of the
            // file, matching nvim v0.12.4 where `gd` and `gD` are identical (verified); `global` marks
            // `gD` and reserves the `gd` enclosing-block refinement as a follow-up.
            'd' => self.action(Command::GotoDeclaration { global: false }),
            'D' => self.action(Command::GotoDeclaration { global: true }),
            // `gn` / `gN` — the search-match text object: select (bare) or operate on (`dgn`/`cgn`/
            // `ygn`) the next / previous match of the last search pattern. Operator-aware.
            'n' => self.search_object(false),
            'N' => self.search_object(true),
            _ => unreachable!("g_motion routed {c:?}"),
        }
    }

    /// The `g` operators, armed over the next motion.
    fn g_operator(&mut self, c: char) -> Feed {
        match c {
            // `gu` / `gU` / `g~` / `g?` — arm a case operator (lower / upper / toggle / ROT13) over
            // the next motion. Only from Normal (no operator already pending): `dgu` is not Vim.
            'u' => self.arm_op(Op::CaseLower),
            'U' => self.arm_op(Op::CaseUpper),
            '~' => self.arm_op(Op::CaseToggle),
            '?' => self.arm_op(Op::Rot13),
            // `gq` / `gw` — arm the reflow operator over the next motion (`gqap`, `gwj`). Normal
            // only. `gqgq` is the doubled line form (through the operator-pending `g` tier, `g_under_op`).
            'q' => self.arm_op(Op::Format),
            'w' => self.arm_op(Op::FormatKeep),
            // `gc` — arm the comment operator (`gcj`, `gcip`; `gcc` is its line form). The `gc`
            // comment text object (`dgc`, `gcgc`) is not modelled: `gcgc` cancels (#618).
            'c' => self.arm_op(Op::Comment),
            _ => unreachable!("g_operator routed {c:?}"),
        }
    }

    /// The remaining `g` edit commands: join, insert entry, paste, repeat, undo travel and Ex mode.
    fn g_edit(&mut self, c: char, mode: Mode) -> Feed {
        match c {
            // `gJ` — join with the next line WITHOUT inserting a space (Vim `gJ`).
            'J' => self.action(Command::JoinLinesNoSpace(self.mcount())),
            // `gi` — resume Insert at the last-insert position (Vim `` `^ ``).
            'i' => self.action(Command::InsertAtLastInsert),
            // `gI` — insert at column 1 (byte column 0), BEFORE all indentation (`I` stops at the
            // first non-blank). Routed through `insert_entry` so `{count}gI` repeats the typed text.
            'I' => self.insert_entry(Command::InsertColumnZero),
            // `gr{char}` — CLASSIC-Vim virtual-replace of ONE (or `{count}`) char, then back to Normal
            // (the one-shot of `gR`). NOTE: nvim 0.11+ maps `gr`/`grn`/`gra`/`grr` to LSP actions by
            // default, but those are keymaps, not a built-in; ruse installs no such map and targets the
            // classic-Vim built-in. Arm the char expectation; the count carries through untouched.
            'r' => self.arm_awaiting(Awaiting::VirtualReplaceChar),
            // `gp` / `gP` — paste like `p`/`P` but leave the cursor JUST AFTER the pasted text. In Visual they
            // replace the selection like Visual `p`: both swap the deleted text into the unnamed register
            // (nvim keeps the registers only for `P`), and `gP` puts a linewise register above a block (#1616).
            'p' | 'P' if matches!(mode, Mode::Visual { .. } | Mode::Select { .. }) => {
                self.action(Command::PasteSelection {
                    swap: true,
                    count: self.mcount(),
                    after: c == 'p',
                    move_after: true,
                })
            }
            'p' | 'P' => self.action(Command::Paste {
                after: c == 'p',
                count: self.mcount(),
                move_after: true,
            }),
            // `g&` — repeat the last `:s` over the whole file with its flags (frontend-resolved).
            '&' => self.action(Command::RepeatSubstituteGlobal),
            // `gv` — re-select the last visual selection (D-027 depth-1 slice).
            'v' => self.action(Command::ReselectVisual),
            // `gR` — enter Virtual Replace mode (tab-aware overwrite).
            'R' => self.insert_entry(Command::EnterVirtualReplace),
            // `g-` / `g+` — chronological undo-time travel across branches (F-005 #3).
            '-' => self.action(Command::UndoOlder),
            '+' => self.action(Command::UndoNewer),
            // `gQ` — enter Ex mode (F-026 #3). `Q` alone is NOT Ex at the pinned Neovim
            // revision, so only this two-key form opens it; the line re-prompts until `:visual`.
            // Like `:`, it ends the command: the count is dropped and a `CTRL-O` one-shot returns
            // to Insert once the Ex line closes. Under an operator or in Visual it only cancels
            // (Vim's `checkclearopq`): `dgQ` edits nothing and opens no Ex mode (#806).
            'Q' => {
                let refused = self.normal.op.is_some()
                    || matches!(mode, Mode::Visual { .. } | Mode::Select { .. });
                self.reset();
                if refused {
                    return Feed::Ignored;
                }
                self.open_cmdline(':', true);
                Feed::Pending
            }
            _ => unreachable!("g_edit routed {c:?}"),
        }
    }
}
