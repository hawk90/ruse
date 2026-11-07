//! The Ex commands added for #609 and #610, parsed after the range has been resolved to line numbers
//! (`app::ex_range`) and the command word to its canonical spelling (`ex_cmd`): the print family (`:p`,
//! `:l`, `:nu`, `:#`, `:z`, `:=`), `:retab`, the `:startinsert` family, `:pwd` and `:cd`, and `:file`.
//! Each parser returns `None` when the line is not its command, so [`super::parse_ex`] falls through.

use ruse_core::SubRange;

use super::ex::{parse_sub_range, Ex};

/// The `[flags]` of a print command (`:help ex-flags`): `#` adds the line number, `l` prints like `:list`.
/// (`p` only asks for a print, which these commands do anyway.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PrintFlags {
    pub number: bool,
    pub list: bool,
}

/// A parsed `:[line]z[!][+-^.=][count]` (`:help :z`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ZSpec {
    /// The address, or `None` when the command had none (a plain `:z` then starts below the cursor).
    pub line: Option<SubRange>,
    /// `!`: the window is the terminal's height rather than twice `'scroll'`.
    pub bang: bool,
    pub flags: PrintFlags,
    /// `-`, `+`, `=`, `^` or `.`, or `None` for the default (`+` from the line itself).
    pub kind: Option<char>,
    /// How many times `kind` is repeated (`:z---`); multiplies the distance for `-` and `+`.
    pub run: usize,
    /// The window size given after the kind.
    pub count: Option<usize>,
}

/// What `:startinsert` and friends start (`:help :startinsert`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartKind {
    /// `:star[tinsert][!]` — Insert, like `i` (`!`: like `A`).
    Insert,
    /// `:startr[eplace][!]` — Replace, like `R` (`!`: at the end of the line).
    Replace,
    /// `:startg[replace][!]` — Virtual Replace, like `gR`.
    VirtualReplace,
}

/// What `:file` does (`:help :file`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileCmd {
    /// `:f[ile]` — show the file information (`CTRL-G`).
    Info,
    /// `:f[ile] {name}` — give the buffer a new name; the old one becomes the alternate file.
    Rename(String),
    /// `:0f[ile]` — remove the buffer's name.
    Clear,
}

/// nvim's error for text after a command that takes none.
fn trailing(rest: &str) -> Ex {
    Ex::Fail(format!("E488: Trailing characters: {rest}"))
}

/// Split an ex line into its range prefix (numbers, `,`, `%`; the range is already resolved) and the rest.
fn split_range(line: &str) -> (&str, &str) {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%'))
        .unwrap_or(line.len());
    line.split_at(split)
}

/// The command word at the start of `rest` (a run of letters) and the text after it.
fn word(rest: &str) -> (&str, &str) {
    let end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    rest.split_at(end)
}

/// Read the `[flags]` of a print command (nvim's `get_flags`): `l`, `p` and `#`, each followed by any
/// blanks. Returns them and the text after.
fn get_flags(mut rest: &str) -> (PrintFlags, &str) {
    let mut flags = PrintFlags::default();
    while let Some(c) = rest.chars().next().filter(|c| "lp#".contains(*c)) {
        match c {
            'l' => flags.list = true,
            '#' => flags.number = true,
            _ => {}
        }
        rest = rest[1..].trim_start();
    }
    (flags, rest)
}

/// Parse the commands this module adds; `None` when the line is none of them.
pub(super) fn parse(line: &str) -> Option<Ex> {
    parse_print(line)
        .or_else(|| parse_z(line))
        .or_else(|| parse_line_number(line))
        .or_else(|| parse_retab(line))
        .or_else(|| parse_start_insert(line))
        .or_else(|| parse_directory(line))
        .or_else(|| parse_file(line))
        .or_else(|| parse_fold(line))
}

