//! The stable trace line form of a [`Command`] (`to_line` / `from_line`) and the token codecs it is built
//! from. Split from `command.rs` (#1384).
//!
//! This file holds [`Command::from_line`] and the token readers. Its per-family decoders are in
//! [`decode`] (#1530), the argument parsers in [`args`], and the encoding half ([`Command::to_line`] and
//! the token writers) in [`encode`] (#1494). `trace.rs` scrapes this file and every file under
//! `line_form/`: the `"verb" =>` arms of the decoders and token readers feed its fuzz verb list, and the
//! body of `motion_token` (which ends at `fn motion_from_token`) feeds its motion list.

use super::*;

mod args;
mod decode;
mod encode;

use args::{find_char_motion, pair_motion, quote_motion};
use decode::{
    decode_insert, decode_mark, decode_operator, decode_operator_extra, decode_paste_register,
    decode_search, decode_session_emacs, decode_simple_edit, decode_visual,
};

pub(crate) use encode::hex_lower;

impl SearchOffset {
    /// Decode a [`to_token`](Self::to_token) token; `None` on a malformed token.
    #[must_use]
    fn from_token(s: &str) -> Option<SearchOffset> {
        if s == "none" {
            return Some(SearchOffset::None);
        }
        // Split on the first char, not byte 1: a malformed trace may start the token with a multi-byte
        // char, or leave it empty (#720). Either is a parse error, never a panic.
        let mut chars = s.chars();
        let tag = chars.next()?;
        let n: i32 = chars.as_str().parse().ok()?;
        match tag {
            's' => Some(SearchOffset::Start(n)),
            'e' => Some(SearchOffset::End(n)),
            'l' => Some(SearchOffset::Line(n)),
            _ => None,
        }
    }
}

fn motion_token(m: Motion) -> String {
    let s = match m {
        Motion::Left => "left",
        Motion::Right => "right",
        Motion::WrapLeft => "wrap_left",
        Motion::WrapRight => "wrap_right",
        Motion::EmacsCharBack => "emacs_char_back",
        Motion::EmacsCharFwd => "emacs_char_fwd",
        Motion::Up => "up",
        Motion::Down => "down",
        Motion::LineStart => "line_start",
        Motion::LineLastNonBlank => "line_last_non_blank",
        Motion::Column => "column",
        Motion::MidLine => "mid_line",
        Motion::LineFirstNonBlank => "line_first_non_blank",
        Motion::LineEnd => "line_end",
        Motion::WordFwd => "word_fwd",
        Motion::WordBack => "word_back",
        Motion::WordEnd => "word_end",
        Motion::WordEndBack => "word_end_back",
        Motion::EmacsWordFwd => "emacs_word_fwd",
        Motion::EmacsWordBack => "emacs_word_back",
        Motion::BigWordFwd => "big_word_fwd",
        Motion::BigWordBack => "big_word_back",
        Motion::BigWordEnd => "big_word_end",
        Motion::BigWordEndBack => "big_word_end_back",
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
        Motion::DownFirstNonBlank => "down_first_non_blank",
        Motion::UpFirstNonBlank => "up_first_non_blank",
        Motion::LineUnderscore => "line_underscore",
        Motion::GotoByte => "goto_byte",
        Motion::MatchBracket => "match_bracket",
        Motion::GotoPercent => "goto_percent",
        Motion::ParagraphFwd => "paragraph_fwd",
        Motion::ParagraphBack => "paragraph_back",
        Motion::SentenceFwd => "sentence_fwd",
        Motion::SentenceBack => "sentence_back",
        Motion::SectionFwd => "section_fwd",
        Motion::SectionBack => "section_back",
        Motion::SectionEndFwd => "section_end_fwd",
        Motion::SectionEndBack => "section_end_back",
        Motion::UnmatchedParenBack => "unmatched_paren_back",
        Motion::UnmatchedParenFwd => "unmatched_paren_fwd",
        Motion::UnmatchedBraceBack => "unmatched_brace_back",
        Motion::UnmatchedBraceFwd => "unmatched_brace_fwd",
        Motion::MethodStartFwd => "method_start_fwd",
        Motion::MethodStartBack => "method_start_back",
        Motion::MethodEndFwd => "method_end_fwd",
        Motion::MethodEndBack => "method_end_back",
        // A single whitespace-free token so the `<count> <motion>` split still works: the char is its decimal
        // scalar value; `f`/`t` and forward/back are flags. e.g. `find_char:120:1:0` = `fx`. A `;`/`,` repeat
        // appends `:r` (`find_char:120:1:1:r` = `;` after `tx`), so the older three-field form still parses.
        Motion::FindChar {
            ch,
            forward,
            till,
            repeat,
        } => {
            let r = if repeat { ":r" } else { "" };
            return format!(
                "find_char:{}:{}:{}{r}",
                ch as u32, forward as u8, till as u8
            );
        }
        // Delimiter/quote text objects carry their char(s) + around flag as a single whitespace-free token,
        // mirroring `find_char` so the `<count> <motion>` split still holds.
        Motion::Pair {
            open,
            close,
            around,
        } => return format!("pair:{}:{}:{}", open as u32, close as u32, around as u8),
        Motion::Quote { ch, around } => return format!("quote:{}:{}", ch as u32, around as u8),
        Motion::Tag { around } => return format!("tag:{}", around as u8),
    };
    s.to_string()
}

