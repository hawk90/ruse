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
    /// `:terminal`/`:term` — open a shell in a new PTY-backed buffer (F-011). Unix-only in slice 1.
    Terminal,
    /// `:fmt`/`:format` — format the focused buffer via its language server (F-014).
    Format,
    /// `:rename {new}`/`:rn {new}` — rename the symbol under the cursor via the language server (F-014).
    Rename(String),
    /// `:references`/`:refs`/`:ref` — list all references to the symbol under the cursor (F-014).
    References,
    /// `:codeaction`/`:ca` — list code actions (quickfixes/assists) at the cursor (F-014).
    CodeAction,
    /// `:diagnostics`/`:diags`/`:diag` — list the focused buffer's diagnostics; Enter jumps (F-014).
    Diagnostics,
    /// `:registers`/`:reg`/`:display` — view the non-empty registers (F-029). View-only.
    Registers,
    /// `:digraphs`/`:dig` — list the curated digraph table (code + glyph + decimal) in a view-only overlay.
    Digraphs,
    /// `:marks` — view the set marks (a-z, `.`, `^`); Enter jumps to the mark (F-003).
    Marks,
    /// `:jumps` — view the jumplist; Enter jumps to the position (F-003).
    Jumps,
    /// `:changes` — view the change list; Enter jumps to the position (F-003).
    Changes,
    /// `:[range]d`/`:delete` — delete the range's lines (no range = the current line), like a linewise `dd`.
    Delete(SubRange),
    /// `:[range]y`/`:yank` — yank the range's lines linewise into the unnamed register (like `yy`).
    Yank(SubRange),
    /// `:[range]j[oin][!]` — join the range's lines into one (no range = the current line, which joins with
    /// the next). `bang` is the `!` (raw `gJ`) form that concatenates without adjusting whitespace; without
    /// it, whitespace is collapsed to a single space exactly like normal-mode `J`.
    Join {
        range: SubRange,
        bang: bool,
    },
    /// `:[range]m {addr}`/`:move` — move the range's lines to after the destination line.
    Move(SubRange, LineAddr),
    /// `:[range]t {addr}`/`:copy`/`:co` — copy the range's lines to after the destination line.
    Copy(SubRange, LineAddr),
    /// `:[range]sort[!] [n][u]` — sort the range's lines (whole file with no range).
    Sort(SubRange, ruse_core::SortOptions),
    /// `:[range]>` / `:[range]<` — shift the range's lines one indent level right (`left = false`) or left
    /// (`left = true`), reusing the core `>>`/`<<` shift (no range = the current line). Repeated verb chars
    /// multiply the level count: `:>>>` is `levels = 3`, `:<<` is `levels = 2` (Vim: each extra `>`/`<` adds
    /// one `shiftwidth`). A trailing `[count]` argument (Vim's `:> 5`) is a documented follow-up — the shared
    /// range-verb parser has no trailing-count seam (neither do `:d`/`:y`/`:j`).
    Shift {
        range: SubRange,
        left: bool,
        levels: u32,
    },
    /// `:[line]put [reg]` / `:pu` — put a register's text LINEWISE as new whole line(s) after the addressed
    /// line (F-029). UNLIKE normal-mode `p`, a put is ALWAYS linewise: a charwise register's text is split
    /// on newlines and each piece inserted as its own line. `addr` is the destination (`Line(0)` = the very
    /// top, `Line(n)` = after line n, `Last` = `$` after the last line, `Current` = the bare `:put` default,
    /// after the cursor's line). `reg` is the optional register name (`None` = the unnamed register). The
    /// cursor lands on the first non-blank of the last inserted line.
    Put {
        addr: LineAddr,
        reg: Option<char>,
    },
    /// `:set {option}` — set one editor option on the focused view (F-009 / indent config).
    Set(EditorOption),
    /// `:earlier [N]` / `:ea` — go back N changes in chronological (branch-aware) undo time (F-005 #3).
    Earlier(u32),
    /// `:later [N]` / `:lat` — go forward N changes in chronological undo time (F-005 #3).
    Later(u32),
    /// `:[range]s/pat/rep/flags` — substitute (F-009 #2). Parsed into its pieces for the core engine.
    Substitute(SubSpec),
    /// The ex repeat-substitute forms that reuse the LAST `:s` (pattern + replacement), like normal-mode
    /// `&`/`g&` (verified vs nvim v0.12.4):
    /// - bare `:[range]s` and `:[range]&` → repeat DROPPING the previous flags (`flags = Some(default)`),
    ///   so only the first match per line is replaced — exactly like normal-mode `&`.
    /// - `:[range]&&` → repeat KEEPING the previous flags (`flags = None`), like `g&` but scoped to the range.
    /// - `:[range]s {flags}` (bare `s` followed by ONLY flag chars, e.g. `:s g`, `:sg`) → repeat with the
    ///   GIVEN flags REPLACING the old ones (`flags = Some(parsed)`; empty flag run = default = the bare `:s`).
    ///
    /// No range = the current line. The core keeps no `:s` history, so — like `Command::RepeatSubstituteLine`
    /// — the FRONTEND resolves this against its last-substitute state; `run_ex` treats it as a no-op.
    RepeatSubstitute {
        range: SubRange,
        flags: Option<ruse_core::SubFlags>,
    },
    /// `:[range]g/pat/cmd` (or `:g!`/`:v` for the inverse) — global two-pass command (F-009 #4).
    Global(GlobalSpec),
    /// `:noh` / `:nohlsearch` — clear the search highlight (F-009 #1).
    NoHighlight,
    /// `:set (no)hlsearch` — toggle persistent search-match highlighting (frontend render preference; ON by
    /// default). Distinct from `:noh`, which is a one-shot clear that leaves the option on.
    SetHlSearch(bool),
    /// `:set (no)incsearch` — toggle incremental highlight while typing `/`…`?` (frontend; ON by default).
    SetIncSearch(bool),
    /// `:set (no)fixeol` / `(no)fixendofline` — opt-in: on save, ADD a final `\n` when the buffer lacks one
    /// (frontend write preference; OFF by default — byte-preserve is the honest default). Vim's fixendofline.
    SetFixEol(bool),
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
    /// `:e!` / `:edit!` — reload the focused buffer's file from disk, discarding unsaved changes.
    EditReload,
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
    /// `:[range]normal[!] {keys}` — execute `{keys}` as if typed in Normal mode. With no range, once at the
    /// cursor (column preserved); with a range, once per line with the cursor at column 0 of each line. The
    /// `!` (ignore user mappings) parses and is accepted, but is a no-op today — this editor has no user
    /// remaps. `keys` is the VERBATIM payload after the single delimiting space (spaces included), carrying
    /// Vim `<>` key-notation the executor resolves.
    Normal {
        bang: bool,
        range: Option<SubRange>,
        keys: String,
    },
    /// `:[addr]r[ead] {file}` / `:[addr]r[ead] !{cmd}` — insert a file's contents (or a shell command's
    /// stdout, the `!{cmd}` form) as new line(s) BELOW the addressed line. `:0r` inserts above line 1; a bare
    /// `:r` reads below the current line. `addr` resolves as for `:m`/`:t`/`:put` (`Line(0)` = the top). The
    /// file read is PURE IO (no shell); the command read shells out. The frontend does the read (core is
    /// IO-free), then splices the bytes in via [`ruse_core::Workspace::read_lines`].
    Read {
        addr: LineAddr,
        source: ReadSource,
    },
    /// `:{range}!{cmd}` (and `:%!{cmd}`) — FILTER the range's lines through a shell command: pipe them to the
    /// command's stdin and REPLACE them with its stdout. No range = the current line. Handled in the run loop
    /// (it shells out, then splices via [`ruse_core::Workspace::filter_lines`]); unix `sh -c` only.
    Filter {
        range: SubRange,
        cmd: String,
    },
    /// `:!{cmd}` — run `{cmd}` through the shell and show its output on the status line; NO buffer change.
    /// Handled in the run loop (unix `sh -c` only).
    Shell(String),
    Unknown(String),
}

