//! Semantic editor commands (INV-CMD-SEMANTIC) — the granularity at which edits are recorded as a trace.
//!
//! A keymap resolves keys onto these (the input engine, C2); a [`crate::trace::Trace`] is a list of them, so
//! it survives keymap changes. Each command has a stable line form (`to_line`/`from_line`) — a dependency-
//! free, human-readable serialization used by the trace file.

use std::fmt::Write as _;

use crate::motion::Motion;

/// A semantic command — the granularity a trace records. Beyond single motions/edits it carries the editing
/// grammar: a counted move, and the `delete`/`change` operators over a motion range (`dw`, `d2w`, `cc`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    // motion
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
    // mode / insert-entry
    EnterInsert,
    EnterInsertAfter,
    EnterNormal,
    /// `I` — insert before the first non-blank char of the line.
    InsertLineStart,
    /// `A` — append at the end of the line.
    AppendLineEnd,
    /// `o` — open a new line below and insert.
    OpenBelow,
    /// `O` — open a new line above and insert.
    OpenAbove,
    // edit
    InsertChar(char),
    InsertNewline,
    DeleteBack,
    DeleteUnder,
    /// `r{char}` — replace the character under the cursor.
    ReplaceChar(char),
    /// `~` — toggle the case of the character under the cursor, then move right.
    ToggleCase,
    /// `J` — join the current line with the next on a single space.
    JoinLines,
    // editing grammar: count + motion / operator (Phase D)
    Move(u32, Motion),
    Delete(u32, Motion),
    Change(u32, Motion),
    // registers (v0 unnamed slot): yank a motion range; paste after (`p`) or before (`P`) the cursor
    Yank(u32, Motion),
    Paste {
        after: bool,
    },
    // visual mode: enter a selection (charwise `v` / linewise `V`); operate on the current selection
    EnterVisual {
        line: bool,
    },
    DeleteSelection,
    YankSelection,
    ChangeSelection,
    // search (literal substring for v0; the pattern is carried so traces replay deterministically)
    SearchNext(String),
    SearchPrev(String),
    // history / file / control
    Undo,
    Redo,
    Save,
    Quit,
}

fn motion_token(m: Motion) -> String {
    let s = match m {
        Motion::Left => "left",
        Motion::Right => "right",
        Motion::Up => "up",
        Motion::Down => "down",
        Motion::LineStart => "line_start",
        Motion::LineEnd => "line_end",
        Motion::WordFwd => "word_fwd",
        Motion::WordBack => "word_back",
        Motion::WordEnd => "word_end",
        Motion::BigWordFwd => "big_word_fwd",
        Motion::BigWordBack => "big_word_back",
        Motion::BigWordEnd => "big_word_end",
        Motion::InnerWord => "inner_word",
        Motion::AWord => "a_word",
        Motion::Line => "line",
        Motion::GotoLine => "goto_line",
        Motion::LastLine => "last_line",
        Motion::MatchBracket => "match_bracket",
        // A single whitespace-free token so the `<count> <motion>` split still works: the char is its decimal
        // scalar value; `f`/`t` and forward/back are flags. e.g. `find_char:120:1:0` = `fx`.
        Motion::FindChar { ch, forward, till } => {
            return format!("find_char:{}:{}:{}", ch as u32, forward as u8, till as u8)
        }
    };
    s.to_string()
}

fn motion_from_token(s: &str) -> Option<Motion> {
    if let Some(rest) = s.strip_prefix("find_char:") {
        let mut it = rest.split(':');
        let ch = char::from_u32(it.next()?.parse().ok()?)?;
        let forward = it.next()? == "1";
        let till = it.next()? == "1";
        if it.next().is_some() {
            return None; // trailing garbage
        }
        return Some(Motion::FindChar { ch, forward, till });
    }
    Some(match s {
        "left" => Motion::Left,
        "right" => Motion::Right,
        "up" => Motion::Up,
        "down" => Motion::Down,
        "line_start" => Motion::LineStart,
        "line_end" => Motion::LineEnd,
        "word_fwd" => Motion::WordFwd,
        "word_back" => Motion::WordBack,
        "word_end" => Motion::WordEnd,
        "big_word_fwd" => Motion::BigWordFwd,
        "big_word_back" => Motion::BigWordBack,
        "big_word_end" => Motion::BigWordEnd,
        "inner_word" => Motion::InnerWord,
        "a_word" => Motion::AWord,
        "line" => Motion::Line,
        "goto_line" => Motion::GotoLine,
        "last_line" => Motion::LastLine,
        "match_bracket" => Motion::MatchBracket,
        _ => return None,
    })
}

