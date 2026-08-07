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
    /// `/` — open the search line.
    OpenSearch,
    /// The key was consumed but the command is not complete yet (a count digit or a pending operator).
    Pending,
    /// Nothing bound.
    Ignored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

/// The motion a movement key names, shared by Normal (bare move / operator) and Visual (extend selection).
/// `0` is deliberately excluded — it is a count digit unless the count is empty, so callers special-case it.
fn motion_key(code: KeyCode) -> Option<Motion> {
    Some(match code {
        KeyCode::Char('h') | KeyCode::Left => Motion::Left,
        KeyCode::Char('l') | KeyCode::Right => Motion::Right,
        KeyCode::Char('j') | KeyCode::Down => Motion::Down,
        KeyCode::Char('k') | KeyCode::Up => Motion::Up,
        KeyCode::Char('w') => Motion::WordFwd,
        KeyCode::Char('b') => Motion::WordBack,
        KeyCode::Char('e') => Motion::WordEnd,
        KeyCode::Char('$') => Motion::LineEnd,
        _ => return None,
    })
}

/// The Normal-mode pending state: an accumulating count and an awaiting operator.
#[derive(Default)]
pub struct InputEngine {
    count: u32,
    op: Option<Op>,
    op_count: u32,
    /// After an operator, `i`/`a` starts a text object; `Some(true)` = inner, `Some(false)` = around.
    textobj: Option<bool>,
    /// The last search pattern, for `n`/`N`. Persists across commands (not cleared by `reset`).
    last_search: Option<String>,
}

impl InputEngine {
    #[must_use]
    pub fn new() -> InputEngine {
        InputEngine {
            count: 0,
            op: None,
            op_count: 1,
            textobj: None,
            last_search: None,
        }
    }

    /// Remember the pattern from a `/search` so `n`/`N` can repeat it.
    pub fn set_last_search(&mut self, pattern: String) {
        self.last_search = Some(pattern);
    }

