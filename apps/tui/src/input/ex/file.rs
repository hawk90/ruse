//! The file commands (`:w`, `:sav`, `:e`, `:r`) and their `++opt` options. Split from `ex.rs` (#1385).

use super::{
    parse_line_addr, parse_sub_range, EditSpec, Ex, FileFormatOpt, FileOpts, LineAddr, ReadSource,
    WriteSpec, WriteTarget, ARGUMENT_REQUIRED,
};

/// The file commands [`parse_file_command`] reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileVerb {
    Write,
    WriteQuit,
    Exit,
    Update,
    SaveAs,
    WriteAll {
        quit: bool,
    },
    Edit,
    /// `:fin[d]` / `:sf[ind]` (`split`).
    Find {
        split: bool,
    },
    Drop,
}

/// The file command `verb` names, with every abbreviation Vim accepts (`:w`…`:write`, `:x`/`:exi`, `:up`,
/// `:sav`, `:wa`, `:wqa`/`:xa`, `:e`…`:edit`, `:ex` and `:vi`, `:fin`, `:sf`, `:dr`).
fn file_verb(verb: &str) -> Option<FileVerb> {
    let abbrev = |full: &str, min: usize| verb.len() >= min && full.starts_with(verb);
    Some(if abbrev("write", 1) {
        FileVerb::Write
    } else if verb == "wq" {
        FileVerb::WriteQuit
    } else if abbrev("xit", 1) || abbrev("exit", 3) {
        FileVerb::Exit
    } else if abbrev("update", 2) {
        FileVerb::Update
    } else if abbrev("saveas", 3) {
        FileVerb::SaveAs
    } else if abbrev("wall", 2) {
        FileVerb::WriteAll { quit: false }
    } else if abbrev("wqall", 3) || abbrev("xall", 2) {
        FileVerb::WriteAll { quit: true }
    } else if abbrev("edit", 1) || verb == "ex" || abbrev("visual", 2) {
        // `:vi[sual]` outside Ex mode is `:edit` (`:help :visual`).
        FileVerb::Edit
    } else if abbrev("find", 3) {
        FileVerb::Find { split: false }
    } else if abbrev("sfind", 2) {
        FileVerb::Find { split: true }
    } else if abbrev("drop", 2) {
        FileVerb::Drop
    } else {
        return None;
    })
}

/// nvim's error for an argument it cannot use (an unknown `++opt`).
const INVALID_ARGUMENT: &str = "E474: Invalid argument";

/// Read the leading `++opt` arguments of a file command (`:help ++opt`): `++ff=unix|dos|mac` (or
/// `++fileformat=`) and `++bin` / `++binary` / `++nobin` / `++nobinary` are honoured; any other `++opt`
/// is an [`Ex::Fail`]. Returns the options and the text after them.
fn parse_file_opts(mut rest: &str) -> Result<(FileOpts, &str), Ex> {
    let mut opts = FileOpts::default();
    while let Some(opt) = rest.strip_prefix("++") {
        let end = opt.find(char::is_whitespace).unwrap_or(opt.len());
        match &opt[..end] {
            "bin" | "binary" => {
                opts.bin = Some(true);
                rest = opt[end..].trim_start();
                continue;
            }
            "nobin" | "nobinary" => {
                opts.bin = Some(false);
                rest = opt[end..].trim_start();
                continue;
            }
            _ => {}
        }
        let ff = opt[..end]
            .strip_prefix("ff=")
            .or_else(|| opt[..end].strip_prefix("fileformat="));
        opts.ff = Some(match ff {
            Some("unix") => FileFormatOpt::Unix,
            Some("dos") => FileFormatOpt::Dos,
            Some("mac") => FileFormatOpt::Mac,
            _ => return Err(Ex::Fail(INVALID_ARGUMENT.into())),
        });
        rest = opt[end..].trim_start();
    }
    Ok((opts, rest))
}

/// Read one file-name argument: `\ ` is a space in the name, and an unescaped blank before more text is
/// `E172`, as in Vim. Other backslashes stay for the `%`/`#` expansion (`\%`). `None` for no argument.
fn file_arg(rest: &str) -> Result<Option<String>, Ex> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(None);
    }
    let mut name = String::with_capacity(rest.len());
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek().is_some_and(|c| c.is_whitespace()) => name.extend(chars.next()),
            c if c.is_whitespace() => {
                return Err(Ex::Fail("E172: Only one file name allowed".into()))
            }
            c => name.push(c),
        }
    }
    Ok(Some(name))
}

