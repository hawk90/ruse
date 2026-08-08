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
    /// `{count}x` — delete `count` chars from the cursor, clamped at end-of-line (Vim).
    DeleteUnder(u32),
    /// `{count}r{char}` — replace `count` chars with `char`; a no-op if fewer than `count` remain (Vim).
    ReplaceChar(u32, char),
    /// `{count}~` — toggle the case of `count` chars, clamped at EOL, then move past the last (Vim).
    ToggleCase(u32),
    /// `J` — join the current line with the next on a single space.
    JoinLines,
    /// `{count}>>` — shift `count` lines one indent level to the right (Vim `>>`). Linewise: the count
    /// is a LINE count, not a motion. The indent unit comes from the editor's indent config.
    ShiftRight(u32),
    /// `{count}<<` — shift `count` lines one indent level to the left (Vim `<<`). Symmetric inverse of
    /// [`Command::ShiftRight`]: removes up to one indent level of leading whitespace, never past column 0.
    ShiftLeft(u32),
    // editing grammar: count + motion / operator (Phase D)
    Move(u32, Motion),
    Delete(u32, Motion),
    Change(u32, Motion),
    // registers (v0 unnamed slot): yank a motion range; paste after (`p`) or before (`P`) the cursor
    Yank(u32, Motion),
    /// `{count}p` / `{count}P` — paste the unnamed register `count` times, after (`p`) or before (`P`)
    /// the cursor. `count` is the repetition count (Vim `2p` pastes twice); it defaults to 1.
    Paste {
        after: bool,
        count: u32,
    },
    // visual mode: enter a selection (charwise `v` / linewise `V`); operate on the current selection
    EnterVisual {
        line: bool,
    },
    /// `CTRL-G` from Visual — enter Select over the SAME selection (Visual↔Select toggle). Select
    /// shares Visual's anchor/shape and differs only in its unmatched-key policy (contracts/vim-style.yaml).
    EnterSelect {
        line: bool,
    },
    DeleteSelection,
    YankSelection,
    ChangeSelection,
    /// Visual/Select `o` — swap the selection's two ends: the cursor jumps to the anchor and the anchor
    /// becomes the old cursor position. The SAME text stays selected, but subsequent motions now extend
    /// the OTHER end. Involutive (`oo` restores the original ends); a clean no-op outside a selection.
    SwapSelectionEnds,
    /// Select's `open/replace-selection` policy: a printable key deletes the selection, inserts the char,
    /// and enters Insert. The one behaviour that distinguishes Select from Visual over identical state.
    ReplaceSelection(char),
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
        Motion::InnerBigWord => "inner_big_word",
        Motion::ABigWord => "a_big_word",
        Motion::InnerParagraph => "inner_paragraph",
        Motion::AParagraph => "a_paragraph",
        Motion::InnerSentence => "inner_sentence",
        Motion::ASentence => "a_sentence",
        Motion::Line => "line",
        Motion::GotoLine => "goto_line",
        Motion::LastLine => "last_line",
        Motion::MatchBracket => "match_bracket",
        Motion::ParagraphFwd => "paragraph_fwd",
        Motion::ParagraphBack => "paragraph_back",
        // A single whitespace-free token so the `<count> <motion>` split still works: the char is its decimal
        // scalar value; `f`/`t` and forward/back are flags. e.g. `find_char:120:1:0` = `fx`.
        Motion::FindChar { ch, forward, till } => {
            return format!("find_char:{}:{}:{}", ch as u32, forward as u8, till as u8)
        }
        // Delimiter/quote text objects carry their char(s) + around flag as a single whitespace-free token,
        // mirroring `find_char` so the `<count> <motion>` split still holds.
        Motion::Pair {
            open,
            close,
            around,
        } => return format!("pair:{}:{}:{}", open as u32, close as u32, around as u8),
        Motion::Quote { ch, around } => return format!("quote:{}:{}", ch as u32, around as u8),
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
    if let Some(rest) = s.strip_prefix("pair:") {
        let mut it = rest.split(':');
        let open = char::from_u32(it.next()?.parse().ok()?)?;
        let close = char::from_u32(it.next()?.parse().ok()?)?;
        let around = it.next()? == "1";
        if it.next().is_some() {
            return None; // trailing garbage
        }
        return Some(Motion::Pair {
            open,
            close,
            around,
        });
    }
    if let Some(rest) = s.strip_prefix("quote:") {
        let mut it = rest.split(':');
        let ch = char::from_u32(it.next()?.parse().ok()?)?;
        let around = it.next()? == "1";
        if it.next().is_some() {
            return None; // trailing garbage
        }
        return Some(Motion::Quote { ch, around });
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
        "inner_big_word" => Motion::InnerBigWord,
        "a_big_word" => Motion::ABigWord,
        "inner_paragraph" => Motion::InnerParagraph,
        "a_paragraph" => Motion::AParagraph,
        "inner_sentence" => Motion::InnerSentence,
        "a_sentence" => Motion::ASentence,
        "line" => Motion::Line,
        "goto_line" => Motion::GotoLine,
        "last_line" => Motion::LastLine,
        "match_bracket" => Motion::MatchBracket,
        "paragraph_fwd" => Motion::ParagraphFwd,
        "paragraph_back" => Motion::ParagraphBack,
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
            Command::DeleteUnder(n) => format!("delete_under {n}"),
            Command::ReplaceChar(n, c) => format!("replace_char {n} {}", *c as u32),
            Command::ToggleCase(n) => format!("toggle_case {n}"),
            Command::JoinLines => "join_lines".into(),
            Command::ShiftRight(n) => format!("shift_right {n}"),
            Command::ShiftLeft(n) => format!("shift_left {n}"),
            Command::Move(n, m) => format!("move {n} {}", motion_token(*m)),
            Command::Delete(n, m) => format!("delete {n} {}", motion_token(*m)),
            Command::Change(n, m) => format!("change {n} {}", motion_token(*m)),
            Command::Yank(n, m) => format!("yank {n} {}", motion_token(*m)),
            Command::Paste { after, count } => {
                if *after {
                    format!("paste_after {count}")
                } else {
                    format!("paste_before {count}")
                }
            }
            Command::EnterVisual { line } => {
                if *line {
                    "enter_visual_line".into()
                } else {
                    "enter_visual".into()
                }
            }
            Command::EnterSelect { line } => {
                if *line {
                    "enter_select_line".into()
                } else {
                    "enter_select".into()
                }
            }
            Command::DeleteSelection => "delete_selection".into(),
            Command::YankSelection => "yank_selection".into(),
            Command::ChangeSelection => "change_selection".into(),
            Command::SwapSelectionEnds => "swap_selection_ends".into(),
            Command::ReplaceSelection(c) => {
                let mut s = String::from("replace_selection ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
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
            "delete_under" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::DeleteUnder(n)
            }
            "replace_char" => {
                let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let mut it = a.split_whitespace();
                let n: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let cp: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ReplaceChar(n, c)
            }
            "toggle_case" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ToggleCase(n)
            }
            "join_lines" => Command::JoinLines,
            "shift_right" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ShiftRight(n)
            }
            "shift_left" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ShiftLeft(n)
            }
            "move" => return op_cmd(arg, Command::Move),
            "delete" => return op_cmd(arg, Command::Delete),
            "change" => return op_cmd(arg, Command::Change),
            "yank" => return op_cmd(arg, Command::Yank),
            "paste_after" => Command::Paste {
                after: true,
                count: arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
            },
            "paste_before" => Command::Paste {
                after: false,
                count: arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
            },
            "enter_visual" => Command::EnterVisual { line: false },
            "enter_visual_line" => Command::EnterVisual { line: true },
            "enter_select" => Command::EnterSelect { line: false },
            "enter_select_line" => Command::EnterSelect { line: true },
            "delete_selection" => Command::DeleteSelection,
            "yank_selection" => Command::YankSelection,
            "change_selection" => Command::ChangeSelection,
            "swap_selection_ends" => Command::SwapSelectionEnds,
            "replace_selection" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ReplaceSelection(c)
            }
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
            Command::ReplaceChar(1, 'z'),
            Command::ReplaceChar(3, 'z'),
            Command::ReplaceChar(1, '가'),
            Command::ToggleCase(1),
            Command::ToggleCase(4),
            Command::JoinLines,
            Command::ShiftRight(1),
            Command::ShiftRight(3),
            Command::ShiftLeft(1),
            Command::ShiftLeft(2),
            Command::InsertChar('🎉'),
            Command::InsertChar(' '),
            Command::InsertNewline,
            Command::DeleteBack,
            Command::DeleteUnder(1),
            Command::DeleteUnder(3),
            Command::Move(2, Motion::WordFwd),
            Command::Move(1, Motion::BigWordFwd),
            Command::Delete(1, Motion::BigWordBack),
            Command::Change(2, Motion::BigWordEnd),
            Command::Delete(1, Motion::Line),
            Command::Delete(1, Motion::InnerWord),
            Command::Change(1, Motion::AWord),
            Command::Delete(1, Motion::InnerBigWord),
            Command::Change(1, Motion::ABigWord),
            Command::Delete(1, Motion::InnerParagraph),
            Command::Yank(1, Motion::AParagraph),
            Command::Delete(1, Motion::InnerSentence),
            Command::Change(1, Motion::ASentence),
            Command::Delete(
                1,
                Motion::Pair {
                    open: '(',
                    close: ')',
                    around: false,
                },
            ),
            Command::Change(
                2,
                Motion::Pair {
                    open: '{',
                    close: '}',
                    around: true,
                },
            ),
            Command::Yank(
                1,
                Motion::Pair {
                    open: '<',
                    close: '>',
                    around: false,
                },
            ),
            Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true,
                },
            ),
            Command::Change(
                1,
                Motion::Quote {
                    ch: '`',
                    around: false,
                },
            ),
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
            Command::Move(1, Motion::ParagraphFwd),
            Command::Move(2, Motion::ParagraphBack),
            Command::Delete(1, Motion::ParagraphFwd),
            Command::Yank(1, Motion::ParagraphBack),
            Command::Yank(1, Motion::Line),
            Command::Yank(2, Motion::WordFwd),
            Command::Paste {
                after: true,
                count: 1,
            },
            Command::Paste {
                after: false,
                count: 1,
            },
            Command::Paste {
                after: true,
                count: 2,
            },
            Command::Paste {
                after: false,
                count: 5,
            },
            Command::EnterVisual { line: false },
            Command::EnterVisual { line: true },
            Command::EnterSelect { line: false },
            Command::EnterSelect { line: true },
            Command::DeleteSelection,
            Command::YankSelection,
            Command::ChangeSelection,
            Command::SwapSelectionEnds,
            Command::ReplaceSelection('z'),
            Command::ReplaceSelection('가'),
            Command::ReplaceSelection('🎉'),
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