/// `:[range]fo[ld]`, `:[range]foldo[pen][!]`, `:[range]foldc[lose][!]` and `:[range]foldd[oopen]
/// {cmd}` / `:[range]folddoc[losed] {cmd}` (#1141). The fold commands take the cursor's line by default,
/// the `:folddo` ones the whole buffer.
fn parse_fold(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let (verb, rest) = word(rest);
    let range = |default: SubRange| {
        if range_str.is_empty() {
            Some(default)
        } else {
            parse_sub_range(range_str)
        }
    };
    let (bang, after) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    match verb {
        "fold" | "foldopen" | "foldclose" => {
            let range = range(SubRange::CurrentLine)?;
            if bang && verb == "fold" {
                return Some(Ex::Fail("E477: No ! allowed".into()));
            }
            let after = after.trim();
            if !after.is_empty() {
                return Some(trailing(after));
            }
            Some(match verb {
                "fold" => Ex::Fold(range),
                _ => Ex::FoldOpen {
                    range,
                    open: verb == "foldopen",
                    recursive: bang,
                },
            })
        }
        "folddoopen" | "folddoclosed" => {
            let range = range(SubRange::WholeFile)?;
            if bang {
                return Some(Ex::Fail("E477: No ! allowed".into()));
            }
            let cmd = after.trim_start();
            if cmd.is_empty() {
                return Some(Ex::Fail("E471: Argument required".into()));
            }
            Some(Ex::FoldDo {
                range,
                closed: verb == "folddoclosed",
                cmd: cmd.to_string(),
            })
        }
        _ => None,
    }
}

/// `:[range]p[rint] [flags]`, `:[range]l[ist] [flags]`, `:[range]nu[mber] [flags]` and `:[range]# [flags]`
/// (a count after the command was folded into the range by `app::ex_range`).
fn parse_print(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let (verb, rest) = match rest.strip_prefix('#') {
        Some(r) => ("#", r),
        None => word(rest),
    };
    let mut flags = match verb {
        "print" => PrintFlags::default(),
        "list" => PrintFlags {
            list: true,
            ..PrintFlags::default()
        },
        "number" | "#" => PrintFlags {
            number: true,
            ..PrintFlags::default()
        },
        _ => return None,
    };
    // `:pl` is not `:p l`: the command word is the whole run of letters.
    if verb != "#" && !rest.is_empty() && !rest.starts_with([' ', '\t', '#', '!']) {
        return None;
    }
    let (more, rest) = get_flags(rest.trim_start());
    flags.number |= more.number;
    flags.list |= more.list;
    if !rest.is_empty() {
        return Some(trailing(rest));
    }
    Some(Ex::Print {
        range: parse_sub_range(range_str)?,
        flags,
    })
}

