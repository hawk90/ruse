//! Range, address and register-verb parsing for the line commands. Split from `ex.rs` (#1385).

use super::{single_char, Ex, LineAddr, SubRange, ARGUMENT_REQUIRED};

/// Parse a `:s` line-range prefix. MVP: empty → current line, `%` → whole file, `N` → that line,
/// `N,M` → the inclusive span. Unsupported forms (`.`, `$`, marks) yield `None`.
pub(in crate::input) fn parse_sub_range(s: &str) -> Option<SubRange> {
    let s = s.trim();
    if s.is_empty() || s == "." {
        return Some(SubRange::CurrentLine);
    }
    if s == "%" {
        return Some(SubRange::WholeFile);
    }
    match s.split_once(',') {
        Some((a, b)) => {
            let a = a.trim().parse::<usize>().ok()?;
            let b = b.trim().parse::<usize>().ok()?;
            Some(SubRange::Lines(a.min(b), a.max(b)))
        }
        None => {
            let n = s.parse::<usize>().ok()?;
            Some(SubRange::Lines(n, n))
        }
    }
}

/// Parse a `:[range]<verb> {addr}` line (`:m`/`:move`, `:t`/`:copy`/`:co`) into `(range, dest)`. The verb
/// (after the range prefix) must be one of `verbs` (longest-first). A missing destination is `Some(None)`,
/// which nvim reports as E16 (`:m`, `:t`).
pub(super) fn parse_range_verb_dest(
    line: &str,
    verbs: &[&str],
) -> Option<Option<(SubRange, LineAddr)>> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    // Strip the verb (longest match first so `move` beats `m`), leaving the destination address.
    let addr_str = verbs.iter().find_map(|v| rest.strip_prefix(v))?;
    let range = parse_sub_range(range_str)?;
    if addr_str.trim().is_empty() {
        return Some(None);
    }
    let dest = parse_line_addr(addr_str.trim())?;
    Some(Some((range, dest)))
}

/// Parse a `:m`/`:t` destination address (`:help {address}`): a base of `N` (after line N, 0 = top), `$`
/// (the last line) or `.` (the cursor's line), followed by any number of `+N`/`-N` offsets, where a bare
/// `+`/`-` means 1 and a leading offset implies `.` (`:m+1` = `:m .+1`, `:m-2` = `:m .-2`). Anything else
/// is not an address. (Before #601 `+1` parsed as the absolute line 1, so `:m+1` acted on the wrong line.)
pub(super) fn parse_line_addr(s: &str) -> Option<LineAddr> {
    enum Base {
        Abs(usize),
        Current,
        Last,
    }
    let digits = |s: &str| s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (base, mut rest) = match s.chars().next()? {
        '$' => (Base::Last, &s[1..]),
        '.' => (Base::Current, &s[1..]),
        '+' | '-' => (Base::Current, s),
        '0'..='9' => {
            let end = digits(s);
            (Base::Abs(s[..end].parse().ok()?), &s[end..])
        }
        _ => return None, // `:m`/`:t` require a destination
    };
    let mut delta: isize = 0;
    while let Some(sign) = rest.chars().next() {
        let sign: isize = match sign {
            '+' => 1,
            '-' => -1,
            _ => return None,
        };
        rest = &rest[1..];
        let end = digits(rest);
        let n: isize = if end == 0 {
            1
        } else {
            rest[..end].parse().ok()?
        };
        delta = delta.checked_add(sign.checked_mul(n)?)?;
        rest = &rest[end..];
    }
    Some(match base {
        Base::Abs(n) => LineAddr::Line(n.checked_add_signed(delta)?),
        Base::Current if delta == 0 => LineAddr::Current,
        Base::Current => LineAddr::CurrentOffset(delta),
        Base::Last if delta == 0 => LineAddr::Last,
        Base::Last => LineAddr::LastOffset(delta),
    })
}

/// Parse `:[range]j[oin][!]` into `(range, bang)`. The verb is `j`/`jo`/`joi`/`join` (Vim's abbreviations)
/// after an optional `[0-9,%.$]` range prefix and an optional trailing `!`. A `[count]` argument (Vim's
/// `:j 3`) was already folded into the range (`app::ex_range`).
pub(super) fn parse_join(line: &str) -> Option<(SubRange, bool)> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, verb) = line.split_at(split);
    let verb = verb.trim();
    let (verb, bang) = verb.strip_suffix('!').map_or((verb, false), |v| (v, true));
    if !matches!(verb, "j" | "jo" | "joi" | "join") {
        return None;
    }
    Some((parse_sub_range(range_str)?, bang))
}