    fn reset(&mut self) {
        self.count = 0;
        self.op = None;
        self.op_count = 1;
        self.textobj = None;
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
            Some(Op::Yank) => Command::Yank(total, m),
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
        // Visual mode: the selection already exists, so operators act on it directly and motions extend it.
        if let Mode::Visual { line } = mode {
            match key.code {
                KeyCode::Esc => return self.action(Command::EnterNormal),
                // `v`/`V` toggle: same kind exits, the other switches charwise↔linewise.
                KeyCode::Char('v') => {
                    return self.action(if line {
                        Command::EnterVisual { line: false }
                    } else {
                        Command::EnterNormal
                    });
                }
                KeyCode::Char('V') => {
                    return self.action(if line {
                        Command::EnterNormal
                    } else {
                        Command::EnterVisual { line: true }
                    });
                }
                KeyCode::Char('d') | KeyCode::Char('x') => {
                    return self.action(Command::DeleteSelection)
                }
                KeyCode::Char('y') => return self.action(Command::YankSelection),
                KeyCode::Char('c') | KeyCode::Char('s') => {
                    return self.action(Command::ChangeSelection)
                }
                // Count digits and motions extend the selection; anything else is ignored in Visual.
                KeyCode::Char('1'..='9') => {}
                KeyCode::Char('0') if self.count > 0 => {}
                KeyCode::Char('0') => return self.motion(Motion::LineStart),
                _ if motion_key(key.code).is_some() => {}
                _ => {
                    self.reset();
                    return Feed::Ignored;
                }
            }
            // fall through to shared count/motion handling below (op is never set in Visual)
        }
        // Completing a text object (`d` `i` then `w` = `diw`): only an object char is valid here.
        if let Some(inner) = self.textobj {
            return match key.code {
                KeyCode::Char('w') => {
                    self.textobj = None;
                    self.motion(if inner {
                        Motion::InnerWord
                    } else {
                        Motion::AWord
                    })
                }
                _ => {
                    self.reset();
                    Feed::Ignored
                }
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
            code if motion_key(code).is_some() => {
                self.motion(motion_key(code).expect("guarded by is_some"))
            }
            KeyCode::Char('v') => self.action(Command::EnterVisual { line: false }),
            KeyCode::Char('V') => self.action(Command::EnterVisual { line: true }),
            KeyCode::Char('d') => {
                self.operator(Op::Delete, Command::Delete(op_count, Motion::Line))
            }
            KeyCode::Char('c') => {
                self.operator(Op::Change, Command::Change(op_count, Motion::Line))
            }
            KeyCode::Char('y') => self.operator(Op::Yank, Command::Yank(op_count, Motion::Line)),
            KeyCode::Char('p') => self.action(Command::Paste { after: true }),
            KeyCode::Char('P') => self.action(Command::Paste { after: false }),
            KeyCode::Char('i') if self.op.is_some() => {
                self.textobj = Some(true);
                Feed::Pending
            }
            KeyCode::Char('a') if self.op.is_some() => {
                self.textobj = Some(false);
                Feed::Pending
            }
            KeyCode::Char('i') => self.action(Command::EnterInsert),
            KeyCode::Char('a') => self.action(Command::EnterInsertAfter),
            KeyCode::Char('x') => self.action(Command::DeleteUnder),
            KeyCode::Char('u') => self.action(Command::Undo),
            KeyCode::Char('r') if ctrl => self.action(Command::Redo),
            KeyCode::Char('n') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchNext(p)),
                None => {
                    self.reset();
                    Feed::Ignored
                }
            },
            KeyCode::Char('N') => match self.last_search.clone() {
                Some(p) => self.action(Command::SearchPrev(p)),
                None => {
                    self.reset();
                    Feed::Ignored
                }
            },
            KeyCode::Char('/') => {
                self.reset();
                Feed::OpenSearch
            }
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

    pub(super) fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    pub(super) fn feed(seq: &str) -> Feed {
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

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    #[test]
    fn enters_visual_from_normal() {
        assert_eq!(feed("v"), Feed::Cmd(Command::EnterVisual { line: false }));
        assert_eq!(feed("V"), Feed::Cmd(Command::EnterVisual { line: true }));
    }

    #[test]
    fn visual_operators_act_on_the_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual { line: false };
        assert_eq!(e.feed(k('d'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(k('y'), vis), Feed::Cmd(Command::YankSelection));
        assert_eq!(e.feed(k('c'), vis), Feed::Cmd(Command::ChangeSelection));
        assert_eq!(e.feed(k('x'), vis), Feed::Cmd(Command::DeleteSelection));
        assert_eq!(e.feed(esc(), vis), Feed::Cmd(Command::EnterNormal));
    }

    #[test]
    fn visual_motion_extends_selection() {
        let mut e = InputEngine::new();
        let vis = Mode::Visual { line: false };
        // A bare motion in Visual is a Move (no operator) — the frontend re-plans it against the anchor.
        assert_eq!(
            e.feed(k('l'), vis),
            Feed::Cmd(Command::Move(1, Motion::Right))
        );
        assert_eq!(
            e.feed(k('w'), vis),
            Feed::Cmd(Command::Move(1, Motion::WordFwd))
        );
    }

    #[test]
    fn visual_toggle_and_switch() {
        let mut e = InputEngine::new();
        // `v` in charwise Visual exits; `V` switches it to linewise.
        assert_eq!(
            e.feed(k('v'), Mode::Visual { line: false }),
            Feed::Cmd(Command::EnterNormal)
        );
        assert_eq!(
            e.feed(k('V'), Mode::Visual { line: false }),
            Feed::Cmd(Command::EnterVisual { line: true })
        );
        assert_eq!(
            e.feed(k('v'), Mode::Visual { line: true }),
            Feed::Cmd(Command::EnterVisual { line: false })
        );
    }

    #[test]
    fn yank_operator_and_paste() {
        assert_eq!(feed("yw"), Feed::Cmd(Command::Yank(1, Motion::WordFwd)));
        assert_eq!(feed("y2w"), Feed::Cmd(Command::Yank(2, Motion::WordFwd)));
        assert_eq!(feed("yy"), Feed::Cmd(Command::Yank(1, Motion::Line)));
        assert_eq!(feed("2yy"), Feed::Cmd(Command::Yank(2, Motion::Line)));
        assert_eq!(feed("yiw"), Feed::Cmd(Command::Yank(1, Motion::InnerWord)));
        assert_eq!(feed("p"), Feed::Cmd(Command::Paste { after: true }));
        assert_eq!(feed("P"), Feed::Cmd(Command::Paste { after: false }));
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

#[cfg(test)]
mod textobj_tests {
    use super::tests::*;
    use super::*;

    #[test]
    fn text_objects_compose() {
        assert_eq!(
            feed("diw"),
            Feed::Cmd(Command::Delete(1, Motion::InnerWord))
        );
        assert_eq!(
            feed("ciw"),
            Feed::Cmd(Command::Change(1, Motion::InnerWord))
        );
        assert_eq!(feed("daw"), Feed::Cmd(Command::Delete(1, Motion::AWord)));
    }

    #[test]
    fn bare_i_still_enters_insert() {
        let mut e = InputEngine::new();
        assert_eq!(
            e.feed(k('i'), Mode::Normal),
            Feed::Cmd(Command::EnterInsert)
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::tests::k;
    use super::*;

    #[test]
    fn slash_opens_search_and_n_repeats() {
        let mut e = InputEngine::new();
        assert_eq!(e.feed(k('/'), Mode::Normal), Feed::OpenSearch);
        assert_eq!(e.feed(k('n'), Mode::Normal), Feed::Ignored); // no prior search yet
        e.set_last_search("foo".into());
        assert_eq!(
            e.feed(k('n'), Mode::Normal),
            Feed::Cmd(Command::SearchNext("foo".into()))
        );
        assert_eq!(
            e.feed(k('N'), Mode::Normal),
            Feed::Cmd(Command::SearchPrev("foo".into()))
        );
    }
}
