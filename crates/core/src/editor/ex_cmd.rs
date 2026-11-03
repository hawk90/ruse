//! Resolved Ex edits (RFC-0020, D-061): the form an Ex command takes on the [`Command`] boundary, so it is
//! recorded in the trace and replays without an Ex parser, cursor context, history or IO.
//!
//! Every field is a value. Ranges are the 1-based inclusive `(first, last)` lines that ran, destinations the
//! resolved insert index, patterns and replacements the literal text used (after the empty pattern became
//! the last search and `&` became the remembered substitute), and the input of `:r` / `:{range}!` is the
//! bytes the frontend read. Each verb runs the same [`EditorState`] method the live command runs, so live
//! and replay share one implementation.
//!
//! [`Command`]: crate::command::Command

use std::fmt::Write as _;

use super::{
    CaretGravity, EditorOption, EditorState, GlobalCmd, LineAddr, SortKind, SortOptions, SubFlags,
    SubOutcome, SubRange, MAX_OPTION_INT, MAX_TABSTOP,
};
use crate::command::CommandParseError;
use crate::pattern::RegexError;
use crate::pos::BytePos;
use crate::register::{RegKind, Register, RegisterStore, MAX_REGISTER_BYTES};
use crate::transaction::TransactionOrigin;

/// The most indent levels one `:>`/`:<` applies (`:>>>` is 3). The Ex parser counts repeated verb chars, so
/// a typed command never comes near it; the bound keeps a hostile trace line from shifting for minutes.
pub const MAX_SHIFT_LEVELS: u32 = 1000;

/// The most edits one [`EditorState::apply_edits`] batch holds, live and in a trace's `ex_edits` record
/// (RFC-0020 R6). A language server's response is one frame of at most 64 MiB, and an LSP `TextEdit` takes
/// more than 64 bytes of it, so no batch a server sends comes near.
pub const MAX_APPLY_EDITS: usize = 1 << 20;

/// The most replacement-text bytes one [`EditorState::apply_edits`] batch holds (RFC-0020 R6): nvim's
/// `INT_MAX` text limit, as for a register ([`crate::register::MAX_REGISTER_BYTES`]).
pub const MAX_APPLY_EDIT_BYTES: usize = i32::MAX as usize;

/// What opened an undo group whose children follow in the trace ([`ExCmd::Begin`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupKind {
    /// `:normal {keys}`: the commands its keys produced, and the cursor placement per line of its range.
    Normal,
    /// `:g/pat/normal {keys}`: the same, per marked line.
    Global,
    /// A macro replay (`@{reg}`, `@:`, `Q`): its keys were not typed, so Vim records one undo step.
    Macro,
}

/// One resolved Ex edit, or a piece of frontend state an edit depends on (RFC-0020 R1–R4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExCmd {
    /// `:s`, `&`, `g&`, `:&`, `:&&`: the lines, literal pattern/replacement and effective flags used.
    /// `answers` is `Some` for `:s///c`: the user's answer per match, in order (missing ones are "no").
    Substitute {
        first: usize,
        last: usize,
        pattern: String,
        replacement: String,
        flags: SubFlags,
        answers: Option<Vec<bool>>,
    },
    /// `:g/pat/d` / `:g/pat/s///`: the 1-based lines the pattern marked, and the command run on them.
    Global { marks: Vec<usize>, cmd: GlobalCmd },
    /// `:d [x]`.
    Delete {
        first: usize,
        last: usize,
        reg: Option<char>,
    },
    /// `:y [x]`.
    Yank {
        first: usize,
        last: usize,
        reg: Option<char>,
    },
    /// `:j[!]`.
    Join {
        first: usize,
        last: usize,
        no_space: bool,
    },
    /// `:>` / `:<`, `levels` times.
    Shift {
        first: usize,
        last: usize,
        left: bool,
        levels: u32,
    },
    /// `:m {dest}`: `dest` is the 0-based insert index ("after line `dest`").
    Move {
        first: usize,
        last: usize,
        dest: usize,
    },
    /// `:t {dest}` / `:co`.
    Copy {
        first: usize,
        last: usize,
        dest: usize,
    },
    /// `:put [x]` / `:put! [x]`: after line `dest` (the insert index).
    Put { dest: usize, reg: Option<char> },
    /// `:sort`.
    Sort {
        first: usize,
        last: usize,
        opts: SortOptions,
    },
    /// `:retab[!] [N]`.
    Retab {
        first: usize,
        last: usize,
        bang: bool,
        tabstop: Option<usize>,
    },
    /// `:r {file}` / `:r !{cmd}`: the bytes read, inserted after line `dest`. `cursor_on_last` is the
    /// command read's landing line (the last inserted one).
    ReadText {
        dest: usize,
        cursor_on_last: bool,
        bytes: Vec<u8>,
    },
    /// `:{range}!{cmd}`: the filter's output that replaced the lines.
    FilterReplace {
        first: usize,
        last: usize,
        bytes: Vec<u8>,
    },
    /// `:[range]mark {x}` / `:k{x}`: named mark `name` at the start of 1-based line `line`.
    Mark { line: usize, name: char },
    /// `:set` of an option the core reads while editing; also the trace's starting option values.
    Set(EditorOption),
    /// The cursor placed at a byte offset by the frontend (`:normal`'s per-line column 0, a `:g` landing
    /// line, a `:s///c` match left under the cursor). Leaves the wanted column alone, as the live placement.
    Place(BytePos),
    /// Vim's `u_sync` before a typed command: the next change starts a new undo step.
    UndoSync,
    /// An edit list that came from outside the core (RFC-0020 R6): a language server's formatting, rename,
    /// code action or completion edits, resolved to the disjoint `(start, end, text)` byte edits the session
    /// passed to [`EditorState::apply_edits`], with their origin. Replay calls the same `apply_edits`.
    Edits {
        origin: TransactionOrigin,
        edits: Vec<(BytePos, BytePos, String)>,
    },
    /// The `:keepjumps` switch (R6): while on, no jumplist or change-list entry is recorded.
    KeepJumps(bool),
    /// A register value the session wrote that no recorded command produced (R6): the read-only
    /// `"/ ": ". "% "#` synced from frontend state, a clipboard read, a macro's raw keys, and the registers
    /// filled when the trace began. Sets exactly that slot ([`RegisterStore::set_slot`]); `None` is the
    /// unnamed register.
    Register { name: Option<char>, value: Register },
    /// The start of a group of child commands that undo as one step.
    Begin(GroupKind),
    /// The end of the innermost open [`ExCmd::Begin`].
    End,
}