/// The source of a `:r`/`:read`: a FILE (pure IO) or a shell COMMAND's stdout (`:r !{cmd}`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReadSource {
    /// `:r {file}` — read the file at this path.
    File(String),
    /// `:r !{cmd}` — read the stdout of running `{cmd}` through the shell.
    Command(String),
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
    pub cmd: GlobalPayload,
}

/// The command a `:g/pat/…` runs on each marked line, split by WHERE it executes. `d`/`s///` run entirely
/// in core's two-pass ([`Workspace::global`]); `normal` drives the input engine, so it is carried here as a
/// frontend payload and executed by the run loop (which owns the engine) rather than descending into core.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GlobalPayload {
    /// A core-executed payload — `:g/pat/d` or `:g/pat/s/pat2/rep/flags` — run by [`Workspace::global`].
    Core(GlobalCmd),
    /// `:g/pat/normal[!] {keys}` — replay `{keys}` as Normal-mode input on each marked line (frontend).
    /// `keys` is the VERBATIM payload after the single delimiting space (internal spaces preserved).
    Normal { bang: bool, keys: String },
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
    /// `n`: report-only — count the matches and echo `N matches on M lines` WITHOUT editing the buffer,
    /// moving the cursor, or adding an undo entry (Vim's `:s///n`). Takes priority over `c` (like Vim).
    pub count_only: bool,
}

/// Parse `:earlier [N]` / `:later [N]` (or `:ea` / `:lat`) — chronological undo time travel. The optional
/// count is a number of CHANGES (default 1); Vim's time (`5m`) and file (`3f`) suffixes are not modeled.
fn parse_time_travel(line: &str) -> Option<Ex> {
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let n: u32 = if rest.trim().is_empty() {
        1
    } else {
        rest.trim().parse().ok()?
    };
    match verb {
        "earlier" | "ea" => Some(Ex::Earlier(n)),
        "later" | "lat" => Some(Ex::Later(n)),
        _ => None,
    }
}

