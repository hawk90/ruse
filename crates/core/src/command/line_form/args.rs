//! The argument and token-field parsers the [`Command::from_line`](super::super::Command::from_line)
//! decoders build on: counts, chars, word cases, hex text, the `<count> <motion>` pair, the parametric
//! motion tokens and the multi-field operator arguments. Split from `line_form.rs` (#1494). They hold no
//! `"verb" =>` arm, so they add nothing to `trace.rs`'s verb scrape of `line_form/`.

use super::super::{Command, Motion, WordCase};
use super::{forced_wise_from_token, motion_from_token, op_kind_from_token, CommandParseError};

/// The fields of a `find_char:<ch>:<forward>:<till>[:r]` motion token (after the prefix).
pub(super) fn find_char_motion(rest: &str) -> Option<Motion> {
    let mut it = rest.split(':');
    let ch = char::from_u32(it.next()?.parse().ok()?)?;
    let forward = it.next()? == "1";
    let till = it.next()? == "1";
    let repeat = match it.next() {
        None => false,
        Some("r") => true,
        Some(_) => return None, // trailing garbage
    };
    if it.next().is_some() {
        return None; // trailing garbage
    }
    Some(Motion::FindChar {
        ch,
        forward,
        till,
        repeat,
    })
}

/// The fields of a `pair:<open>:<close>:<around>` motion token (after the prefix).
pub(super) fn pair_motion(rest: &str) -> Option<Motion> {
    let mut it = rest.split(':');
    let open = char::from_u32(it.next()?.parse().ok()?)?;
    let close = char::from_u32(it.next()?.parse().ok()?)?;
    let around = it.next()? == "1";
    if it.next().is_some() {
        return None; // trailing garbage
    }
    Some(Motion::Pair {
        open,
        close,
        around,
    })
}

/// The fields of a `quote:<ch>:<around>` motion token (after the prefix).
pub(super) fn quote_motion(rest: &str) -> Option<Motion> {
    let mut it = rest.split(':');
    let ch = char::from_u32(it.next()?.parse().ok()?)?;
    let around = it.next()? == "1";
    if it.next().is_some() {
        return None; // trailing garbage
    }
    Some(Motion::Quote { ch, around })
}

/// Parse an operator/move argument `"<count> <motion>"` into a command via `ctor`.
pub(super) fn op_cmd(
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

/// Parse a `u32` argument (a count or a char codepoint) from a command line's token, or a `BadArgument`
/// error naming the line. The single source for the trace codec's count/codepoint parsing.
pub(super) fn arg_u32(arg: Option<&str>, line: &str) -> Result<u32, CommandParseError> {
    arg.and_then(|a| a.parse().ok())
        .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))
}

/// Parse a `char` argument (its decimal Unicode scalar value), or a `BadArgument` error naming the line.
pub(super) fn arg_char(arg: Option<&str>, line: &str) -> Result<char, CommandParseError> {
    let cp = arg_u32(arg, line)?;
    char::from_u32(cp).ok_or_else(|| CommandParseError::BadArgument(line.to_string()))
}

/// Parse a `{count} {codepoint}` argument pair (`replace_char`, `virtual_replace_char`), or a
/// `BadArgument` error naming the line.
pub(super) fn arg_count_char(
    arg: Option<&str>,
    line: &str,
) -> Result<(u32, char), CommandParseError> {
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
    let c = char::from_u32(cp).ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    Ok((n, c))
}

/// Parse a [`WordCase`] argument (`upcase` / `downcase` / `capitalize`), or a `BadArgument` error.
pub(super) fn arg_word_case(arg: Option<&str>, line: &str) -> Result<WordCase, CommandParseError> {
    match arg {
        Some("upcase") => Ok(WordCase::Upcase),
        Some("downcase") => Ok(WordCase::Downcase),
        Some("capitalize") => Ok(WordCase::Capitalize),
        Some("toggle") => Ok(WordCase::Toggle),
        Some("rot13") => Ok(WordCase::Rot13),
        _ => Err(CommandParseError::BadArgument(line.to_string())),
    }
}

/// The inverse of `encode::hex_encode`: an odd length, a non-hex digit, or bytes that are not UTF-8 are errors.
pub(super) fn hex_decode(arg: &str, line: &str) -> Result<String, CommandParseError> {
    let bad = || CommandParseError::BadArgument(line.to_string());
    let digits = arg.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err(bad());
    }
    let bytes = digits
        .chunks(2)
        .map(|p| {
            std::str::from_utf8(p)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(bad)
        })
        .collect::<Result<Vec<u8>, _>>()?;
    String::from_utf8(bytes).map_err(|_| bad())
}

/// The `set_indent_cols {first} {last} {col|-}...` argument of [`decode_operator_extra`](super::decode_operator_extra).
pub(super) fn set_indent_cols_arg(
    arg: Option<&str>,
    line: &str,
) -> Result<Command, CommandParseError> {
    let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    let mut it = a.split_whitespace();
    let mut num = || {
        it.next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))
    };
    let first_line = num()?;
    let last_line = num()?;
    let cols = a
        .split_whitespace()
        .skip(2)
        .map(|s| match s {
            "-" => Ok(None),
            s => s.parse::<usize>().map(Some),
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CommandParseError::BadArgument(a.to_string()))?;
    Ok(Command::SetIndentCols {
        first_line,
        last_line,
        cols,
    })
}

/// The `set_indents {first} {last} {level}...` argument of [`decode_operator_extra`](super::decode_operator_extra).
pub(super) fn set_indents_arg(arg: Option<&str>, line: &str) -> Result<Command, CommandParseError> {
    let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    let mut it = a.split_whitespace();
    let first_line = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
    let last_line = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
    let levels = it
        .map(|s| s.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CommandParseError::BadArgument(a.to_string()))?;
    Ok(Command::SetIndents {
        first_line,
        last_line,
        levels,
    })
}

/// The `op_forced {op} {wise} {count} {motion}` argument of [`decode_operator_extra`](super::decode_operator_extra).
pub(super) fn op_forced_arg(arg: Option<&str>, line: &str) -> Result<Command, CommandParseError> {
    // `op_forced {op} {wise} {count} {motion}`
    let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    let mut it = a.split_whitespace();
    let op = it
        .next()
        .and_then(op_kind_from_token)
        .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    let wise = it
        .next()
        .and_then(forced_wise_from_token)
        .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    let count: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    let motion = it
        .next()
        .and_then(motion_from_token)
        .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
    Ok(Command::OpForced {
        op,
        count,
        motion,
        wise,
    })
}
