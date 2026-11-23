//! The base Normal grammar (#1480): `base_grammar` routes each key that `feed_base` passed on to one
//! decoder per key family — the count, the motions, the doubled operators, the operators, the insert
//! entries, the edit commands and the search / command-line keys. Every guard stays on the top-level
//! arm, so a key whose guards all fail still reaches the Normal `unmatched` policy; each family decoder
//! only sees the chars its arms route to and ends in `unreachable!`.

use crossterm::event::{KeyCode, KeyEvent};
use ruse_core::{Command, ForcedWise, Motion, SelectKind};

use super::super::{motion_key, push_count_digit, Awaiting, Feed, InputEngine, Ns, Op, OpPending};

impl InputEngine {
    /// The count / operator / motion grammar and the Normal-only keys: what is left of `feed_base` once
    /// the guards, the initiators and the selection keys passed the key on.
    pub(super) fn base_grammar(&mut self, key: KeyEvent) -> Feed {
        let armed = self.normal.op.is_some();
        match key.code {
            KeyCode::Char(d @ '1'..='9') => self.count_digit(d),
            KeyCode::Char('0') if self.normal.count > 0 => self.count_digit('0'),
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            // `<Space>` / `<BS>` move like `l` / `h` but wrap across lines (nvim's default 'whichwrap' is
            // `b,s`); under an operator see [`Motion::WrapRight`] / [`Motion::WrapLeft`].
            KeyCode::Char(' ') if key.modifiers.is_empty() => self.motion(Motion::WrapRight),
            KeyCode::Backspace if key.modifiers.is_empty() => self.motion(Motion::WrapLeft),
            // With an operator armed, `v`/`V` (and `CTRL-V`, in `feed_chord`) FORCE the next motion's wise
            // (Vim `o_v`/`o_V`/`o_CTRL-V`): `dvj`, `dVe`, `d<C-v>j`. They stay operator-pending (the motion
            // still follows); `motion` emits `OpForced`. Bare (no operator) they enter Visual/Visual-line.
            KeyCode::Char(c @ ('v' | 'V')) if armed => self.force_wise(c),
            KeyCode::Char(c @ ('v' | 'V')) => self.enter_visual(c),
            // `gcc` — the doubled comment operator: its `c` is the doubling key, not `c` (#618).
            KeyCode::Char('c')
                if matches!(
                    self.normal.op,
                    Some(OpPending {
                        op: Op::Comment,
                        ..
                    })
                ) =>
            {
                self.op_linewise()
            }
            // Doubled case operators (`guu` / `gUU` / `g~~` / `g??`): the second key repeats the operator to
            // make it linewise. With an operator armed only its doubling key passes the operator-pending
            // guard, so `u`/`U`/`~` here ARE the doubling key; `?` is also the backward search (`d?pat`).
            KeyCode::Char('u' | 'U' | '~') if armed => self.op_linewise(),
            KeyCode::Char('?')
                if matches!(self.normal.op, Some(OpPending { op: Op::Rot13, .. })) =>
            {
                self.op_linewise()
            }
            // `gqq` / `gww` — the doubled reflow operators format the current `count` lines (reachable now
            // that the session's macro layer leaves a `q` to an armed operator, #679). Only the doubling key
            // passes the operator-pending guard, so an armed operator here IS `gq` / `gw`.
            KeyCode::Char('q' | 'w') if armed => self.op_linewise(),
            KeyCode::Char(c @ ('d' | 'c' | 'y' | '>' | '<' | '=' | '!')) => self.base_operator(c),
            KeyCode::Char(c @ ('i' | 'a')) if armed => {
                self.arm_awaiting(Awaiting::TextObjectChar { inner: c == 'i' })
            }
            KeyCode::Char(c @ ('i' | 'a' | 'I' | 'A' | 'o' | 'O' | 'R')) => self.base_insert(c),
            // `<Insert>` = `i`.
            KeyCode::Insert if !armed => self.insert_entry(Command::EnterInsert),
            // With 'tildeop' `~` is the `g~` operator (`~w`; `~~` is its line form, above).
            KeyCode::Char('~') if self.tildeop => self.arm_op(Op::CaseToggle),
            // `s` and `X` are guarded to Normal (no pending operator) so `ds` / `dX` still abort.
            KeyCode::Char(c @ ('s' | 'X')) if !armed => self.base_edit(c),
            KeyCode::Char(
                c @ ('p' | 'P' | 'D' | 'C' | 'Y' | 'S' | 'x' | 'u' | 'U' | 'r' | '~' | 'J'),
            ) => self.base_edit(c),
            // `{count}<Del>` drops the count's last digit (`:help N<Del>`); a bare `<Del>` is `x`.
            KeyCode::Delete if self.normal.count > 0 => {
                self.normal.count /= 10;
                Feed::Pending
            }
            KeyCode::Delete if !armed => self.action(Command::DeleteUnder(1)),
            // `Z` arms `ZZ` / `ZQ`.
            KeyCode::Char('Z') if !armed => self.arm_awaiting(Awaiting::ZSecond),
            KeyCode::Tab => self.action(Command::GotoNewerJump),
            KeyCode::Char('.') => self.dot_repeat(key),
            KeyCode::Char(c @ ('n' | 'N' | '*' | '#' | '/' | '?' | ':')) => self.base_search(c),
            // The base namespace's own declared policy — not a shared fallthrough. In Visual this
            // line is unreachable (the Visual arm above returns first), which is why the two are
            // separate calls rather than one `mode`-derived namespace.
            _ => self.unmatched(Ns::Normal, key),
        }
    }