/// Parse `:set {option}` for the options this MVP honors: `ignorecase`/`ic`, `smartcase`/`scs`,
/// `expandtab`/`et` (each with a `no` prefix to turn off), and `shiftwidth`/`sw`/`tabstop`/`ts`/
/// `textwidth`/`tw` `=N`.
/// An unknown option returns `None` (→ `Unknown`); a bare `:set` (no option) is not handled here.
fn parse_set(line: &str) -> Option<Ex> {
    let opt = line
        .strip_prefix("set ")
        .or_else(|| line.strip_prefix("se "))?
        .trim();
    // hlsearch/incsearch are frontend render preferences, not core options — return their own Ex variants.
    match opt {
        "hlsearch" | "hls" => return Some(Ex::SetHlSearch(true)),
        "nohlsearch" | "nohls" => return Some(Ex::SetHlSearch(false)),
        "incsearch" | "is" => return Some(Ex::SetIncSearch(true)),
        "noincsearch" | "nois" => return Some(Ex::SetIncSearch(false)),
        "fixeol" | "fixendofline" => return Some(Ex::SetFixEol(true)),
        "nofixeol" | "nofixendofline" => return Some(Ex::SetFixEol(false)),
        _ => {}
    }
    let ex = match opt {
        "ignorecase" | "ic" => EditorOption::IgnoreCase(true),
        "noignorecase" | "noic" => EditorOption::IgnoreCase(false),
        "smartcase" | "scs" => EditorOption::SmartCase(true),
        "nosmartcase" | "noscs" => EditorOption::SmartCase(false),
        "expandtab" | "et" => EditorOption::ExpandTab(true),
        "noexpandtab" | "noet" => EditorOption::ExpandTab(false),
        _ => {
            let (k, v) = opt.split_once('=')?;
            let n: usize = v.trim().parse().ok()?;
            match k.trim() {
                "shiftwidth" | "sw" | "tabstop" | "ts" => EditorOption::ShiftWidth(n),
                "textwidth" | "tw" => EditorOption::TextWidth(n),
                _ => return None,
            }
        }
    };
    Some(Ex::Set(ex))
}

/// Parse `:[range]sort[!] [i][n][r][u] [/pattern/]` (`sort`/`sor`). No range = the WHOLE FILE (Vim). Honors
/// `!` reverse, `n` numeric, `u` unique, `i` case-insensitive, and a trailing `/pattern/` (with `r` = sort
/// on the matched text). Other Vim flags (`b`/`x` binary/hex numeric, `l`/`o`) are accepted but ignored.
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
    // A trailing `/pattern/` (Vim): everything from the first `/` is the pattern (optionally closed by a
    // second `/`); the flag run is what precedes it. `//` with no closer means "to end of line".
    let (flag_str, pattern) = match rest.find('/') {
        Some(i) => {
            let after = &rest[i + 1..];
            let pat = after.strip_suffix('/').unwrap_or(after);
            (&rest[..i], (!pat.is_empty()).then(|| pat.to_string()))
        }
        None => (rest, None),
    };
    let flags = flag_str.trim();
    // Only a bare flag run before any pattern is a sort we understand.
    if !flags
        .chars()
        .all(|c| matches!(c, 'n' | 'u' | 'i' | 'r' | 'b' | 'x' | 'l' | 'o'))
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
        ruse_core::SortOptions {
            reverse,
            numeric: flags.contains('n'),
            unique: flags.contains('u'),
            ignore_case: flags.contains('i'),
            // `r` only means "sort on the match" when there is a pattern; otherwise it is inert.
            use_match: flags.contains('r') && pattern.is_some(),
            pattern,
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
        count_only: flags.contains('n'),
    })
}

/// The `:s`/`:&` repeat-substitute flag chars this MVP honors, mirroring [`parse_substitute`]: `g` (all
/// matches), `i`/`I` (case override). `c` (confirm) is the interactive-loop form and is NOT wired through the
/// repeat path (it needs the frontend confirm loop, and no confirm state is stored in the `:s` history).
const REPEAT_SUB_FLAG_CHARS: &[char] = &['g', 'i', 'I'];

/// Build [`ruse_core::SubFlags`] from a flag run (used for `:s {flags}`). `I` wins over `i` if both appear
/// (last one set); `g` sets global. `gdefault` inverts `g` exactly like [`parse_substitute`].
fn repeat_sub_flags(flags: &str, gdefault: bool) -> ruse_core::SubFlags {
    let mut global = flags.contains('g');
    if gdefault {
        global = !global;
    }
    let ignore_case = if flags.contains('i') {
        Some(true)
    } else if flags.contains('I') {
        Some(false)
    } else {
        None
    };
    ruse_core::SubFlags {
        global,
        ignore_case,
    }
}

