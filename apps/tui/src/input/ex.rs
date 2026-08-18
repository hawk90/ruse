use super::*;

/// A parsed ex command (the `:` line).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ex {
    Save,
    Quit,
    SaveQuit,
    SaveTrace(String),
    /// `:split`/`:sp` — split the focused window horizontally onto the same buffer (F-007).
    Split,
    /// `:vsplit`/`:vs` — split the focused window vertically onto the same buffer (F-007).
    VSplit,
    /// `:close`/`:clo` — close the focused window (keeps the shared buffer while another holds it).
    Close,
    /// `:[range]s/pat/rep/flags` — substitute (F-009 #2). Parsed into its pieces for the core engine.
    Substitute(SubSpec),
    /// `:[range]g/pat/cmd` (or `:g!`/`:v` for the inverse) — global two-pass command (F-009 #4).
    Global(GlobalSpec),
    /// `:noh` / `:nohlsearch` — clear the search highlight (F-009 #1).
    NoHighlight,
    /// `:lmap {lhs} {rhs}` — install a Lang-Arg (`lmap`) mapping (F-027). Single-char lhs/rhs for MVP.
    Lmap {
        lhs: char,
        rhs: char,
    },
    /// `:lunmap {lhs}` — remove a Lang-Arg mapping (F-027).
    Lunmap {
        lhs: char,
    },
    /// `:checkhealth` / `:che` — report the running editor's health (F-030 / CAP-HEALTHCHECK).
    CheckHealth,
    Unknown(String),
}

/// A parsed `:g/pat/cmd` command (F-009 #4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GlobalSpec {
    /// The line range (default whole file for `:g`).
    pub range: SubRange,
    /// The `:g` selector pattern.
    pub pattern: String,
    /// `:g!` / `:v` — act on NON-matching lines.
    pub negate: bool,
    /// The command to run on each marked line.
    pub cmd: GlobalCmd,
}

/// A parsed `:s///` command (F-009 #2): the line range, the Vim pattern + replacement, and the flags.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SubSpec {
    /// The line range the substitution acts on.
    pub range: SubRange,
    /// The Vim search pattern (between the first two delimiters).
    pub pattern: String,
    /// The replacement text (between the second and third delimiters).
    pub replacement: String,
    /// `g`: replace all matches on each line. (Already gdefault-adjusted by [`parse_ex`].)
    pub global: bool,
    /// Case override: `i` → `Some(true)`, `I` → `Some(false)`, else `None`.
    pub ignore_case: Option<bool>,
    /// `c`: confirm each substitution interactively (handled by the frontend; PR-c2).
    pub confirm: bool,
}

/// Parse a `:[range]s/pat/rep/flags` line into a [`SubSpec`], or `None` if it is not a substitute.
/// `gdefault` inverts the meaning of the `g` flag (Vim `'gdefault'`); pass it through so the caller's
/// config toggles it. MVP ranges: none (current line), `%` (whole file), `N` / `N,M` (line numbers).
pub(crate) fn parse_substitute(line: &str, gdefault: bool) -> Option<SubSpec> {
    // Split the leading range prefix (chars `[0-9,%.$]`) from the command.
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    // The command verb is `s` or `substitute`, then a single-char delimiter.
    let rest = rest
        .strip_prefix("substitute")
        .or_else(|| rest.strip_prefix('s'))?;
    let delim = rest.chars().next()?;
    if !delim.is_ascii_punctuation() {
        return None; // e.g. `:sort` — `o` is not a delimiter
    }
    let body = &rest[delim.len_utf8()..];
    // Split into up to three parts on UNESCAPED delimiters (`\/` stays literal in the pattern/rep).
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            cur.push('\\');
            if let Some(n) = chars.next() {
                cur.push(n);
            }
        } else if c == delim {
            parts.push(std::mem::take(&mut cur));
            if parts.len() == 2 {
                // Everything after the third delimiter is the flags run (no more splitting).
                parts.push(chars.as_str().to_string());
                cur.clear();
                break;
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() || parts.len() < 2 {
        parts.push(cur);
    }
    let pattern = parts.first().cloned().unwrap_or_default();
    let replacement = parts.get(1).cloned().unwrap_or_default();
    let flags = parts.get(2).map(String::as_str).unwrap_or("");

    let mut global = flags.contains('g');
    if gdefault {
        global = !global; // 'gdefault' inverts the g flag
    }
    let ignore_case = if flags.contains('i') {
        Some(true)
    } else if flags.contains('I') {
        Some(false)
    } else {
        None
    };
    let range = parse_sub_range(range_str)?;
    Some(SubSpec {
        range,
        pattern,
        replacement,
        global,
        ignore_case,
        confirm: flags.contains('c'),
    })
}

/// Parse a `:[range]g/pat/cmd` line (or `:g!` / `:v` for the inverse) into a [`GlobalSpec`], or `None`
/// if it is not a global command. The `:g` default range is the WHOLE FILE. MVP commands: `d` and
/// `s/pat/rep/flags`.
fn parse_global(line: &str) -> Option<GlobalSpec> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let (rest, negate) = strip_global_verb(rest)?;
    let delim = rest.chars().next()?;
    if !delim.is_ascii_punctuation() {
        return None;
    }
    let body = &rest[delim.len_utf8()..];
    let (pattern, cmd_str) = split_first_unescaped(body, delim)?;
    let cmd = parse_global_cmd(&cmd_str)?;
    let range = if range_str.is_empty() {
        SubRange::WholeFile
    } else {
        parse_sub_range(range_str)?
    };
    Some(GlobalSpec {
        range,
        pattern,
        negate,
        cmd,
    })
}

