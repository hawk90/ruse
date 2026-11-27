//! The Visual and Select keys over an existing selection (#1502): `base_selection` routes each key
//! that `feed_base` passed on to one decoder per key family — the shape switch, the blockwise forms,
//! the selection edits, the recase, the command line and the line operators. Every guard stays on the
//! top-level arm, so a key whose guards all fail still reaches the shared count / motion fallthrough or
//! the namespace's `unmatched` policy; each family decoder only sees the chars its arms route to and
//! ends in `unreachable!`.

use crossterm::event::{KeyCode, KeyEvent};
use ruse_core::{Command, Mode, Motion, SelectKind};

// `input/mod.rs` imports these for the selection keys; they reach this module through it.
use super::super::{
    motion_key, Awaiting, BlockInsertKind, Feed, InputEngine, Ns, OpKind, VisualOp, WordCase,
};

impl InputEngine {
    /// Visual and Select keys over an existing selection; `None` falls through to the shared count /
    /// motion grammar (no operator is ever armed in a selection).
    pub(super) fn base_selection(
        &mut self,
        key: KeyEvent,
        mode: Mode,
        kind: SelectKind,
    ) -> Option<Feed> {
        let is_block = kind == SelectKind::Blockwise;
        let visual = matches!(mode, Mode::Visual { .. });
        match key.code {
            KeyCode::Esc => Some(self.action(Command::EnterNormal)),
            KeyCode::Char(c @ ('v' | 'V')) => Some(self.selection_shape(c, kind)),
            // The blockwise forms come before their charwise / linewise twins (`c`, `s`, `O`, `D`, `C`,
            // `X`, `Y`).
            KeyCode::Char(c @ ('I' | 'A' | 'c' | 's' | 'O' | 'D' | 'C' | 'X' | 'Y'))
                if is_block =>
            {
                Some(self.selection_block(c))
            }
            KeyCode::Char(c @ ('d' | 'x' | 'y' | 'c' | 's' | 'r' | 'p' | 'P' | 'o' | 'O')) => {
                Some(self.selection_edit(c))
            }
            // Visual `u`/`U`/`~` recase the selection (Visual only; in Select a printable key replaces
            // it via the namespace policy). `gu`/`gU`/`g~` in Visual are a follow-up.
            KeyCode::Char(c @ ('u' | 'U' | '~')) if visual => Some(self.selection_case(c)),
            // `/` and `?` open the search line; the match it lands on extends the selection, as any
            // motion does (`v_/`, #1113). In Select a printable key replaces the selection instead.
            KeyCode::Char(c @ ('/' | '?')) if visual => Some(self.enter_search(c == '?')),
            // `*` / `#` search forward / backward for the selected text and leave Visual (nvim's default
            // `v_star-default` mapping, #1557); the frontend reads the selection.
            KeyCode::Char(c @ ('*' | '#')) if visual => {
                let count = self.mcount();
                Some(self.action(Command::SearchSelection {
                    forward: c == '*',
                    count,
                }))
            }
            // `n` / `N` repeat the last search as a motion that extends the selection.
            KeyCode::Char('n' | 'N') if visual => None,
            KeyCode::Char(c @ (':' | '!')) => Some(self.selection_cmdline(c)),
            KeyCode::Char(c @ ('>' | '<' | '=' | 'J' | 'D' | 'X' | 'Y' | 'C' | 'S' | 'R')) => {
                Some(self.selection_lines(c))
            }
            // In a selection, `i`/`a` always begin a text object (there is no insert here); the next key
            // is its selector. The completed object re-spans the selection (see the core's `Move` arm).
            KeyCode::Char(c @ ('i' | 'a')) => {
                Some(self.arm_awaiting(Awaiting::TextObjectChar { inner: c == 'i' }))
            }
            // Count digits and motions extend the selection; an unmatched key hits the namespace's
            // own policy — `closed/ignore` for Visual, `open/replace-selection` for Select.
            KeyCode::Char('1'..='9') => None,
            KeyCode::Char('0') if self.normal.count > 0 => None,
            KeyCode::Char('0') => Some(self.motion(Motion::LineStart)),
            _ if motion_key(key.code).is_some() => None,
            // `<Space>`/`<BS>` extend a Visual selection like `l`/`h`, wrapping across lines. In Select
            // they stay with the namespace policy (a typed space replaces the selection).
            KeyCode::Char(' ') | KeyCode::Backspace if visual && key.modifiers.is_empty() => None,
            _ => {
                let ns = if visual { Ns::Visual } else { Ns::Select };
                Some(self.unmatched(ns, key))
            }
        }
    }

