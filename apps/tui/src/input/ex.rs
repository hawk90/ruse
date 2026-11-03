use super::*;

/// A parsed ex command (the `:` line).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ex {
    /// `:[range]w[rite][!] [++opt] [{file} | >> [{file}] | !{cmd}]`, `:up[date]`, `:wq` and `:x[it]` (#610).
    Write(WriteSpec),
    /// `:sav[eas][!] [++opt] {file}` — write the buffer to `{file}`, which becomes its file.
    SaveAs {
        bang: bool,
        file: String,
        opts: FileOpts,
    },
    /// `:wa[ll][!]` — write every modified buffer; `:wqa[ll]`/`:xa[ll]` (`quit`) then quit the editor.
    WriteAll {
        quit: bool,
    },
    /// `:q`/`:quit` — close the focused window; on the last window quit the editor, refusing with E37 when
    /// the focused buffer has unsaved changes and E162 when a hidden one does.
    Quit,
    /// `:q!`/`:quit!` — as `:q`, but the focused buffer's changes do not block it. A modified hidden buffer
    /// still does (nvim); `:qa!` is the way to abandon every change.
    QuitForce,
    /// `:qa`/`:qall`/`:quitall` (`force` with `!`) — quit the editor whatever the window layout. Without
    /// `!` any modified buffer refuses (E37 for the focused one, E162 for another).
    QuitAll {
        force: bool,
    },
    /// `:cq[uit][!] [N]` — quit at once without writing, exiting with status `N` (default 1).
    CQuit(u8),
    /// `:hide` — close the focused window, keeping its buffer loaded (hidden).
    Hide,
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
    /// `:trust` — trust the current workspace for this session, allowing language servers to start
    /// (INV-TRUST-1, D-058). ruse-specific (no Vim counterpart).
    Trust,
    /// `:registers`/`:reg`/`:display` — view the non-empty registers (F-029). View-only.
    Registers,
    /// `:digraphs`/`:dig` — list the curated digraph table (code + glyph + decimal) in a view-only overlay.
    Digraphs,
    /// `:marks` — view the set marks (a-z, `.`, `^`); Enter jumps to the mark (F-003).
    Marks,
    /// `:[range]ma[rk] {a-zA-Z}` / `:[range]k{a-zA-Z}` — set mark `name` at column 0 of the range's last
    /// line (no range: the cursor's line), as nvim's `ex_mark` does (#1088).
    Mark {
        range: SubRange,
        name: char,
    },
    /// `:jumps` — view the jumplist; Enter jumps to the position (F-003).
    Jumps,
    /// `:changes` — view the change list; Enter jumps to the position (F-003).
    Changes,
    /// `:ascii`/`:as` — print the numeric value of the character under the cursor to the status line (the
    /// ex synonym of Normal-mode `ga`). View-only; no buffer mutation.
    Ascii,
    /// `:[range]d [x]`/`:delete` — delete the range's lines (no range = the current line) into register `x`
    /// (`None` = the unnamed one), like a linewise `dd`. A count after it (`:d 3`) is folded into the range
    /// before parsing (`app::ex_range`).
    Delete(SubRange, Option<char>),
    /// `:[range]y [x]`/`:yank` — yank the range's lines linewise into register `x` (like `yy`).
    Yank(SubRange, Option<char>),
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
        /// `:put!` — above the addressed line instead of below it.
        above: bool,
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
    ///   so only the first match per line is replaced — Vim's normal-mode `&`.
    /// - `:[range]&&` → repeat KEEPING the previous flags (`flags = None`), like `g&` but scoped to the range;
    ///   nvim's normal-mode `&` is `:&&` (`:help &-default`).
    /// - `:[range]s {flags}` (bare `s` followed by ONLY flag chars, e.g. `:s g`, `:sg`) → repeat with the
    ///   GIVEN flags REPLACING the old ones (`flags = Some(parsed)`; empty flag run = default = the bare `:s`).
    ///
    /// No range = the current line. The core keeps no `:s` history, so — like `Command::RepeatSubstituteLine`
    /// — the FRONTEND resolves this against its last-substitute state.
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
    /// `:set (no)tildeop` / `:set (no)top` — `~` as an operator (an input-engine option).
    SetTildeOp(bool),
    /// `:set foldmethod=indent|manual` (`fdm`) — select how folds are formed for the focused buffer (folds
    /// are a frontend concern; slice 3). `manual` keeps the hand-made `zf` model; `indent` derives nested
    /// folds from indentation. Handled in the run loop.
    SetFoldMethod(FoldMethod),
    /// `:set foldlevel=N` (`fdl`) — folds deeper than `N` are closed under `foldmethod=indent` (frontend).
    SetFoldLevel(usize),
    /// `:set (no)foldenable` (`fen`) — master switch: when off, no fold is closed regardless of foldlevel
    /// (Vim's `zn`/`zN`; frontend render state). ON by default.
    SetFoldEnable(bool),
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
    /// `:e[dit][!] [++opt] [+cmd] [{file}]` — open `{file}` into a buffer and focus it (F-007), or, with no
    /// file (or the buffer's own), re-read the focused buffer's file (#610).
    Edit(EditSpec),
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
    /// remaps. `keys` is the VERBATIM payload after the single delimiting space (spaces included), taken
    /// literally like Vim's (`<Esc>` is five keys, #815).
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
    /// `:[range]p[rint]`, `:[range]l[ist]`, `:[range]nu[mber]`, `:[range]#` `[count] [flags]` — show the
    /// range's lines in the message area and move the cursor to the last one (#609).
    Print {
        range: SubRange,
        flags: super::PrintFlags,
    },
    /// `:[line]z[!][+-^.=][count]` — show a window of lines around `line` (#609).
    Z(super::ZSpec),
    /// `:[range]=` — show the range's last line number (no range: the buffer's last line) (#609).
    LineNumber(Option<SubRange>),
    /// `:[range]ret[ab][!] [new_tabstop]` — redo the white space that holds a tab with the current
    /// `'expandtab'` and the new tabstop (`!`: runs of spaces too) (#609).
    Retab {
        range: SubRange,
        bang: bool,
        tabstop: Option<usize>,
    },
    /// `:startinsert[!]` / `:startreplace[!]` / `:startgreplace[!]` — start Insert (or Replace) mode once
    /// the command line is done; `!` appends at the end of the line (#609).
    StartInsert {
        kind: super::StartKind,
        bang: bool,
    },
    /// `:stopi[nsert]` — leave Insert mode once the command is done (#609).
    StopInsert,
    /// `:pw[d]` — show the working directory (#610).
    Pwd,
    /// `:cd [{path}]` / `:chdir` — change the working directory: `None` is the home directory, `-` the
    /// previous one (#610).
    Cd(Option<String>),
    /// `:f[ile] [{name}]` / `:0f[ile]` — show the file information, rename the buffer, or unname it (#610).
    File(super::FileCmd),
    /// `:fin[d][!] [++opt] [+cmd] {file}` / `:sf[ind]` — edit `{file}` found in `'path'` (`.,,`: the current
    /// file's directory, then the working directory); `split` opens it in a new window (#610).
    Find {
        split: bool,
        spec: EditSpec,
    },
    /// `:dr[op] [++opt] [+cmd] {file}` — go to the window that shows `{file}`, or edit it (#610).
    Drop(EditSpec),
    /// A known command whose argument is wrong or missing (`:normal` with no keys, `:m` with no address,
    /// `:set nosuchoption`): the status is nvim's error, and nothing runs.
    Fail(String),
    Unknown(String),
}

/// What a `:w`-family command writes to (`:help :w`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WriteTarget {
    /// `:w [{file}]` — replace `{file}`, or the buffer's own file when `None`.
    File(Option<String>),
    /// `:w >> [{file}]` — append to an existing `{file}` (the buffer's own file when `None`).
    Append(Option<String>),
    /// `:w !{cmd}` — feed the lines to a shell command's stdin and show its output.
    Command(String),
}

/// A parsed `:w`-family command: `:w`, `:up`, `:wq`, `:x` (#610).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WriteSpec {
    /// The lines to write; `None` = the whole buffer.
    pub range: Option<SubRange>,
    /// `!`: overwrite an existing file, write part of a buffer to its own file, and (with `quit`) let the
    /// focused buffer's changes go.
    pub bang: bool,
    pub target: WriteTarget,
    pub opts: FileOpts,
    /// `:up`/`:x`: write only when the buffer is modified.
    pub if_modified: bool,
    /// `:wq`/`:x`: close the window after a successful write.
    pub quit: bool,
}

