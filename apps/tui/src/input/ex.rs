use super::*;

/// A parsed ex command (the `:` line).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ex {
    Save,
    /// `:q`/`:quit` — quit, refusing with E37 when the focused buffer has unsaved changes.
    Quit,
    /// `:q!`/`:quit!` — quit, discarding unsaved changes.
    QuitForce,
    SaveQuit,
    SaveTrace(String),
    /// `:split`/`:sp` — split the focused window horizontally onto the same buffer (F-007).
    Split,
    /// `:vsplit`/`:vs` — split the focused window vertically onto the same buffer (F-007).
    VSplit,
    /// `:close`/`:clo` — close the focused window (keeps the shared buffer while another holds it).
    Close,
    /// `:only`/`:on` — close every window except the focused one (the buffers stay loaded).
    Only,
    /// `:[range]d`/`:delete` — delete the range's lines (no range = the current line), like a linewise `dd`.
    Delete(SubRange),
    /// `:[range]y`/`:yank` — yank the range's lines linewise into the unnamed register (like `yy`).
    Yank(SubRange),
    /// `:[range]m {addr}`/`:move` — move the range's lines to after the destination line.
    Move(SubRange, LineAddr),
    /// `:[range]t {addr}`/`:copy`/`:co` — copy the range's lines to after the destination line.
    Copy(SubRange, LineAddr),
    /// `:[range]sort[!] [n][u]` — sort the range's lines (whole file with no range).
    Sort(SubRange, SortSpec),
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
    /// `:enew` / `:ene` — open a new empty (scratch) buffer in the focused window (F-007 multi-buffer).
    Enew,
    /// `:e {file}` / `:edit {file}` — open a file into a new buffer and focus it (F-007 multi-buffer).
    Edit(String),
    /// `:ls` / `:buffers` — list the buffers on the status line (F-007 multi-buffer).
    Buffers,
    /// `:bnext` / `:bn` — switch the focused window to the next buffer in list order (F-007).
    BufferNext,
    /// `:bprevious` / `:bp` — switch the focused window to the previous buffer in list order (F-007).
    BufferPrev,
    /// `:b {n}` (by buffer number) or `:b#` (the alternate buffer) — switch the focused window (F-007).
    Buffer(BufTarget),
    /// `:bd`/`:bdelete` (`!` to force past unsaved changes) — delete the focused buffer from the list.
    BufferDelete {
        force: bool,
    },
    Unknown(String),
}

/// The target of a `:b` command: a buffer NUMBER (its `DocumentId`, as shown in `:ls`) or the `#`
/// alternate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BufTarget {
    /// `:b {n}` — the buffer whose id is `n`.
    Number(u64),
    /// `:b#` — the alternate buffer.
    Alternate,
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

/// A parsed `:sort` command: the flags this MVP honors (`!` reverse, `n` numeric, `u` unique).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SortSpec {
    /// `!` — sort descending.
    pub reverse: bool,
    /// `n` — sort on each line's first decimal number.
    pub numeric: bool,
    /// `u` — drop duplicate lines after sorting.
    pub unique: bool,
}