/// Parse an operator/move argument `"<count> <motion>"` into a command via `ctor`.
fn op_cmd(
    arg: Option<&str>,
    ctor: fn(u32, Motion) -> Command,
) -> Result<Command, CommandParseError> {
    let a = arg.ok_or_else(|| CommandParseError::BadArgument("missing count/motion".into()))?;
    let mut it = a.split_whitespace();
    let count: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
    let motion = it
        .next()
        .and_then(motion_from_token)
        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
    Ok(ctor(count, motion))
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
            Command::InsertLineStart => "insert_line_start".into(),
            Command::AppendLineEnd => "append_line_end".into(),
            Command::OpenBelow => "open_below".into(),
            Command::OpenAbove => "open_above".into(),
            Command::InsertChar(c) => {
                let mut s = String::from("insert_char ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
            Command::InsertNewline => "insert_newline".into(),
            Command::DeleteBack => "delete_back".into(),
            Command::DeleteUnder => "delete_under".into(),
            Command::ReplaceChar(c) => {
                let mut s = String::from("replace_char ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
            Command::ToggleCase => "toggle_case".into(),
            Command::JoinLines => "join_lines".into(),
            Command::Move(n, m) => format!("move {n} {}", motion_token(*m)),
            Command::Delete(n, m) => format!("delete {n} {}", motion_token(*m)),
            Command::Change(n, m) => format!("change {n} {}", motion_token(*m)),
            Command::Yank(n, m) => format!("yank {n} {}", motion_token(*m)),
            Command::Paste { after } => {
                if *after {
                    "paste_after".into()
                } else {
                    "paste_before".into()
                }
            }
            Command::EnterVisual { line } => {
                if *line {
                    "enter_visual_line".into()
                } else {
                    "enter_visual".into()
                }
            }
            Command::DeleteSelection => "delete_selection".into(),
            Command::YankSelection => "yank_selection".into(),
            Command::ChangeSelection => "change_selection".into(),
            Command::SearchNext(p) => format!("search_next {p}"),
            Command::SearchPrev(p) => format!("search_prev {p}"),
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
        // The raw remainder (untrimmed) — search patterns may contain spaces.
        let raw = line.split_once(' ').map(|(_, r)| r).unwrap_or("");
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
            "insert_line_start" => Command::InsertLineStart,
            "append_line_end" => Command::AppendLineEnd,
            "open_below" => Command::OpenBelow,
            "open_above" => Command::OpenAbove,
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
            "replace_char" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ReplaceChar(c)
            }
            "toggle_case" => Command::ToggleCase,
            "join_lines" => Command::JoinLines,
            "move" => return op_cmd(arg, Command::Move),
            "delete" => return op_cmd(arg, Command::Delete),
            "change" => return op_cmd(arg, Command::Change),
            "yank" => return op_cmd(arg, Command::Yank),
            "paste_after" => Command::Paste { after: true },
            "paste_before" => Command::Paste { after: false },
            "enter_visual" => Command::EnterVisual { line: false },
            "enter_visual_line" => Command::EnterVisual { line: true },
            "delete_selection" => Command::DeleteSelection,
            "yank_selection" => Command::YankSelection,
            "change_selection" => Command::ChangeSelection,
            "search_next" => Command::SearchNext(raw.to_string()),
            "search_prev" => Command::SearchPrev(raw.to_string()),
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
            Command::InsertLineStart,
            Command::AppendLineEnd,
            Command::OpenBelow,
            Command::OpenAbove,
            Command::InsertChar('h'),
            Command::ReplaceChar('z'),
            Command::ReplaceChar('가'),
            Command::ToggleCase,
            Command::JoinLines,
            Command::InsertChar('🎉'),
            Command::InsertChar(' '),
            Command::InsertNewline,
            Command::DeleteBack,
            Command::DeleteUnder,
            Command::Move(2, Motion::WordFwd),
            Command::Move(1, Motion::BigWordFwd),
            Command::Delete(1, Motion::BigWordBack),
            Command::Change(2, Motion::BigWordEnd),
            Command::Delete(1, Motion::Line),
            Command::Delete(1, Motion::InnerWord),
            Command::Change(1, Motion::AWord),
            Command::Change(3, Motion::WordEnd),
            Command::Move(
                1,
                Motion::FindChar {
                    ch: 'x',
                    forward: true,
                    till: false,
                },
            ),
            Command::Delete(
                2,
                Motion::FindChar {
                    ch: ')',
                    forward: true,
                    till: true,
                },
            ),
            Command::Move(
                1,
                Motion::FindChar {
                    ch: '가',
                    forward: false,
                    till: false,
                },
            ),
            Command::Move(1, Motion::LastLine),
            Command::Move(5, Motion::GotoLine),
            Command::Delete(1, Motion::GotoLine),
            Command::Move(1, Motion::MatchBracket),
            Command::Delete(1, Motion::MatchBracket),
            Command::Yank(1, Motion::Line),
            Command::Yank(2, Motion::WordFwd),
            Command::Paste { after: true },
            Command::Paste { after: false },
            Command::EnterVisual { line: false },
            Command::EnterVisual { line: true },
            Command::DeleteSelection,
            Command::YankSelection,
            Command::ChangeSelection,
            Command::SearchNext("foo bar".into()),
            Command::SearchPrev("x".into()),
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
