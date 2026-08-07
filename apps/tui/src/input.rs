//! The input engine: a small pending-state machine that folds keys into semantic [`Command`]s
//! (`d`, `2w`, `d3w`, `dd`, `cw`=`ce`), plus ex-command (`:…`) parsing. The trace records the resulting
//! commands, so re-keymapping never invalidates a corpus. Pure and unit-tested.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, Mode, Motion};

/// The outcome of feeding one key to the engine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Feed {
    /// A completed command to apply.
    Cmd(Command),
    /// `:` — open the ex command line.
    OpenExLine,
    /// The key was consumed but the command is not complete yet (a count digit or a pending operator).
    Pending,
    /// Nothing bound.
    Ignored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
}

/// The Normal-mode pending state: an accumulating count and an awaiting operator.
#[derive(Default)]
pub struct InputEngine {
    count: u32,
    op: Option<Op>,
    op_count: u32,
}

impl InputEngine {
    #[must_use]
    pub fn new() -> InputEngine {
        InputEngine {
            count: 0,
            op: None,
            op_count: 1,
        }
    }

    fn reset(&mut self) {
        self.count = 0;
        self.op = None;
        self.op_count = 1;
    }

    fn mcount(&self) -> u32 {
        self.count.max(1)
    }

    /// Emit a motion — either as an operator command (if one is pending) or a bare move.
    fn motion(&mut self, m: Motion) -> Feed {
        let total = self.op_count * self.mcount();
        let cmd = match self.op {
            Some(Op::Delete) => Command::Delete(total, m),
            // Vim `cw` behaves like `ce` (does not eat the trailing space).
            Some(Op::Change) => Command::Change(
                total,
                if m == Motion::WordFwd {
                    Motion::WordEnd
                } else {
                    m
                },
            ),
            None => Command::Move(self.mcount(), m),
        };
        self.reset();
        Feed::Cmd(cmd)
    }

    fn action(&mut self, cmd: Command) -> Feed {
        self.reset();
        Feed::Cmd(cmd)
    }

    /// Set (or double) an operator. A repeated operator (`dd`/`cc`) is a linewise command.
    fn operator(&mut self, op: Op, linewise: Command) -> Feed {
        if self.op == Some(op) {
            self.reset();
            return Feed::Cmd(linewise);
        }
        self.op = Some(op);
        self.op_count = self.mcount();
        self.count = 0;
        Feed::Pending
    }

    /// Feed one key given the current mode.
    pub fn feed(&mut self, key: KeyEvent, mode: Mode) -> Feed {
        if mode == Mode::Insert {
            self.reset();
            return match key.code {
                KeyCode::Esc => Feed::Cmd(Command::EnterNormal),
                KeyCode::Enter => Feed::Cmd(Command::InsertNewline),
                KeyCode::Backspace => Feed::Cmd(Command::DeleteBack),
                KeyCode::Char(c) => Feed::Cmd(Command::InsertChar(c)),
                _ => Feed::Ignored,
            };
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let op_count = self.op_count;
        match key.code {
            KeyCode::Char(d @ '1'..='9') => {
                self.count = self.count.saturating_mul(10) + (d as u32 - '0' as u32);
                Feed::Pending
            }
            KeyCode::Char('0') if self.count > 0 => {
                self.count = self.count.saturating_mul(10);
                Feed::Pending
            }
            KeyCode::Char('0') => self.motion(Motion::LineStart),
            KeyCode::Char('$') => self.motion(Motion::LineEnd),
            KeyCode::Char('h') | KeyCode::Left => self.motion(Motion::Left),
            KeyCode::Char('l') | KeyCode::Right => self.motion(Motion::Right),
            KeyCode::Char('j') | KeyCode::Down => self.motion(Motion::Down),
            KeyCode::Char('k') | KeyCode::Up => self.motion(Motion::Up),
            KeyCode::Char('w') => self.motion(Motion::WordFwd),
            KeyCode::Char('b') => self.motion(Motion::WordBack),
            KeyCode::Char('e') => self.motion(Motion::WordEnd),
            KeyCode::Char('d') => {
                self.operator(Op::Delete, Command::Delete(op_count, Motion::Line))
            }
            KeyCode::Char('c') => {
                self.operator(Op::Change, Command::Change(op_count, Motion::Line))
            }
            KeyCode::Char('i') => self.action(Command::EnterInsert),
            KeyCode::Char('a') => self.action(Command::EnterInsertAfter),
            KeyCode::Char('x') => self.action(Command::DeleteUnder),
            KeyCode::Char('u') => self.action(Command::Undo),
            KeyCode::Char('r') if ctrl => self.action(Command::Redo),
            KeyCode::Char(':') => {
                self.reset();
                Feed::OpenExLine
            }
            _ => {
                self.reset();
                Feed::Ignored
            }
        }
    }
}

/// A parsed ex command (the `:` line).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ex {
    Save,
    Quit,
    SaveQuit,
    SaveTrace(String),
    Unknown(String),
}

