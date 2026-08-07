//! The input engine: keys → semantic [`Command`]s, and ex-command (`:…`) parsing. Pure and unit-tested;
//! the trace records the resulting commands, so a re-keymap never invalidates a corpus.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ruse_core::{Command, Mode};

/// The outcome of a keypress.
pub enum Input {
    /// A semantic command to apply.
    Cmd(Command),
    /// `:` in Normal mode — open the ex command line.
    OpenExLine,
    /// Nothing bound.
    Ignored,
}

/// Map a key to an [`Input`] given the current mode (the v0 Vim-ish keymap).
#[must_use]
pub fn map_key(key: KeyEvent, mode: Mode) -> Input {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match mode {
        Mode::Normal => match key.code {
            KeyCode::Char('h') | KeyCode::Left => Input::Cmd(Command::MoveLeft),
            KeyCode::Char('l') | KeyCode::Right => Input::Cmd(Command::MoveRight),
            KeyCode::Char('j') | KeyCode::Down => Input::Cmd(Command::MoveDown),
            KeyCode::Char('k') | KeyCode::Up => Input::Cmd(Command::MoveUp),
            KeyCode::Char('0') => Input::Cmd(Command::MoveLineStart),
            KeyCode::Char('$') => Input::Cmd(Command::MoveLineEnd),
            KeyCode::Char('i') => Input::Cmd(Command::EnterInsert),
            KeyCode::Char('a') => Input::Cmd(Command::EnterInsertAfter),
            KeyCode::Char('x') => Input::Cmd(Command::DeleteUnder),
            KeyCode::Char('u') => Input::Cmd(Command::Undo),
            KeyCode::Char('r') if ctrl => Input::Cmd(Command::Redo),
            KeyCode::Char(':') => Input::OpenExLine,
            _ => Input::Ignored,
        },
        Mode::Insert => match key.code {
            KeyCode::Esc => Input::Cmd(Command::EnterNormal),
            KeyCode::Enter => Input::Cmd(Command::InsertNewline),
            KeyCode::Backspace => Input::Cmd(Command::DeleteBack),
            KeyCode::Char(c) => Input::Cmd(Command::InsertChar(c)),
            _ => Input::Ignored,
        },
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
        _ => {
            if let Some(rest) = line.strip_prefix("trace save") {
                Ex::SaveTrace(rest.trim().to_string())
            } else {
                Ex::Unknown(line.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn normal_mode_keys() {
        assert!(matches!(
            map_key(key('h'), Mode::Normal),
            Input::Cmd(Command::MoveLeft)
        ));
        assert!(matches!(
            map_key(key('i'), Mode::Normal),
            Input::Cmd(Command::EnterInsert)
        ));
        assert!(matches!(
            map_key(key('x'), Mode::Normal),
            Input::Cmd(Command::DeleteUnder)
        ));
        assert!(matches!(map_key(key(':'), Mode::Normal), Input::OpenExLine));
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(matches!(
            map_key(ctrl_r, Mode::Normal),
            Input::Cmd(Command::Redo)
        ));
    }

    #[test]
    fn insert_mode_keys() {
        assert!(matches!(
            map_key(key('z'), Mode::Insert),
            Input::Cmd(Command::InsertChar('z'))
        ));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            map_key(esc, Mode::Insert),
            Input::Cmd(Command::EnterNormal)
        ));
    }

    #[test]
    fn ex_commands() {
        assert_eq!(parse_ex("w"), Ex::Save);
        assert_eq!(parse_ex("q"), Ex::Quit);
        assert_eq!(parse_ex("wq"), Ex::SaveQuit);
        assert_eq!(
            parse_ex("trace save /tmp/t.trace"),
            Ex::SaveTrace("/tmp/t.trace".into())
        );
        assert_eq!(parse_ex("bogus"), Ex::Unknown("bogus".into()));
    }
}
