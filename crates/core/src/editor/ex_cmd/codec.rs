//! The trace codec for [`ExCmd`]: one trace line per command and back.

use crate::cells::VarStops;
use std::fmt::Write as _;

use super::{ExCmd, GroupKind, MAX_APPLY_EDITS, MAX_APPLY_EDIT_BYTES, MAX_SHIFT_LEVELS};
use crate::command::CommandParseError;
use crate::editor::{
    CaretGravity, EditorOption, GlobalCmd, SortKind, SortOptions, SubFlags, MAX_OPTION_INT,
    MAX_TABSTOP,
};
use crate::pos::BytePos;
use crate::register::{RegKind, Register, RegisterStore, MAX_REGISTER_BYTES};
use crate::transaction::TransactionOrigin;

// ---- the trace codec -------------------------------------------------------------------------------------
//
// One line per command, `ex_<verb>` then space-separated tokens. Text and bytes are lowercase hex (`-` for
// empty), a register is its decimal scalar value (`-` for the unnamed one), a flag set is its letters (`-`
// for none). Every token is required, so a truncated line is an error rather than a different command.

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    crate::command::hex_lower(bytes)
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
        EditorOption::VarTabStop(v) => ("vartabstop", hex(v.as_bytes())),
        EditorOption::VarSoftTabStop(v) => ("varsofttabstop", hex(v.as_bytes())),
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
        EditorOption::Lisp(v) => ("lisp", flag(v)),
        EditorOption::LispWords(v) => ("lispwords", hex(v.as_bytes())),
        EditorOption::CIndent(v) => ("cindent", flag(v)),
        EditorOption::CinOptions(v) => ("cinoptions", hex(v.as_bytes())),
        EditorOption::CinKeys(v) => ("cinkeys", hex(v.as_bytes())),
        EditorOption::CinScopeDecls(v) => ("cinscopedecls", hex(v.as_bytes())),
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
        TransactionOrigin::Reload => "reload",
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
        "lispwords" => EditorOption::LispWords,
        "cinoptions" => EditorOption::CinOptions,
        "vartabstop" => EditorOption::VarTabStop,
        "varsofttabstop" => EditorOption::VarSoftTabStop,
        "cinkeys" => EditorOption::CinKeys,
        "cinscopedecls" => EditorOption::CinScopeDecls,
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
                tabstop.map_or_else(|| "-".into(), |v| super::super::stops_text(&v))
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
            ExCmd::Mark {
                line,
                name,
                nonblank,
            } => {
                let at = if *nonblank { " ^" } else { "" };
                format!("ex_mark {line} {}{at}", *name as u32)
            }
            ExCmd::Set(opt) => {
                let (name, value) = option_name(opt);
                format!("ex_set {name} {value}")
            }
            ExCmd::Place(at) => format!("ex_place {}", at.0),
            ExCmd::Want(col) if *col == super::super::range::MAXCOL => "ex_want $".into(),
            ExCmd::Want(col) => format!("ex_want {col}"),
            ExCmd::UndoSync => "ex_undo_sync".into(),
            ExCmd::Edits { origin, edits } => {
                let mut s = format!("ex_edits {}", origin_token(*origin));
                for (start, end, text) in edits {
                    let _ = write!(s, " {}:{}:{}", start.0, end.0, hex(text.as_bytes()));
                }
                s
            }
            ExCmd::KeepJumps(on) => format!("ex_keepjumps {}", bool_token(*on)),
            ExCmd::Modifiable(on) => format!("ex_modifiable {}", bool_token(*on)),
            ExCmd::Modified(on) => format!("ex_modified {}", bool_token(*on)),
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
        // The families' verbs are disjoint, so trying them in turn finds the one arm a single `match`
        // would. The decoders sit in this order in the file too: `trace.rs` scrapes the `"verb" =>` arms
        // of this file for its fuzz verb list, so every new verb arm belongs in one of them, here.
        let mut found = None;
        for decode in [
            decode_line_edit,
            decode_text_replace,
            decode_mark_option,
            decode_state,
        ] {
            if let Some(ex) = decode(verb, &mut a)? {
                found = Some(ex);
                break;
            }
        }
        let Some(ex) = found else {
            return Err(CommandParseError::UnknownVerb(verb.to_string()));
        };
        if a.toks.next().is_some() {
            return Err(a.bad());
        }
        Ok(ex)
    }
}