/// Parse `:[range]sort[!] [flags]` (`sort`/`sor`). No range = the WHOLE FILE (Vim). Honors `!`, `n`, `u`;
/// other Vim sort flags (`i`/`r`/`b`/`x`) are accepted but ignored, and a `/pattern/` form is not a sort.
fn parse_sort(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let rest = rest
        .strip_prefix("sort")
        .or_else(|| rest.strip_prefix("sor"))?;
    let (reverse, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let flags = rest.trim();
    // Only a bare flag run is a sort we understand; anything else (e.g. `/pat/`) falls through to Unknown.
    if !flags
        .chars()
        .all(|c| matches!(c, 'n' | 'u' | 'i' | 'r' | 'b' | 'x'))
    {
        return None;
    }
    let range = if range_str.is_empty() {
        SubRange::WholeFile
    } else {
        parse_sub_range(range_str)?
    };
    Some(Ex::Sort(
        range,
        SortSpec {
            reverse,
            numeric: flags.contains('n'),
            unique: flags.contains('u'),
        },
    ))
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
        "q" | "quit" => Ex::Quit,
        "q!" | "quit!" => Ex::QuitForce,
        "wq" | "x" => Ex::SaveQuit,
        "split" | "sp" => Ex::Split,
        "vsplit" | "vsp" | "vs" => Ex::VSplit,
        "close" | "clo" => Ex::Close,
        "only" | "on" => Ex::Only,
        "noh" | "nohl" | "nohlsearch" => Ex::NoHighlight,
        "checkhealth" | "checkhealt" | "checkheal" | "che" => Ex::CheckHealth,
        "enew" | "ene" => Ex::Enew,
        "ls" | "buffers" | "files" => Ex::Buffers,
        "bnext" | "bn" => Ex::BufferNext,
        "bprevious" | "bprev" | "bp" => Ex::BufferPrev,
        "bd" | "bdelete" => Ex::BufferDelete { force: false },
        "bd!" | "bdelete!" => Ex::BufferDelete { force: true },
        "b#" | "buffer#" => Ex::Buffer(BufTarget::Alternate),
        // `:lmap`/`:lunmap` (F-027), then `:b {n}`, `:trace save`, `:[range]s///`, `:[range]g//` — each
        // returns `None`/falls through to the next so an unrecognised line lands on `Ex::Unknown`.
        _ => {
            if let Some(ex) = parse_lmap(line) {
                ex
            } else if let Some(ex) = parse_buffer(line) {
                ex
            } else if let Some(ex) = parse_edit(line) {
                ex
            } else if let Some(rest) = line.strip_prefix("trace save") {
                Ex::SaveTrace(rest.trim().to_string())
            } else if let Some(range) = parse_range_verb(line, &["d", "delete"]) {
                Ex::Delete(range)
            } else if let Some(range) = parse_range_verb(line, &["y", "yank"]) {
                Ex::Yank(range)
            } else if let Some((range, dest)) = parse_range_verb_dest(line, &["move", "m"]) {
                Ex::Move(range, dest)
            } else if let Some((range, dest)) = parse_range_verb_dest(line, &["copy", "co", "t"]) {
                Ex::Copy(range, dest)
            } else if let Some(ex) = parse_sort(line) {
                ex
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

/// Parse a `:[range]<verb> {addr}` line (`:m`/`:move`, `:t`/`:copy`/`:co`) into `(range, dest)`. The verb
/// (after the range prefix) must be one of `verbs` (longest-first), and a destination address is required.
fn parse_range_verb_dest(line: &str, verbs: &[&str]) -> Option<(SubRange, LineAddr)> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    // Strip the verb (longest match first so `move` beats `m`), leaving the destination address.
    let addr_str = verbs.iter().find_map(|v| rest.strip_prefix(v))?;
    let dest = parse_line_addr(addr_str.trim())?;
    let range = parse_sub_range(range_str)?;
    Some((range, dest))
}

/// Parse a `:m`/`:t` destination address: `0`/`N` (after line N, 0 = top), `$` (last line), `.` (current).
fn parse_line_addr(s: &str) -> Option<LineAddr> {
    match s {
        "" => None, // `:m`/`:t` require a destination
        "$" => Some(LineAddr::Last),
        "." => Some(LineAddr::Current),
        _ => s.parse::<usize>().ok().map(LineAddr::Line),
    }
}

/// Split a `:[range]<verb>` line into its parsed [`SubRange`] and the trimmed verb, if the verb (after the
/// leading `[0-9,%.$]` range prefix) is exactly one of `verbs`. Shared by the line-range ops (`:d`/`:y`/…).
fn parse_range_verb(line: &str, verbs: &[&str]) -> Option<SubRange> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, verb) = line.split_at(split);
    if verbs.contains(&verb.trim()) {
        parse_sub_range(range_str)
    } else {
        None
    }
}

/// Parse `:e {file}` / `:edit {file}` (open a file into a new buffer). Requires a whitespace-separated,
/// non-empty path, so `:enew` (matched literally earlier) and a bare `:e` do not reach here as an edit.
fn parse_edit(line: &str) -> Option<Ex> {
    let rest = line
        .strip_prefix("edit")
        .or_else(|| line.strip_prefix('e'))?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let file = rest.trim();
    if file.is_empty() {
        return None;
    }
    Some(Ex::Edit(file.to_string()))
}

/// Parse `:b {n}` / `:buffer {n}` (switch to buffer number `n`). `:b#` is handled as a literal in
/// [`parse_ex`]. Returns `None` for any other line so `parse_ex` falls through. A verb must be followed
/// by whitespace, so `:box` stays `Unknown`, not `:b ox`.
fn parse_buffer(line: &str) -> Option<Ex> {
    let rest = line
        .strip_prefix("buffer")
        .or_else(|| line.strip_prefix('b'))?;
    // Accept `:b 2`, `:b2`, `:buffer 2`. The remainder must be a bare buffer number (verbs like `bnext`
    // are matched literally before this, so they never reach here).
    let n: u64 = rest.trim().parse().ok()?;
    Some(Ex::Buffer(BufTarget::Number(n)))
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