/// The line ending a `++ff=` option forces (`:help ++ff`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileFormatOpt {
    /// `++ff=unix`: `\n`.
    Unix,
    /// `++ff=dos`: `\r\n`.
    Dos,
}

/// The `++opt` arguments of a file command (`:help ++opt`). Only `++ff` is honoured.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FileOpts {
    pub ff: Option<FileFormatOpt>,
}

/// A parsed `:e[dit][!] [++opt] [+cmd] [{file}]` (#610).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EditSpec {
    /// `!`: re-read the buffer's own file even when it has unsaved changes.
    pub bang: bool,
    /// The file to edit; `None` = the focused buffer's own file.
    pub file: Option<String>,
    /// `+cmd`: the Ex command to run once the file is open (`+` alone is `$`, `+N` is line N).
    pub cmd: Option<String>,
    pub opts: FileOpts,
}

/// How folds are formed for a buffer (`:set foldmethod`). Slice 3 implements `manual` (existing `zf` folds)
/// and `indent` (nested folds derived from indentation); the other Vim methods (`expr`/`syntax`/`marker`/
/// `diff`) are deferred.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldMethod {
    /// `manual` — folds are created by hand (`zf`) and never recomputed. The default.
    Manual,
    /// `indent` — folds are derived from each line's indentation and recomputed on edit.
    Indent,
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
    /// `:g/pat/p` (also `l`, `nu`, `#`, and a bare `:g/pat/`) — show each marked line (#609).
    Print(super::PrintFlags),
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
    /// `e`: no `E486` when the pattern matches nothing (`:help :s_e`).
    pub quiet: bool,
    /// `&` (first flag): start from the previous substitute's flags. `global` then holds whether the `g`
    /// flags given here toggle it (an odd number of them), and `ignore_case` is `None` unless `i`/`I` came.
    pub keep_flags: bool,
    /// `[count]` after the flags: act on `count` lines from the range's last line (`:help :s_c`).
    pub count: Option<usize>,
    /// `p`, `#` or `l`: show the last substituted line afterwards (`:help :s_flags`).
    pub print: Option<super::PrintFlags>,
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
/// `expandtab`/`et` (each with a `no` prefix to turn off), and `shiftwidth`/`sw`, `tabstop`/`ts` and
/// `textwidth`/`tw` `=N`. `tabstop` and `shiftwidth` are distinct options (#601); `sw=0` follows `ts`.
/// Any other option is [`Ex::Fail`]: `E518` for a name nvim does not know either, `E521` for a number
/// option given something else, and a plain notice for an nvim option ruse does not implement. A bare
/// `:set` (no option) is not handled here.
fn parse_set(line: &str) -> Option<Ex> {
    let opt = line
        .strip_prefix("set ")
        .or_else(|| line.strip_prefix("se "))?
        .trim();
    Some(parse_set_option(opt).unwrap_or_else(|| Ex::Fail(set_error(opt))))
}

/// Why `:set {opt}` cannot be honoured (see [`parse_set`]).
fn set_error(opt: &str) -> String {
    use super::nvim_options::{NVIM_BOOLEAN_OPTIONS, NVIM_OPTIONS};
    let token = opt.split_whitespace().next().unwrap_or(opt);
    let name_end = token
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(token.len());
    let name = &token[..name_end];
    let known = |n: &str| NVIM_OPTIONS.split(' ').any(|o| o == n);
    let boolean = |n: &str| NVIM_BOOLEAN_OPTIONS.split(' ').any(|o| o == n);
    let negated = |prefix: &str| name.strip_prefix(prefix).is_some_and(boolean);
    if name.is_empty() || !(known(name) || negated("no") || negated("inv")) {
        // `no`/`inv` before an option that is not a boolean is E474 (`:set nosw`).
        let misused = |prefix: &str| name.strip_prefix(prefix).is_some_and(known);
        if misused("no") || misused("inv") {
            return format!("E474: Invalid argument: {token}");
        }
        return format!("E518: Unknown option: {token}");
    }
    let numeric = matches!(
        name,
        "shiftwidth" | "sw" | "tabstop" | "ts" | "textwidth" | "tw" | "foldlevel" | "fdl"
    );
    if numeric && token[name_end..].starts_with('=') {
        return format!("E521: Number required after =: {token}");
    }
    format!("option not supported: {opt}")
}

/// The options [`parse_set`] honours, or `None`.
fn parse_set_option(opt: &str) -> Option<Ex> {
    // hlsearch/incsearch are frontend render preferences, not core options — return their own Ex variants.
    match opt {
        "hlsearch" | "hls" => return Some(Ex::SetHlSearch(true)),
        "nohlsearch" | "nohls" => return Some(Ex::SetHlSearch(false)),
        "incsearch" | "is" => return Some(Ex::SetIncSearch(true)),
        "noincsearch" | "nois" => return Some(Ex::SetIncSearch(false)),
        "fixeol" | "fixendofline" => return Some(Ex::SetFixEol(true)),
        "nofixeol" | "nofixendofline" => return Some(Ex::SetFixEol(false)),
        "tildeop" | "top" => return Some(Ex::SetTildeOp(true)),
        "notildeop" | "notop" => return Some(Ex::SetTildeOp(false)),
        // Fold options (frontend; slice 3). `foldenable`/`fen` bool + `no` prefix.
        "foldenable" | "fen" => return Some(Ex::SetFoldEnable(true)),
        "nofoldenable" | "nofen" => return Some(Ex::SetFoldEnable(false)),
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
            let k = k.trim();
            let v = v.trim();
            // Fold `=`-valued options (frontend; slice 3) are parsed before the numeric core options.
            match k {
                "foldmethod" | "fdm" => {
                    return match v {
                        "indent" => Some(Ex::SetFoldMethod(FoldMethod::Indent)),
                        "manual" => Some(Ex::SetFoldMethod(FoldMethod::Manual)),
                        // expr/syntax/marker/diff are deferred — unknown value → no-op (Unknown).
                        _ => None,
                    };
                }
                "foldlevel" | "fdl" => return Some(Ex::SetFoldLevel(v.parse().ok()?)),
                _ => {}
            }
            let is = |names: [&str; 2]| names.contains(&k);
            let numeric =
                is(["shiftwidth", "sw"]) || is(["tabstop", "ts"]) || is(["textwidth", "tw"]);
            let n: usize = match v.parse() {
                Ok(n) => n,
                // Too many digits for any number option.
                Err(_) if numeric && !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()) => {
                    return Some(Ex::Fail(format!("E474: Invalid argument: {opt}")));
                }
                Err(_) => return None,
            };
            // nvim's bounds (v0.12.6, #684): a number option holds a C `int`, and `'tabstop'` is 1..=9999
            // (`TABSTOP_MAX`). An unbounded `'shiftwidth'` let `>>` allocate without limit.
            if is(["tabstop", "ts"]) && n == 0 {
                return Some(Ex::Fail(format!("E487: Argument must be positive: {opt}")));
            }
            if (is(["tabstop", "ts"]) && n > 9999) || (numeric && n > i32::MAX as usize) {
                return Some(Ex::Fail(format!("E474: Invalid argument: {opt}")));
            }
            match k {
                "shiftwidth" | "sw" => EditorOption::ShiftWidth(n),
                "tabstop" | "ts" => EditorOption::TabStop(n),
                "textwidth" | "tw" => EditorOption::TextWidth(n),
                _ => return None,
            }
        }
    };
    Some(Ex::Set(ex))
}

