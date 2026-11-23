use super::{GlobalCmd, LineAddr, SubRange};
// The children name these in code as `super::PrintFlags`, `super::ex_cmd::…` and `super::ex_extra::…`.
use super::{ex_cmd, ex_extra, PrintFlags};

mod expand;
mod file;
mod last_search;
mod lines;
mod split;
mod substitute;

pub use expand::*;
use file::*;
pub(crate) use last_search::*;
pub(crate) use lines::*;
pub use split::*;
pub(crate) use substitute::*;

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
    /// (INV-TRUST-1, D-058), and record the content-bound trust that loads its `.ruse/config.toml` (D-062).
    Trust,
    /// `:untrust` (or nvim's `:trust ++remove`) — delete the trust record and take back the session grant.
    Untrust,
    /// `:config-reload` (Helix's name) — read the config layers again and install what changed; it also
    /// leaves safe mode (#1152).
    ConfigReload,
    /// `:inspect config [key]` — the config layers, values with their sources, and diagnostics (#614).
    InspectConfig(Option<String>),
    /// `:registers`/`:reg`/`:display` — view the non-empty registers (F-029). View-only.
    Registers,
    /// `:digraphs`/`:dig` — list the curated digraph table (code + glyph + decimal) in a view-only overlay.
    Digraphs,
    /// `:marks` — view the set marks (a-z, `.`, `^`); Enter jumps to the mark (F-003).
    Marks,
    /// `:[range]ma[rk] {x}` / `:[range]k{x}` — set mark `name` (a letter, a digit, `'`/`` ` ``, `[`/`]` or
    /// `<`/`>`) at the first non-blank of the range's last line (no range: the cursor's line), as nvim's
    /// `ex_mark` does (#1088, #1241).
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
    /// before parsing (`app::ex_range`). The flags are those of `:dl`/`:dp` and their longer spellings
    /// (`:help :dl`, #1241): show the new current line afterwards, as `:l`/`:p` would.
    Delete(SubRange, Option<char>, Option<super::PrintFlags>),
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
    /// one `shiftwidth`). A trailing `[count]` argument (Vim's `:> 5`) was already folded into the range
    /// before parsing (`app::ex_range`), as for `:d`/`:y`/`:j`.
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
    /// `:[line]put[!] ={expr}` (#1342) — put the result of `expr` LINEWISE through the `"=` register, as
    /// [`Ex::Put`] puts a register. `expr` is the rest of the line up to a comment, with `\"` and `\|`
    /// unescaped; an empty one means the last expression (nvim's `expr_line`).
    PutExpr {
        addr: LineAddr,
        expr: String,
        above: bool,
    },
    /// `:set` / `:setlocal` / `:setglobal` and its arguments (#614, `set_cmd.rs`). The session runs it
    /// against the option values it and the core hold.
    SetOptions(super::set_cmd::SetLine),
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
    /// `:checkt[ime]`: look at every file buffer's file on disk again (`[disk changed]`, W11/W12; RFC-0023
    /// §8). No reload is offered.
    CheckTime,
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
        tabstop: Option<Box<ruse_core::VarStops>>,
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
    /// `:[range]fo[ld]` — create a closed manual fold over the range, as `zf` does (#1141).
    Fold(SubRange),
    /// `:[range]foldo[pen][!]` / `:[range]foldc[lose][!]` — open (`open`) or close one level of the folds in
    /// the range, every level with `!` (`recursive`) (#1141).
    FoldOpen {
        range: SubRange,
        open: bool,
        recursive: bool,
    },
    /// `:[range]foldd[oopen] {cmd}` / `:[range]folddoc[losed] {cmd}` — run `{cmd}` on each line of the
    /// range that is not (or, `closed`, is) in a closed fold, as `:g` does on its marked lines (#1141).
    FoldDo {
        range: SubRange,
        closed: bool,
        cmd: String,
    },
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
    /// `++ff=mac`: `\r` (#619).
    Mac,
}