/// Parse the file commands (#610): `:[range]w[rite][!] [++opt] [{file} | >> [{file}] | !{cmd}]`,
/// `:[range]up[date][!]` (the same, written only when modified), `:[range]wq[!]` and `:[range]x[it][!]`
/// (then close the window), `:sav[eas][!] [++opt] {file}`, `:wa[ll][!]`, `:wqa[ll][!]`/`:xa[ll][!]`, and
/// `:e[dit][!] [++opt] [+cmd] [{file}]`. `None` when the line is none of them.
pub(super) fn parse_file_command(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let verb_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let verb = file_verb(&rest[..verb_len])?;
    let rest = &rest[verb_len..];
    // The verb ends at a `!`, a blank, `>>`/`+` (`:w>>f`, `:e+5 f`), or the line's end.
    if !(rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || "!>+".contains(c))) {
        return None;
    }
    let (bang, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let range = if range_str.is_empty() {
        None
    } else {
        Some(parse_sub_range(range_str)?)
    };
    // Only the `:w` family takes a range (`E481` otherwise, as in nvim).
    let ranged = matches!(
        verb,
        FileVerb::Write | FileVerb::WriteQuit | FileVerb::Exit | FileVerb::Update
    );
    if range.is_some() && !ranged {
        return Some(Ex::Fail("E481: No range allowed".into()));
    }
    let (opts, rest) = match parse_file_opts(rest.trim_start()) {
        Ok(r) => r,
        Err(fail) => return Some(fail),
    };
    Some(match verb {
        FileVerb::Write | FileVerb::WriteQuit | FileVerb::Exit | FileVerb::Update => {
            let target = if let Some(after) = rest.strip_prefix(">>") {
                match file_arg(after) {
                    Ok(f) => WriteTarget::Append(f),
                    Err(fail) => return Some(fail),
                }
            } else if let Some(cmd) = rest.strip_prefix('!') {
                WriteTarget::Command(cmd.trim_start().to_string())
            } else {
                match file_arg(rest) {
                    Ok(f) => WriteTarget::File(f),
                    Err(fail) => return Some(fail),
                }
            };
            Ex::Write(WriteSpec {
                range,
                bang,
                target,
                opts,
                if_modified: matches!(verb, FileVerb::Exit | FileVerb::Update),
                quit: matches!(verb, FileVerb::WriteQuit | FileVerb::Exit),
            })
        }
        FileVerb::SaveAs => match file_arg(rest) {
            Ok(Some(file)) => Ex::SaveAs { bang, file, opts },
            Ok(None) => Ex::Fail(ARGUMENT_REQUIRED.into()),
            Err(fail) => fail,
        },
        FileVerb::WriteAll { quit } => {
            if !rest.trim().is_empty() {
                return Some(Ex::Fail(format!(
                    "E488: Trailing characters: {}",
                    rest.trim()
                )));
            }
            Ex::WriteAll { quit }
        }
        FileVerb::Edit | FileVerb::Find { .. } | FileVerb::Drop => {
            // `+cmd` runs to the next unescaped blank; `\ ` is a space inside it.
            let (cmd, rest) = match rest.strip_prefix('+') {
                Some(after) => {
                    let mut cmd = String::new();
                    let mut chars = after.char_indices().peekable();
                    let mut end = after.len();
                    while let Some((i, c)) = chars.next() {
                        match c {
                            '\\' if chars.peek().is_some_and(|(_, c)| c.is_whitespace()) => {
                                cmd.extend(chars.next().map(|(_, c)| c));
                            }
                            c if c.is_whitespace() => {
                                end = i;
                                break;
                            }
                            c => cmd.push(c),
                        }
                    }
                    (Some(cmd), &after[end..])
                }
                None => (None, rest),
            };
            let (more, rest) = match parse_file_opts(rest.trim_start()) {
                Ok(r) => r,
                Err(fail) => return Some(fail),
            };
            let file = match file_arg(rest) {
                Ok(file) => file,
                Err(fail) => return Some(fail),
            };
            let spec = EditSpec {
                bang,
                file,
                cmd,
                opts: FileOpts {
                    ff: more.ff.or(opts.ff),
                    bin: more.bin.or(opts.bin),
                },
            };
            match verb {
                FileVerb::Find { .. } | FileVerb::Drop if spec.file.is_none() => {
                    Ex::Fail(ARGUMENT_REQUIRED.into())
                }
                FileVerb::Find { split } => Ex::Find { split, spec },
                FileVerb::Drop => Ex::Drop(spec),
                _ => Ex::Edit(spec),
            }
        }
    })
}

/// Parse `:[addr]r[ead] {file}` / `:[addr]r[ead] !{cmd}`. After an optional `[0-9.$]` single-line address
/// prefix (default = the current line; `0` = the top, Vim's `:0r`) the verb is `read`/`re`/`r` (longest
/// first), then WHITESPACE, then the argument: a leading `!` makes it a command read, otherwise a file path.
/// Returns `None` (→ falls through) when the line is not a `:read` or has no argument. `:rename`/`:registers`
/// are consumed earlier in the dispatch chain, so they never reach here.
pub(super) fn parse_read(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '$'))
        .unwrap_or(line.len());
    let (addr_str, rest) = line.split_at(split);
    // Longest verb first so `read` wins over `re`/`r`.
    let rest = ["read", "re", "r"]
        .into_iter()
        .find_map(|v| rest.strip_prefix(v))?;
    // A verb must be followed by whitespace + an argument, or directly by the `!` of a command read
    // (#809). A bare `:r` (re-read the current file) is a documented non-goal and falls through.
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '!') {
        return None;
    }
    let arg = rest.trim();
    if arg.is_empty() {
        return None;
    }
    let source = match arg.strip_prefix('!') {
        Some(cmd) => {
            let cmd = cmd.trim_start();
            if cmd.is_empty() {
                return None; // `:r !` with no command is not a valid read
            }
            ReadSource::Command(cmd.to_string())
        }
        None => ReadSource::File(arg.to_string()),
    };
    let addr = if addr_str.is_empty() {
        LineAddr::Current
    } else {
        parse_line_addr(addr_str.trim())?
    };
    Some(Ex::Read { addr, source })
}
