//! The `:set` / `:setlocal` / `:setglobal` argument grammar (#614), read the way nvim v0.12.6's
//! `do_set` / `do_one_set_option` read it, and the table of the options ruse honours.
//!
//! One command line holds any number of arguments, separated by blanks (a `\` escapes a blank inside a
//! string value). Each argument is one of
//!
//! | form                     | meaning                                                      |
//! |--------------------------|--------------------------------------------------------------|
//! | `opt`                    | boolean: switch on; number/string: show the value             |
//! | `noopt` / `invopt`       | boolean: switch off / invert (any other type: `E474`)         |
//! | `opt!`                   | boolean: invert (any other type: `E488`)                      |
//! | `opt?`                   | show the value                                                |
//! | `opt&` / `opt&vim`       | reset to ruse's default / to Vim's default                    |
//! | `opt<`                   | copy the global value into the local one                      |
//! | `opt=v` / `opt:v`        | assign (boolean: `E474`; not a number: `E521`)                |
//! | `opt+=v` `opt-=v` `opt^=v` | number: add, subtract, multiply; string: append, remove, prepend |
//! | `all` / `all&`           | show every option / reset every option                        |
//! | `"…`                     | a comment: the rest of the line is ignored                    |
//!
//! Parsing stops at the first malformed argument; the arguments before it still run, in order, and the
//! error shows after them (nvim stops the same way). Range checks that depend on the current value
//! (`sw-=100` is `E487`) belong to the executor, which also stops at its first error.
//!
//! `:set` writes the local and the global value, `:setlocal` the local one and `:setglobal` the global
//! one. ruse models a local value per View for the buffer-local options the core reads (`ts`, `sw`, `et`,
//! `tw`) and per buffer for the fold options; `'scrolloff'` is global-local, with a value per window
//! that `:setlocal` sets and `:set` unsets (#1151); the other options have one value, so every form
//! writes it.
//!
//! Options honoured in part (#1151; nvim v0.12.6 is the reference):
//! - `'cpoptions'` reads `aABceFs_`, nvim's default, which is how ruse behaves; another value is refused
//!   with "option not supported", since each flag would change a different command.
//! - `'verbosefile'` reads empty; a file name is refused the same way (ruse writes no message log).
//! - `'verbose'` above 0 makes `:set opt?` add the `Last set from` line, as nvim does; ruse has no other
//!   verbose messages.
//! - `'scrollback'` stays unsupported: a terminal's scrollback cannot be viewed yet, so a limit would
//!   change nothing a user sees.

use std::borrow::Cow;

use super::nvim_options::{NVIM_BOOLEAN_OPTIONS, NVIM_OPTIONS};

mod assign;
mod show;
mod table;

pub use assign::*;
pub use show::*;
pub use table::*;

/// Which copy of an option a command writes and shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetScope {
    /// `:set`.
    Both,
    /// `:setlocal`.
    Local,
    /// `:setglobal`.
    Global,
}

/// A `+=` / `-=` / `^=` / `=` assignment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    /// `^=`: multiply a number, prepend to a string.
    Prepend,
}

/// An assigned value, as typed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RawVal {
    Num(i64),
    Str(String),
}

/// One parsed argument.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SetArg {
    /// `opt?`, or a bare number/string option.
    Show(Opt),
    /// `opt` / `noopt` on a boolean.
    Bool(Opt, bool),
    /// `invopt` / `opt!`.
    Toggle(Opt),
    /// `opt&` (`vim: false`) / `opt&vim` (`vim: true`).
    Default { opt: Opt, vim: bool },
    /// `opt<`.
    FromGlobal(Opt),
    /// `opt=v`, `opt+=v`, …
    Assign { opt: Opt, op: AssignOp, val: RawVal },
    /// `all`.
    ShowAll,
    /// `all&`.
    ResetAll,
    /// An option nvim knows and ruse does not implement: skipped with a notice.
    Unsupported(String),
    /// A terminal key code (`t_xx`, `<F1>`): nvim accepts and ignores it.
    Ignored,
}

/// A parsed `:set` line: the arguments in order, each with its text (for an error message the executor
/// raises), and the error that ended parsing early, if any. No arguments and no error is a bare `:set`
/// (show the options that differ from their default).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SetLine {
    pub scope: SetScope,
    pub args: Vec<(SetArg, String)>,
    pub error: Option<String>,
}