    /// A count digit (`1`-`9`, or `0` once a count is being typed) appends to the count.
    fn count_digit(&mut self, d: char) -> Feed {
        self.normal.count = push_count_digit(self.normal.count, d as u32 - '0' as u32);
        Feed::Pending
    }

    /// `v` / `V` under an operator: force the motion charwise / linewise.
    fn force_wise(&mut self, c: char) -> Feed {
        self.normal.forced_wise = Some(match c {
            'v' => ForcedWise::Charwise,
            'V' => ForcedWise::Linewise,
            _ => unreachable!(),
        });
        Feed::Pending
    }

    /// Bare `v` / `V`: enter Visual / Visual-line.
    fn enter_visual(&mut self, c: char) -> Feed {
        let kind = match c {
            'v' => SelectKind::Charwise,
            'V' => SelectKind::Linewise,
            _ => unreachable!(),
        };
        self.action(Command::EnterVisual { kind })
    }

    /// The operator keys: each arms its operator (or doubles the armed one).
    fn base_operator(&mut self, c: char) -> Feed {
        let op = match c {
            'd' => Op::Delete,
            'c' => Op::Change,
            'y' => Op::Yank,
            // `>`/`<` are operators: doubled (`>>`) shifts the current `count` lines, and over a motion
            // (`>j`, `>ap`) shifts the motion's lines (always linewise).
            '>' => Op::ShiftRight,
            '<' => Op::ShiftLeft,
            // `=` reindents (bracket-depth): doubled `==` is linewise, `=motion`/`=ap` over a motion.
            '=' => Op::Reindent,
            // `!{motion}{cmd}<CR>` — the filter operator (linewise, like `=`/`gq`); doubled `!!` (and
            // `{count}!!`) filters the current `count` lines. The frontend numbers the lines and opens the
            // `:{range}!` cmdline.
            '!' => Op::Filter,
            _ => unreachable!(),
        };
        self.operator(op)
    }

    /// The keys that enter Insert (or Replace) with no operator armed.
    fn base_insert(&mut self, c: char) -> Feed {
        let entry = match c {
            'i' => Command::EnterInsert,
            'a' => Command::EnterInsertAfter,
            'I' => Command::InsertLineStart,
            'A' => Command::AppendLineEnd,
            'o' => Command::OpenBelow,
            'O' => Command::OpenAbove,
            'R' => Command::EnterReplace,
            _ => unreachable!(),
        };
        self.insert_entry(entry)
    }