/// Parse `:[range]>` / `:[range]<` into `(range, left, levels)`. After the optional `[0-9,%.$]` range
/// prefix the verb is a run of one-or-more IDENTICAL `>` or `<` chars; the run length is the level count
/// (Vim `:>>>` = 3 levels). `left = true` for `<`. Returns `None` (→ falls through) unless the whole verb
/// is such a run — a mixed run (`:><`) or trailing junk (`:> x`; a count was folded into the range before) is not
/// this command.
pub(super) fn parse_shift(line: &str) -> Option<(SubRange, bool, u32)> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, verb) = line.split_at(split);
    let verb = verb.trim();
    let first = verb.chars().next()?;
    if first != '>' && first != '<' {
        return None;
    }
    // The verb must be a run of the SAME char, and nothing else.
    if !verb.chars().all(|c| c == first) {
        return None;
    }
    let range = parse_sub_range(range_str)?;
    Some((range, first == '<', verb.len() as u32))
}

/// Parse `:[line]put[!] [reg]` ([`Ex::Put`]) and `:[line]put[!] ={expr}` ([`Ex::PutExpr`]). After the
/// optional `[0-9.$]` address prefix the verb is `put`/`pu` (longest first). A register argument, when
/// present, is a SINGLE non-whitespace char that must be separated from the verb by whitespace (`:put a` —
/// not `:puta`, which Vim rejects as E492). The `=` register may follow the verb directly (`:put='z'`: the
/// command name ends at the `=`) and takes the rest of the line as its expression (#1342). No address
/// prefix means the bare `:put` default (after the current line → `LineAddr::Current`). Returns `None`
/// (→ falls through) unless the line is a `:put`.
pub(super) fn parse_put(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (addr_str, rest) = line.split_at(split);
    // Strip the verb (longest match first so `put` beats `pu`).
    let rest = rest
        .strip_prefix("put")
        .or_else(|| rest.strip_prefix("pu"))?;
    let (above, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    // Empty address prefix → the bare `:put` default: after the current line.
    let addr = || {
        if addr_str.is_empty() {
            Some(LineAddr::Current)
        } else {
            parse_line_addr(addr_str.trim())
        }
    };
    if let Some(expr) = rest.trim_start().strip_prefix('=') {
        return Some(Ex::PutExpr {
            addr: addr()?,
            expr: expr_argument(expr),
            above,
        });
    }
    // The register arg: nothing (unnamed), or a whitespace-separated single register char. A char directly
    // after the verb (`:puta`) is NOT the put verb — Vim errors E492, so fall through to `Unknown`.
    let reg = if rest.is_empty() {
        None
    } else {
        let arg = rest.strip_prefix(char::is_whitespace)?.trim();
        // A trailing-whitespace-only arg (already trimmed away in `parse_ex`) reaches here empty → unnamed.
        if arg.is_empty() {
            None
        } else {
            Some(single_char(arg)?)
        }
    };
    Some(Ex::Put {
        addr: addr()?,
        reg,
        above,
    })
}

/// The expression of a `:put ={expr}` argument as nvim's `separate_nextcmd` leaves it: an unescaped `"`
/// starts a comment that ends it, and the backslash before a `"` or a `|` is dropped (`\"` is a quote in
/// the expression, `\|` a bar). Whitespace before the comment goes too (#1342).
fn expr_argument(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len());
    for c in arg.chars() {
        match c {
            '"' | '|' if out.ends_with('\\') => {
                out.pop();
                out.push(c);
            }
            '"' => break,
            _ => out.push(c),
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Parse `:[range]ma[rk] {x}` and `:[range]k[ ]{x}` (nvim's `ex_mark`, #1088, #1241). `{x}` is a mark
/// `:mark` can set: a letter, a digit, `'`/`` ` ``, `[`/`]` or `<`/`>`; any other char is `E191`. No
/// argument is `E471`, more than one char `E488`, a `!` `E477`, and a `"` starts a comment. `:keepjumps`
/// and the other `:kee…` commands are not `:k` and fall through.
pub(super) fn parse_mark(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let spelled = ["mark", "mar", "ma"].into_iter().find_map(|v| {
        rest.strip_prefix(v)
            .filter(|a| a.is_empty() || a.starts_with([' ', '!']))
    });
    // Vim's one exception to reading the whole word as the command name (`find_ex_command`): a `k` not
    // followed by `ee` is `:k` with its argument glued on (`:ka` is `:k a`, `:kfoo` is `:k foo`), while
    // `:kee…` is `:keepmarks` and its longer spellings (#1234).
    let arg = spelled.or_else(|| rest.strip_prefix('k').filter(|a| !a.starts_with("ee")))?;
    // The command takes no `!` (`:k!`, `:mark!`): nvim reads a bang glued to the name before the argument.
    if arg.starts_with('!') {
        return Some(Ex::Fail("E477: No ! allowed".into()));
    }
    let arg = comment_free(arg).trim();
    let mut chars = arg.chars();
    let Some(name) = chars.next() else {
        return Some(Ex::Fail(ARGUMENT_REQUIRED.into()));
    };
    if chars.next().is_some() {
        return Some(Ex::Fail(format!("E488: Trailing characters: {arg}")));
    }
    if !(name.is_ascii_alphanumeric() || matches!(name, '\'' | '`' | '[' | ']' | '<' | '>')) {
        return Some(Ex::Fail(
            "E191: Argument must be a letter or forward/backward quote".into(),
        ));
    }
    let range = parse_sub_range(range_str)?;
    Some(Ex::Mark { range, name })
}

/// `arg` up to the `"` that starts a comment in a command that allows one after its argument (`:help
/// :quote`): `:d`, `:y`, `:j`, `:k`, … (#1240).
pub(crate) fn comment_free(arg: &str) -> &str {
    arg.split('"').next().unwrap_or(arg)
}

/// The print flags of `:delete` spelled with a trailing `l` or `p` (`:help :dl`, #1241), as Vim's
/// `find_ex_command` reads it: an abbreviation of `delete` (`d`, `de`, …, `delete` itself) followed by a
/// letter that does not continue the word, which must be `l` (list) or `p` (print). So `:dl`, `:dell`,
/// `:delel`, `:deletel`, `:dp`, `:dep`, `:deletep` are, but `:del` (an abbreviation) and `:dll` are not.
pub(crate) fn delete_print_flags(verb: &str) -> Option<super::PrintFlags> {
    let (stem, flag) = verb.split_at(verb.len().checked_sub(1)?);
    let list = match flag {
        "l" => true,
        "p" => false,
        _ => return None,
    };
    let continues = "delete".get(..verb.len()) == Some(verb);
    (!stem.is_empty() && "delete".starts_with(stem) && !continues).then_some(super::PrintFlags {
        number: false,
        list,
    })
}

/// The spellings of `:delete` and `:yank`.
const DELETE_VERBS: &[&str] = &["d", "de", "del", "dele", "delet", "delete"];

const YANK_VERBS: &[&str] = &["y", "ya", "yan", "yank"];

/// Parse `:[range]{verb} [x]` (`:d` when `delete`, else `:y`) into the range, the register `x`, a single
/// char after whitespace (`:d a`, `:3y A`), and the flags of a `:dl`/`:dp` spelling. A count after the
/// register was already folded into the range, and a comment dropped.
pub(super) fn parse_register_verb(
    line: &str,
    delete: bool,
) -> Option<(SubRange, Option<char>, Option<super::PrintFlags>)> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let rest = rest.trim_start();
    let verb_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (verb, arg) = rest.split_at(verb_len);
    let flags = if delete {
        let flags = delete_print_flags(verb);
        if flags.is_none() && !DELETE_VERBS.contains(&verb) {
            return None;
        }
        flags
    } else if YANK_VERBS.contains(&verb) {
        None
    } else {
        return None;
    };
    let arg = arg.trim();
    let reg = if arg.is_empty() {
        None
    } else {
        Some(single_char(arg)?)
    };
    Some((parse_sub_range(range_str)?, reg, flags))
}
