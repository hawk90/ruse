//! Semantic editor commands (INV-CMD-SEMANTIC) — the granularity at which edits are recorded as a trace.
//!
//! A keymap resolves keys onto these (the input engine, C2); a [`crate::trace::Trace`] is a list of them, so
//! it survives keymap changes. Each command has a stable line form (`to_line`/`from_line`) — a dependency-
//! free, human-readable serialization used by the trace file.

use std::fmt::Write as _;

/// A semantic command. This is the v0 set — motions, mode switches, single edits, undo/redo, file/control.
/// Counts, operators, and text objects (Vim `d`/`w`/`iw`) arrive in Phase D on top of this.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    // motion
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
    // mode
    EnterInsert,
    EnterInsertAfter,
    EnterNormal,
    // edit
    InsertChar(char),
    InsertNewline,
    DeleteBack,
    DeleteUnder,
    // history / file / control
    Undo,
    Redo,
    Save,
    Quit,
}

/// Why a trace line could not be parsed into a [`Command`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommandParseError {
    Empty,
    UnknownVerb(String),
    BadArgument(String),
}

impl Command {
    /// The stable single-line serialization of this command (used by the trace format). A `char` argument
    /// is encoded as its decimal Unicode scalar value so whitespace/newlines never need escaping.
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            Command::MoveLeft => "move_left".into(),
            Command::MoveRight => "move_right".into(),
            Command::MoveUp => "move_up".into(),
            Command::MoveDown => "move_down".into(),
            Command::MoveLineStart => "move_line_start".into(),
            Command::MoveLineEnd => "move_line_end".into(),
            Command::EnterInsert => "enter_insert".into(),
            Command::EnterInsertAfter => "enter_insert_after".into(),
            Command::EnterNormal => "enter_normal".into(),
            Command::InsertChar(c) => {
                let mut s = String::from("insert_char ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
            Command::InsertNewline => "insert_newline".into(),
            Command::DeleteBack => "delete_back".into(),
            Command::DeleteUnder => "delete_under".into(),
            Command::Undo => "undo".into(),
            Command::Redo => "redo".into(),
            Command::Save => "save".into(),
            Command::Quit => "quit".into(),
        }
    }

    /// Parse one trace line back into a [`Command`].
    pub fn from_line(line: &str) -> Result<Command, CommandParseError> {
        let line = line.trim();
        if line.is_empty() {
            return Err(CommandParseError::Empty);
        }
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v, Some(a.trim())),
            None => (line, None),
        };
        Ok(match verb {
            "move_left" => Command::MoveLeft,
            "move_right" => Command::MoveRight,
            "move_up" => Command::MoveUp,
            "move_down" => Command::MoveDown,
            "move_line_start" => Command::MoveLineStart,
            "move_line_end" => Command::MoveLineEnd,
            "enter_insert" => Command::EnterInsert,
            "enter_insert_after" => Command::EnterInsertAfter,
            "enter_normal" => Command::EnterNormal,
            "insert_char" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::InsertChar(c)
            }
            "insert_newline" => Command::InsertNewline,
            "delete_back" => Command::DeleteBack,
            "delete_under" => Command::DeleteUnder,
            "undo" => Command::Undo,
            "redo" => Command::Redo,
            "save" => Command::Save,
            "quit" => Command::Quit,
            other => return Err(CommandParseError::UnknownVerb(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_variant() {
        let cases = [
            Command::MoveLeft,
            Command::MoveRight,
            Command::MoveUp,
            Command::MoveDown,
            Command::MoveLineStart,
            Command::MoveLineEnd,
            Command::EnterInsert,
            Command::EnterInsertAfter,
            Command::EnterNormal,
            Command::InsertChar('h'),
            Command::InsertChar('🎉'),
            Command::InsertChar(' '),
            Command::InsertNewline,
            Command::DeleteBack,
            Command::DeleteUnder,
            Command::Undo,
            Command::Redo,
            Command::Save,
            Command::Quit,
        ];
        for c in cases {
            assert_eq!(Command::from_line(&c.to_line()), Ok(c.clone()), "{c:?}");
        }
    }

    #[test]
    fn rejects_unknown_and_bad() {
        assert_eq!(Command::from_line(""), Err(CommandParseError::Empty));
        assert!(matches!(
            Command::from_line("frobnicate"),
            Err(CommandParseError::UnknownVerb(_))
        ));
        assert!(matches!(
            Command::from_line("insert_char zzz"),
            Err(CommandParseError::BadArgument(_))
        ));
    }
}