/// `:[line]z[!] [flags] [+-^.=][count]` (nvim's `ex_z`).
fn parse_z(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let rest = rest.strip_prefix('z')?;
    // `:zx` is another command word.
    if rest.starts_with(|c: char| c.is_ascii_alphabetic() && !"lp".contains(c)) {
        return None;
    }
    let (bang, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let (flags, mut rest) = get_flags(rest.trim_start());
    let kind = rest.chars().next().filter(|c| "-+=^.".contains(*c));
    let mut run = 0;
    if let Some(k) = kind {
        rest = &rest[1..];
        run = 1;
        // The repeats of the kind multiply the distance; other `-`/`+` after it are skipped.
        while rest.starts_with(k) && "-+".contains(k) {
            rest = &rest[1..];
            run += 1;
        }
    }
    rest = rest.trim_start_matches(['-', '+']);
    let count = if rest.is_empty() {
        None
    } else if rest.starts_with(|c: char| c.is_ascii_digit()) {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        Some(rest[..digits].parse().unwrap_or(usize::MAX))
    } else {
        return Some(Ex::Fail("E144: Non-numeric argument to :z".into()));
    };
    let line = if range_str.is_empty() {
        None
    } else {
        Some(parse_sub_range(range_str)?)
    };
    Some(Ex::Z(ZSpec {
        line,
        bang,
        flags,
        kind,
        run,
        count,
    }))
}

/// `:[range]=` — the last line number of the range (no range: the buffer's last line). An argument is a
/// Lua expression in nvim, which ruse does not run, so that line stays unknown.
fn parse_line_number(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    if rest != "=" {
        return None;
    }
    Some(Ex::LineNumber(if range_str.is_empty() {
        None
    } else {
        Some(parse_sub_range(range_str)?)
    }))
}

/// `:[range]ret[ab][!] [new_tabstop]` — the whole buffer without a range. The new tab stops are one width
/// or a comma list of widths (a `'vartabstop'`, #1163), parsed as nvim's `tabstop_set`.
fn parse_retab(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let (verb, rest) = word(rest);
    if verb != "retab" {
        return None;
    }
    let (bang, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let arg = rest.trim();
    let tabstop = if arg.is_empty() {
        None
    } else {
        match ruse_core::VarStops::parse(arg) {
            Some(v) => Some(v).filter(|v| !v.is_empty()).map(Box::new),
            // nvim's `tabstop_set`: a stop that is 0 or negative is "must be positive".
            None if arg.split(',').any(|p| {
                !p.is_empty()
                    && p.bytes().all(|b| b.is_ascii_digit())
                    && p.parse::<u64>().is_ok_and(|n| n == 0)
            }) =>
            {
                return Some(Ex::Fail("E487: Argument must be positive".into()));
            }
            None => return Some(Ex::Fail(format!("E475: Invalid argument: {arg}"))),
        }
    };
    let range = if range_str.is_empty() {
        SubRange::WholeFile
    } else {
        parse_sub_range(range_str)?
    };
    Some(Ex::Retab {
        range,
        bang,
        tabstop,
    })
}

/// `:star[tinsert][!]`, `:startr[eplace][!]`, `:startg[replace][!]` and `:stopi[nsert]`.
fn parse_start_insert(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let (verb, rest) = word(rest);
    let kind = match verb {
        "startinsert" => Some(StartKind::Insert),
        "startreplace" => Some(StartKind::Replace),
        "startgreplace" => Some(StartKind::VirtualReplace),
        "stopinsert" => None,
        _ => return None,
    };
    if !range_str.is_empty() {
        return Some(Ex::Fail("E481: No range allowed".into()));
    }
    let (bang, rest) = match rest.strip_prefix('!').filter(|_| kind.is_some()) {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let rest = rest.trim();
    if !rest.is_empty() {
        return Some(trailing(rest));
    }
    Some(match kind {
        Some(kind) => Ex::StartInsert { kind, bang },
        None => Ex::StopInsert,
    })
}

/// `:pw[d]` and `:cd[!] [{path}]` / `:chd[ir][!] [{path}]` (the command word is already `cd`).
fn parse_directory(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let (verb, rest) = word(rest);
    if !matches!(verb, "pwd" | "cd") {
        return None;
    }
    if !range_str.is_empty() {
        return Some(Ex::Fail("E481: No range allowed".into()));
    }
    if verb == "pwd" {
        let rest = rest.trim();
        return Some(if rest.is_empty() {
            Ex::Pwd
        } else {
            trailing(rest)
        });
    }
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let arg = rest.trim();
    // `\ ` is a space in the name, as in every file argument.
    let dir = (!arg.is_empty()).then(|| arg.replace("\\ ", " "));
    Some(Ex::Cd(dir))
}

/// `:f[ile][!] [{name}]` and `:0f[ile][!]`.
fn parse_file(line: &str) -> Option<Ex> {
    let (range_str, rest) = split_range(line);
    let (verb, rest) = word(rest);
    if verb != "file" {
        return None;
    }
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let arg = rest.trim();
    match range_str {
        "" => {}
        "0" if arg.is_empty() => return Some(Ex::File(FileCmd::Clear)),
        _ => return Some(Ex::Fail("E474: Invalid argument".into())),
    }
    Some(Ex::File(if arg.is_empty() {
        FileCmd::Info
    } else {
        // `:file` takes the whole argument as the name, blanks included (nvim: `:file a b` names `a b`).
        FileCmd::Rename(arg.replace("\\ ", " "))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::parse_ex;

    fn lines(a: usize, b: usize) -> SubRange {
        SubRange::Lines(a, b)
    }

    #[test]
    fn print_family_parses_flags_and_trailing_text() {
        let print = |range, number, list| Ex::Print {
            range,
            flags: PrintFlags { number, list },
        };
        assert_eq!(parse_ex("p"), print(SubRange::CurrentLine, false, false));
        assert_eq!(parse_ex("2,3print"), print(lines(2, 3), false, false));
        assert_eq!(parse_ex("l"), print(SubRange::CurrentLine, false, true));
        assert_eq!(parse_ex("nu"), print(SubRange::CurrentLine, true, false));
        assert_eq!(parse_ex("2#"), print(lines(2, 2), true, false));
        assert_eq!(parse_ex("p l#"), print(SubRange::CurrentLine, true, true));
        assert_eq!(parse_ex("%p #"), print(SubRange::WholeFile, true, false));
        assert_eq!(
            parse_ex("p x"),
            Ex::Fail("E488: Trailing characters: x".into())
        );
        assert!(matches!(parse_ex("pl"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("P"), Ex::Unknown(_)));
    }

    #[test]
    fn z_parses_kind_run_and_count() {
        let z = |line, kind, run, count| {
            Ex::Z(ZSpec {
                line,
                bang: false,
                flags: PrintFlags::default(),
                kind,
                run,
                count,
            })
        };
        assert_eq!(parse_ex("z"), z(None, None, 0, None));
        assert_eq!(parse_ex("2z"), z(Some(lines(2, 2)), None, 0, None));
        assert_eq!(parse_ex("z-"), z(None, Some('-'), 1, None));
        assert_eq!(parse_ex("z--3"), z(None, Some('-'), 2, Some(3)));
        assert_eq!(parse_ex("z=1"), z(None, Some('='), 1, Some(1)));
        assert_eq!(parse_ex("z 2"), z(None, None, 0, Some(2)));
        assert_eq!(
            parse_ex("zq"),
            Ex::Unknown("zq".into()),
            "another command word"
        );
        assert_eq!(
            parse_ex("z-x"),
            Ex::Fail("E144: Non-numeric argument to :z".into())
        );
        match parse_ex("z!#.") {
            Ex::Z(spec) => {
                assert!(spec.bang && spec.flags.number);
                assert_eq!(spec.kind, Some('.'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn line_number_retab_and_insert_commands_parse() {
        assert_eq!(parse_ex("="), Ex::LineNumber(None));
        assert_eq!(parse_ex("2="), Ex::LineNumber(Some(lines(2, 2))));
        assert_eq!(
            parse_ex("ret"),
            Ex::Retab {
                range: SubRange::WholeFile,
                bang: false,
                tabstop: None
            }
        );
        assert_eq!(
            parse_ex("2,3retab! 4"),
            Ex::Retab {
                range: lines(2, 3),
                bang: true,
                tabstop: ruse_core::VarStops::parse("4").map(Box::new)
            }
        );
        assert!(matches!(parse_ex("retab x"), Ex::Fail(_)));
        assert_eq!(
            parse_ex("star"),
            Ex::StartInsert {
                kind: StartKind::Insert,
                bang: false
            }
        );
        assert_eq!(
            parse_ex("startr!"),
            Ex::StartInsert {
                kind: StartKind::Replace,
                bang: true
            }
        );
        assert_eq!(
            parse_ex("startg"),
            Ex::StartInsert {
                kind: StartKind::VirtualReplace,
                bang: false
            }
        );
        assert_eq!(parse_ex("stopi"), Ex::StopInsert);
        assert_eq!(
            parse_ex("startinsert x"),
            Ex::Fail("E488: Trailing characters: x".into())
        );
    }

    #[test]
    fn directory_and_file_commands_parse() {
        assert_eq!(parse_ex("pw"), Ex::Pwd);
        assert_eq!(parse_ex("cd"), Ex::Cd(None));
        assert_eq!(parse_ex("cd sub"), Ex::Cd(Some("sub".into())));
        assert_eq!(parse_ex("chd -"), Ex::Cd(Some("-".into())));
        assert_eq!(parse_ex("cd! a\\ b"), Ex::Cd(Some("a b".into())));
        assert_eq!(parse_ex("f"), Ex::File(FileCmd::Info));
        assert_eq!(
            parse_ex("file x y"),
            Ex::File(FileCmd::Rename("x y".into()))
        );
        assert_eq!(parse_ex("0f"), Ex::File(FileCmd::Clear));
    }
}