/// Parse `:[range]sort[!] [b][i][l][n][o][r][u][x] [/pattern/]` (`sort`/`sor`) the way nvim's `ex_sort`
/// reads its argument (v0.12.6). No range = the WHOLE FILE (Vim). Flags and one `/pattern/` may come in any
/// order, with blanks between; the pattern is delimited by any non-letter and ends as `:s` patterns do
/// ([`ruse_core::pattern::skip_regexp`]), and a `"` starts a comment. Honors `!` reverse, the number formats, `u`
/// unique, `i` case-insensitive, and the pattern (with `r` = sort on the matched text). An empty pattern
/// (`//`) is `Some("")`, for the caller to fill from the last search. `n`/`x`/`o`/`b`/`f` sort on the
/// first decimal, hex, octal, binary or float number, and `l` in locale order. A bad argument is
/// [`Ex::Fail`]: `E475` for an unknown flag or a second pattern, `E474` for two number formats, `E654`
/// for a pattern without its closing delimiter.
fn parse_sort(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let rest = rest
        .strip_prefix("sort")
        .or_else(|| rest.strip_prefix("sor"))?;
    // `:sortn` is not `:sort n`: Vim reads the whole name, which is no command (E492).
    if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let (reverse, rest) = match rest.strip_prefix('!') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let range = if range_str.is_empty() {
        SubRange::WholeFile
    } else {
        parse_sub_range(range_str)?
    };
    let mut opts = ruse_core::SortOptions {
        reverse,
        ..ruse_core::SortOptions::default()
    };
    let mut use_match = false;
    let mut locale = false;
    let mut formats = 0;
    let mut p = rest;
    while let Some(c) = p.chars().next() {
        let after = &p[c.len_utf8()..];
        match c {
            ' ' | '\t' => {}
            'i' => opts.ignore_case = true,
            'l' => locale = true,
            'r' => use_match = true,
            'n' | 'b' | 'o' | 'x' | 'f' => {
                opts.key = match c {
                    'n' => ruse_core::SortKind::Decimal,
                    'b' => ruse_core::SortKind::Binary,
                    'o' => ruse_core::SortKind::Octal,
                    'x' => ruse_core::SortKind::Hex,
                    _ => ruse_core::SortKind::Float,
                };
                formats += 1;
            }
            'u' => opts.unique = true,
            '"' => break,
            c if !c.is_ascii_alphabetic() && opts.pattern.is_none() => {
                let end = ruse_core::pattern::skip_regexp(after, c, true);
                if !after[end..].starts_with(c) {
                    return Some(Ex::Fail(format!(
                        "E654: missing delimiter after search pattern: {after}"
                    )));
                }
                opts.pattern = Some(after[..end].to_string());
                p = &after[end + c.len_utf8()..];
                continue;
            }
            _ => return Some(Ex::Fail(format!("E475: Invalid argument: {p}"))),
        }
        p = after;
    }
    if formats > 1 {
        return Some(Ex::Fail("E474: Invalid argument".into()));
    }
    // A number format decides the order; `l` only changes how text compares.
    if locale && formats == 0 {
        opts.key = ruse_core::SortKind::Locale;
    }
    // `r` only means "sort on the match" when there is a pattern; otherwise it is inert.
    opts.use_match = use_match && opts.pattern.is_some();
    Some(Ex::Sort(range, opts))
}

/// Why the flags and count of a `:s` line are wrong. `Display` is nvim's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubError {
    /// `E488`: text after the flags and count.
    Trailing(String),
    /// `E939`: a zero count.
    ZeroCount,
}

impl std::fmt::Display for SubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubError::Trailing(t) => write!(f, "E488: Trailing characters: {t}"),
            SubError::ZeroCount => f.write_str("E939: Positive count required"),
        }
    }
}

/// Parse a `:[range]s/pat/rep/[flags] [count]` line into a [`SubSpec`], or `None` if it is not a
/// substitute. `gdefault` inverts the meaning of the `g` flag (Vim `'gdefault'`); pass it through so the
/// caller's config toggles it. The flags are read the way nvim's `do_sub` reads them: `&` first keeps the
/// previous flags, `g`, `c` and `e` toggle, `n` counts only, `i`/`I` set the case (the last one wins), `r`
/// is accepted, and `p`/`#`/`l` print the last line; then a count, then only a `"` comment may follow.
///
/// # Errors
/// [`SubError::Trailing`] (`E488`) for any other character after the flags and count,
/// [`SubError::ZeroCount`] (`E939`) for a zero count.
pub(crate) fn parse_substitute(line: &str, gdefault: bool) -> Option<Result<SubSpec, SubError>> {
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
    // The pattern ends where Vim's `skip_regexp` says (a delimiter inside `[...]` is part of it, #803); the
    // replacement runs to the next UNESCAPED delimiter, and the rest is the flags run. Escapes stay as typed
    // (`\/` is a literal `/` to the pattern reader and the replacement expander alike).
    let pend = ruse_core::pattern::skip_regexp(body, delim, true);
    let pattern = body[..pend].to_string();
    let (replacement, flags) = match body[pend..].strip_prefix(delim) {
        Some(after) => match skip_unescaped(after, 0, delim, 1) {
            Some(end) => (&after[..end - delim.len_utf8()], &after[end..]),
            None => (after, ""),
        },
        None => ("", ""),
    };
    let replacement = replacement.to_string();
    let range = parse_sub_range(range_str)?;
    let (keep_flags, mut f) = match flags.strip_prefix('&') {
        Some(r) => (true, r),
        None => (false, flags),
    };
    let mut spec = SubSpec {
        range,
        pattern,
        replacement,
        global: gdefault && !keep_flags,
        ignore_case: None,
        confirm: false,
        count_only: false,
        quiet: false,
        keep_flags,
        count: None,
        print: None,
    };
    while let Some(c) = f.chars().next() {
        match c {
            'g' => spec.global = !spec.global,
            'c' => spec.confirm = !spec.confirm,
            'n' => spec.count_only = true,
            'e' => spec.quiet = !spec.quiet,
            'r' => {}
            'p' => {
                spec.print.get_or_insert_default();
            }
            '#' => spec.print.get_or_insert_default().number = true,
            'l' => spec.print.get_or_insert_default().list = true,
            'i' => spec.ignore_case = Some(true),
            'I' => spec.ignore_case = Some(false),
            _ => break,
        }
        f = &f[1..];
    }
    // `n` reports only, so it never asks.
    if spec.count_only {
        spec.confirm = false;
    }
    let f = f.trim_start();
    let digits = f.find(|c: char| !c.is_ascii_digit()).unwrap_or(f.len());
    let f = if digits > 0 {
        let n: usize = f[..digits].parse().unwrap_or(usize::MAX);
        if n == 0 {
            return Some(Err(SubError::ZeroCount));
        }
        spec.count = Some(n);
        f[digits..].trim_start()
    } else {
        f
    };
    if !f.is_empty() && !f.starts_with('"') {
        return Some(Err(SubError::Trailing(f.to_string())));
    }
    Some(Ok(spec))
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
    // Vim reads `:si` followed by `g`/`l`/`m` as another command's abbreviation (`:sig[n]`, `:sil[ent]`,
    // `:sim[alt]`), never as `:s` with flags (`find_ex_command`), so `:sig` is not a repeat-substitute (#601).
    // Only the unspaced form: `:s ig` is still `:s` with the flags `ig`.
    if rest.starts_with("si") && !rest.starts_with("substitute") && after_verb.starts_with("ig") {
        return None;
    }
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
    // Vim's `skip_regexp`: a delimiter inside `[...]` does not end the pattern (#803).
    let pend = ruse_core::pattern::skip_regexp(body, delim, true);
    let pattern = body[..pend].to_string();
    let cmd_str = body[pend..].strip_prefix(delim).unwrap_or(&body[pend..]);
    let cmd = parse_global_cmd(cmd_str)?;
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

/// Parse the command that follows `:g/pat/`: `normal[!] {keys}` (frontend-executed), `d`/`delete`, or a
/// `s/pat/rep/flags` substitute (both core-executed).
fn parse_global_cmd(cmd: &str) -> Option<GlobalPayload> {
    let resolved = super::ex_cmd::resolve_command_word(cmd);
    let cmd = &*resolved;
    // `normal[!] {keys}` is recognized BEFORE the `.trim()` below so the key payload keeps its internal
    // spaces — only the single delimiting space after the verb is consumed (as `:normal` does).
    if let Some(payload) = parse_global_normal(cmd) {
        return Some(payload);
    }
    if let Some(Ex::Print {
        range: SubRange::CurrentLine,
        flags,
    }) = super::ex_extra::parse(cmd.trim())
    {
        return Some(GlobalPayload::Print(flags));
    }
    match cmd.trim() {
        // A bare `:g/pat/` prints the marked lines (`:help :g`).
        "" => Some(GlobalPayload::Print(super::PrintFlags::default())),
        "d" | "delete" => Some(GlobalPayload::Core(GlobalCmd::Delete)),
        other => {
            let spec = parse_substitute(other, false)?.ok()?;
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
pub(super) fn parse_sub_range(s: &str) -> Option<SubRange> {
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

/// Split a typed `:`-line into the commands its `|` separators chain (`:help :bar`), in order. Each piece
/// has its leading whitespace and `:` stripped. As in Vim:
/// - `:g`/`:v`, `:normal`, and the `!` shell forms (`:!`, `:{range}!`, `:r !`) see the `|` as part of their
///   argument, so the rest of the line is theirs;
/// - `:s/pat/rep/` splits only after its flags: a `|` inside the pattern or replacement is literal;
/// - `\|` never splits. Unlike Vim the backslash is KEPT, so a regex alternation in a `:sort /a\|b/`
///   pattern survives (Vim drops it for commands that do not parse their own `|`).
///
/// A line without a separator comes back as one piece, unchanged.
#[must_use]
pub fn split_bar(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    loop {
        let cmd = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ':');
        let (piece, next) = split_first_command(cmd);
        out.push(piece.to_string());
        match next {
            Some(n) => rest = n,
            None => return out,
        }
    }
}

/// One step of [`split_bar`]: the first command of `cmd` and the text after its separating `|`, if any.
fn split_first_command(cmd: &str) -> (&str, Option<&str>) {
    // Skip the range prefix (line numbers, `.`/`$`/`%`, `,`/`;`, relative `+`/`-` offsets, `'x` marks).
    let bytes = cmd.as_bytes();
    // Command modifiers (`:silent normal x|y`) come before the range; the command after them decides.
    let mut i = cmd.len() - super::ex_cmd::strip_modifiers(cmd).1.len();
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' | b',' | b';' | b'%' | b'*' | b'.' | b'$' | b'+' | b'-' | b' ' | b'\t' => {
                i += 1
            }
            b'\'' | b'\\' if i + 1 < bytes.len() => {
                i += 1;
                i += cmd[i..].chars().next().map_or(0, char::len_utf8);
            }
            // A pattern address (`:/a|b/d`): its `|` is part of the pattern.
            delim @ (b'/' | b'?') => {
                i = skip_unescaped(cmd, i + 1, delim as char, 1).unwrap_or(bytes.len());
            }
            _ => break,
        }
    }
    let after_range = &cmd[i..];
    let verb_len = after_range
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(after_range.len());
    let verb = &after_range[..verb_len];
    let arg = &after_range[verb_len..];
    let whole = || (cmd, None);
    // Commands whose argument runs to the end of the line, `|` included.
    let is_prefix = |full: &str, min: usize| verb.len() >= min && full.starts_with(verb);
    if verb.is_empty() && arg.starts_with('!')
        || is_prefix("global", 1)
        || is_prefix("vglobal", 1)
        || is_prefix("normal", 4)
        || (is_prefix("read", 1) && arg.trim_start().starts_with('!'))
        // `:w !cmd` / `:up !cmd` pipe to a shell command (the `!` after a blank; `:w!` is the bang).
        || ((is_prefix("write", 1) || is_prefix("update", 2))
            && arg.starts_with(char::is_whitespace)
            && arg.trim_start().starts_with('!'))
    {
        return whole();
    }
    // `:s/pat/rep/flags` — scan past the pattern and the replacement first.
    let mut scan_from = i;
    if is_prefix("substitute", 1) {
        if let Some(delim) = arg.chars().next() {
            if delim.is_ascii_punctuation() && !matches!(delim, '\\' | '"' | '|') {
                let body_start = i + verb_len + delim.len_utf8();
                let pend =
                    body_start + ruse_core::pattern::skip_regexp(&cmd[body_start..], delim, true);
                let rep_start = cmd[pend..]
                    .starts_with(delim)
                    .then(|| pend + delim.len_utf8());
                match rep_start.and_then(|r| skip_unescaped(cmd, r, delim, 1)) {
                    Some(end) => scan_from = end,
                    None => return whole(), // an unterminated pattern/replacement owns the `|`
                }
            }
        }
    }
    let mut chars = cmd[scan_from..].char_indices();
    while let Some((j, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next(); // an escaped char (`\|` included) never separates
            }
            '|' => {
                let at = scan_from + j;
                return (&cmd[..at], Some(&cmd[at + 1..]));
            }
            _ => {}
        }
    }
    (cmd, None)
}