/// What an [`ExCmd`] did, for the frontend's status line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExOutcome {
    /// Nothing to report (a `Set`, `Place`, group marker).
    Done,
    /// A line count: deleted, yanked, joined, shifted, put, read, filtered, acted on by `:g`, or removed by
    /// `:sort u`.
    Lines(usize),
    /// `:m`/`:t`: the lines moved or copied, `None` when a move lands inside its own range.
    Relocated(Option<usize>),
    /// `:s`: replacements and lines.
    Substituted(SubOutcome),
    /// `:retab`: whether a line changed.
    Retabbed(bool),
    /// An edit list: whether it applied (`apply_edits` refuses a bad batch whole).
    Applied(bool),
    /// A pattern that does not compile; nothing changed.
    Failed(RegexError),
}

impl EditorState {
    /// Run one resolved Ex edit (RFC-0020). The group markers are inert here: undo grouping across a group
    /// is the caller's ([`crate::trace::Trace::replay`] opens one undo block per outermost group).
    pub fn apply_ex(&mut self, ex: &ExCmd) -> ExOutcome {
        let lines = |first: usize, last: usize| SubRange::Lines(first, last);
        match ex {
            ExCmd::Substitute {
                first,
                last,
                pattern,
                replacement,
                flags,
                answers,
            } => {
                let range = lines(*first, *last);
                let from = self.view.cursor;
                let result = match answers {
                    None => self
                        .substitute(range, pattern, replacement, *flags)
                        .inspect(|out| {
                            // A `:s` that matched is a jump (Vim sets the pcmark, #940).
                            if out.lines > 0 {
                                self.view.push_jump(from);
                            }
                        }),
                    Some(answers) => self
                        .substitute_preview(range, pattern, replacement, *flags)
                        .map(|subs| {
                            // A match is a jump before the first prompt (nvim's `do_sub`, #1119).
                            if !subs.is_empty() {
                                self.view.push_jump(from);
                            }
                            let accepted: Vec<_> = subs
                                .into_iter()
                                .zip(answers.iter().copied().chain(std::iter::repeat(false)))
                                .filter_map(|(s, yes)| yes.then_some(s))
                                .collect();
                            self.apply_substitutions(&accepted)
                        }),
                };
                match result {
                    Ok(out) => ExOutcome::Substituted(out),
                    Err(e) => ExOutcome::Failed(e),
                }
            }
            ExCmd::Global { marks, cmd } => {
                // A recorded mark set is ascending and in the buffer; a hand-edited one is made so.
                let n = super::line_spans(self.doc.bytes()).len();
                let mut marked: Vec<usize> = marks
                    .iter()
                    .filter(|&&m| (1..=n).contains(&m))
                    .map(|m| m - 1)
                    .collect();
                marked.sort_unstable();
                marked.dedup();
                match self.global_run(&marked, cmd) {
                    Ok(n) => ExOutcome::Lines(n),
                    Err(e) => ExOutcome::Failed(e),
                }
            }
            ExCmd::Delete { first, last, reg } => {
                // `:d` is a jump from where the cursor stood (nvim's `ex_operators`, #1119).
                self.view.push_jump(self.view.cursor);
                ExOutcome::Lines(self.delete_lines_into(lines(*first, *last), *reg))
            }
            ExCmd::Yank { first, last, reg } => {
                ExOutcome::Lines(self.yank_lines_into(lines(*first, *last), *reg))
            }
            ExCmd::Join {
                first,
                last,
                no_space,
            } => ExOutcome::Lines(self.join_lines(lines(*first, *last), *no_space)),
            ExCmd::Shift {
                first,
                last,
                left,
                levels,
            } => {
                self.view.push_jump(self.view.cursor);
                let levels = (*levels).min(MAX_SHIFT_LEVELS);
                ExOutcome::Lines(self.shift_lines(lines(*first, *last), *left, levels))
            }
            ExCmd::Move { first, last, dest } => {
                ExOutcome::Relocated(self.move_lines(lines(*first, *last), LineAddr::Line(*dest)))
            }
            ExCmd::Copy { first, last, dest } => {
                ExOutcome::Relocated(self.copy_lines(lines(*first, *last), LineAddr::Line(*dest)))
            }
            ExCmd::Put { dest, reg } => {
                ExOutcome::Lines(self.put_lines(LineAddr::Line(*dest), *reg))
            }
            ExCmd::Sort { first, last, opts } => {
                match self.sort_lines(lines(*first, *last), opts) {
                    Ok(n) => ExOutcome::Lines(n),
                    Err(e) => ExOutcome::Failed(e),
                }
            }
            ExCmd::Retab {
                first,
                last,
                bang,
                tabstop,
            } => {
                let tabstop = tabstop.map(|n| n.min(MAX_TABSTOP));
                ExOutcome::Retabbed(self.retab(lines(*first, *last), *bang, tabstop))
            }
            ExCmd::ReadText {
                dest,
                cursor_on_last,
                bytes,
            } => ExOutcome::Lines(self.read_lines(LineAddr::Line(*dest), bytes, *cursor_on_last)),
            ExCmd::FilterReplace { first, last, bytes } => {
                ExOutcome::Lines(self.filter_lines(lines(*first, *last), bytes))
            }
            ExCmd::Mark { line, name } => {
                let b = self.doc.bytes();
                let n = crate::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
                let at = crate::pos::raw::nth_line_start(b, (*line).clamp(1, n.max(1)) - 1);
                self.view.set_named_mark(*name, at);
                ExOutcome::Done
            }
            ExCmd::Set(opt) => {
                self.set_option(opt.clone());
                ExOutcome::Done
            }
            ExCmd::Place(at) => {
                let b = self.doc.bytes();
                let at = crate::motion::snap(b, at.0.min(b.len()));
                self.view.set_cursor(BytePos(at));
                ExOutcome::Done
            }
            ExCmd::UndoSync => {
                self.doc.sync_undo();
                ExOutcome::Done
            }
            ExCmd::Edits { origin, edits } => ExOutcome::Applied(self.apply_edits(edits, *origin)),
            ExCmd::KeepJumps(on) => {
                self.view.keep_jumps = *on;
                ExOutcome::Done
            }
            ExCmd::Register { name, value } => {
                self.view.registers.set_slot(*name, value.clone());
                ExOutcome::Done
            }
            ExCmd::Begin(_) | ExCmd::End => ExOutcome::Done,
        }
    }
}

