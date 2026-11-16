//! The encoding half of the trace line form: [`Command::to_line`], its per-family encoders and the
//! token writers they use. Split from `line_form.rs` (#1494). `motion_token` stays in the parent:
//! `trace.rs` scrapes its body there for the motion fuzz list.

use std::fmt::Write as _;

use super::super::{
    BlockInsertKind, Command, ForcedWise, MarkOp, OpKind, OpenKind, SearchOffset, SearchOp,
    SelectKind, VisualOp, WordCase,
};
use super::motion_token;

impl SearchOffset {
    /// Encode as a single whitespace-free token for the trace line form (so the pattern can stay LAST
    /// and keep its spaces): `none`, `s{+N}`, `e{+N}`, `l{+N}` (the sign is always emitted).
    #[must_use]
    fn to_token(self) -> String {
        match self {
            SearchOffset::None => "none".into(),
            SearchOffset::Start(n) => format!("s{n:+}"),
            SearchOffset::End(n) => format!("e{n:+}"),
            SearchOffset::Line(n) => format!("l{n:+}"),
        }
    }
}

fn op_kind_token(op: OpKind) -> &'static str {
    match op {
        OpKind::Delete => "delete",
        OpKind::Change => "change",
        OpKind::Yank => "yank",
    }
}

/// The trace-codec token for a [`MarkOp`] — a single whitespace-free token so the `op_to_mark` line's
/// `<op> <name> <wise>` split still holds. Case ops carry their [`WordCase`] as `case:<word-case>`.
fn mark_op_token(op: MarkOp) -> String {
    match op {
        MarkOp::Delete => "delete".into(),
        MarkOp::Change => "change".into(),
        MarkOp::Yank => "yank".into(),
        MarkOp::Case(c) => format!("case:{}", word_case_token(c)),
        MarkOp::Shift { left: true } => "shift_left".into(),
        MarkOp::Shift { left: false } => "shift_right".into(),
        MarkOp::Reindent => "reindent".into(),
        MarkOp::Format { keep: false } => "format".into(),
        MarkOp::Format { keep: true } => "format_keep".into(),
        MarkOp::Comment => "comment".into(),
    }
}

/// The trace-codec argument of a [`Command::VisualOp`] line (`visual_op <arg>`).
fn visual_op_token(op: VisualOp) -> String {
    match op {
        VisualOp::Lines(k) => format!("lines {}", op_kind_token(k)),
        VisualOp::BlockToEol(k) => format!("block_eol {}", op_kind_token(k)),
        VisualOp::Shift { left, count } => {
            format!("shift {} {count}", if left { "left" } else { "right" })
        }
        VisualOp::Join { spaces: true } => "join".into(),
        VisualOp::Join { spaces: false } => "join_raw".into(),
        VisualOp::Reindent => "reindent".into(),
        VisualOp::Format => "format".into(),
        VisualOp::Comment => "comment".into(),
        VisualOp::OtherCorner => "other_corner".into(),
        VisualOp::YankTrim => "yank_trim".into(),
        VisualOp::LeaveAtStart => "leave_at_start".into(),
    }
}

fn forced_wise_token(w: ForcedWise) -> &'static str {
    match w {
        ForcedWise::Charwise => "charwise",
        ForcedWise::Linewise => "linewise",
        ForcedWise::Blockwise => "blockwise",
    }
}

/// The trace-codec token for a [`SearchOp`]: `move`, or the same single token [`mark_op_token`] writes for
/// the operator (`delete`, `case:upcase`, `shift_left`, `format_keep`, ...).
fn search_op_token(op: SearchOp) -> String {
    match op {
        SearchOp::Move => "move".into(),
        SearchOp::Delete => "delete".into(),
        SearchOp::Change => "change".into(),
        SearchOp::Yank => "yank".into(),
        other => mark_op_token(other.span_op().expect("every other SearchOp is a span op")),
    }
}

/// The trace-codec token for a [`WordCase`].
fn word_case_token(case: WordCase) -> &'static str {
    match case {
        WordCase::Upcase => "upcase",
        WordCase::Downcase => "downcase",
        WordCase::Capitalize => "capitalize",
        WordCase::Toggle => "toggle",
        WordCase::Rot13 => "rot13",
    }
}

/// Lowercase hex of `text`'s UTF-8 bytes, for a trace argument that may hold any character.
fn hex_encode(text: &str) -> String {
    hex_lower(text.as_bytes())
}