/// The byte offset just past the `n`-th unescaped `delim` in `s` from `from` (a `\` escapes the next char),
/// or `None` when fewer than `n` delimiters follow.
fn skip_unescaped(s: &str, from: usize, delim: char, n: usize) -> Option<usize> {
    let mut seen = 0;
    let mut chars = s[from..].char_indices();
    while let Some((j, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == delim {
            seen += 1;
            if seen == n {
                return Some(from + j + c.len_utf8());
            }
        }
    }
    None
}

/// Why a cmdline-special expansion ([`expand_bang`], [`expand_filename`]) failed. `Display` is nvim's
/// message, shown on the status line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpandError {
    /// A `!` with no previous shell command (`E34`).
    NoPreviousCommand,
    /// A `%` (or `#`) that names no file (`E499`).
    EmptyFileName,
    /// A `#` with no alternate file (`E194`).
    NoAlternateFile,
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExpandError::NoPreviousCommand => "E34: No previous command",
            ExpandError::EmptyFileName => {
                "E499: Empty file name for '%' or '#', only works with \":p:h\""
            }
            ExpandError::NoAlternateFile => "E194: No alternate file name to substitute for '#'",
        })
    }
}

/// Expand the `!` in a shell command (`:!`, `:{range}!`, `:r !`): each unescaped `!` becomes the previous
/// shell command (`:help :!`), so `:!!` repeats it; `\!` is a literal `!`. With no previous command a `!`
/// is `E34`.
///
/// # Errors
/// [`ExpandError::NoPreviousCommand`] when `cmd` holds an unescaped `!` and `prev` is `None`.
pub fn expand_bang(cmd: &str, prev: Option<&str>) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'!') => {
                out.push('!');
                chars.next();
            }
            '!' => out.push_str(prev.ok_or(ExpandError::NoPreviousCommand)?),
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// The file names a cmdline-special character stands for ([`expand_filename`]).
#[derive(Clone, Copy, Default, Debug)]
pub struct FileNames<'a> {
    /// `%`: the focused buffer's file.
    pub current: Option<&'a str>,
    /// `#`: the alternate buffer's file.
    pub alternate: Option<&'a str>,
    /// `#{n}`: buffer `n`'s file, for every buffer that has one.
    pub buffers: &'a [(u64, String)],
}

/// Expand the cmdline-special file names in a file argument or shell command (`:help cmdline-special`):
/// `%` is the current file name, `#` the alternate one and `#{n}` buffer `n`'s; `\%` and `\#` are literal.
/// Each may be followed by file-name modifiers (`:help filename-modifiers`), applied left to right: `:p`
/// (full path), `:~` (relative to the home directory), `:.` (relative to the working directory), `:h`
/// (head), `:t` (tail), `:r` (root) and `:e` (extension; `:e:e` takes two). Other modifiers (`:s`, `:gs`,
/// `:S`) are not modeled and stay as text.
///
/// # Errors
/// [`ExpandError::EmptyFileName`] when `%` names no file, [`ExpandError::NoAlternateFile`] when there is no
/// alternate file for `#` or no buffer `n` with a file for `#{n}`.
pub fn expand_filename(arg: &str, names: &FileNames<'_>) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        let name = match c {
            '\\' if rest.starts_with(['%', '#']) => {
                out.push_str(&rest[..1]);
                rest = &rest[1..];
                continue;
            }
            '%' => names.current.ok_or(ExpandError::EmptyFileName)?.to_string(),
            '#' => {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    names
                        .alternate
                        .ok_or(ExpandError::NoAlternateFile)?
                        .to_string()
                } else {
                    let n: Option<u64> = rest[..digits].parse().ok();
                    rest = &rest[digits..];
                    names
                        .buffers
                        .iter()
                        .find(|(id, _)| Some(*id) == n)
                        .map(|(_, name)| name.clone())
                        .ok_or(ExpandError::NoAlternateFile)?
                }
            }
            c => {
                out.push(c);
                continue;
            }
        };
        let (name, after) = apply_filename_modifiers(name, rest);
        out.push_str(&name);
        rest = after;
    }
    Ok(out)
}