/// Strip the `:g` command verb — `vglobal` / `global` / `g!` / `g` / `v` (longest first; `!` and `v`
/// mark the inverse) — returning the remainder and whether the selection is negated, or `None` if the
/// line is not a global command.
fn strip_global_verb(rest: &str) -> Option<(&str, bool)> {
    if let Some(r) = rest.strip_prefix("vglobal") {
        Some((r, true))
    } else if let Some(r) = rest.strip_prefix("global") {
        Some((r, false))
    } else if let Some(r) = rest.strip_prefix("g!") {
        Some((r, true))
    } else if let Some(r) = rest.strip_prefix('g') {
        Some((r, false))
    } else if let Some(r) = rest.strip_prefix('v') {
        Some((r, true))
    } else {
        None
    }
}

/// Split `s` at the FIRST unescaped `delim` into `(before, after)`; `None` if there is no delimiter.
fn split_first_unescaped(s: &str, delim: char) -> Option<(String, String)> {
    let mut before = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            before.push('\\');
            if let Some(n) = chars.next() {
                before.push(n);
            }
        } else if c == delim {
            return Some((before, chars.as_str().to_string()));
        } else {
            before.push(c);
        }
    }
    None
}

/// Parse the command that follows `:g/pat/`. MVP: `d`/`delete` and a `s/pat/rep/flags` substitute.
fn parse_global_cmd(cmd: &str) -> Option<GlobalCmd> {
    match cmd.trim() {
        "d" | "delete" => Some(GlobalCmd::Delete),
        other => {
            let spec = parse_substitute(other, false)?;
            Some(GlobalCmd::Substitute {
                pattern: spec.pattern,
                replacement: spec.replacement,
                flags: SubFlags {
                    global: spec.global,
                    ignore_case: spec.ignore_case,
                },
            })
        }
    }
}

/// Parse a `:s` line-range prefix. MVP: empty → current line, `%` → whole file, `N` → that line,
/// `N,M` → the inclusive span. Unsupported forms (`.`, `$`, marks) yield `None`.
fn parse_sub_range(s: &str) -> Option<SubRange> {
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

/// Parse the text typed after `:` (without the leading colon).
#[must_use]
pub fn parse_ex(line: &str) -> Ex {
    let line = line.trim();
    match line {
        "w" => Ex::Save,
        "q" | "q!" => Ex::Quit,
        "wq" | "x" => Ex::SaveQuit,
        "split" | "sp" => Ex::Split,
        "vsplit" | "vsp" | "vs" => Ex::VSplit,
        "close" | "clo" => Ex::Close,
        "noh" | "nohl" | "nohlsearch" => Ex::NoHighlight,
        "checkhealth" | "checkhealt" | "checkheal" | "che" => Ex::CheckHealth,
        // `:lmap`/`:lunmap` (F-027), then `:trace save`, `:[range]s///`, `:[range]g//` — each returns
        // `None`/falls through to the next so an unrecognised line lands on `Ex::Unknown`.
        _ => {
            if let Some(ex) = parse_lmap(line) {
                ex
            } else if let Some(rest) = line.strip_prefix("trace save") {
                Ex::SaveTrace(rest.trim().to_string())
            } else if let Some(spec) = parse_substitute(line, false) {
                // `:[range]s/pat/rep/flags` — `'gdefault'` defaults off (Vim factory; config seam deferred).
                Ex::Substitute(spec)
            } else if let Some(spec) = parse_global(line) {
                // `:[range]g/pat/cmd` (or `:g!` / `:v`).
                Ex::Global(spec)
            } else {
                Ex::Unknown(line.to_string())
            }
        }
    }
}

/// Parse `:lmap {lhs} {rhs}` / `:lunmap {lhs}` (F-027 Lang-Arg mappings). Returns `None` for any other
/// line so [`parse_ex`] falls through. MVP is single-char lhs and rhs — a multi-key RHS is an RFC-0013
/// follow-up. A verb must be followed by whitespace, so `:lmapx` stays `Unknown`, not `:lmap x`.
fn parse_lmap(line: &str) -> Option<Ex> {
    if let Some(rest) = line.strip_prefix("lunmap") {
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        return single_char(rest.trim()).map(|lhs| Ex::Lunmap { lhs });
    }
    let rest = line.strip_prefix("lmap")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut parts = rest.split_whitespace();
    let lhs = single_char(parts.next()?)?;
    let rhs = single_char(parts.next()?)?;
    // Extra tokens are not the single-char MVP form.
    if parts.next().is_some() {
        return None;
    }
    Some(Ex::Lmap { lhs, rhs })
}

/// The one `char` of `s`, or `None` if `s` is empty or holds more than one char.
fn single_char(s: &str) -> Option<char> {
    let mut cs = s.chars();
    let c = cs.next()?;
    cs.next().is_none().then_some(c)
}