    /// `v` / `V` (and `CTRL-V`, in `feed_chord`) switch the selection SHAPE: pressing the key of the
    /// current shape leaves the namespace (to Normal), any other switches to that shape (F-025 c1).
    fn selection_shape(&mut self, c: char, kind: SelectKind) -> Feed {
        let target = match c {
            'v' => SelectKind::Charwise,
            'V' => SelectKind::Linewise,
            _ => unreachable!(),
        };
        self.action(if kind == target {
            Command::EnterNormal
        } else {
            Command::EnterVisual { kind: target }
        })
    }

    /// The blockwise-only keys.
    fn selection_block(&mut self, c: char) -> Feed {
        match c {
            // Blockwise insert-replicate: `I` at the left edge, `A` at the right edge, `c`/`s` delete
            // the block then insert at the left edge — each replicates on `<Esc>` (blockwise slice 2).
            'I' => self.action(Command::BlockInsert(BlockInsertKind::Insert)),
            'A' => self.action(Command::BlockInsert(BlockInsertKind::Append)),
            'c' | 's' => self.action(Command::BlockInsert(BlockInsertKind::Change)),
            // `O`: in a block, the other corner on the same row.
            'O' => self.action(Command::VisualOp(VisualOp::OtherCorner)),
            // In a block `D`/`C` run to each line's end and `X`/`Y` are the block `d`/`y` (nvim v0.12.6).
            'D' => self.action(Command::VisualOp(VisualOp::BlockToEol(OpKind::Delete))),
            'C' => self.action(Command::VisualOp(VisualOp::BlockToEol(OpKind::Change))),
            'X' => self.action(Command::DeleteSelection),
            'Y' => self.action(Command::YankSelection),
            _ => unreachable!(),
        }
    }

    /// The selection edits: delete, yank, change, replace, paste and the end swap.
    fn selection_edit(&mut self, c: char) -> Feed {
        match c {
            'd' | 'x' => self.action(Command::DeleteSelection),
            'y' => self.action(Command::YankSelection),
            'c' | 's' => self.action(Command::ChangeSelection),
            // Visual `r{char}` — replace every selected char with the next key (Vim `v_r`). Arm the
            // shared ReplaceChar expectation; its resolution sees the selection mode and emits the
            // selection form. Keeps the count axis (unused here) like Normal `r`.
            'r' => self.arm_awaiting(Awaiting::ReplaceChar),
            // Visual `p`/`P` — replace the selection with the register. `p` swaps the deleted text into
            // the unnamed register; `P` preserves it (paste the same thing over successive selections).
            'p' | 'P' => {
                let count = self.mcount();
                self.action(Command::PasteSelection {
                    swap: c == 'p',
                    count,
                    after: c == 'p',
                    move_after: false,
                })
            }
            // `o` swaps the selection's ends (cursor <-> anchor); the SAME text stays selected but a
            // later motion extends the other end. In Normal `o` is OpenBelow — here it is the swap.
            // Outside a block `O` is the same as `o`.
            'o' | 'O' => self.action(Command::SwapSelectionEnds),
            _ => unreachable!(),
        }
    }

    /// Visual `u` / `U` / `~`: recase the selection.
    fn selection_case(&mut self, c: char) -> Feed {
        let case = match c {
            'u' => WordCase::Downcase,
            'U' => WordCase::Upcase,
            '~' => WordCase::Toggle,
            _ => unreachable!(),
        };
        self.action(Command::CaseSelection(case))
    }

    /// `:` leaves Visual (setting `'<`/`'>`) and opens the command line on `'<,'>`; `!` opens it on
    /// `'<,'>!`, the filter over the selected lines (`:help v_:`, `v_!`).
    fn selection_cmdline(&mut self, c: char) -> Feed {
        let out = self.action(Command::VisualOp(VisualOp::LeaveAtStart));
        self.open_filter_cmdline(match c {
            ':' => "'<,'>",
            '!' => "'<,'>!",
            _ => unreachable!(),
        });
        out
    }

    /// Line operators over the selection (`:help v_>`, `v_J`, `v_=`): `{count}>` shifts `count` levels.
    /// The uppercase forms work on whole lines (the blockwise `D`/`C`/`X`/`Y` are taken before).
    fn selection_lines(&mut self, c: char) -> Feed {
        let op = match c {
            '>' | '<' => VisualOp::Shift {
                left: c == '<',
                count: self.mcount(),
            },
            '=' => VisualOp::Reindent,
            'J' => VisualOp::Join { spaces: true },
            'D' | 'X' => VisualOp::Lines(OpKind::Delete),
            'Y' => VisualOp::Lines(OpKind::Yank),
            'C' | 'S' | 'R' => VisualOp::Lines(OpKind::Change),
            _ => unreachable!(),
        };
        self.action(Command::VisualOp(op))
    }
}