/// Apply the file-name modifiers at the start of `mods` to `name`; returns the result and the text after
/// the modifiers.
fn apply_filename_modifiers(mut name: String, mut mods: &str) -> (String, &str) {
    // `:e` after `:e` widens the extension by one (`k.tar.gz` -> `gz` -> `tar.gz`): the name before the
    // first `:e`, and how many extensions to keep.
    let mut ext_of: Option<(String, usize)> = None;
    while let Some(m) = mods.strip_prefix(':').and_then(|r| r.chars().next()) {
        if !"p~.htre".contains(m) {
            break;
        }
        mods = &mods[2..];
        if m != 'e' {
            ext_of = None;
        }
        name = match m {
            'p' => full_path(&name),
            '~' => home_relative(&full_path(&name)).unwrap_or(name),
            '.' => cwd_relative(&full_path(&name)).unwrap_or(name),
            'h' => head(&name),
            't' => name.rsplit('/').next().unwrap_or("").to_string(),
            'r' => {
                let (root, _) = split_extension(&name);
                root.to_string()
            }
            _ => {
                let (base, n) = match ext_of.take() {
                    Some((base, n)) => (base, n + 1),
                    None => (name.clone(), 1),
                };
                let mut root = base.as_str();
                for _ in 0..n {
                    root = split_extension(root).0;
                }
                let ext = base
                    .get(root.len()..)
                    .and_then(|e| e.strip_prefix('.'))
                    .unwrap_or("")
                    .to_string();
                ext_of = Some((base, n));
                ext
            }
        };
    }
    (name, mods)
}

/// `:p`: the absolute path (a directory gets a trailing `/`, as in Vim).
fn full_path(name: &str) -> String {
    let path = std::path::Path::new(name);
    let abs =
        std::path::absolute(path).map_or_else(|_| name.to_string(), |p| p.display().to_string());
    let abs = abs.replace("/./", "/");
    if path.is_dir() && !abs.ends_with('/') {
        format!("{abs}/")
    } else {
        abs
    }
}

/// `:~`: `abs` with the home directory written `~`, when it is under it.
fn home_relative(abs: &str) -> Option<String> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let rest = abs.strip_prefix(home.trim_end_matches('/'))?;
    (rest.is_empty() || rest.starts_with('/')).then(|| format!("~{rest}"))
}

/// `:.`: `abs` relative to the working directory, when it is under it.
fn cwd_relative(abs: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.to_str()?.trim_end_matches('/');
    let rest = abs.strip_prefix(cwd)?.strip_prefix('/')?;
    Some(if rest.is_empty() {
        ".".into()
    } else {
        rest.into()
    })
}

/// `:h`: the name without its last component (`.` when it has no directory part); a trailing `/` alone
/// is removed first.
fn head(name: &str) -> String {
    if let Some(dir) = name.strip_suffix('/').filter(|d| !d.is_empty()) {
        return dir.to_string();
    }
    match name.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => name[..i].to_string(),
        None => ".".into(),
    }
}

/// Split the last extension off `name`: `("dir/k.tar", "gz")` for `dir/k.tar.gz`. A dot that starts the
/// tail (`.bashrc`) is not an extension.
fn split_extension(name: &str) -> (&str, &str) {
    let tail_at = name.rfind('/').map_or(0, |i| i + 1);
    match name[tail_at..].rfind('.') {
        Some(i) if i > 0 => (&name[..tail_at + i], &name[tail_at + i + 1..]),
        _ => (name, ""),
    }
}

/// nvim's error for a command that needs an argument (`:normal`, `:sign`).
const ARGUMENT_REQUIRED: &str = "E471: Argument required";
/// nvim's error for a missing or out-of-range address (`:m` with no destination).
const INVALID_RANGE: &str = "E16: Invalid range";