/// Parse `:set …` / `:setlocal …` / `:setglobal …` (the command word already resolved), or `None` for
/// any other command.
#[must_use]
pub fn parse_set_command(line: &str) -> Option<SetLine> {
    let (word, rest) = line
        .find(|c: char| !c.is_ascii_alphabetic())
        .map_or((line, ""), |i| line.split_at(i));
    let scope = match word {
        "set" => SetScope::Both,
        "setlocal" => SetScope::Local,
        "setglobal" => SetScope::Global,
        _ => return None,
    };
    // `:set!` lists termcap options too in Vim; ruse has none, so it is `:set`.
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    if !(rest.is_empty() || rest.starts_with([' ', '\t', '"'])) {
        return None;
    }
    Some(parse_args(scope, rest))
}

/// Parse the arguments of a `:set` line.
#[must_use]
pub fn parse_args(scope: SetScope, args: &str) -> SetLine {
    let mut out = SetLine {
        scope,
        args: Vec::new(),
        error: None,
    };
    let b = args.as_bytes();
    let mut i = skip_white(b, 0);
    while i < b.len() {
        if b[i] == b'"' {
            break;
        }
        match parse_one(args, i) {
            Ok((arg, end, comment)) => {
                let next = skip_white(b, end);
                out.args.push((arg, args[i..next].to_string()));
                if comment {
                    break;
                }
                i = next;
            }
            Err((msg, at)) => {
                let next = skip_white(b, skip_to_white(b, at));
                out.error = Some(format!("{msg}: {}", &args[i..next]));
                break;
            }
        }
    }
    out
}

const E474: &str = "E474: Invalid argument";

const E488: &str = "E488: Trailing characters";

const E518: &str = "E518: Unknown option";

const E521: &str = "E521: Number required after =";

/// What [`parse_one`] and its form parsers return: the argument, where it ends, and whether a `"`
/// comment follows it directly; or an error with its message and the position its text runs from.
type Parsed = Result<(SetArg, usize, bool), (&'static str, usize)>;

/// One argument starting at `start`: the argument, where it ends, and whether a `"` comment follows it
/// directly. An error carries its message and the position its text runs from (see [`parse_args`]).
///
/// The forms are tried in nvim's order: `all` / `all&`, then the option name (with its `no` / `inv`
/// prefix), then an assignment (`=` `:` `+=` `-=` `^=`), then a suffix (`?` `!` `&` `<`) or none.
fn parse_one(s: &str, start: usize) -> Parsed {
    if let Some(all) = parse_all(s, start) {
        return Ok(all);
    }
    let item = match parse_name(s, start)? {
        Named::Known(item) => item,
        Named::Done(arg) => return Ok(arg),
    };
    if let Some((op, value_at)) = assign_op(s.as_bytes(), item.op_at, item.op) {
        return parse_assign(&item, op, value_at);
    }
    parse_suffix(&item)
}

/// An argument read past its option name: the state the form parsers share.
struct Item<'a> {
    s: &'a str,
    /// Where the argument starts; most errors run from here.
    start: usize,
    prefix: Prefix,
    def: &'static OptDef,
    is_bool: bool,
    /// Where the name ends.
    name_end: usize,
    /// Where the operator is, past any white space after the name.
    op_at: usize,
    /// The byte at `op_at`.
    op: Option<u8>,
}

/// What [`parse_name`] read: an option ruse implements, or an argument already complete.
enum Named<'a> {
    Known(Item<'a>),
    /// A key code, a termcap option, or an option ruse does not implement.
    Done((SetArg, usize, bool)),
}

/// Whether the argument ends at `at`: the end, a blank, or a `"` comment.
fn ends_or_comment(b: &[u8], at: usize) -> bool {
    at >= b.len() || matches!(b[at], b' ' | b'\t' | b'"')
}

/// `all` / `all&`.
fn parse_all(s: &str, start: usize) -> Option<(SetArg, usize, bool)> {
    let rest = &s[start..];
    if rest.starts_with("all") && ends_or_comment(s.as_bytes(), start + 3) {
        return Some((SetArg::ShowAll, start + 3, false));
    }
    if rest.starts_with("all&") {
        return Some((SetArg::ResetAll, start + 4, false));
    }
    None
}