/// The `++opt` arguments of a file command (`:help ++opt`): `++ff` and `++bin` / `++nobin`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FileOpts {
    pub ff: Option<FileFormatOpt>,
    /// `++bin` (`Some(true)`) / `++nobin` (`Some(false)`): read or write with `'binary'` on or off (#619).
    pub bin: Option<bool>,
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

/// The command a `:g/pat/…` runs on each marked line, split by WHERE it executes. A bare `d` and a plain
/// `s///` run entirely in core's two-pass ([`Workspace::global`]); `normal` drives the input engine and any
/// other command runs through the session's Ex dispatch, so both are carried here as frontend payloads and
/// executed by the run loop rather than descending into core.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GlobalPayload {
    /// A core-executed payload — `:g/pat/d` or `:g/pat/s/pat2/rep/flags` — run by [`Workspace::global`].
    Core(GlobalCmd),
    /// Any other command (`:g/pat/d 2`, `:g/pat/m0`, `:g/pat/j|s/a/b/`, a nested `:g`): its VERBATIM text,
    /// `|` included, run once per marked line through the Ex dispatch, as Vim's `global_exe` does (#1245).
    Ex(String),
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
    // So is `:g/pat/normal {keys}`: a trailing space or `<C-v><CR>` in its keys is part of them.
    if let Some(spec) = parse_global(line.trim_start()) {
        if matches!(spec.cmd, GlobalPayload::Normal { .. }) {
            return Ex::Global(spec);
        }
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
        "untrust" | "trust ++remove" => Ex::Untrust,
        "config-reload" => Ex::ConfigReload,
        "inspect config" => Ex::InspectConfig(None),
        l if l.starts_with("inspect config ") => {
            Ex::InspectConfig(Some(l["inspect config ".len()..].trim().to_string()))
        }
        "registers" | "reg" | "display" | "di" => Ex::Registers,
        "digraphs" | "digraph" | "dig" => Ex::Digraphs,
        "marks" => Ex::Marks,
        "jumps" => Ex::Jumps,
        "changes" => Ex::Changes,
        "ascii" | "as" => Ex::Ascii,
        "noh" | "nohl" | "nohlsearch" => Ex::NoHighlight,
        "checkhealth" | "checkhealt" | "checkheal" | "che" => Ex::CheckHealth,
        "checktime" | "checktim" | "checkti" | "checkt" => Ex::CheckTime,
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
            } else if let Some(rest) = line
                .strip_prefix("trace save")
                .filter(|r| r.is_empty() || r.starts_with([' ', '\t']))
            {
                // A whole word, as every command name is (#1234): `:trace savex` is no command.
                Ex::SaveTrace(rest.trim().to_string())
            } else if let Some(ex) = parse_mark(line) {
                ex
            } else if let Some((range, reg, flags)) = parse_register_verb(line, true) {
                Ex::Delete(range, reg, flags)
            } else if let Some((range, reg, _)) = parse_register_verb(line, false) {
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
            } else if let Some(put) = parse_put(line) {
                put
            } else if let Some(ex) = parse_sort(line) {
                ex
            } else if let Some(set) = super::set_cmd::parse_set_command(line) {
                Ex::SetOptions(set)
            } else if let Some(rest) = line.strip_prefix("options") {
                // `:options` (#1151): nvim opens its option window; ruse lists every option it honours
                // with its value, as `:set all` does.
                match rest.trim() {
                    "" => Ex::SetOptions(super::set_cmd::parse_args(
                        super::set_cmd::SetScope::Both,
                        "all",
                    )),
                    t => Ex::Fail(format!("E488: Trailing characters: {t}")),
                }
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

#[cfg(test)]
mod bar_and_expansion_tests;
#[cfg(test)]
mod file_command_tests;
#[cfg(test)]
mod normal_parse_tests;
#[cfg(test)]
mod read_filter_parse_tests;
#[cfg(test)]
mod repeat_substitute_parse_tests;
#[cfg(test)]
mod reuse_last_search_tests;
#[cfg(test)]
mod set_command_tests;
#[cfg(test)]
mod shift_parse_tests;