// ---- the trace codec -------------------------------------------------------------------------------------
//
// One line per command, `ex_<verb>` then space-separated tokens. Text and bytes are lowercase hex (`-` for
// empty), a register is its decimal scalar value (`-` for the unnamed one), a flag set is its letters (`-`
// for none). Every token is required, so a truncated line is an error rather than a different command.

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn reg_token(reg: Option<char>) -> String {
    reg.map_or_else(|| "-".into(), |c| (c as u32).to_string())
}

fn sub_flags_token(f: SubFlags) -> String {
    let mut s = String::new();
    if f.global {
        s.push('g');
    }
    match f.ignore_case {
        Some(true) => s.push('i'),
        Some(false) => s.push('I'),
        None => {}
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

fn sort_flags_token(o: &SortOptions) -> String {
    let mut s = String::new();
    if o.reverse {
        s.push('!');
    }
    match o.key {
        SortKind::Text => {}
        SortKind::Locale => s.push('l'),
        SortKind::Decimal => s.push('n'),
        SortKind::Hex => s.push('x'),
        SortKind::Octal => s.push('o'),
        SortKind::Binary => s.push('b'),
        SortKind::Float => s.push('f'),
    }
    for (on, c) in [(o.unique, 'u'), (o.ignore_case, 'i'), (o.use_match, 'r')] {
        if on {
            s.push(c);
        }
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

fn bool_token(b: bool) -> &'static str {
    if b {
        "1"
    } else {
        "0"
    }
}

/// An option's trace name and value token (a boolean is `0`/`1`; `'softtabstop'` may be negative; a
/// string is hex, `-` for empty).
fn option_name(opt: &EditorOption) -> (&'static str, String) {
    let flag = |v: bool| bool_token(v).to_string();
    match opt.clone() {
        EditorOption::IgnoreCase(v) => ("ignorecase", flag(v)),
        EditorOption::SmartCase(v) => ("smartcase", flag(v)),
        EditorOption::TabStop(n) => ("tabstop", n.to_string()),
        EditorOption::ShiftWidth(n) => ("shiftwidth", n.to_string()),
        EditorOption::ExpandTab(v) => ("expandtab", flag(v)),
        EditorOption::TextWidth(n) => ("textwidth", n.to_string()),
        EditorOption::SoftTabStop(n) => ("softtabstop", n.to_string()),
        EditorOption::AutoIndent(v) => ("autoindent", flag(v)),
        EditorOption::SmartIndent(v) => ("smartindent", flag(v)),
        EditorOption::CopyIndent(v) => ("copyindent", flag(v)),
        EditorOption::PreserveIndent(v) => ("preserveindent", flag(v)),
        EditorOption::SmartTab(v) => ("smarttab", flag(v)),
        EditorOption::ShiftRound(v) => ("shiftround", flag(v)),
        // `0` rests on a character (Vim), `1` between characters (Emacs).
        EditorOption::CaretGravity(g) => ("caretgravity", flag(g == CaretGravity::BetweenChar)),
        EditorOption::FormatOptions(v) => ("formatoptions", hex(v.as_bytes())),
        EditorOption::Comments(v) => ("comments", hex(v.as_bytes())),
        EditorOption::CommentString(v) => ("commentstring", hex(v.as_bytes())),
        EditorOption::FormatListPat(v) => ("formatlistpat", hex(v.as_bytes())),
        EditorOption::FormatPrg(v) => ("formatprg", hex(v.as_bytes())),
        EditorOption::EqualPrg(v) => ("equalprg", hex(v.as_bytes())),
        EditorOption::CinWords(v) => ("cinwords", hex(v.as_bytes())),
        EditorOption::JoinSpaces(v) => ("joinspaces", flag(v)),
        EditorOption::WrapMargin(n) => ("wrapmargin", n.to_string()),
    }
}

fn origin_token(origin: TransactionOrigin) -> &'static str {
    match origin {
        TransactionOrigin::UserInput => "user",
        TransactionOrigin::Macro => "macro",
        TransactionOrigin::Plugin => "plugin",
        TransactionOrigin::Lsp => "lsp",
        TransactionOrigin::AiAgent => "ai",
        TransactionOrigin::RemotePeer => "remote",
    }
}

/// A register's kind token: `c`, `l`, or `b{width}` (a block's width in cells, which a put pads to).
fn reg_kind_token(r: &Register) -> String {
    match r.kind() {
        RegKind::Charwise => "c".into(),
        RegKind::Linewise => "l".into(),
        RegKind::Blockwise => format!("b{}", r.block_width()),
    }
}

/// The constructor of the string option a trace names `name`, if it is one.
fn string_option(name: &str) -> Option<fn(String) -> EditorOption> {
    Some(match name {
        "formatoptions" => EditorOption::FormatOptions,
        "comments" => EditorOption::Comments,
        "commentstring" => EditorOption::CommentString,
        "formatlistpat" => EditorOption::FormatListPat,
        "formatprg" => EditorOption::FormatPrg,
        "equalprg" => EditorOption::EqualPrg,
        "cinwords" => EditorOption::CinWords,
        _ => return None,
    })
}

fn group_token(kind: GroupKind) -> &'static str {
    match kind {
        GroupKind::Normal => "normal",
        GroupKind::Global => "global",
        GroupKind::Macro => "macro",
    }
}

impl ExCmd {
    /// The trace line of this command (the inverse of [`ExCmd::from_parts`]).
    #[must_use]
    pub fn to_line(&self) -> String {
        match self {
            ExCmd::Substitute {
                first,
                last,
                pattern,
                replacement,
                flags,
                answers,
            } => {
                let mut s = format!(
                    "ex_substitute {first} {last} {} {} {}",
                    sub_flags_token(*flags),
                    hex(pattern.as_bytes()),
                    hex(replacement.as_bytes())
                );
                if let Some(a) = answers {
                    s.push_str(" c:");
                    s.extend(a.iter().map(|&y| if y { 'y' } else { 'n' }));
                }
                s
            }
            ExCmd::Global { marks, cmd } => {
                let marks = if marks.is_empty() {
                    "-".to_string()
                } else {
                    marks
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                };
                match cmd {
                    GlobalCmd::Delete => format!("ex_global d {marks}"),
                    GlobalCmd::Substitute {
                        pattern,
                        replacement,
                        flags,
                    } => format!(
                        "ex_global s {marks} {} {} {}",
                        sub_flags_token(*flags),
                        hex(pattern.as_bytes()),
                        hex(replacement.as_bytes())
                    ),
                }
            }
            ExCmd::Delete { first, last, reg } => {
                format!("ex_delete {first} {last} {}", reg_token(*reg))
            }
            ExCmd::Yank { first, last, reg } => {
                format!("ex_yank {first} {last} {}", reg_token(*reg))
            }
            ExCmd::Join {
                first,
                last,
                no_space,
            } => format!("ex_join {first} {last} {}", bool_token(*no_space)),
            ExCmd::Shift {
                first,
                last,
                left,
                levels,
            } => format!(
                "ex_shift {first} {last} {} {levels}",
                if *left { '<' } else { '>' }
            ),
            ExCmd::Move { first, last, dest } => format!("ex_move {first} {last} {dest}"),
            ExCmd::Copy { first, last, dest } => format!("ex_copy {first} {last} {dest}"),
            ExCmd::Put { dest, reg } => format!("ex_put {dest} {}", reg_token(*reg)),
            ExCmd::Sort { first, last, opts } => {
                let pat = match &opts.pattern {
                    Some(p) => format!("p:{}", hex(p.as_bytes())),
                    None => "-".into(),
                };
                format!("ex_sort {first} {last} {} {pat}", sort_flags_token(opts))
            }
            ExCmd::Retab {
                first,
                last,
                bang,
                tabstop,
            } => format!(
                "ex_retab {first} {last} {} {}",
                bool_token(*bang),
                tabstop.map_or_else(|| "-".into(), |n| n.to_string())
            ),
            ExCmd::ReadText {
                dest,
                cursor_on_last,
                bytes,
            } => format!(
                "ex_read {dest} {} {}",
                bool_token(*cursor_on_last),
                hex(bytes)
            ),
            ExCmd::FilterReplace { first, last, bytes } => {
                format!("ex_filter {first} {last} {}", hex(bytes))
            }
            ExCmd::Mark { line, name } => format!("ex_mark {line} {}", *name as u32),
            ExCmd::Set(opt) => {
                let (name, value) = option_name(opt);
                format!("ex_set {name} {value}")
            }
            ExCmd::Place(at) => format!("ex_place {}", at.0),
            ExCmd::UndoSync => "ex_undo_sync".into(),
            ExCmd::Edits { origin, edits } => {
                let mut s = format!("ex_edits {}", origin_token(*origin));
                for (start, end, text) in edits {
                    let _ = write!(s, " {}:{}:{}", start.0, end.0, hex(text.as_bytes()));
                }
                s
            }
            ExCmd::KeepJumps(on) => format!("ex_keepjumps {}", bool_token(*on)),
            ExCmd::Register { name, value } => format!(
                "ex_register {} {} {}",
                reg_token(*name),
                reg_kind_token(value),
                hex(value.text())
            ),
            ExCmd::Begin(kind) => format!("ex_begin {}", group_token(*kind)),
            ExCmd::End => "ex_end".into(),
        }
    }

    /// Parse the tokens after an `ex_*` verb. An unknown `ex_*` verb is [`CommandParseError::UnknownVerb`].
    pub(crate) fn from_parts(
        verb: &str,
        raw: &str,
        line: &str,
    ) -> Result<ExCmd, CommandParseError> {
        let mut a = Args {
            toks: raw.split_whitespace(),
            line,
        };
        let ex = match verb {
            "ex_substitute" => {
                let (first, last) = (a.num()?, a.num()?);
                let flags = a.sub_flags()?;
                let (pattern, replacement) = (a.text()?, a.text()?);
                let answers = match a.toks.next() {
                    None => None,
                    Some(t) => Some(a.answers(t)?),
                };
                ExCmd::Substitute {
                    first,
                    last,
                    pattern,
                    replacement,
                    flags,
                    answers,
                }
            }
            "ex_global" => {
                let kind = a.tok()?;
                let marks = a.marks()?;
                let cmd = match kind {
                    "d" => GlobalCmd::Delete,
                    "s" => {
                        let flags = a.sub_flags()?;
                        GlobalCmd::Substitute {
                            flags,
                            pattern: a.text()?,
                            replacement: a.text()?,
                        }
                    }
                    _ => return Err(a.bad()),
                };
                ExCmd::Global { marks, cmd }
            }
            "ex_delete" => ExCmd::Delete {
                first: a.num()?,
                last: a.num()?,
                reg: a.reg()?,
            },
            "ex_yank" => ExCmd::Yank {
                first: a.num()?,
                last: a.num()?,
                reg: a.reg()?,
            },
            "ex_join" => ExCmd::Join {
                first: a.num()?,
                last: a.num()?,
                no_space: a.flag()?,
            },
            "ex_shift" => {
                let (first, last) = (a.num()?, a.num()?);
                let left = match a.tok()? {
                    "<" => true,
                    ">" => false,
                    _ => return Err(a.bad()),
                };
                let levels: u32 = a.tok()?.parse().map_err(|_| a.bad())?;
                if levels > MAX_SHIFT_LEVELS {
                    return Err(a.bad());
                }
                ExCmd::Shift {
                    first,
                    last,
                    left,
                    levels,
                }
            }
            "ex_move" => ExCmd::Move {
                first: a.num()?,
                last: a.num()?,
                dest: a.num()?,
            },
            "ex_copy" => ExCmd::Copy {
                first: a.num()?,
                last: a.num()?,
                dest: a.num()?,
            },
            "ex_put" => ExCmd::Put {
                dest: a.num()?,
                reg: a.reg()?,
            },
            "ex_sort" => {
                let (first, last) = (a.num()?, a.num()?);
                let mut opts = a.sort_flags()?;
                opts.pattern = match a.tok()? {
                    "-" => None,
                    t => {
                        let h = t.strip_prefix("p:").ok_or_else(|| a.bad())?;
                        Some(a.decode_text(h)?)
                    }
                };
                ExCmd::Sort { first, last, opts }
            }
            "ex_retab" => {
                let (first, last, bang) = (a.num()?, a.num()?, a.flag()?);
                let tabstop = match a.tok()? {
                    "-" => None,
                    t => Some(
                        t.parse::<usize>()
                            .ok()
                            .filter(|&n| n <= MAX_TABSTOP)
                            .ok_or_else(|| a.bad())?,
                    ),
                };
                ExCmd::Retab {
                    first,
                    last,
                    bang,
                    tabstop,
                }
            }
            "ex_read" => ExCmd::ReadText {
                dest: a.num()?,
                cursor_on_last: a.flag()?,
                bytes: a.bytes()?,
            },
            "ex_filter" => ExCmd::FilterReplace {
                first: a.num()?,
                last: a.num()?,
                bytes: a.bytes()?,
            },
            "ex_mark" => {
                let line = a.num()?;
                let name = a.tok()?.parse::<u32>().ok().and_then(char::from_u32);
                ExCmd::Mark {
                    line,
                    name: name.ok_or_else(|| a.bad())?,
                }
            }
            "ex_set" => {
                let name = a.tok()?;
                if name == "softtabstop" || name == "wrapmargin" {
                    let n: i64 = a.tok()?.parse().map_err(|_| a.bad())?;
                    let ok = (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&n);
                    if !ok {
                        return Err(a.bad());
                    }
                    ExCmd::Set(if name == "wrapmargin" {
                        EditorOption::WrapMargin(n)
                    } else {
                        EditorOption::SoftTabStop(n)
                    })
                } else if let Some(make) = string_option(name) {
                    ExCmd::Set(make(a.text()?))
                } else {
                    let value = a.num()?;
                    let flag = |v: usize| match v {
                        0 => Some(false),
                        1 => Some(true),
                        _ => None,
                    };
                    let opt = match name {
                        "autoindent" => flag(value).map(EditorOption::AutoIndent),
                        "smartindent" => flag(value).map(EditorOption::SmartIndent),
                        "copyindent" => flag(value).map(EditorOption::CopyIndent),
                        "preserveindent" => flag(value).map(EditorOption::PreserveIndent),
                        "smarttab" => flag(value).map(EditorOption::SmartTab),
                        "shiftround" => flag(value).map(EditorOption::ShiftRound),
                        "ignorecase" => flag(value).map(EditorOption::IgnoreCase),
                        "smartcase" => flag(value).map(EditorOption::SmartCase),
                        "expandtab" => flag(value).map(EditorOption::ExpandTab),
                        "tabstop" => (1..=MAX_TABSTOP)
                            .contains(&value)
                            .then_some(EditorOption::TabStop(value)),
                        "shiftwidth" => {
                            (value <= MAX_OPTION_INT).then_some(EditorOption::ShiftWidth(value))
                        }
                        "textwidth" => {
                            (value <= MAX_OPTION_INT).then_some(EditorOption::TextWidth(value))
                        }
                        "caretgravity" => flag(value).map(|between| {
                            EditorOption::CaretGravity(if between {
                                CaretGravity::BetweenChar
                            } else {
                                CaretGravity::OnChar
                            })
                        }),
                        "joinspaces" => flag(value).map(EditorOption::JoinSpaces),
                        _ => None,
                    };
                    ExCmd::Set(opt.ok_or_else(|| a.bad())?)
                }
            }
            "ex_place" => ExCmd::Place(BytePos(a.num()?)),
            "ex_undo_sync" => ExCmd::UndoSync,
            "ex_edits" => {
                let origin = match a.tok()? {
                    "user" => TransactionOrigin::UserInput,
                    "macro" => TransactionOrigin::Macro,
                    "plugin" => TransactionOrigin::Plugin,
                    "lsp" => TransactionOrigin::Lsp,
                    "ai" => TransactionOrigin::AiAgent,
                    "remote" => TransactionOrigin::RemotePeer,
                    _ => return Err(a.bad()),
                };
                // Both limits are checked before a byte is decoded (INV-FAIL-BOUNDED).
                let toks: Vec<&str> = a.toks.by_ref().take(MAX_APPLY_EDITS + 1).collect();
                if toks.len() > MAX_APPLY_EDITS {
                    return Err(a.too_large());
                }
                let text_len = toks
                    .iter()
                    .map(|t| t.rsplit(':').next().map_or(0, |h| h.len() / 2))
                    .fold(0usize, usize::saturating_add);
                if text_len > MAX_APPLY_EDIT_BYTES {
                    return Err(a.too_large());
                }
                let edits = toks
                    .into_iter()
                    .map(|t| {
                        let mut parts = t.splitn(3, ':');
                        let mut pos = || -> Result<BytePos, CommandParseError> {
                            parts
                                .next()
                                .and_then(|p| p.parse().ok())
                                .map(BytePos)
                                .ok_or_else(|| a.bad())
                        };
                        let (start, end) = (pos()?, pos()?);
                        let text = a.decode_text(parts.next().ok_or_else(|| a.bad())?)?;
                        Ok((start, end, text))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ExCmd::Edits { origin, edits }
            }
            "ex_keepjumps" => ExCmd::KeepJumps(a.flag()?),
            "ex_register" => {
                let name = a.reg()?;
                if !RegisterStore::is_slot(name) {
                    return Err(a.bad());
                }
                let kind = a.tok()?;
                let t = a.tok()?;
                if t.len() / 2 > MAX_REGISTER_BYTES {
                    return Err(a.too_large());
                }
                let bytes = a.decode(t)?;
                let value = match kind {
                    "c" => Register::charwise(bytes),
                    // A linewise value ends in a newline; one without would come back changed.
                    "l" if bytes.last() == Some(&b'\n') => Register::linewise(bytes),
                    k => match k.strip_prefix('b').map(str::parse::<usize>) {
                        Some(Ok(width)) if width <= MAX_REGISTER_BYTES => {
                            Register::blockwise_width(bytes, width)
                        }
                        _ => return Err(a.bad()),
                    },
                };
                ExCmd::Register { name, value }
            }
            "ex_begin" => ExCmd::Begin(match a.tok()? {
                "normal" => GroupKind::Normal,
                "global" => GroupKind::Global,
                "macro" => GroupKind::Macro,
                _ => return Err(a.bad()),
            }),
            "ex_end" => ExCmd::End,
            other => return Err(CommandParseError::UnknownVerb(other.to_string())),
        };
        if a.toks.next().is_some() {
            return Err(a.bad());
        }
        Ok(ex)
    }
}

/// The tokens of one `ex_*` line, read in order.
struct Args<'a> {
    toks: std::str::SplitWhitespace<'a>,
    line: &'a str,
}

impl<'a> Args<'a> {
    fn bad(&self) -> CommandParseError {
        CommandParseError::BadArgument(self.line.to_string())
    }

    /// A record past a live limit. Only the verb is kept, not the (large) line.
    fn too_large(&self) -> CommandParseError {
        let verb = self.line.split_whitespace().next().unwrap_or_default();
        CommandParseError::TooLarge(verb.to_string())
    }

    fn tok(&mut self) -> Result<&'a str, CommandParseError> {
        self.toks.next().ok_or_else(|| self.bad())
    }

    fn num(&mut self) -> Result<usize, CommandParseError> {
        self.tok()?.parse().map_err(|_| self.bad())
    }

    fn flag(&mut self) -> Result<bool, CommandParseError> {
        match self.tok()? {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(self.bad()),
        }
    }

    fn reg(&mut self) -> Result<Option<char>, CommandParseError> {
        match self.tok()? {
            "-" => Ok(None),
            t => t
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(Some)
                .ok_or_else(|| self.bad()),
        }
    }

    fn decode(&self, t: &str) -> Result<Vec<u8>, CommandParseError> {
        if t == "-" {
            return Ok(Vec::new());
        }
        let d = t.as_bytes();
        if d.is_empty() || !d.len().is_multiple_of(2) {
            return Err(self.bad());
        }
        d.chunks(2)
            .map(|p| {
                std::str::from_utf8(p)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| self.bad())
            })
            .collect()
    }

    fn decode_text(&self, t: &str) -> Result<String, CommandParseError> {
        String::from_utf8(self.decode(t)?).map_err(|_| self.bad())
    }

    fn bytes(&mut self) -> Result<Vec<u8>, CommandParseError> {
        let t = self.tok()?;
        self.decode(t)
    }

    fn text(&mut self) -> Result<String, CommandParseError> {
        let t = self.tok()?;
        self.decode_text(t)
    }

    fn sub_flags(&mut self) -> Result<SubFlags, CommandParseError> {
        let t = self.tok()?;
        let mut f = SubFlags::default();
        if t == "-" {
            return Ok(f);
        }
        for c in t.chars() {
            match c {
                'g' => f.global = true,
                'i' => f.ignore_case = Some(true),
                'I' => f.ignore_case = Some(false),
                _ => return Err(self.bad()),
            }
        }
        Ok(f)
    }

    fn sort_flags(&mut self) -> Result<SortOptions, CommandParseError> {
        let t = self.tok()?;
        let mut o = SortOptions::default();
        if t == "-" {
            return Ok(o);
        }
        for c in t.chars() {
            match c {
                '!' => o.reverse = true,
                'l' => o.key = SortKind::Locale,
                'n' => o.key = SortKind::Decimal,
                'x' => o.key = SortKind::Hex,
                'o' => o.key = SortKind::Octal,
                'b' => o.key = SortKind::Binary,
                'f' => o.key = SortKind::Float,
                'u' => o.unique = true,
                'i' => o.ignore_case = true,
                'r' => o.use_match = true,
                _ => return Err(self.bad()),
            }
        }
        Ok(o)
    }

    fn marks(&mut self) -> Result<Vec<usize>, CommandParseError> {
        match self.tok()? {
            "-" => Ok(Vec::new()),
            t => t
                .split(',')
                .map(|m| m.parse().map_err(|_| self.bad()))
                .collect(),
        }
    }

    fn answers(&self, t: &str) -> Result<Vec<bool>, CommandParseError> {
        let a = t.strip_prefix("c:").ok_or_else(|| self.bad())?;
        a.chars()
            .map(|c| match c {
                'y' => Ok(true),
                'n' => Ok(false),
                _ => Err(self.bad()),
            })
            .collect()
    }
}