/// The [`ExCmd::from_parts`] decoder for the line edits on a range: `:s`, `:g`, `:d`, `:y`, `:j`,
/// `:>`/`:<`, `:m`, `:t` and `:put`; `Ok(None)` for another family's verb.
fn decode_line_edit(verb: &str, a: &mut Args<'_>) -> Result<Option<ExCmd>, CommandParseError> {
    Ok(Some(match verb {
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
        _ => return Ok(None),
    }))
}

/// The [`ExCmd::from_parts`] decoder for the range rewrites with resolved text: `:sort`, `:retab`,
/// `:read` and the filter; `Ok(None)` for another family's verb.
fn decode_text_replace(verb: &str, a: &mut Args<'_>) -> Result<Option<ExCmd>, CommandParseError> {
    Ok(Some(match verb {
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
            // One number (older traces) or a comma list of stops (#1163).
            let tabstop = match a.tok()? {
                "-" => None,
                t => Some(
                    VarStops::parse(t)
                        .filter(|v| !v.is_empty())
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
        _ => return Ok(None),
    }))
}

/// The [`ExCmd::from_parts`] decoder for `:mark` and `:set`; `Ok(None)` for another family's verb.
fn decode_mark_option(verb: &str, a: &mut Args<'_>) -> Result<Option<ExCmd>, CommandParseError> {
    Ok(Some(match verb {
        "ex_mark" => {
            let line = a.num()?;
            let name = a.tok()?.parse::<u32>().ok().and_then(char::from_u32);
            let nonblank = match a.toks.next() {
                None => false,
                Some("^") => true,
                Some(_) => return Err(a.bad()),
            };
            ExCmd::Mark {
                line,
                name: name.ok_or_else(|| a.bad())?,
                nonblank,
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
                    "lisp" => flag(value).map(EditorOption::Lisp),
                    "cindent" => flag(value).map(EditorOption::CIndent),
                    _ => None,
                };
                ExCmd::Set(opt.ok_or_else(|| a.bad())?)
            }
        }
        _ => return Ok(None),
    }))
}

/// The [`ExCmd::from_parts`] decoder for the cursor, edit-batch, buffer-flag, register and undo-group
/// records; `Ok(None)` for another family's verb.
fn decode_state(verb: &str, a: &mut Args<'_>) -> Result<Option<ExCmd>, CommandParseError> {
    Ok(Some(match verb {
        "ex_place" => ExCmd::Place(BytePos(a.num()?)),
        "ex_want" => match a.tok()? {
            "$" => ExCmd::Want(super::super::range::MAXCOL),
            n => ExCmd::Want(n.parse().map_err(|_| a.bad())?),
        },
        "ex_undo_sync" => ExCmd::UndoSync,
        "ex_edits" => {
            let origin = match a.tok()? {
                "user" => TransactionOrigin::UserInput,
                "macro" => TransactionOrigin::Macro,
                "plugin" => TransactionOrigin::Plugin,
                "lsp" => TransactionOrigin::Lsp,
                "ai" => TransactionOrigin::AiAgent,
                "remote" => TransactionOrigin::RemotePeer,
                "reload" => TransactionOrigin::Reload,
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
        "ex_modifiable" => ExCmd::Modifiable(a.flag()?),
        "ex_modified" => ExCmd::Modified(a.flag()?),
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
        _ => return Ok(None),
    }))
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

#[cfg(test)]
mod tests;