fn motion_from_token(s: &str) -> Option<Motion> {
    if let Some(rest) = s.strip_prefix("find_char:") {
        return find_char_motion(rest);
    }
    if let Some(rest) = s.strip_prefix("pair:") {
        return pair_motion(rest);
    }
    if let Some(rest) = s.strip_prefix("quote:") {
        return quote_motion(rest);
    }
    if let Some(rest) = s.strip_prefix("tag:") {
        return Some(Motion::Tag {
            around: rest == "1",
        });
    }
    Some(match s {
        "left" => Motion::Left,
        "right" => Motion::Right,
        "wrap_left" => Motion::WrapLeft,
        "wrap_right" => Motion::WrapRight,
        "emacs_char_back" => Motion::EmacsCharBack,
        "emacs_char_fwd" => Motion::EmacsCharFwd,
        "up" => Motion::Up,
        "down" => Motion::Down,
        "line_start" => Motion::LineStart,
        "line_last_non_blank" => Motion::LineLastNonBlank,
        "column" => Motion::Column,
        "mid_line" => Motion::MidLine,
        "line_first_non_blank" => Motion::LineFirstNonBlank,
        "line_end" => Motion::LineEnd,
        "word_fwd" => Motion::WordFwd,
        "word_back" => Motion::WordBack,
        "word_end" => Motion::WordEnd,
        "word_end_back" => Motion::WordEndBack,
        "emacs_word_fwd" => Motion::EmacsWordFwd,
        "emacs_word_back" => Motion::EmacsWordBack,
        "big_word_fwd" => Motion::BigWordFwd,
        "big_word_back" => Motion::BigWordBack,
        "big_word_end" => Motion::BigWordEnd,
        "big_word_end_back" => Motion::BigWordEndBack,
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
        "down_first_non_blank" => Motion::DownFirstNonBlank,
        "up_first_non_blank" => Motion::UpFirstNonBlank,
        "line_underscore" => Motion::LineUnderscore,
        "goto_byte" => Motion::GotoByte,
        "match_bracket" => Motion::MatchBracket,
        "goto_percent" => Motion::GotoPercent,
        "paragraph_fwd" => Motion::ParagraphFwd,
        "paragraph_back" => Motion::ParagraphBack,
        "sentence_fwd" => Motion::SentenceFwd,
        "sentence_back" => Motion::SentenceBack,
        "section_fwd" => Motion::SectionFwd,
        "section_back" => Motion::SectionBack,
        "section_end_fwd" => Motion::SectionEndFwd,
        "section_end_back" => Motion::SectionEndBack,
        "unmatched_paren_back" => Motion::UnmatchedParenBack,
        "unmatched_paren_fwd" => Motion::UnmatchedParenFwd,
        "unmatched_brace_back" => Motion::UnmatchedBraceBack,
        "unmatched_brace_fwd" => Motion::UnmatchedBraceFwd,
        "method_start_fwd" => Motion::MethodStartFwd,
        "method_start_back" => Motion::MethodStartBack,
        "method_end_fwd" => Motion::MethodEndFwd,
        "method_end_back" => Motion::MethodEndBack,
        _ => return None,
    })
}

fn op_kind_from_token(s: &str) -> Option<OpKind> {
    Some(match s {
        "delete" => OpKind::Delete,
        "change" => OpKind::Change,
        "yank" => OpKind::Yank,
        _ => return None,
    })
}