/// The `no` / `inv` prefix and the option name after it, resolved against the option tables.
fn parse_name(s: &str, start: usize) -> Result<Named<'_>, (&'static str, usize)> {
    let b = s.as_bytes();
    let rest = &s[start..];
    let (prefix, mut i) = if rest.starts_with("no") {
        (Prefix::No, start + 2)
    } else if rest.starts_with("inv") {
        (Prefix::Inv, start + 3)
    } else {
        (Prefix::None, start)
    };
    // A key code (`<F1>`) or a termcap option (`t_xx`): nvim has none and ignores them.
    if b.get(i) == Some(&b'<') || s[i..].starts_with("t_") {
        return Ok(Named::Done((SetArg::Ignored, skip_to_white(b, i), false)));
    }
    let name_start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    let name = &s[name_start..i];
    let known = |n: &str| NVIM_OPTIONS.split(' ').any(|o| o == n);
    if name.is_empty() || !known(name) {
        return Err((E518, start));
    }
    let def = lookup(name);
    let is_bool = def.map_or_else(
        || NVIM_BOOLEAN_OPTIONS.split(' ').any(|o| o == name),
        |d| d.kind == OptKind::Bool,
    );
    if prefix != Prefix::None && !is_bool {
        return Err((E474, start));
    }
    let name_end = i;
    // White space may come before the operator (`:set ts =4`, `:set ai  ?`).
    let op_at = skip_white(b, name_end);
    let op = b.get(op_at).copied();
    let Some(def) = def else {
        // A boolean takes no value (`E474`), whether or not ruse implements it.
        if is_bool && matches!(op, Some(b'=' | b':')) {
            return Err((E474, start));
        }
        return Ok(Named::Done((
            SetArg::Unsupported(name.to_string()),
            skip_to_white(b, i),
            false,
        )));
    };
    Ok(Named::Known(Item {
        s,
        start,
        prefix,
        def,
        is_bool,
        name_end,
        op_at,
        op,
    }))
}

/// The assignment operator `op` at `op_at` is, if any, and where its value starts.
fn assign_op(b: &[u8], op_at: usize, op: Option<u8>) -> Option<(AssignOp, usize)> {
    match op {
        Some(b'=' | b':') => Some((AssignOp::Set, op_at + 1)),
        Some(c @ (b'+' | b'-' | b'^')) if b.get(op_at + 1) == Some(&b'=') => Some((
            match c {
                b'+' => AssignOp::Add,
                b'-' => AssignOp::Sub,
                _ => AssignOp::Prepend,
            },
            op_at + 2,
        )),
        _ => None,
    }
}

/// `opt=v`, `opt+=v`, `opt-=v`, `opt^=v`: a number or a string value from `value_at`.
fn parse_assign(item: &Item<'_>, op: AssignOp, value_at: usize) -> Parsed {
    let b = item.s.as_bytes();
    if item.is_bool {
        return Err((E474, item.start));
    }
    let opt = item.def.opt;
    match item.def.kind {
        OptKind::Num { .. } => {
            let (n, end) = parse_number(b, value_at).ok_or((E521, value_at))?;
            if !ends_or_comment(b, end) {
                return Err((E521, value_at));
            }
            let comment = end < b.len() && b[end] == b'"';
            Ok((
                SetArg::Assign {
                    opt,
                    op,
                    val: RawVal::Num(n),
                },
                end,
                comment,
            ))
        }
        _ => {
            let (v, end) = string_value(item.s, value_at);
            Ok((
                SetArg::Assign {
                    opt,
                    op,
                    val: RawVal::Str(v),
                },
                end,
                false,
            ))
        }
    }
}

/// `opt?`, `opt!`, `opt&` / `opt&vim`, `opt<`, or a bare `opt` / `noopt` / `invopt`.
fn parse_suffix(item: &Item<'_>) -> Parsed {
    let b = item.s.as_bytes();
    let (opt, op_at) = (item.def.opt, item.op_at);
    match item.op {
        Some(b'?') => {
            if !ends_or_comment(b, op_at + 1) {
                return Err((E488, item.start));
            }
            Ok((SetArg::Show(opt), op_at + 1, comment_at(b, op_at + 1)))
        }
        Some(b'!') => {
            if !item.is_bool || !ends_or_comment(b, op_at + 1) {
                return Err((E488, item.start));
            }
            Ok((SetArg::Toggle(opt), op_at + 1, comment_at(b, op_at + 1)))
        }
        Some(b'&') => parse_default(item),
        Some(b'<') => {
            if !ends_or_comment(b, op_at + 1) {
                return Err((E488, item.start));
            }
            Ok((SetArg::FromGlobal(opt), op_at + 1, comment_at(b, op_at + 1)))
        }
        _ => parse_bare(item),
    }
}