/// Parse the text typed after `:` (without the leading colon).
#[must_use]
pub fn parse_ex(line: &str) -> Ex {
    let line = line.trim();
    match line {
        "w" => Ex::Save,
        "q" | "q!" => Ex::Quit,
        "wq" | "x" => Ex::SaveQuit,
        _ => match line.strip_prefix("trace save") {
            Some(rest) => Ex::SaveTrace(rest.trim().to_string()),
            None => Ex::Unknown(line.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn feed(seq: &str) -> Feed {
        let mut e = InputEngine::new();
        let mut last = Feed::Ignored;
        for c in seq.chars() {
            last = e.feed(k(c), Mode::Normal);
        }
        last
    }

    #[test]
    fn bare_motions_and_counts() {
        assert_eq!(feed("w"), Feed::Cmd(Command::Move(1, Motion::WordFwd)));
        assert_eq!(feed("3w"), Feed::Cmd(Command::Move(3, Motion::WordFwd)));
        assert_eq!(feed("l"), Feed::Cmd(Command::Move(1, Motion::Right)));
    }

    #[test]
    fn operators_with_counts() {
        assert_eq!(feed("dw"), Feed::Cmd(Command::Delete(1, Motion::WordFwd)));
        assert_eq!(feed("d2w"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2dw"), Feed::Cmd(Command::Delete(2, Motion::WordFwd)));
        assert_eq!(feed("2d3w"), Feed::Cmd(Command::Delete(6, Motion::WordFwd)));
    }

    #[test]
    fn doubled_operator_is_linewise() {
        assert_eq!(feed("dd"), Feed::Cmd(Command::Delete(1, Motion::Line)));
        assert_eq!(feed("2dd"), Feed::Cmd(Command::Delete(2, Motion::Line)));
        assert_eq!(feed("cc"), Feed::Cmd(Command::Change(1, Motion::Line)));
    }

    #[test]
    fn cw_is_ce() {
        assert_eq!(feed("cw"), Feed::Cmd(Command::Change(1, Motion::WordEnd)));
    }

    #[test]
    fn pending_states() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('2'), Mode::Normal), Feed::Pending);
        assert_eq!(e.feed(k('d'), Mode::Normal), Feed::Pending);
        assert_eq!(
            e.feed(k('w'), Mode::Normal),
            Feed::Cmd(Command::Delete(2, Motion::WordFwd))
        );
    }

    #[test]
    fn insert_mode_and_ex() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('z'), Mode::Insert),
            Feed::Cmd(Command::InsertChar('z'))
        );
        assert_eq!(e.feed(k(':'), Mode::Normal), Feed::OpenExLine);
        assert_eq!(parse_ex("wq"), Ex::SaveQuit);
        assert_eq!(
            parse_ex("trace save t.trace"),
            Ex::SaveTrace("t.trace".into())
        );
    }
}