/// Parse the ex repeat-substitute forms — bare `:[range]s`, `:[range]s {flags}`, `:[range]&`, `:[range]&&` —
/// into an [`Ex::RepeatSubstitute`], or `None` if the line is not one of them. Runs AFTER [`parse_substitute`]
/// in the dispatch chain, so a real `:s/pat/rep/` (with a delimiter) is already consumed here. `gdefault`
/// inverts the `g` flag for the `:s {flags}` form, matching [`parse_substitute`].
fn parse_repeat_substitute(line: &str, gdefault: bool) -> Option<Ex> {
    // Split the leading range prefix (chars `[0-9,%.$]`) from the verb, exactly like the other range verbs.
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);

    // `:&` / `:&&` — the verb is a run of ONLY `&` (like `parse_shift`'s `>`/`<` run). One `&` drops the flags
    // (like `:s`/`&`); two or more keep them (`:&&`). No collision with `:>`/`:<` (different verb char).
    let verb = rest.trim();
    if !verb.is_empty() && verb.chars().all(|c| c == '&') {
        let range = parse_sub_range(range_str)?;
        let flags = if verb.len() == 1 {
            Some(ruse_core::SubFlags::default()) // `:&` — drop flags
        } else {
            None // `:&&` — keep the stored flags
        };
        return Some(Ex::RepeatSubstitute { range, flags });
    }

    // Bare `:s` / `:s {flags}` — the verb is `s`/`substitute` (matching `parse_substitute`) followed by ONLY
    // flag chars (optionally space-separated: `:s g` and `:sg` both work in nvim). A real `:s/pat/…/` was
    // already taken by `parse_substitute`; anything with a non-flag char (`:sort`, `:s 3`) falls through.
    let after_verb = rest
        .strip_prefix("substitute")
        .or_else(|| rest.strip_prefix('s'))?;
    let flag_run = after_verb.trim();
    if !flag_run.chars().all(|c| REPEAT_SUB_FLAG_CHARS.contains(&c)) {
        return None;
    }
    let range = parse_sub_range(range_str)?;
    Some(Ex::RepeatSubstitute {
        range,
        flags: Some(repeat_sub_flags(flag_run, gdefault)),
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

/// Parse the command that follows `:g/pat/`: `normal[!] {keys}` (frontend-executed), `d`/`delete`, or a
/// `s/pat/rep/flags` substitute (both core-executed).
fn parse_global_cmd(cmd: &str) -> Option<GlobalPayload> {
    // `normal[!] {keys}` is recognized BEFORE the `.trim()` below so the key payload keeps its internal
    // spaces — only the single delimiting space after the verb is consumed (as `:normal` does).
    if let Some(payload) = parse_global_normal(cmd) {
        return Some(payload);
    }
    match cmd.trim() {
        "d" | "delete" => Some(GlobalPayload::Core(GlobalCmd::Delete)),
        other => {
            let spec = parse_substitute(other, false)?;
            Some(GlobalPayload::Core(GlobalCmd::Substitute {
                pattern: spec.pattern,
                replacement: spec.replacement,
                flags: SubFlags {
                    global: spec.global,
                    ignore_case: spec.ignore_case,
                },
            }))
        }
    }
}

/// Parse `:g/pat/normal[!] {keys}` — the `normal`/`norma`/`norm` verb (Vim's abbreviations, longest first)
/// after optional leading whitespace and an optional `!`, then exactly ONE delimiting space, then the
/// VERBATIM key payload. `None` (→ falls through to `d`/`s`) unless the payload is a `normal`. A bare
/// `normal` with no space/keys is not a valid invocation. `normalx` is not the verb (no `!`/space follows).
fn parse_global_normal(cmd: &str) -> Option<GlobalPayload> {
    let rest = cmd.trim_start();
    let rest = ["normal", "norma", "norm"]
        .into_iter()
        .find_map(|v| rest.strip_prefix(v))?;
    let (bang, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let keys = rest.strip_prefix(' ')?;
    Some(GlobalPayload::Normal {
        bang,
        keys: keys.to_string(),
    })
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
    // `:[range]normal[!] {keys}` is parsed FIRST, on the RAW line: everything after the one delimiting space
    // is the verbatim key payload, so a trailing space in the keys must survive the `line.trim()` below.
    if let Some(ex) = parse_normal(line) {
        return ex;
    }
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
        "terminal" | "term" => Ex::Terminal,
        "fmt" | "format" => Ex::Format,
        "references" | "refs" | "ref" => Ex::References,
        "codeaction" | "codeactions" | "ca" => Ex::CodeAction,
        "diagnostics" | "diags" | "diag" => Ex::Diagnostics,
        "registers" | "reg" | "display" | "di" => Ex::Registers,
        "digraphs" | "digraph" | "dig" => Ex::Digraphs,
        "marks" => Ex::Marks,
        "jumps" => Ex::Jumps,
        "changes" => Ex::Changes,
        "noh" | "nohl" | "nohlsearch" => Ex::NoHighlight,
        "checkhealth" | "checkhealt" | "checkheal" | "che" => Ex::CheckHealth,
        "e!" | "edit!" => Ex::EditReload,
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
            } else if let Some(name) = parse_rename(line) {
                Ex::Rename(name)
            } else if let Some(ex) = parse_read(line) {
                ex
            } else if let Some(ex) = parse_bang(line) {
                ex
            } else if let Some(rest) = line.strip_prefix("trace save") {
                Ex::SaveTrace(rest.trim().to_string())
            } else if let Some(range) = parse_range_verb(line, &["d", "delete"]) {
                Ex::Delete(range)
            } else if let Some(range) = parse_range_verb(line, &["y", "yank"]) {
                Ex::Yank(range)
            } else if let Some((range, bang)) = parse_join(line) {
                Ex::Join { range, bang }
            } else if let Some((range, dest)) = parse_range_verb_dest(line, &["move", "m"]) {
                Ex::Move(range, dest)
            } else if let Some((range, dest)) = parse_range_verb_dest(line, &["copy", "co", "t"]) {
                Ex::Copy(range, dest)
            } else if let Some((range, left, levels)) = parse_shift(line) {
                Ex::Shift {
                    range,
                    left,
                    levels,
                }
            } else if let Some((addr, reg)) = parse_put(line) {
                Ex::Put { addr, reg }
            } else if let Some(ex) = parse_sort(line) {
                ex
            } else if let Some(ex) = parse_set(line) {
                ex
            } else if let Some(ex) = parse_time_travel(line) {
                ex
            } else if let Some(spec) = parse_substitute(line, false) {
                // `:[range]s/pat/rep/flags` — `'gdefault'` defaults off (Vim factory; config seam deferred).
                Ex::Substitute(spec)
            } else if let Some(ex) = parse_repeat_substitute(line, false) {
                // Bare `:s` / `:s {flags}` / `:&` / `:&&` — repeat the last `:s` (resolved in the run loop).
                ex
            } else if let Some(spec) = parse_global(line) {
                // `:[range]g/pat/cmd` (or `:g!` / `:v`).
                Ex::Global(spec)
            } else {
                Ex::Unknown(line.to_string())
            }
        }
    }
}