/// `opt&` / `opt&vi` / `opt&vim`.
fn parse_default(item: &Item<'_>) -> Parsed {
    let (s, b) = (item.s, item.s.as_bytes());
    let mut end = item.op_at + 1;
    let vim = if s[end..].starts_with("vim") {
        end += 3;
        true
    } else if s[end..].starts_with("vi") {
        end += 2;
        true
    } else {
        false
    };
    if !ends_or_comment(b, end) {
        return Err((E488, item.start));
    }
    Ok((
        SetArg::Default {
            opt: item.def.opt,
            vim,
        },
        end,
        comment_at(b, end),
    ))
}

/// No operator: `opt` / `noopt` / `invopt` on a boolean, or `opt` (show) on any other type.
fn parse_bare(item: &Item<'_>) -> Parsed {
    let b = item.s.as_bytes();
    let (opt, name_end) = (item.def.opt, item.name_end);
    // Right after the name only a blank, the end or a comment may follow.
    if !ends_or_comment(b, name_end) {
        return Err((E488, item.start));
    }
    let arg = match (item.is_bool, item.prefix) {
        (true, Prefix::None) => SetArg::Bool(opt, true),
        (true, Prefix::No) => SetArg::Bool(opt, false),
        (true, Prefix::Inv) => SetArg::Toggle(opt),
        (false, _) => SetArg::Show(opt),
    };
    Ok((arg, name_end, comment_at(b, name_end)))
}

fn comment_at(b: &[u8], at: usize) -> bool {
    b.get(at) == Some(&b'"')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prefix {
    None,
    No,
    Inv,
}

fn skip_white(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// The end of the argument at `i`: the next blank a `\` does not escape (nvim's `skiptowhite_esc`).
fn skip_to_white(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && !matches!(b[i], b' ' | b'\t') {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
        }
        i += 1;
    }
    i
}

/// A number the way `:set` reads one (nvim's `vim_str2nr` with every base): an optional `-`, then decimal,
/// `0x` hex, `0b` binary, `0o` or a leading-`0` octal. Saturates at `i64` (the executor rejects anything
/// past `i32` with `E474`). Returns the value and where it ends, or `None` with no digits.
fn parse_number(b: &[u8], at: usize) -> Option<(i64, usize)> {
    let mut i = at;
    let neg = b.get(i) == Some(&b'-');
    if neg {
        i += 1;
    }
    let lower = |k: usize| b.get(k).map(u8::to_ascii_lowercase);
    let (radix, mut j) = match (b.get(i), lower(i + 1)) {
        (Some(b'0'), Some(b'x')) if b.get(i + 2).is_some_and(u8::is_ascii_hexdigit) => (16, i + 2),
        (Some(b'0'), Some(b'b')) if matches!(b.get(i + 2), Some(b'0' | b'1')) => (2, i + 2),
        (Some(b'0'), Some(b'o')) if matches!(b.get(i + 2), Some(b'0'..=b'7')) => (8, i + 2),
        (Some(b'0'), Some(b'0'..=b'7')) => {
            // A leading 0 is octal only when every digit is one (`019` is decimal).
            let run = b[i..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .collect::<Vec<_>>();
            if run.iter().all(|c| (b'0'..=b'7').contains(c)) {
                (8, i + 1)
            } else {
                (10, i)
            }
        }
        _ => (10, i),
    };
    let digits_start = j;
    let mut n: i64 = 0;
    while let Some(d) = b.get(j).and_then(|&c| (c as char).to_digit(radix)) {
        n = n
            .saturating_mul(i64::from(radix))
            .saturating_add(i64::from(d));
        j += 1;
    }
    if j == digits_start {
        return None;
    }
    Some((if neg { -n } else { n }, j))
}

/// A string value from `at` to the next unescaped blank, with each `\` escape resolved.
fn string_value(s: &str, at: usize) -> (String, usize) {
    let mut out = String::new();
    let mut chars = s[at..].char_indices();
    while let Some((k, c)) = chars.next() {
        match c {
            ' ' | '\t' => return (out, at + k),
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    out.push(n);
                }
            }
            c => out.push(c),
        }
    }
    (out, s.len())
}

#[cfg(test)]
mod assign_tests;
#[cfg(test)]
mod show_tests;
#[cfg(test)]
mod tests;