fn mark_op_from_token(s: &str) -> Option<MarkOp> {
    if let Some(rest) = s.strip_prefix("case:") {
        return Some(MarkOp::Case(match rest {
            "upcase" => WordCase::Upcase,
            "downcase" => WordCase::Downcase,
            "capitalize" => WordCase::Capitalize,
            "toggle" => WordCase::Toggle,
            "rot13" => WordCase::Rot13,
            _ => return None,
        }));
    }
    Some(match s {
        "delete" => MarkOp::Delete,
        "change" => MarkOp::Change,
        "yank" => MarkOp::Yank,
        "shift_left" => MarkOp::Shift { left: true },
        "shift_right" => MarkOp::Shift { left: false },
        "reindent" => MarkOp::Reindent,
        "format" => MarkOp::Format { keep: false },
        "format_keep" => MarkOp::Format { keep: true },
        "comment" => MarkOp::Comment,
        _ => return None,
    })
}

fn visual_op_from_token(s: &str) -> Option<VisualOp> {
    let mut it = s.split_whitespace();
    let op = match it.next()? {
        "lines" => VisualOp::Lines(op_kind_from_token(it.next()?)?),
        "block_eol" => VisualOp::BlockToEol(op_kind_from_token(it.next()?)?),
        "shift" => {
            let left = match it.next()? {
                "left" => true,
                "right" => false,
                _ => return None,
            };
            VisualOp::Shift {
                left,
                count: it.next()?.parse().ok()?,
            }
        }
        "join" => VisualOp::Join { spaces: true },
        "join_raw" => VisualOp::Join { spaces: false },
        "reindent" => VisualOp::Reindent,
        "format" => VisualOp::Format,
        "comment" => VisualOp::Comment,
        "other_corner" => VisualOp::OtherCorner,
        "yank_trim" => VisualOp::YankTrim,
        "leave_at_start" => VisualOp::LeaveAtStart,
        _ => return None,
    };
    it.next().is_none().then_some(op)
}

fn forced_wise_from_token(s: &str) -> Option<ForcedWise> {
    Some(match s {
        "charwise" => ForcedWise::Charwise,
        "linewise" => ForcedWise::Linewise,
        "blockwise" => ForcedWise::Blockwise,
        _ => return None,
    })
}

fn search_op_from_token(s: &str) -> Option<SearchOp> {
    if s == "move" {
        return Some(SearchOp::Move);
    }
    Some(match mark_op_from_token(s)? {
        MarkOp::Delete => SearchOp::Delete,
        MarkOp::Change => SearchOp::Change,
        MarkOp::Yank => SearchOp::Yank,
        MarkOp::Case(c) => SearchOp::Case(c),
        MarkOp::Shift { left } => SearchOp::Shift { left },
        MarkOp::Reindent => SearchOp::Reindent,
        MarkOp::Format { keep } => SearchOp::Format { keep },
        MarkOp::Comment => SearchOp::Comment,
    })
}

/// Why a trace line could not be parsed into a [`Command`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommandParseError {
    Empty,
    UnknownVerb(String),
    BadArgument(String),
    /// A record past a live limit (RFC-0020 R6: an `ex_edits` batch past [`crate::MAX_APPLY_EDITS`] or
    /// [`crate::MAX_APPLY_EDIT_BYTES`], an `ex_register` value past [`crate::MAX_REGISTER_BYTES`]). Found
    /// before the bytes are decoded, so a hostile line is refused without a large allocation.
    TooLarge(String),
}

impl Command {
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
        let parts = LineParts {
            verb,
            arg,
            raw,
            line,
        };
        // The families' verbs are disjoint, so trying them in turn finds the one arm a single `match`
        // would. The decoders sit in this order in `line_form/decode.rs` too: `trace.rs` scrapes the
        // `"verb" =>` arms there for its fuzz verb list, so every new verb arm belongs in one of them.
        for decode in [
            decode_insert,
            decode_simple_edit,
            decode_mark,
            decode_operator,
            decode_operator_extra,
            decode_paste_register,
            decode_visual,
            decode_search,
            decode_session_emacs,
        ] {
            if let Some(command) = decode(parts)? {
                return Ok(command);
            }
        }
        Ok(match verb {
            // Resolved Ex edits (RFC-0020); their verbs are parsed in `editor/ex_cmd.rs`.
            other if other.starts_with("ex_") => {
                Command::Ex(crate::editor::ExCmd::from_parts(other, raw, line)?)
            }
            other => return Err(CommandParseError::UnknownVerb(other.to_string())),
        })
    }
}

/// One trace line split for the family decoders: the verb, its trimmed argument, the untrimmed remainder
/// (search patterns may contain spaces) and the whole trimmed line (for error messages).
#[derive(Clone, Copy)]
struct LineParts<'a> {
    verb: &'a str,
    arg: Option<&'a str>,
    raw: &'a str,
    line: &'a str,
}