/// Parse the text typed after `:` (without the leading colon).
#[must_use]
pub fn parse_ex(line: &str) -> Ex {
    // Any accepted abbreviation of the command word becomes the one spelling the parsers below match
    // (`:wr` → `:w`, `:del` → `:delete`, #609).
    let resolved = super::ex_cmd::resolve_command_word(line);
    let line = &*resolved;
    // `:[range]normal[!] {keys}` is parsed FIRST, on the RAW line: everything after the one delimiting space
    // is the verbatim key payload, so a trailing space in the keys must survive the `line.trim()` below.
    if let Some(ex) = parse_normal(line) {
        return ex;
    }
    let line = line.trim();
    match line {
        "q" | "qu" | "qui" | "quit" => Ex::Quit,
        "q!" | "qu!" | "qui!" | "quit!" => Ex::QuitForce,
        "qa" | "qal" | "qall" | "quita" | "quital" | "quitall" => Ex::QuitAll { force: false },
        "qa!" | "qal!" | "qall!" | "quita!" | "quital!" | "quitall!" => Ex::QuitAll { force: true },
        "hid" | "hide" => Ex::Hide,
        "split" | "sp" => Ex::Split,
        "vsplit" | "vsp" | "vs" => Ex::VSplit,
        // `!` forces past a buffer that would be abandoned; buffers here always stay loaded (hidden), so
        // the bang forms behave the same as the plain ones (as in nvim, whose 'hidden' is on by default).
        "close" | "clos" | "clo" | "close!" | "clos!" | "clo!" => Ex::Close,
        "only" | "onl" | "on" | "only!" | "onl!" | "on!" => Ex::Only,
        "terminal" | "term" => Ex::Terminal,
        "fmt" | "format" => Ex::Format,
        "references" | "refs" | "ref" => Ex::References,
        // Not `:ca`: nvim reads it as `:cabbrev` (#609).
        "codeaction" | "codeactions" => Ex::CodeAction,
        "diagnostics" | "diags" | "diag" => Ex::Diagnostics,
        "trust" => Ex::Trust,
        "registers" | "reg" | "display" | "di" => Ex::Registers,
        "digraphs" | "digraph" | "dig" => Ex::Digraphs,
        "marks" => Ex::Marks,
        "jumps" => Ex::Jumps,
        "changes" => Ex::Changes,
        "ascii" | "as" => Ex::Ascii,
        "noh" | "nohl" | "nohlsearch" => Ex::NoHighlight,
        "checkhealth" | "checkhealt" | "checkheal" | "che" => Ex::CheckHealth,
        "enew" | "ene" => Ex::Enew,
        "ls" | "buffers" | "files" => Ex::Buffers,
        "bnext" | "bn" => Ex::BufferNext,
        "bprevious" | "bprev" | "bp" => Ex::BufferPrev,
        "bd" | "bdelete" => Ex::BufferDelete { force: false },
        "bd!" | "bdelete!" => Ex::BufferDelete { force: true },
        "b#" | "buffer#" => Ex::Buffer(BufTarget::Alternate),
        // `:sign` with no argument (nvim reads `:sig` as `:sign`, not as `:s` with the `ig` flags); ruse
        // has no signs, so every other `:sign` line stays unknown.
        "sig" | "sign" => Ex::Fail(ARGUMENT_REQUIRED.into()),
        // `:lmap`/`:lunmap` (F-027), then `:b {n}`, `:trace save`, `:[range]s///`, `:[range]g//` — each
        // returns `None`/falls through to the next so an unrecognised line lands on `Ex::Unknown`.
        _ => {
            if let Some(ex) = parse_lmap(line) {
                ex
            } else if let Some(ex) = parse_cquit(line) {
                ex
            } else if let Some(ex) = parse_buffer(line) {
                ex
            } else if let Some(ex) = parse_file_command(line) {
                ex
            } else if let Some(name) = parse_rename(line) {
                Ex::Rename(name)
            } else if let Some(ex) = parse_read(line) {
                ex
            } else if let Some(ex) = parse_bang(line) {
                ex
            } else if let Some(ex) = super::ex_extra::parse(line) {
                ex
            } else if let Some(rest) = line.strip_prefix("trace save") {
                Ex::SaveTrace(rest.trim().to_string())
            } else if let Some(ex) = parse_mark(line) {
                ex
            } else if let Some((range, reg)) =
                parse_register_verb(line, &["d", "de", "del", "dele", "delet", "delete"])
            {
                Ex::Delete(range, reg)
            } else if let Some((range, reg)) =
                parse_register_verb(line, &["y", "ya", "yan", "yank"])
            {
                Ex::Yank(range, reg)
            } else if let Some((range, bang)) = parse_join(line) {
                Ex::Join { range, bang }
            } else if let Some(dest) = parse_range_verb_dest(line, &["move", "m"]) {
                dest.map_or_else(|| Ex::Fail(INVALID_RANGE.into()), |(r, d)| Ex::Move(r, d))
            } else if let Some(dest) = parse_range_verb_dest(line, &["copy", "co", "t"]) {
                dest.map_or_else(|| Ex::Fail(INVALID_RANGE.into()), |(r, d)| Ex::Copy(r, d))
            } else if let Some((range, left, levels)) = parse_shift(line) {
                Ex::Shift {
                    range,
                    left,
                    levels,
                }
            } else if let Some((addr, reg, above)) = parse_put(line) {
                Ex::Put { addr, reg, above }
            } else if let Some(ex) = parse_sort(line) {
                ex
            } else if let Some(ex) = parse_set(line) {
                ex
            } else if let Some(ex) = parse_time_travel(line) {
                ex
            } else if let Some(spec) = parse_substitute(line, false) {
                // `:[range]s/pat/rep/flags` — `'gdefault'` defaults off (Vim factory; config seam deferred).
                spec.map_or_else(|e| Ex::Fail(e.to_string()), Ex::Substitute)
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
    // No keys at all is E471, as in nvim (`:normal`, `:norm! `).
    if rest.trim().is_empty() {
        return Some(Ex::Fail(ARGUMENT_REQUIRED.into()));
    }
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

/// Parse `:cq[uit][!] [N]` into the exit status (`N`, default 1). `None` unless the line is a `:cquit` with
/// a status that fits a process exit code.
fn parse_cquit(line: &str) -> Option<Ex> {
    let rest = ["cquit", "cqui", "cqu", "cq"]
        .into_iter()
        .find_map(|v| line.strip_prefix(v))?;
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Ex::CQuit(1));
    }
    let status = rest.strip_prefix(char::is_whitespace)?.trim();
    Some(Ex::CQuit(status.parse().ok()?))
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
/// (after the range prefix) must be one of `verbs` (longest-first). A missing destination is `Some(None)`,
/// which nvim reports as E16 (`:m`, `:t`).
fn parse_range_verb_dest(line: &str, verbs: &[&str]) -> Option<Option<(SubRange, LineAddr)>> {
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
fn parse_line_addr(s: &str) -> Option<LineAddr> {
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

/// Parse `:[line]put[!] [reg]` into `(addr, reg, above)`. After the optional `[0-9.$]` address prefix the verb is
/// `put`/`pu` (longest first). A register argument, when present, is a SINGLE non-whitespace char that must
/// be separated from the verb by whitespace (`:put a` — not `:puta`, which Vim rejects as E492). No address
/// prefix means the bare `:put` default (after the current line → `LineAddr::Current`). Returns `None`
/// (→ falls through) unless the line is a `:put`.
fn parse_put(line: &str) -> Option<(LineAddr, Option<char>, bool)> {
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
    Some((addr, reg, above))
}

/// Parse `:[range]ma[rk] {x}` and `:[range]k[ ]{x}` with `{x}` a letter (nvim's `ex_mark`, #1088): no
/// argument is `E471`, more than one char `E488`. A numbered mark (which ruse does not keep) is not a
/// `:mark` here and falls through to `Unknown`, as do `:keepjumps` and the other `:k…` commands.
fn parse_mark(line: &str) -> Option<Ex> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let spelled = ["mark", "mar", "ma"].into_iter().find_map(|v| {
        rest.strip_prefix(v)
            .filter(|a| a.is_empty() || a.starts_with(' '))
    });
    let arg = spelled.or_else(|| rest.strip_prefix('k'))?;
    // `:kfoo` is another command; `:k foo` and `:mark foo` are a `:mark` with trailing characters.
    let spaced = spelled.is_some() || arg.starts_with(' ');
    let arg = arg.trim();
    let mut chars = arg.chars();
    let Some(name) = chars.next() else {
        return Some(Ex::Fail(ARGUMENT_REQUIRED.into()));
    };
    if chars.next().is_some() {
        return spaced.then(|| Ex::Fail(format!("E488: Trailing characters: {arg}")));
    }
    if !name.is_ascii_alphabetic() {
        return None;
    }
    let range = parse_sub_range(range_str)?;
    Some(Ex::Mark { range, name })
}

/// Parse `:[range]{verb} [x]` (`:d`, `:y`) into the range and the register `x`, a single char after
/// whitespace (`:d a`, `:3y A`). A count after the register was already folded into the range.
fn parse_register_verb(line: &str, verbs: &[&str]) -> Option<(SubRange, Option<char>)> {
    let split = line
        .find(|c: char| !matches!(c, '0'..='9' | ',' | '%' | '.' | '$'))
        .unwrap_or(line.len());
    let (range_str, rest) = line.split_at(split);
    let rest = rest.trim_start();
    let verb_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let (verb, arg) = rest.split_at(verb_len);
    if !verbs.contains(&verb) {
        return None;
    }
    let arg = arg.trim();
    let reg = if arg.is_empty() {
        None
    } else {
        Some(single_char(arg)?)
    };
    Some((parse_sub_range(range_str)?, reg))
}

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

/// nvim's error for an argument it cannot use (`++ff=mac`, an unknown `++opt`).
const INVALID_ARGUMENT: &str = "E474: Invalid argument";

/// Read the leading `++opt` arguments of a file command (`:help ++opt`): `++ff=unix|dos` (or
/// `++fileformat=`) is honoured; any other `++opt`, and `++ff=mac` (ruse has no `\r` line ending), is an
/// [`Ex::Fail`]. Returns the options and the text after them.
fn parse_file_opts(mut rest: &str) -> Result<(FileOpts, &str), Ex> {
    let mut opts = FileOpts::default();
    while let Some(opt) = rest.strip_prefix("++") {
        let end = opt.find(char::is_whitespace).unwrap_or(opt.len());
        let ff = opt[..end]
            .strip_prefix("ff=")
            .or_else(|| opt[..end].strip_prefix("fileformat="));
        opts.ff = Some(match ff {
            Some("unix") => FileFormatOpt::Unix,
            Some("dos") => FileFormatOpt::Dos,
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
fn parse_file_command(line: &str) -> Option<Ex> {
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
fn parse_read(line: &str) -> Option<Ex> {
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
    if n == 0 {
        // Buffer numbers start at 1 (nvim v0.12.6, #870).
        return Some(Ex::Fail("E939: Positive count required".into()));
    }
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

/// An empty pattern with no last search to borrow ([`reuse_last_search`]). `Display` is the status message
/// Vim shows: `E476` for `:s` and `:g` (`do_sub` and `ex_global` report it after `E35`), `E35` for `:sort`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoPreviousPattern {
    /// `:s//repl/`.
    Substitute,
    /// `:g//` / `:v//`.
    Global,
    /// `:sort //`.
    Sort,
}

impl std::fmt::Display for NoPreviousPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NoPreviousPattern::Substitute => {
                "E476: Invalid command: E35: No previous regular expression"
            }
            NoPreviousPattern::Global => "E476: Invalid command",
            NoPreviousPattern::Sort => "E35: No previous regular expression",
        })
    }
}

/// Fill an empty pattern from the last search pattern, as Vim does for every Ex command that takes one
/// (#804): `:s//repl/`, `:g//cmd` / `:v//cmd`, `:sort //`, and the `:s//` inside `:g/pat/s//repl/`, which
/// gets the `:g` pattern (Vim's `:g` makes its pattern the last search before it runs the command). A
/// no-op for a non-empty pattern or another command.
///
/// # Errors
/// The status message when an empty pattern has nothing to borrow: `E476` for `:s` and `:g` (Vim's `ex_global`
/// reports it after `E35`), `E35` for `:sort`.
pub(crate) fn reuse_last_search(
    ex: &mut Ex,
    last_search: Option<&str>,
) -> Result<(), NoPreviousPattern> {
    match ex {
        Ex::Substitute(spec) => {
            if spec.pattern.is_empty() {
                spec.pattern = last_search
                    .ok_or(NoPreviousPattern::Substitute)?
                    .to_string();
            }
        }
        Ex::Global(spec) => {
            if spec.pattern.is_empty() {
                spec.pattern = last_search.ok_or(NoPreviousPattern::Global)?.to_string();
            }
            if let GlobalPayload::Core(GlobalCmd::Substitute { pattern, .. }) = &mut spec.cmd {
                if pattern.is_empty() {
                    pattern.clone_from(&spec.pattern);
                }
            }
        }
        Ex::Sort(_, opts) if opts.pattern.as_deref() == Some("") => {
            opts.pattern = Some(last_search.ok_or(NoPreviousPattern::Sort)?.to_string());
        }
        _ => {}
    }
    Ok(())
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
            quiet: false,
            keep_flags: false,
            count: None,
            print: None,
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
        reuse_last_search(&mut ex, Some("foo")).unwrap();
        assert_eq!(pattern_of(&ex), "foo");
    }

    #[test]
    fn non_empty_pattern_is_left_alone() {
        let mut ex = sub("bar");
        reuse_last_search(&mut ex, Some("foo")).unwrap();
        assert_eq!(
            pattern_of(&ex),
            "bar",
            "an explicit pattern wins over the last search"
        );
    }

    /// #1087: `:s//y/` before any search is `E35`, then `do_sub`'s `E476` (nvim v0.12.6).
    #[test]
    fn empty_pattern_without_a_last_search_is_e476() {
        let mut ex = sub("");
        assert_eq!(
            reuse_last_search(&mut ex, None).map_err(|e| e.to_string()),
            Err("E476: Invalid command: E35: No previous regular expression".into())
        );
    }

    /// #804: `:g//` takes the last search, and the `s//` inside `:g` takes the `:g` pattern.
    #[test]
    fn global_and_its_substitute_borrow_too() {
        let mut ex = parse_ex("g//s//X/");
        reuse_last_search(&mut ex, Some("a")).unwrap();
        let Ex::Global(spec) = &ex else {
            panic!("not a :g: {ex:?}")
        };
        assert_eq!(spec.pattern, "a");
        assert!(matches!(
            &spec.cmd,
            GlobalPayload::Core(GlobalCmd::Substitute { pattern, .. }) if pattern == "a"
        ));
        let mut ex = parse_ex("g/b/s//X/");
        reuse_last_search(&mut ex, Some("a")).unwrap();
        assert!(matches!(
            &ex,
            Ex::Global(GlobalSpec { cmd: GlobalPayload::Core(GlobalCmd::Substitute { pattern, .. }), .. })
                if pattern == "b"
        ));
        let mut ex = parse_ex("g//d");
        assert_eq!(
            reuse_last_search(&mut ex, None).map_err(|e| e.to_string()),
            Err("E476: Invalid command".to_string())
        );
    }

    #[test]
    fn sort_borrows_or_reports_e35() {
        let mut ex = parse_ex("sort //");
        reuse_last_search(&mut ex, Some("x")).unwrap();
        assert!(matches!(&ex, Ex::Sort(_, o) if o.pattern.as_deref() == Some("x")));
        let mut ex = parse_ex("sort //");
        assert_eq!(
            reuse_last_search(&mut ex, None).map_err(|e| e.to_string()),
            Err("E35: No previous regular expression".to_string())
        );
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
        // The payload is stored VERBATIM (and run literally, #815).
        assert_eq!(
            normal("normal Ihi<Esc>"),
            (false, None, "Ihi<Esc>".to_string())
        );
    }

    #[test]
    fn non_normal_and_bare_normal_fall_through() {
        // `:normalx` is not the `:normal` verb.
        assert!(matches!(parse_ex("normalx"), Ex::Unknown(_)));
        // A bare `:normal` (no keys) is E471, as in nvim.
        let e471 = Ex::Fail("E471: Argument required".into());
        assert_eq!(parse_ex("normal"), e471);
        assert_eq!(parse_ex("normal!"), e471);
        assert_eq!(parse_ex("norm "), e471);
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
        // The `!` may follow the verb directly (#809).
        for line in ["r!echo hi", "read!echo hi", "r! echo hi"] {
            assert_eq!(
                parse_ex(line),
                Ex::Read {
                    addr: LineAddr::Current,
                    source: ReadSource::Command("echo hi".into())
                },
                "{line}"
            );
        }
        assert_eq!(
            parse_ex("0r!echo hi"),
            Ex::Read {
                addr: LineAddr::Line(0),
                source: ReadSource::Command("echo hi".into())
            }
        );
    }

    /// `:d`/`:y` take a register after the verb, and every abbreviation of the verb (#608).
    #[test]
    fn delete_and_yank_take_a_register() {
        assert_eq!(
            parse_ex("d a"),
            Ex::Delete(SubRange::CurrentLine, Some('a'))
        );
        assert_eq!(
            parse_ex("2,3del _"),
            Ex::Delete(SubRange::Lines(2, 3), Some('_'))
        );
        assert_eq!(parse_ex("de"), Ex::Delete(SubRange::CurrentLine, None));
        assert_eq!(
            parse_ex("yan A"),
            Ex::Yank(SubRange::CurrentLine, Some('A'))
        );
        assert!(matches!(parse_ex("d ab"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("dx"), Ex::Unknown(_)));
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

    #[test]
    fn parses_fold_options() {
        // foldmethod (fdm): only manual/indent are honored; the deferred methods are Unknown.
        assert_eq!(
            parse_ex("set foldmethod=indent"),
            Ex::SetFoldMethod(FoldMethod::Indent)
        );
        assert_eq!(
            parse_ex("set fdm=manual"),
            Ex::SetFoldMethod(FoldMethod::Manual)
        );
        // nvim's other fold methods are known options ruse does not implement: a notice, not an error.
        assert_eq!(
            parse_ex("set foldmethod=expr"),
            Ex::Fail("option not supported: foldmethod=expr".into())
        );
        assert!(matches!(parse_ex("set fdm=marker"), Ex::Fail(_)));
        // foldlevel (fdl) = N.
        assert_eq!(parse_ex("set foldlevel=2"), Ex::SetFoldLevel(2));
        assert_eq!(parse_ex("set fdl=0"), Ex::SetFoldLevel(0));
        // foldenable (fen) bool + `no` prefix.
        assert_eq!(parse_ex("set foldenable"), Ex::SetFoldEnable(true));
        assert_eq!(parse_ex("set nofoldenable"), Ex::SetFoldEnable(false));
        assert_eq!(parse_ex("set fen"), Ex::SetFoldEnable(true));
        assert_eq!(parse_ex("set nofen"), Ex::SetFoldEnable(false));
    }
}

/// #601: `|` chaining, `:sig`, and the `!` / `%` / `#` expansions. Behaviour probed against nvim v0.12.6.
#[cfg(test)]
mod bar_and_expansion_tests {
    use super::*;

    fn split(line: &str) -> Vec<String> {
        split_bar(line)
    }

    #[test]
    fn bar_separates_commands() {
        assert_eq!(split("s/a/b/|d"), ["s/a/b/", "d"]);
        assert_eq!(split("s/a/b/g|d"), ["s/a/b/g", "d"]);
        assert_eq!(split("s/a/b/ | d"), ["s/a/b/ ", "d"]);
        assert_eq!(
            split("set ts=8|set sw=4|:set noet"),
            ["set ts=8", "set sw=4", "set noet"]
        );
        assert_eq!(split("%s/a/b/|d"), ["%s/a/b/", "d"]);
        assert_eq!(split("w"), ["w"]);
    }

    #[test]
    fn substitute_keeps_a_bar_in_its_pattern_and_replacement() {
        assert_eq!(split("s/a|b/X/"), ["s/a|b/X/"]);
        assert_eq!(split("s/a/X|Y/"), ["s/a/X|Y/"]);
        assert_eq!(split("s/a/X|Y/|d"), ["s/a/X|Y/", "d"]);
        // An unterminated replacement runs to the end of the line, `|` included.
        assert_eq!(split("s/a/X|Y"), ["s/a/X|Y"]);
        // A regex alternation `\|` is never a separator, and keeps its backslash.
        assert_eq!(split(r"s/a\|b/X/|d"), [r"s/a\|b/X/", "d"]);
        assert_eq!(split(r"sort /a\|b/"), [r"sort /a\|b/"]);
    }

    #[test]
    fn global_normal_and_shell_own_the_rest_of_the_line() {
        assert_eq!(split("g/a/d|d"), ["g/a/d|d"]);
        assert_eq!(split("v/a/s/x/y/|d"), ["v/a/s/x/y/|d"]);
        assert_eq!(split("normal ix|y"), ["normal ix|y"]);
        assert_eq!(split("!ls | wc -l"), ["!ls | wc -l"]);
        assert_eq!(split("%!sort | uniq"), ["%!sort | uniq"]);
        assert_eq!(split("r !ls | wc"), ["r !ls | wc"]);
        // `:r {file}` is an ordinary command.
        assert_eq!(split("r f.txt|d"), ["r f.txt", "d"]);
    }

    #[test]
    fn sig_is_not_a_repeat_substitute() {
        // nvim reads `:sig` as `:sign` (E471), `:sil`/`:sim` as `:silent`/`:simalt`.
        assert_eq!(parse_ex("sig"), Ex::Fail("E471: Argument required".into()));
        // The other `:s` + flag shorthands still repeat.
        for line in ["si", "sg", "sgi", "sI", "s ig"] {
            assert!(
                matches!(parse_ex(line), Ex::RepeatSubstitute { .. }),
                "{line:?}"
            );
        }
    }

    #[test]
    fn bang_expands_to_the_previous_command() {
        assert_eq!(expand_bang("!", Some("ls -l")), Ok("ls -l".into()));
        assert_eq!(expand_bang("! | wc", Some("ls")), Ok("ls | wc".into()));
        assert_eq!(expand_bang(r"echo a\!b", None), Ok("echo a!b".into()));
        assert_eq!(expand_bang("echo hi", None), Ok("echo hi".into()));
        assert_eq!(expand_bang("!", None), Err(ExpandError::NoPreviousCommand));
        // `:!!` parses as a shell run of `!`, which the session expands.
        assert_eq!(parse_ex("!!"), Ex::Shell("!".into()));
    }

    #[test]
    fn percent_and_hash_expand_to_the_file_names() {
        let buffers = [(1, "src/a.rs".to_string()), (3, "dir/k.tar.gz".to_string())];
        let names = FileNames {
            current: Some("src/a.rs"),
            alternate: Some("b.txt"),
            buffers: &buffers,
        };
        let x = |arg: &str| expand_filename(arg, &names);
        assert_eq!(x("%"), Ok("src/a.rs".into()));
        assert_eq!(x("#"), Ok("b.txt".into()));
        assert_eq!(x("cat % #"), Ok("cat src/a.rs b.txt".into()));
        assert_eq!(x(r"a\%b\#"), Ok("a%b#".into()));
        // `#{n}` is buffer n's file; a number with no such buffer is E194 (nvim v0.12.6).
        assert_eq!(x("#3"), Ok("dir/k.tar.gz".into()));
        assert_eq!(x("#2"), Err(ExpandError::NoAlternateFile));
        let no_cur = FileNames {
            current: None,
            ..names
        };
        assert_eq!(
            expand_filename("%", &no_cur),
            Err(ExpandError::EmptyFileName)
        );
        let no_alt = FileNames {
            alternate: None,
            ..names
        };
        assert_eq!(
            expand_filename("#", &no_alt),
            Err(ExpandError::NoAlternateFile)
        );
        assert!(ExpandError::EmptyFileName.to_string().starts_with("E499:"));
        assert!(ExpandError::NoAlternateFile
            .to_string()
            .starts_with("E194:"));
        assert_eq!(
            ExpandError::NoPreviousCommand.to_string(),
            "E34: No previous command"
        );
    }
}

#[cfg(test)]
mod file_command_tests {
    use super::*;

    fn write(line: &str) -> WriteSpec {
        match parse_ex(line) {
            Ex::Write(spec) => spec,
            other => panic!(":{line} is not a write: {other:?}"),
        }
    }

    /// #610: the `:w` family's forms, as `:help :w` lists them.
    #[test]
    fn write_forms_parse() {
        let w = write("w");
        assert_eq!(w.target, WriteTarget::File(None));
        assert!(!w.bang && !w.quit && !w.if_modified && w.range.is_none());
        assert_eq!(
            write("write! a.txt").target,
            WriteTarget::File(Some("a.txt".into()))
        );
        assert!(write("w! a.txt").bang);
        assert_eq!(
            write("w >> a.txt").target,
            WriteTarget::Append(Some("a.txt".into()))
        );
        assert_eq!(write("w>>").target, WriteTarget::Append(None));
        assert_eq!(
            write("w !wc -l").target,
            WriteTarget::Command("wc -l".into())
        );
        assert_eq!(write("2,3w b.txt").range, Some(SubRange::Lines(2, 3)));
        assert_eq!(write("%w").range, Some(SubRange::WholeFile));
        assert_eq!(
            write(r"w my\ file").target,
            WriteTarget::File(Some("my file".into()))
        );
        assert_eq!(write("w ++ff=dos d.txt").opts.ff, Some(FileFormatOpt::Dos));
        let up = write("up");
        assert!(up.if_modified && !up.quit);
        let wq = write("wq! x.txt");
        assert!(wq.quit && wq.bang && !wq.if_modified);
        for x in ["x", "xi", "xit", "exi", "exit"] {
            let spec = write(x);
            assert!(spec.quit && spec.if_modified, ":{x}");
        }
        assert_eq!(
            parse_ex("w a b"),
            Ex::Fail("E172: Only one file name allowed".into())
        );
        assert_eq!(
            parse_ex("w ++ff=mac a"),
            Ex::Fail("E474: Invalid argument".into())
        );
        // Other `w…` commands are not writes.
        assert!(matches!(parse_ex("wfoo"), Ex::Unknown(_)));
        assert!(matches!(parse_ex("wn"), Ex::Unknown(_)));
    }

    #[test]
    fn saveas_wall_and_edit_parse() {
        assert_eq!(
            parse_ex("sav! n.txt"),
            Ex::SaveAs {
                bang: true,
                file: "n.txt".into(),
                opts: FileOpts::default()
            }
        );
        assert_eq!(
            parse_ex("saveas"),
            Ex::Fail("E471: Argument required".into())
        );
        assert_eq!(parse_ex("wa"), Ex::WriteAll { quit: false });
        assert_eq!(parse_ex("wall!"), Ex::WriteAll { quit: false });
        assert_eq!(parse_ex("wqa"), Ex::WriteAll { quit: true });
        assert_eq!(parse_ex("xall"), Ex::WriteAll { quit: true });
        assert_eq!(parse_ex("2wa"), Ex::Fail("E481: No range allowed".into()));
        assert_eq!(
            parse_ex(r"e! +/a\ b ++ff=unix f.txt"),
            Ex::Edit(EditSpec {
                bang: true,
                file: Some("f.txt".into()),
                cmd: Some("/a b".into()),
                opts: FileOpts {
                    ff: Some(FileFormatOpt::Unix)
                },
            })
        );
        let Ex::Edit(plus) = parse_ex("e + f.txt") else {
            panic!("not an edit")
        };
        assert_eq!(plus.cmd.as_deref(), Some(""));
        assert_eq!(
            split_bar("w !a | b"),
            ["w !a | b"],
            "`:w !cmd` owns the bar"
        );
        assert_eq!(split_bar("w! a|q"), ["w! a", "q"]);
    }

    /// #610: file-name modifiers, checked against nvim v0.12.6's `expand()`.
    #[test]
    fn filename_modifiers() {
        let names = FileNames {
            current: Some("sub/k.tar.gz"),
            alternate: Some(".bashrc"),
            buffers: &[],
        };
        let x = |arg: &str| expand_filename(arg, &names).unwrap();
        assert_eq!(x("%:h"), "sub");
        assert_eq!(x("%:t"), "k.tar.gz");
        assert_eq!(x("%:r"), "sub/k.tar");
        assert_eq!(x("%:e"), "gz");
        assert_eq!(x("%:t:r:r"), "k");
        assert_eq!(x("%:e:e"), "tar.gz");
        assert_eq!(x("%:r.bak"), "sub/k.tar.bak");
        assert_eq!(x("#:r"), ".bashrc", "a leading dot is not an extension");
        assert_eq!(x("#:h"), ".");
        assert_eq!(x("%:x"), "sub/k.tar.gz:x", "an unknown modifier stays text");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(x("%:p"), cwd.join("sub/k.tar.gz").display().to_string());
        assert_eq!(x("%:p:h:h"), cwd.display().to_string());
        assert_eq!(x("%:p:."), "sub/k.tar.gz");
    }
}