/// Two lowercase hex digits per byte, the trace's encoding of free text (an empty input is empty). The
/// `Command` and `ExCmd` codecs both build on it (#1446).
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Command {
    /// The stable single-line serialization of this command (used by the trace format). A `char` argument
    /// is encoded as its decimal Unicode scalar value so whitespace/newlines never need escaping.
    #[must_use]
    pub fn to_line(&self) -> String {
        // Every variant is named (no `_` arm), so a new `Command` fails to build until it is routed to
        // the family encoder that writes its arm.
        match self {
            Command::MoveLeft
            | Command::MoveRight
            | Command::MoveUp
            | Command::MoveDown
            | Command::MoveLineStart
            | Command::MoveLineEnd
            | Command::EnterInsert
            | Command::EnterInsertAfter
            | Command::EnterNormal
            | Command::EnterTerminal
            | Command::EnterTerminalNormal
            | Command::InsertLineStart
            | Command::InsertColumnZero
            | Command::AppendLineEnd
            | Command::OpenBelow
            | Command::OpenAbove
            | Command::EnterReplace
            | Command::ReplaceType(..)
            | Command::ReplaceBackspace
            | Command::EnterVirtualReplace
            | Command::VirtualReplaceType(..)
            | Command::VirtualReplaceChar(..)
            | Command::InsertChar(..)
            | Command::InsertRegister(..)
            | Command::InsertEval(..)
            | Command::InsertPaste(..)
            | Command::ReplacePaste(..)
            | Command::EmacsPaste(..)
            | Command::InsertDeleteWordBack
            | Command::InsertDeleteToLineStart
            | Command::CompleteWord { .. }
            | Command::InsertIndent
            | Command::InsertDedent
            | Command::InsertDeleteIndent
            | Command::InsertDeleteIndentRestore
            | Command::InsertStartColumnLine { .. }
            | Command::InsertCaretStep { .. }
            | Command::InsertTab
            | Command::InsertNewline
            | Command::OpenLineIndent { .. }
            | Command::InsertCloser { .. } => self.encode_insert(),
            Command::DeleteBack
            | Command::DeleteUnder(..)
            | Command::DeleteForward(..)
            | Command::ReplaceChar(..)
            | Command::ReplaceSelectionChar(..)
            | Command::ToggleCase(..)
            | Command::JoinLines(..)
            | Command::JoinLinesNoSpace(..)
            | Command::IncrementNumber(..)
            | Command::IncrementSelection { .. } => self.encode_simple_edit(),
            Command::GotoLastChange
            | Command::GotoLastChangeLine
            | Command::GotoChangeMarkStart
            | Command::GotoChangeMarkEnd
            | Command::GotoChangeMarkStartLine
            | Command::GotoChangeMarkEndLine
            | Command::GotoVisualMarkStart
            | Command::GotoVisualMarkEnd
            | Command::GotoVisualMarkStartLine
            | Command::GotoVisualMarkEndLine
            | Command::GotoContextMark
            | Command::GotoContextMarkLine
            | Command::GotoOlderChange
            | Command::GotoNewerChange
            | Command::GotoOlderJump
            | Command::GotoNewerJump
            | Command::InsertAtLastInsert
            | Command::AsciiInfo
            | Command::Utf8Info
            | Command::FileInfo
            | Command::CursorInfo
            | Command::SetNamedMark(..)
            | Command::GotoNamedMark(..)
            | Command::GotoNamedMarkLine(..)
            | Command::OpToMark { .. } => self.encode_mark(),
            Command::BreakUndo
            | Command::ShiftRight(..)
            | Command::ShiftLeft(..)
            | Command::ShiftMotion { .. }
            | Command::Reindent { .. }
            | Command::Comment { .. }
            | Command::Format { .. }
            | Command::SetIndents { .. }
            | Command::SetIndentCols { .. }
            | Command::InsertReindent
            | Command::Move(..)
            | Command::Delete(..)
            | Command::Change(..)
            | Command::CaseMotion { .. }
            | Command::CaseSelection(..)
            | Command::OpForced { .. }
            | Command::Yank(..) => self.encode_operator(),
            Command::Paste { .. }
            | Command::PasteIndent { .. }
            | Command::PasteBlockInner { .. }
            | Command::PasteSelection { .. }
            | Command::SetRegister(..)
            | Command::SetExprRegister(..) => self.encode_paste_register(),
            Command::EnterVisual { .. }
            | Command::EnterSelect { .. }
            | Command::DeleteSelection
            | Command::YankSelection
            | Command::ChangeSelection
            | Command::ReselectVisual
            | Command::SwapSelectionEnds
            | Command::ReplaceSelection(..)
            | Command::VisualOp(..)
            | Command::BlockInsert(..) => self.encode_visual(),
            Command::SearchNext(..)
            | Command::SearchPrev(..)
            | Command::SearchWordUnder { .. }
            | Command::RepeatSubstituteGlobal
            | Command::RepeatSubstituteLine
            | Command::GotoDeclaration { .. }
            | Command::GotoFirstMatch(..)
            | Command::ShowKeywordLines { .. }
            | Command::Search { .. }
            | Command::SearchObject { .. } => self.encode_search(),
            Command::Undo
            | Command::Redo
            | Command::UndoOlder
            | Command::UndoNewer
            | Command::LineUndo
            | Command::Save
            | Command::Quit
            | Command::SetMark
            | Command::KillRegion
            | Command::CopyRegion
            | Command::EmacsYank { .. }
            | Command::EmacsBufferEdge { .. }
            | Command::ExchangePointMark
            | Command::EmacsKillLine
            | Command::EmacsTransposeChars
            | Command::EmacsTransposeWords
            | Command::EmacsKillWord { .. }
            | Command::EmacsBackwardKillWord { .. }
            | Command::EmacsHorizontalSpace { .. }
            | Command::EmacsOpenLine
            | Command::EmacsMarkWord
            | Command::EmacsKillWholeLine
            | Command::EmacsCaseRegion { .. }
            | Command::EmacsDeleteIndentation
            | Command::EmacsCaseWord { .. } => self.encode_session_emacs(),
            Command::Ex(ex) => ex.to_line(),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the cursor steps, mode entries and Insert/Replace-mode
    /// typing.
    fn encode_insert(&self) -> String {
        match self {
            Command::MoveLeft => "move_left".into(),
            Command::MoveRight => "move_right".into(),
            Command::MoveUp => "move_up".into(),
            Command::MoveDown => "move_down".into(),
            Command::MoveLineStart => "move_line_start".into(),
            Command::MoveLineEnd => "move_line_end".into(),
            Command::EnterInsert => "enter_insert".into(),
            Command::EnterInsertAfter => "enter_insert_after".into(),
            Command::EnterNormal => "enter_normal".into(),
            Command::EnterTerminal => "enter_terminal".into(),
            Command::EnterTerminalNormal => "enter_terminal_normal".into(),
            Command::InsertLineStart => "insert_line_start".into(),
            Command::InsertColumnZero => "insert_column_zero".into(),
            Command::AppendLineEnd => "append_line_end".into(),
            Command::OpenBelow => "open_below".into(),
            Command::OpenAbove => "open_above".into(),
            Command::EnterReplace => "enter_replace".into(),
            Command::ReplaceType(c) => format!("replace_type {}", *c as u32),
            Command::ReplaceBackspace => "replace_backspace".into(),
            Command::EnterVirtualReplace => "enter_virtual_replace".into(),
            Command::VirtualReplaceType(c) => format!("virtual_replace_type {}", *c as u32),
            Command::VirtualReplaceChar(n, c) => format!("virtual_replace_char {n} {}", *c as u32),
            Command::InsertChar(c) => {
                let mut s = String::from("insert_char ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
            Command::InsertRegister(c) => format!("insert_register {}", *c as u32),
            // The expression text is the untrimmed remainder (it may contain spaces), like `search_next`.
            Command::InsertEval(e) => format!("insert_eval {e}"),
            // Pasted text may hold any byte (newlines, spaces), so it is hex-encoded to stay one line.
            Command::InsertPaste(t) => format!("insert_paste {}", hex_encode(t)),
            Command::ReplacePaste(t) => format!("replace_paste {}", hex_encode(t)),
            Command::EmacsPaste(t) => format!("emacs_paste {}", hex_encode(t)),
            Command::InsertDeleteWordBack => "insert_delete_word_back".into(),
            Command::InsertDeleteToLineStart => "insert_delete_to_line_start".into(),
            // `back` then the candidate text (the untrimmed remainder, like `insert_eval`; a keyword has no
            // spaces, and an empty text round-trips through the trailing space).
            Command::CompleteWord { back, text } => format!("complete_word {back} {text}"),
            Command::InsertIndent => "insert_indent".into(),
            Command::InsertDedent => "insert_dedent".into(),
            Command::InsertDeleteIndent => "insert_delete_indent".into(),
            Command::InsertDeleteIndentRestore => "insert_delete_indent_restore".into(),
            Command::InsertStartColumnLine { down } => {
                format!(
                    "insert_start_column_line {}",
                    if *down { "down" } else { "up" }
                )
            }
            Command::InsertCaretStep { right } => {
                format!(
                    "insert_caret_step {}",
                    if *right { "right" } else { "left" }
                )
            }
            Command::InsertTab => "insert_tab".into(),
            Command::InsertNewline => "insert_newline".into(),
            Command::OpenLineIndent { kind, level } => {
                let k = match kind {
                    OpenKind::Below => "below",
                    OpenKind::Above => "above",
                    OpenKind::Split => "split",
                };
                format!("open_line_indent {k} {level}")
            }
            Command::InsertCloser { ch } => format!("insert_closer {}", *ch as u32),
            _ => unreachable!("not a insert command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the counted single-character and line edits (`x`, `r`, `~`,
    /// `J`, `CTRL-A`).
    fn encode_simple_edit(&self) -> String {
        match self {
            Command::DeleteBack => "delete_back".into(),
            Command::DeleteUnder(n) => format!("delete_under {n}"),
            Command::DeleteForward(n) => format!("delete_forward {n}"),
            Command::ReplaceChar(n, c) => format!("replace_char {n} {}", *c as u32),
            Command::ReplaceSelectionChar(c) => format!("replace_selection_char {}", *c as u32),
            Command::ToggleCase(n) => format!("toggle_case {n}"),
            Command::JoinLines(n) => format!("join_lines {n}"),
            Command::JoinLinesNoSpace(n) => format!("join_lines_no_space {n}"),
            Command::IncrementNumber(d) => format!("increment_number {d}"),
            Command::IncrementSelection { delta, sequential } => {
                format!("increment_selection {delta} {sequential}")
            }
            _ => unreachable!("not a simple edit command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the mark and jump-list motions, named marks and the info
    /// commands.
    fn encode_mark(&self) -> String {
        match self {
            Command::GotoLastChange => "goto_last_change".into(),
            Command::GotoLastChangeLine => "goto_last_change_line".into(),
            Command::GotoChangeMarkStart => "goto_change_mark_start".into(),
            Command::GotoChangeMarkEnd => "goto_change_mark_end".into(),
            Command::GotoChangeMarkStartLine => "goto_change_mark_start_line".into(),
            Command::GotoChangeMarkEndLine => "goto_change_mark_end_line".into(),
            Command::GotoVisualMarkStart => "goto_visual_mark_start".into(),
            Command::GotoVisualMarkEnd => "goto_visual_mark_end".into(),
            Command::GotoVisualMarkStartLine => "goto_visual_mark_start_line".into(),
            Command::GotoVisualMarkEndLine => "goto_visual_mark_end_line".into(),
            Command::GotoContextMark => "goto_context_mark".into(),
            Command::GotoContextMarkLine => "goto_context_mark_line".into(),
            Command::GotoOlderChange => "goto_older_change".into(),
            Command::GotoNewerChange => "goto_newer_change".into(),
            Command::GotoOlderJump => "goto_older_jump".into(),
            Command::GotoNewerJump => "goto_newer_jump".into(),
            Command::InsertAtLastInsert => "insert_at_last_insert".into(),
            Command::AsciiInfo => "ascii_info".into(),
            Command::Utf8Info => "utf8_info".into(),
            Command::FileInfo => "file_info".into(),
            Command::CursorInfo => "cursor_info".into(),
            Command::SetNamedMark(c) => format!("set_named_mark {}", *c as u32),
            Command::GotoNamedMark(c) => format!("goto_named_mark {}", *c as u32),
            Command::GotoNamedMarkLine(c) => format!("goto_named_mark_line {}", *c as u32),
            Command::OpToMark { op, name, linewise } => format!(
                "op_to_mark {} {} {}",
                mark_op_token(*op),
                *name as u32,
                if *linewise { "linewise" } else { "charwise" }
            ),
            _ => unreachable!("not a mark command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the operator + motion commands, indent and case operators.
    fn encode_operator(&self) -> String {
        match self {
            Command::BreakUndo => "break_undo".into(),
            Command::ShiftRight(n) => format!("shift_right {n}"),
            Command::ShiftLeft(n) => format!("shift_left {n}"),
            Command::ShiftMotion {
                left,
                count,
                motion,
            } => format!(
                "shift_motion {} {count} {}",
                if *left { "left" } else { "right" },
                motion_token(*motion)
            ),
            Command::Reindent { count, motion } => {
                format!("reindent {count} {}", motion_token(*motion))
            }
            Command::Comment { count, motion } => {
                format!("comment {count} {}", motion_token(*motion))
            }
            Command::Format {
                count,
                motion,
                keep_cursor,
            } => format!(
                "format {count} {} {}",
                motion_token(*motion),
                if *keep_cursor { "keep" } else { "move" }
            ),
            Command::SetIndents {
                first_line,
                last_line,
                levels,
            } => {
                let mut s = format!("set_indents {first_line} {last_line}");
                for l in levels {
                    s.push(' ');
                    s.push_str(&l.to_string());
                }
                s
            }
            Command::SetIndentCols {
                first_line,
                last_line,
                cols,
            } => {
                let mut s = format!("set_indent_cols {first_line} {last_line}");
                for c in cols {
                    s.push(' ');
                    match c {
                        Some(c) => s.push_str(&c.to_string()),
                        None => s.push('-'),
                    }
                }
                s
            }
            Command::InsertReindent => "insert_reindent".into(),
            Command::Move(n, m) => format!("move {n} {}", motion_token(*m)),
            Command::Delete(n, m) => format!("delete {n} {}", motion_token(*m)),
            Command::Change(n, m) => format!("change {n} {}", motion_token(*m)),
            Command::CaseMotion {
                count,
                motion,
                case,
            } => format!(
                "case_motion {count} {} {}",
                motion_token(*motion),
                word_case_token(*case)
            ),
            Command::CaseSelection(case) => format!("case_selection {}", word_case_token(*case)),
            Command::OpForced {
                op,
                count,
                motion,
                wise,
            } => format!(
                "op_forced {} {} {count} {}",
                op_kind_token(*op),
                forced_wise_token(*wise),
                motion_token(*motion)
            ),
            Command::Yank(n, m) => format!("yank {n} {}", motion_token(*m)),
            _ => unreachable!("not a operator command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the puts and register selection.
    fn encode_paste_register(&self) -> String {
        match self {
            Command::Paste {
                after,
                count,
                move_after,
            } => match (*after, *move_after) {
                (true, false) => format!("paste_after {count}"),
                (false, false) => format!("paste_before {count}"),
                (true, true) => format!("paste_after_move {count}"),
                (false, true) => format!("paste_before_move {count}"),
            },
            Command::PasteIndent { after, count } => {
                if *after {
                    format!("paste_after_indent {count}")
                } else {
                    format!("paste_before_indent {count}")
                }
            }
            Command::PasteBlockInner { after, count } => {
                if *after {
                    format!("paste_after_inner {count}")
                } else {
                    format!("paste_before_inner {count}")
                }
            }
            Command::PasteSelection { swap, count } => {
                let mode = if *swap { "swap" } else { "keep" };
                if *count == 1 {
                    format!("paste_selection {mode}")
                } else {
                    format!("paste_selection {mode} {count}")
                }
            }
            Command::SetRegister(name) => match name {
                Some(c) => format!("select_register {}", *c as u32),
                None => "select_register".into(),
            },
            // The expression text is the untrimmed remainder (it may contain spaces), like `search_next`.
            Command::SetExprRegister(e) => format!("set_expr_register {e}"),
            _ => unreachable!("not a paste register command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the visual/Select mode entries and selection operators.
    fn encode_visual(&self) -> String {
        match self {
            Command::EnterVisual { kind } => match kind {
                SelectKind::Charwise => "enter_visual".into(),
                SelectKind::Linewise => "enter_visual_line".into(),
                SelectKind::Blockwise => "enter_visual_block".into(),
            },
            Command::EnterSelect { kind } => match kind {
                SelectKind::Charwise => "enter_select".into(),
                SelectKind::Linewise => "enter_select_line".into(),
                SelectKind::Blockwise => "enter_select_block".into(),
            },
            Command::DeleteSelection => "delete_selection".into(),
            Command::YankSelection => "yank_selection".into(),
            Command::ChangeSelection => "change_selection".into(),
            Command::ReselectVisual => "reselect_visual".into(),
            Command::SwapSelectionEnds => "swap_selection_ends".into(),
            Command::ReplaceSelection(c) => {
                let mut s = String::from("replace_selection ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
            Command::VisualOp(op) => format!("visual_op {}", visual_op_token(*op)),
            Command::BlockInsert(kind) => match kind {
                BlockInsertKind::Insert => "block_insert".into(),
                BlockInsertKind::Append => "block_append".into(),
                BlockInsertKind::Change => "block_change".into(),
            },
            _ => unreachable!("not a visual command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the searches, `*`/`#`, substitute repeats and keyword
    /// lookups.
    fn encode_search(&self) -> String {
        match self {
            Command::SearchNext(p) => format!("search_next {p}"),
            Command::SearchPrev(p) => format!("search_prev {p}"),
            // The operator and count trail the direction/scope, so pre-#1071 traces (no op/count) still
            // parse as the bare `Move` with count 1.
            Command::SearchWordUnder {
                forward,
                whole_word,
                op,
                count,
            } => format!(
                "search_word_under {} {} {} {count}",
                if *forward { "fwd" } else { "bwd" },
                if *whole_word { "whole" } else { "any" },
                search_op_token(*op),
            ),
            Command::RepeatSubstituteGlobal => "repeat_substitute_global".into(),
            Command::RepeatSubstituteLine => "repeat_substitute_line".into(),
            Command::GotoDeclaration { global } => {
                format!(
                    "goto_declaration {}",
                    if *global { "global" } else { "local" }
                )
            }
            // Pattern is LAST so it may contain spaces (like search_next).
            Command::GotoFirstMatch(p) => format!("goto_first_match {p}"),
            Command::ShowKeywordLines { above, list, count } => format!(
                "show_keyword_lines {} {} {count}",
                if *above { "above" } else { "below" },
                if *list { "list" } else { "echo" }
            ),
            // Direction + offset before the pattern (pattern is LAST so it may contain spaces, like
            // search_next). The offset token is whitespace-free so it never collides with the pattern.
            Command::Search {
                op,
                count,
                pattern,
                backward,
                offset,
            } => format!(
                "search {} {count} {} {} {pattern}",
                search_op_token(*op),
                if *backward { "bwd" } else { "fwd" },
                offset.to_token(),
            ),
            // Direction before the pattern (pattern is LAST so it may contain spaces).
            Command::SearchObject {
                op,
                count,
                pattern,
                backward,
            } => format!(
                "search_object {} {count} {} {pattern}",
                search_op_token(*op),
                if *backward { "bwd" } else { "fwd" }
            ),
            _ => unreachable!("not a search command"),
        }
    }

    /// The [`to_line`](Self::to_line) encoder for the undo/redo, save/quit and the Emacs region, kill and
    /// case commands.
    fn encode_session_emacs(&self) -> String {
        match self {
            Command::Undo => "undo".into(),
            Command::Redo => "redo".into(),
            Command::UndoOlder => "undo_older".into(),
            Command::UndoNewer => "undo_newer".into(),
            Command::LineUndo => "line_undo".into(),
            Command::Save => "save".into(),
            Command::Quit => "quit".into(),
            Command::SetMark => "set_mark".into(),
            Command::KillRegion => "kill_region".into(),
            Command::CopyRegion => "copy_region".into(),
            Command::EmacsYank { count } => format!("emacs_yank {count}"),
            Command::EmacsBufferEdge { start } => {
                format!("emacs_buffer_edge {}", if *start { "start" } else { "end" })
            }
            Command::ExchangePointMark => "exchange_point_mark".into(),
            Command::EmacsKillLine => "emacs_kill_line".into(),
            Command::EmacsTransposeChars => "emacs_transpose_chars".into(),
            Command::EmacsTransposeWords => "emacs_transpose_words".into(),
            Command::EmacsKillWord { count } => format!("emacs_kill_word {count}"),
            Command::EmacsBackwardKillWord { count } => format!("emacs_backward_kill_word {count}"),
            Command::EmacsHorizontalSpace { keep_one } => {
                format!(
                    "emacs_horizontal_space {}",
                    if *keep_one { "one" } else { "none" }
                )
            }
            Command::EmacsOpenLine => "emacs_open_line".into(),
            Command::EmacsMarkWord => "emacs_mark_word".into(),
            Command::EmacsKillWholeLine => "emacs_kill_whole_line".into(),
            Command::EmacsCaseRegion { case } => {
                format!("emacs_case_region {}", word_case_token(*case))
            }
            Command::EmacsDeleteIndentation => "emacs_delete_indentation".into(),
            Command::EmacsCaseWord { case } => {
                format!("emacs_case_word {}", word_case_token(*case))
            }
            _ => unreachable!("not a session emacs command"),
        }
    }
}