/// Parse `:[range]normal[!] {keys}` (`:help :normal`). The verb is `norm`/`norma`/`normal` (Vim's
/// abbreviations) after an optional `[0-9,%.$]` range prefix and an optional `!`; exactly ONE space then
/// delimits the VERBATIM key payload (which may itself contain spaces). Runs on the RAW line so a trailing
/// space in the keys is preserved. Returns `None` (→ falls through to the rest of `parse_ex`) unless the
/// line is a `:normal`. A bare `:normal` with no space/keys is not a valid invocation and returns `None`.
fn parse_normal(raw: &str) -> Option<Ex> {
    let line = raw.trim_start();
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    // Longest verb first so `normal` wins over `norma`/`norm`; a following char that is not `!`/space means
    // this is not the `:normal` verb (e.g. `:normalx` is not `:normal x`).
    let rest = ["normal", "norma", "norm"]
        .into_iter()
        .find_map(|v| rest.strip_prefix(v))?;
    let (bang, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    // The single delimiting space is mandatory and consumed; the rest is the literal key payload.
    let keys = rest.strip_prefix(' ')?;
    let range = if range_str.is_empty() {
        None
    } else {
        Some(parse_sub_range(range_str)?)
    };
    Some(Ex::Normal {
        bang,
        range,
        keys: keys.to_string(),
    })
}

/// Parse `:rename {new}` / `:rn {new}` (F-014) — the trimmed new name, or `None` when the verb is absent or
/// the name is empty. The name is a single token; internal whitespace makes it invalid (the server validates).
fn parse_rename(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("rename ")
        .or_else(|| line.strip_prefix("rn "))?
        .trim();
    (!rest.is_empty() && !rest.contains(char::is_whitespace)).then(|| rest.to_string())
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
/// Parse `:[range]j[oin][!]` into `(range, bang)`. The verb is `j`/`jo`/`joi`/`join` (Vim's abbreviations)
/// after an optional `[0-9,%.$]` range prefix and an optional trailing `!`. A `[count]` argument (Vim's
/// `:j 3`) is a documented follow-up — the shared range-verb parser has no count seam yet.
fn parse_join(line: &str) -> Option<(SubRange, bool)> {
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
/// is such a run — a mixed run (`:><`) or trailing junk (`:> x`, the deferred trailing-count form) is not
/// this command.
fn parse_shift(line: &str) -> Option<(SubRange, bool, u32)> {
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

/// Parse `:[line]put [reg]` into `(addr, reg)`. After the optional `[0-9.$]` address prefix the verb is
/// `put`/`pu` (longest first). A register argument, when present, is a SINGLE non-whitespace char that must
/// be separated from the verb by whitespace (`:put a` — not `:puta`, which Vim rejects as E492). No address
/// prefix means the bare `:put` default (after the current line → `LineAddr::Current`). Returns `None`
/// (→ falls through) unless the line is a `:put`.
fn parse_put(line: &str) -> Option<(LineAddr, Option<char>)> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (addr_str, rest) = line.split_at(split);
    // Strip the verb (longest match first so `put` beats `pu`).
    let rest = rest
        .strip_prefix("put")
        .or_else(|| rest.strip_prefix("pu"))?;
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
    // Empty address prefix → the bare `:put` default: after the current line.
    let addr = if addr_str.is_empty() {
        LineAddr::Current
    } else {
        parse_line_addr(addr_str.trim())?
    };
    Some((addr, reg))
}

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

/// Parse `:[addr]r[ead] {file}` / `:[addr]r[ead] !{cmd}`. After an optional `[0-9.$]` single-line address
/// prefix (default = the current line; `0` = the top, Vim's `:0r`) the verb is `read`/`re`/`r` (longest
/// first), then WHITESPACE, then the argument: a leading `!` makes it a command read, otherwise a file path.
/// Returns `None` (→ falls through) when the line is not a `:read` or has no argument. `:rename`/`:registers`
/// are consumed earlier in the dispatch chain, so they never reach here.
fn parse_read(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '$'))
        .unwrap_or(line.len());
    let (addr_str, rest) = line.split_at(split);
    // Longest verb first so `read` wins over `re`/`r`.
    let rest = ["read", "re", "r"]
        .into_iter()
        .find_map(|v| rest.strip_prefix(v))?;
    // A verb must be followed by whitespace + an argument (a bare `:r` — re-read the current file — is a
    // documented non-goal and falls through).
    if !rest.starts_with(char::is_whitespace) {
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

/// Parse the `!` shell forms: `:!{cmd}` (run + show output, NO range) and `:{range}!{cmd}` / `:%!{cmd}`
/// (FILTER the range's lines through `{cmd}`). After an optional `[0-9,%.$]` range prefix the verb is a
/// single `!`, then the command (the rest of the line, VERBATIM). A range distinguishes a FILTER from a bare
/// run: `:!ls` runs, `:.!ls` / `:%!sort` / `:2,3!sort` filter. Returns `None` unless the line is a `!` form
/// with a non-empty command.
fn parse_bang(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let cmd = rest.strip_prefix('!')?;
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    if range_str.is_empty() {
        // No range → `:!cmd` runs the command and shows its output (no buffer change).
        Some(Ex::Shell(cmd.to_string()))
    } else {
        // A range → `:{range}!cmd` filters those lines through the command.
        Some(Ex::Filter {
            range: parse_sub_range(range_str)?,
            cmd: cmd.to_string(),
        })
    }
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

/// Fill an empty `:s` pattern from the last search (`/` / `*`), exactly as Vim resolves `:s//repl/`. A
/// no-op for a non-empty pattern, a non-substitute command, or when there is no last search to borrow.
pub(crate) fn reuse_last_search(ex: &mut Ex, last_search: Option<&str>) {
    if let Ex::Substitute(spec) = ex {
        if spec.pattern.is_empty() {
            if let Some(last) = last_search {
                spec.pattern = last.to_string();
            }
        }
    }
}

#[cfg(test)]
mod reuse_last_search_tests {
    use super::*;

    fn sub(pattern: &str) -> Ex {
        Ex::Substitute(SubSpec {
            range: SubRange::CurrentLine,
            pattern: pattern.to_string(),
            replacement: "X".to_string(),
            global: false,
            ignore_case: None,
            confirm: false,
            count_only: false,
        })
    }

    fn pattern_of(ex: &Ex) -> &str {
        match ex {
            Ex::Substitute(s) => &s.pattern,
            _ => unreachable!(),
        }
    }

    #[test]
    fn empty_pattern_borrows_the_last_search() {
        let mut ex = sub("");
        reuse_last_search(&mut ex, Some("foo"));
        assert_eq!(pattern_of(&ex), "foo");
    }

    #[test]
    fn non_empty_pattern_is_left_alone() {
        let mut ex = sub("bar");
        reuse_last_search(&mut ex, Some("foo"));
        assert_eq!(
            pattern_of(&ex),
            "bar",
            "an explicit pattern wins over the last search"
        );
    }

    #[test]
    fn empty_pattern_without_a_last_search_stays_empty() {
        let mut ex = sub("");
        reuse_last_search(&mut ex, None);
        assert_eq!(pattern_of(&ex), "");
    }
}

#[cfg(test)]
mod normal_parse_tests {
    use super::*;

    fn normal(line: &str) -> (bool, Option<SubRange>, String) {
        match parse_ex(line) {
            Ex::Normal { bang, range, keys } => (bang, range, keys),
            other => panic!("expected Ex::Normal, got {other:?}"),
        }
    }

    #[test]
    fn no_range_takes_the_whole_verbatim_payload() {
        assert_eq!(normal("normal dwx"), (false, None, "dwx".to_string()));
        // The abbreviations `norm`/`norma` resolve the same.
        assert_eq!(normal("norm dwx"), (false, None, "dwx".to_string()));
        assert_eq!(normal("norma dwx"), (false, None, "dwx".to_string()));
        // Only the FIRST space is the delimiter; further spaces are part of the keys.
        assert_eq!(normal("normal f x"), (false, None, "f x".to_string()));
    }

    #[test]
    fn bang_is_parsed_and_accepted() {
        assert_eq!(normal("normal! A;"), (true, None, "A;".to_string()));
    }

    #[test]
    fn range_prefixes_resolve() {
        assert_eq!(
            normal("%normal A;"),
            (false, Some(SubRange::WholeFile), "A;".to_string())
        );
        assert_eq!(
            normal("2,3normal 0x"),
            (false, Some(SubRange::Lines(2, 3)), "0x".to_string())
        );
        assert_eq!(
            normal(".normal x"),
            (false, Some(SubRange::CurrentLine), "x".to_string())
        );
    }

    #[test]
    fn join_parses_range_verb_and_bang() {
        // No range → current line.
        assert_eq!(
            parse_ex("j"),
            Ex::Join {
                range: SubRange::CurrentLine,
                bang: false
            }
        );
        // The abbreviations `jo`/`joi`/`join` all resolve.
        for v in ["jo", "joi", "join"] {
            assert_eq!(
                parse_ex(v),
                Ex::Join {
                    range: SubRange::CurrentLine,
                    bang: false
                }
            );
        }
        // A line range.
        assert_eq!(
            parse_ex("2,4join"),
            Ex::Join {
                range: SubRange::Lines(2, 4),
                bang: false
            }
        );
        // The `!` (raw `gJ`) form.
        assert_eq!(
            parse_ex("j!"),
            Ex::Join {
                range: SubRange::CurrentLine,
                bang: true
            }
        );
        assert_eq!(
            parse_ex("%join!"),
            Ex::Join {
                range: SubRange::WholeFile,
                bang: true
            }
        );
        // `:jumps` is NOT a join (its exact-match arm wins first).
        assert_eq!(parse_ex("jumps"), Ex::Jumps);
    }

    #[test]
    fn key_notation_stays_verbatim_in_the_payload() {
        // The payload is stored VERBATIM; the executor resolves `<Esc>` etc. later.
        assert_eq!(
            normal("normal Ihi<Esc>"),
            (false, None, "Ihi<Esc>".to_string())
        );
    }

    #[test]
    fn non_normal_and_bare_normal_fall_through() {
        // `:normalx` is not the `:normal` verb.
        assert!(matches!(parse_ex("normalx"), Ex::Unknown(_)));
        // A bare `:normal` (no delimiting space / keys) is not a valid invocation.
        assert!(matches!(parse_ex("normal"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("normal!"), Ex::Unknown(_)));
    }
}

#[cfg(test)]
mod shift_parse_tests {
    use super::*;

    #[test]
    fn shift_parses_range_direction_and_levels() {
        // No range → current line, one level.
        assert_eq!(
            parse_ex(">"),
            Ex::Shift {
                range: SubRange::CurrentLine,
                left: false,
                levels: 1
            }
        );
        assert_eq!(
            parse_ex("<"),
            Ex::Shift {
                range: SubRange::CurrentLine,
                left: true,
                levels: 1
            }
        );
        // Repeated verb chars are the level count (Vim `:>>>` = 3, `:<<` = 2).
        assert_eq!(
            parse_ex(">>>"),
            Ex::Shift {
                range: SubRange::CurrentLine,
                left: false,
                levels: 3
            }
        );
        assert_eq!(
            parse_ex("<<"),
            Ex::Shift {
                range: SubRange::CurrentLine,
                left: true,
                levels: 2
            }
        );
        // A line range and the whole-file `%`.
        assert_eq!(
            parse_ex("2,4>"),
            Ex::Shift {
                range: SubRange::Lines(2, 4),
                left: false,
                levels: 1
            }
        );
        assert_eq!(
            parse_ex("%<<"),
            Ex::Shift {
                range: SubRange::WholeFile,
                left: true,
                levels: 2
            }
        );
    }

    #[test]
    fn mixed_run_or_trailing_junk_is_not_a_shift() {
        // A mixed `>`/`<` run is not this command.
        assert!(matches!(parse_ex("><"), Ex::Unknown(_)));
        // The trailing-count form `:> 5` is deferred; with trailing junk it is not a shift.
        assert!(matches!(parse_ex("> 5"), Ex::Unknown(_)));
        assert!(matches!(parse_ex(">x"), Ex::Unknown(_)));
    }
}

#[cfg(test)]
mod repeat_substitute_parse_tests {
    use super::*;

    fn drop_flags() -> Option<ruse_core::SubFlags> {
        Some(ruse_core::SubFlags::default())
    }
    fn g_flags() -> Option<ruse_core::SubFlags> {
        Some(ruse_core::SubFlags {
            global: true,
            ignore_case: None,
        })
    }

    #[test]
    fn bare_s_and_ampersand_drop_flags_on_the_current_line() {
        // Bare `:s` and `:&` both repeat the last `:s` on the current line WITHOUT its flags.
        for line in ["s", "&"] {
            assert_eq!(
                parse_ex(line),
                Ex::RepeatSubstitute {
                    range: SubRange::CurrentLine,
                    flags: drop_flags(),
                },
                "`:{line}` = flag-less current-line repeat"
            );
        }
    }

    #[test]
    fn double_ampersand_keeps_flags() {
        // `:&&` (a run of two or more `&`) KEEPS the stored flags (`flags = None`).
        assert_eq!(
            parse_ex("&&"),
            Ex::RepeatSubstitute {
                range: SubRange::CurrentLine,
                flags: None,
            }
        );
    }

    #[test]
    fn s_with_only_flags_uses_the_given_flags() {
        // `:s g` and `:sg` (space optional) both mean "repeat with the given flag(s)".
        for line in ["s g", "sg"] {
            assert_eq!(
                parse_ex(line),
                Ex::RepeatSubstitute {
                    range: SubRange::CurrentLine,
                    flags: g_flags(),
                },
                "`:{line}` = repeat with `g`"
            );
        }
        // `i`/`I` case flags come through too.
        assert_eq!(
            parse_ex("s i"),
            Ex::RepeatSubstitute {
                range: SubRange::CurrentLine,
                flags: Some(ruse_core::SubFlags {
                    global: false,
                    ignore_case: Some(true),
                }),
            }
        );
    }

    #[test]
    fn ranges_resolve_for_every_form() {
        assert_eq!(
            parse_ex("2,3&&"),
            Ex::RepeatSubstitute {
                range: SubRange::Lines(2, 3),
                flags: None,
            }
        );
        assert_eq!(
            parse_ex("%s"),
            Ex::RepeatSubstitute {
                range: SubRange::WholeFile,
                flags: drop_flags(),
            }
        );
        assert_eq!(
            parse_ex("5&"),
            Ex::RepeatSubstitute {
                range: SubRange::Lines(5, 5),
                flags: drop_flags(),
            }
        );
    }

    #[test]
    fn real_substitute_and_other_verbs_are_not_repeat_forms() {
        // A real `:s/pat/rep/` stays a `Substitute`, not a repeat.
        assert!(matches!(parse_ex("s/a/b/"), Ex::Substitute(_)));
        // Other `s`-verbs and trailing junk fall through (they never reach the repeat parser or it rejects).
        assert!(matches!(parse_ex("sort"), Ex::Sort(..)));
        assert!(matches!(parse_ex("split"), Ex::Split));
        assert!(matches!(parse_ex("s 3"), Ex::Unknown(_))); // trailing count is deferred (like `:d`/`:>`)
        assert!(matches!(parse_ex("sx"), Ex::Unknown(_))); // `x` is not a repeat flag
    }
}

#[cfg(test)]
mod read_filter_parse_tests {
    use super::*;

    #[test]
    fn read_file_forms_parse_addr_and_path() {
        // Bare `:r`/`:read` → below the current line.
        assert_eq!(
            parse_ex("r ins.txt"),
            Ex::Read {
                addr: LineAddr::Current,
                source: ReadSource::File("ins.txt".into())
            }
        );
        assert_eq!(
            parse_ex("read ins.txt"),
            Ex::Read {
                addr: LineAddr::Current,
                source: ReadSource::File("ins.txt".into())
            }
        );
        // A line address: `:2r` below line 2, `:0r` at the top, `:$r` after the last line.
        assert_eq!(
            parse_ex("2r ins.txt"),
            Ex::Read {
                addr: LineAddr::Line(2),
                source: ReadSource::File("ins.txt".into())
            }
        );
        assert_eq!(
            parse_ex("0r ins.txt"),
            Ex::Read {
                addr: LineAddr::Line(0),
                source: ReadSource::File("ins.txt".into())
            }
        );
        assert_eq!(
            parse_ex("$r ins.txt"),
            Ex::Read {
                addr: LineAddr::Last,
                source: ReadSource::File("ins.txt".into())
            }
        );
    }

    #[test]
    fn read_command_form_parses_after_the_bang() {
        assert_eq!(
            parse_ex("r !sort"),
            Ex::Read {
                addr: LineAddr::Current,
                source: ReadSource::Command("sort".into())
            }
        );
        // Leading space after `!` is trimmed; the rest of the command is verbatim.
        assert_eq!(
            parse_ex("3r ! ls -la"),
            Ex::Read {
                addr: LineAddr::Line(3),
                source: ReadSource::Command("ls -la".into())
            }
        );
    }

    #[test]
    fn bang_run_vs_filter_is_decided_by_the_range() {
        // No range → `:!cmd` runs and shows output.
        assert_eq!(parse_ex("!ls"), Ex::Shell("ls".into()));
        assert_eq!(parse_ex("!echo hi"), Ex::Shell("echo hi".into()));
        // A range → `:{range}!cmd` filters.
        assert_eq!(
            parse_ex("%!sort"),
            Ex::Filter {
                range: SubRange::WholeFile,
                cmd: "sort".into()
            }
        );
        assert_eq!(
            parse_ex("2,3!sort"),
            Ex::Filter {
                range: SubRange::Lines(2, 3),
                cmd: "sort".into()
            }
        );
        // An explicit `.` is a range (current line), so it filters — distinct from a bare `:!`.
        assert_eq!(
            parse_ex(".!tr a-z A-Z"),
            Ex::Filter {
                range: SubRange::CurrentLine,
                cmd: "tr a-z A-Z".into()
            }
        );
    }

    #[test]
    fn non_read_and_empty_forms_fall_through() {
        // A bare `:r`/`:read` (no argument — re-read the current file) is a documented non-goal.
        assert!(matches!(parse_ex("r"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("read"), Ex::Unknown(_)));
        // `:r !` with no command is not a valid read.
        assert!(matches!(parse_ex("r !"), Ex::Unknown(_)));
        // A `!` with no command, and a range-only `!`, are not shell forms.
        assert!(matches!(parse_ex("!"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("%!"), Ex::Unknown(_)));
        // `:registers`/`:rename`/`:redo` are NOT swallowed by the `:r` verb.
        assert_eq!(parse_ex("registers"), Ex::Registers);
        assert_eq!(parse_ex("rename foo"), Ex::Rename("foo".into()));
        assert!(matches!(parse_ex("redo"), Ex::Unknown(_)));
    }
}

#[cfg(test)]
mod set_hlsearch_incsearch_tests {
    use super::*;

    #[test]
    fn parses_hlsearch_and_incsearch_toggles() {
        assert_eq!(parse_ex("set hlsearch"), Ex::SetHlSearch(true));
        assert_eq!(parse_ex("set nohls"), Ex::SetHlSearch(false));
        assert_eq!(parse_ex("set incsearch"), Ex::SetIncSearch(true));
        assert_eq!(parse_ex("set nois"), Ex::SetIncSearch(false));
        // The abbreviations still resolve, and unrelated options are untouched.
        assert_eq!(parse_ex("set hls"), Ex::SetHlSearch(true));
        assert_eq!(parse_ex("set ic"), Ex::Set(EditorOption::IgnoreCase(true)));
    }

    #[test]
    fn parses_fixeol_toggles() {
        assert_eq!(parse_ex("set fixeol"), Ex::SetFixEol(true));
        assert_eq!(parse_ex("set nofixeol"), Ex::SetFixEol(false));
        // Vim's long spelling resolves the same way.
        assert_eq!(parse_ex("set fixendofline"), Ex::SetFixEol(true));
        assert_eq!(parse_ex("set nofixendofline"), Ex::SetFixEol(false));
    }
}