/// One of each [`ExCmd`] shape, for the codec round-trip test (`round_trips_every_variant`).
#[cfg(test)]
pub(crate) fn samples() -> Vec<ExCmd> {
    let sub = |answers| ExCmd::Substitute {
        first: 1,
        last: 12,
        pattern: "a b\\n".into(),
        replacement: String::new(),
        flags: SubFlags {
            global: true,
            ignore_case: Some(false),
        },
        answers,
    };
    vec![
        sub(None),
        sub(Some(Vec::new())),
        sub(Some(vec![true, false, true])),
        ExCmd::Global {
            marks: vec![1, 3, 5],
            cmd: GlobalCmd::Delete,
        },
        ExCmd::Global {
            marks: Vec::new(),
            cmd: GlobalCmd::Substitute {
                pattern: "x".into(),
                replacement: "가".into(),
                flags: SubFlags::default(),
            },
        },
        ExCmd::Delete {
            first: 3,
            last: 3,
            reg: Some('"'),
        },
        ExCmd::Delete {
            first: 1,
            last: 2,
            reg: None,
        },
        ExCmd::Yank {
            first: 1,
            last: 2,
            reg: Some('a'),
        },
        ExCmd::Join {
            first: 1,
            last: 4,
            no_space: true,
        },
        ExCmd::Shift {
            first: 2,
            last: 3,
            left: true,
            levels: 3,
        },
        ExCmd::Shift {
            first: 2,
            last: 3,
            left: false,
            levels: 1,
        },
        ExCmd::Move {
            first: 2,
            last: 3,
            dest: 0,
        },
        ExCmd::Copy {
            first: 1,
            last: 1,
            dest: 9,
        },
        ExCmd::Put {
            dest: 4,
            reg: Some('+'),
        },
        ExCmd::Sort {
            first: 1,
            last: 9,
            opts: SortOptions::default(),
        },
        ExCmd::Sort {
            first: 1,
            last: 9,
            opts: SortOptions {
                reverse: true,
                key: SortKind::Hex,
                unique: true,
                ignore_case: true,
                pattern: Some("\\d\\+ ".into()),
                use_match: true,
            },
        },
        ExCmd::Sort {
            first: 1,
            last: 9,
            opts: SortOptions {
                pattern: Some(String::new()),
                ..SortOptions::default()
            },
        },
        ExCmd::Retab {
            first: 1,
            last: 2,
            bang: true,
            tabstop: Some(8),
        },
        ExCmd::Retab {
            first: 1,
            last: 2,
            bang: false,
            tabstop: None,
        },
        ExCmd::ReadText {
            dest: 7,
            cursor_on_last: true,
            bytes: b"hex\x00\xff\n".to_vec(),
        },
        ExCmd::ReadText {
            dest: 0,
            cursor_on_last: false,
            bytes: Vec::new(),
        },
        ExCmd::FilterReplace {
            first: 1,
            last: 3,
            bytes: b"a\nb\n".to_vec(),
        },
        ExCmd::Mark { line: 2, name: 'a' },
        ExCmd::Set(EditorOption::IgnoreCase(true)),
        ExCmd::Set(EditorOption::SmartCase(false)),
        ExCmd::Set(EditorOption::TabStop(8)),
        ExCmd::Set(EditorOption::ShiftWidth(0)),
        ExCmd::Set(EditorOption::ExpandTab(false)),
        ExCmd::Set(EditorOption::TextWidth(79)),
        ExCmd::Set(EditorOption::SoftTabStop(-1)),
        ExCmd::Set(EditorOption::SoftTabStop(4)),
        ExCmd::Set(EditorOption::AutoIndent(false)),
        ExCmd::Set(EditorOption::SmartIndent(true)),
        ExCmd::Set(EditorOption::CopyIndent(true)),
        ExCmd::Set(EditorOption::PreserveIndent(true)),
        ExCmd::Set(EditorOption::SmartTab(false)),
        ExCmd::Set(EditorOption::ShiftRound(true)),
        ExCmd::Set(EditorOption::CaretGravity(CaretGravity::OnChar)),
        ExCmd::Set(EditorOption::CaretGravity(CaretGravity::BetweenChar)),
        ExCmd::Place(BytePos(17)),
        ExCmd::UndoSync,
        ExCmd::Edits {
            origin: TransactionOrigin::Lsp,
            edits: vec![
                (BytePos(0), BytePos(3), "가 b".into()),
                (BytePos(5), BytePos(5), String::new()),
            ],
        },
        ExCmd::Edits {
            origin: TransactionOrigin::UserInput,
            edits: Vec::new(),
        },
        ExCmd::KeepJumps(true),
        ExCmd::KeepJumps(false),
        ExCmd::Register {
            name: None,
            value: Register::linewise(b"a\n".to_vec()),
        },
        ExCmd::Register {
            name: Some('/'),
            value: Register::charwise(Vec::new()),
        },
        ExCmd::Register {
            name: Some('+'),
            value: Register::charwise(b"x\x00 y".to_vec()),
        },
        ExCmd::Register {
            name: Some('a'),
            value: Register::blockwise_width(b"ab\nc".to_vec(), 4),
        },
        ExCmd::Begin(GroupKind::Normal),
        ExCmd::Begin(GroupKind::Global),
        ExCmd::Begin(GroupKind::Macro),
        ExCmd::End,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    #[test]
    fn truncated_extra_or_out_of_bound_tokens_are_errors() {
        for bad in [
            "ex_delete 1 2",
            "ex_delete 1 2 - extra",
            "ex_substitute 1 2 q 61 62",
            "ex_substitute 1 2 g 6 62",
            "ex_substitute 1 2 g 61 62 yn",
            "ex_global x 1",
            "ex_global d 1,,2",
            "ex_shift 1 1 > 1001",
            "ex_shift 1 1 ^ 1",
            "ex_retab 1 1 0 10000",
            "ex_set tabstop 0",
            "ex_set shiftwidth 2147483648",
            "ex_set ignorecase 2",
            "ex_set caretgravity 2",
            "ex_set wrap 1",
            "ex_sort 1 2 z -",
            "ex_sort 1 2 - 61",
            "ex_mark 1 1114112",
            "ex_begin visual",
            "ex_edits lsp 1:2",
            "ex_edits lsp 1:2:6",
            "ex_edits lsp x:2:-",
            "ex_edits vim 1:2:-",
            "ex_edits lsp 0:0:ff",
            "ex_keepjumps 2",
            "ex_register 65 c 61",
            "ex_register 95 c 61",
            "ex_register - l 61",
            "ex_register - z 61",
            "ex_register - bx 61",
            "ex_register - c",
            "ex_nope",
        ] {
            assert!(Command::from_line(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn a_resolved_substitute_runs_on_its_lines_with_its_answers() {
        let mut st = EditorState::new(b"a a\na a\n".to_vec());
        let all = SubFlags {
            global: true,
            ignore_case: None,
        };
        st.apply_ex(&ExCmd::Substitute {
            first: 2,
            last: 2,
            pattern: "a".into(),
            replacement: "b".into(),
            flags: all,
            answers: None,
        });
        assert_eq!(st.bytes(), b"a a\nb b\n");
        // Confirm answers: only the "yes" matches change; missing answers are "no".
        st.apply_ex(&ExCmd::Substitute {
            first: 1,
            last: 2,
            pattern: "[ab]".into(),
            replacement: "c".into(),
            flags: all,
            answers: Some(vec![false, true, true]),
        });
        assert_eq!(st.bytes(), b"a c\nc b\n");
    }

    #[test]
    fn a_hand_edited_global_mark_set_is_clamped_and_ordered() {
        let mut st = EditorState::new(b"a\nb\nc\n".to_vec());
        let out = st.apply_ex(&ExCmd::Global {
            marks: vec![3, 0, 1, 99, 1],
            cmd: GlobalCmd::Delete,
        });
        assert_eq!(out, ExOutcome::Lines(2));
        assert_eq!(st.bytes(), b"b\n");
    }

    #[test]
    fn read_and_filter_replay_their_recorded_bytes() {
        let mut st = EditorState::new(b"a\nb\n".to_vec());
        st.apply_ex(&ExCmd::ReadText {
            dest: 1,
            cursor_on_last: true,
            bytes: b"x\ny\n".to_vec(),
        });
        assert_eq!(st.bytes(), b"a\nx\ny\nb\n");
        st.apply_ex(&ExCmd::FilterReplace {
            first: 1,
            last: 4,
            bytes: b"z\n".to_vec(),
        });
        assert_eq!(st.bytes(), b"z\n");
    }
}