    /// The edit commands: paste, the line-operator synonyms, the char deletes, undo, replace, case and join.
    fn base_edit(&mut self, c: char) -> Feed {
        match c {
            'p' | 'P' => self.action(Command::Paste {
                after: c == 'p',
                count: self.mcount(),
                move_after: false,
            }),
            // Line-operator synonyms: `D`=`d$`, `C`=`c$`, `Y`=`y$` (nvim 0.6+ charwise), `S`=`cc`.
            // Each is the existing operator applied to an implicit motion, routed through the same
            // plan/commit path (so register geometry, cursor clamping, and dot-replayability match).
            'D' => self.action(Command::Delete(self.mcount(), Motion::LineEnd)),
            'C' => self.action(Command::Change(self.mcount(), Motion::LineEnd)),
            'Y' => self.action(Command::Yank(self.mcount(), Motion::LineEnd)),
            'S' => self.action(Command::Change(self.mcount(), Motion::Line)),
            // `s` (substitute char) = `cl`: change `count` chars rightward and enter Insert. Same
            // `Change(count, Right)` a `c l` produces, so register geometry and dot-repeat match. The
            // router guards it to Normal (no pending operator) so `ds` still aborts — `s` is not a motion.
            's' => self.action(Command::Change(self.mcount(), Motion::Right)),
            'x' => self.action(Command::DeleteUnder(self.mcount())),
            // `X` = `dh`: delete `count` chars before the cursor, never past the line's first char.
            'X' => self.action(Command::Delete(self.mcount(), Motion::Left)),
            'u' => self.counted(Command::Undo),
            // `U` — undo the latest changes on the last changed line (a count does nothing).
            'U' => self.action(Command::LineUndo),
            'r' => self.arm_awaiting(Awaiting::ReplaceChar),
            '~' => self.action(Command::ToggleCase(self.mcount())),
            'J' => self.action(Command::JoinLines(self.mcount())),
            _ => unreachable!(),
        }
    }

    /// Dot-repeat: replay the last recorded change at the current cursor (D-047). A leading `N`
    /// overrides the change's count (Vim `3.`). `.` itself never rewrites the record (beyond the
    /// numbered-register step below), so `..` repeats the same change; with no prior change it is a clean
    /// no-op (the Normal namespace's `closed/ignore` policy — Vim rings the bell).
    fn dot_repeat(&mut self, key: KeyEvent) -> Feed {
        if let Some(intent) = self.last_change.as_mut() {
            // A change that named `"1`-`"8` repeats with the next numbered register, and the record
            // keeps it, so `"1p...` walks the delete ring (nvim `do_redo`, #816); `"9` stays.
            if let Some(c @ '1'..='8') = intent.register {
                intent.register = char::from_digit(c.to_digit(10).unwrap_or(0) + 1, 10);
            }
            let intent = intent.clone();
            let count = (self.normal.typed() > 0).then_some(self.normal.typed());
            self.reset();
            // A change over a screen-line motion resolves the motion again where the cursor is (#1539).
            if let Some((motion, n)) = intent.screen {
                return self.redo_screen_motion(intent, motion, count.unwrap_or(n));
            }
            intent.replay(count).into_feed()
        } else {
            self.unmatched(Ns::Normal, key)
        }
    }

    /// The search repeats, the keyword searches and the search / command-line openers.
    fn base_search(&mut self, c: char) -> Feed {
        match c {
            // `n` repeats the last search in the SAME direction it was issued; `N` in the OPPOSITE. So
            // after `?foo`, `n` continues BACKWARD and `N` goes forward; after `/foo` they stay
            // forward-relative. Direction comes from the stored last-search flag. Both REAPPLY the stored
            // offset (`:help search-offset`), so they route through [`Command::Search`] (which carries the
            // offset) rather than the plain `SearchNext`/`SearchPrev`; the offset-aware match rule advances
            // past the current match even when the offset landed the cursor before it (`s-1`).
            'n' => self.repeat_search(false),
            'N' => self.repeat_search(true),
            // `*`/`#` — search the whole keyword under the cursor forward / backward; operator-aware
            // (`d*`). The planner reads the keyword from the buffer (the engine has no buffer).
            '*' => self.word_search(true, true),
            '#' => self.word_search(false, true),
            '/' => self.enter_search(false),
            // `?` — open the BACKWARD search line. Reachable here only when no ROT13 operator is pending
            // (the guarded `g??` arm in the router wins that case); an armed d/c/y operator is captured so
            // `d?pat` works, exactly like `d/pat`.
            '?' => self.enter_search(true),
            // `{count}:` opens the line on the count's lines (`Feed::CountCmdline`).
            ':' => {
                let count = self.normal.typed();
                self.reset();
                if count > 0 {
                    return Feed::CountCmdline { count };
                }
                self.open_cmdline(':', false);
                Feed::Pending
            }
            _ => unreachable!(),
        }
    }
}
