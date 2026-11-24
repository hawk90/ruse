//! The per-family decoders [`Command::from_line`](super::super::Command::from_line) tries in turn, one per
//! verb family. Split from `line_form.rs` (#1530). `trace.rs` scrapes the `"verb" =>` arms here (it reads
//! `line_form.rs` and every file under `line_form/`), so a new verb arm belongs in one of these decoders.

use super::super::{BlockInsertKind, Command, OpenKind, SearchOffset, SearchOp, SelectKind};
use super::args::{
    arg_char, arg_count_char, arg_u32, arg_word_case, hex_decode, op_cmd, op_forced_arg,
    set_indent_cols_arg, set_indents_arg,
};
use super::{
    forced_wise_from_token, mark_op_from_token, motion_from_token, search_op_from_token,
    visual_op_from_token, CommandParseError, LineParts,
};

/// The [`Command::from_line`] decoder for the cursor steps, mode entries and Insert/Replace-mode typing;
/// `Ok(None)` for another family's verb.
pub(super) fn decode_insert(parts: LineParts<'_>) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw,
        line,
    } = parts;
    Ok(Some(match verb {
        "move_left" => Command::MoveLeft,
        "move_right" => Command::MoveRight,
        "move_up" => Command::MoveUp,
        "move_down" => Command::MoveDown,
        "move_line_start" => Command::MoveLineStart,
        "move_line_end" => Command::MoveLineEnd,
        "enter_insert" => Command::EnterInsert,
        "enter_insert_after" => Command::EnterInsertAfter,
        "enter_normal" => Command::EnterNormal,
        "enter_terminal" => Command::EnterTerminal,
        "enter_terminal_normal" => Command::EnterTerminalNormal,
        "insert_line_start" => Command::InsertLineStart,
        "insert_column_zero" => Command::InsertColumnZero,
        "append_line_end" => Command::AppendLineEnd,
        "open_below" => Command::OpenBelow,
        "open_above" => Command::OpenAbove,
        "enter_replace" => Command::EnterReplace,
        "replace_type" => Command::ReplaceType(arg_char(arg, line)?),
        "replace_backspace" => Command::ReplaceBackspace,
        "enter_virtual_replace" => Command::EnterVirtualReplace,
        "virtual_replace_type" => Command::VirtualReplaceType(arg_char(arg, line)?),
        "virtual_replace_char" => {
            let (n, c) = arg_count_char(arg, line)?;
            Command::VirtualReplaceChar(n, c)
        }
        "insert_char" => Command::InsertChar(arg_char(arg, line)?),
        "insert_register" => Command::InsertRegister(arg_char(arg, line)?),
        // The expression is the untrimmed remainder (may contain spaces), like `search_next`.
        "insert_eval" => Command::InsertEval(raw.to_string()),
        "insert_paste" => Command::InsertPaste(hex_decode(arg.unwrap_or(""), line)?),
        "replace_paste" => Command::ReplacePaste(hex_decode(arg.unwrap_or(""), line)?),
        "emacs_paste" => Command::EmacsPaste(hex_decode(arg.unwrap_or(""), line)?),
        "insert_delete_word_back" => Command::InsertDeleteWordBack,
        "insert_delete_to_line_start" => Command::InsertDeleteToLineStart,
        "complete_word" => {
            // `back` is the leading token; the candidate text is the untrimmed remainder (empty when
            // the original base was empty, so a bare `complete_word N ` round-trips).
            let (b, t) = raw.split_once(' ').unwrap_or((raw, ""));
            let back = b
                .parse::<u32>()
                .map_err(|_| CommandParseError::BadArgument(line.to_string()))?;
            Command::CompleteWord {
                back,
                text: t.to_string(),
            }
        }
        "insert_indent" => Command::InsertIndent,
        "insert_dedent" => Command::InsertDedent,
        "insert_delete_indent" => Command::InsertDeleteIndent,
        "insert_delete_indent_restore" => Command::InsertDeleteIndentRestore,
        "insert_start_column_line" => Command::InsertStartColumnLine {
            down: match arg {
                Some("down") => true,
                Some("up") => false,
                _ => return Err(CommandParseError::BadArgument(line.to_string())),
            },
        },
        "insert_caret_step" => Command::InsertCaretStep {
            right: match arg {
                Some("right") => true,
                Some("left") => false,
                _ => return Err(CommandParseError::BadArgument(line.to_string())),
            },
        },
        "insert_tab" => Command::InsertTab,
        "insert_newline" => Command::InsertNewline,
        "open_line_indent" => {
            let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let mut it = a.split_whitespace();
            let kind = match it.next() {
                Some("below") => OpenKind::Below,
                Some("above") => OpenKind::Above,
                Some("split") => OpenKind::Split,
                _ => return Err(CommandParseError::BadArgument(a.to_string())),
            };
            let level = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            Command::OpenLineIndent { kind, level }
        }
        "insert_closer" => Command::InsertCloser {
            ch: arg_char(arg, line)?,
        },
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the counted single-character and line edits (`x`, `r`, `~`, `J`,
/// `CTRL-A`); `Ok(None)` for another family's verb.
pub(super) fn decode_simple_edit(
    parts: LineParts<'_>,
) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw: _,
        line,
    } = parts;
    Ok(Some(match verb {
        "delete_back" => Command::DeleteBack,
        "delete_under" => {
            let n = arg_u32(arg, line)?;
            Command::DeleteUnder(n)
        }
        "delete_forward" => {
            let n = arg_u32(arg, line)?;
            Command::DeleteForward(n)
        }
        "replace_char" => {
            let (n, c) = arg_count_char(arg, line)?;
            Command::ReplaceChar(n, c)
        }
        "replace_selection_char" => Command::ReplaceSelectionChar(arg_char(arg, line)?),
        "toggle_case" => {
            let n = arg_u32(arg, line)?;
            Command::ToggleCase(n)
        }
        "join_lines" => Command::JoinLines(arg_u32(arg, line)?),
        "join_lines_no_space" => Command::JoinLinesNoSpace(arg_u32(arg, line)?),
        "increment_number" => {
            let d: i64 = arg
                .and_then(|a| a.trim().parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            Command::IncrementNumber(d)
        }
        "increment_selection" => {
            let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let mut it = a.split_whitespace();
            let delta: i64 = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let sequential: bool = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            Command::IncrementSelection { delta, sequential }
        }
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the mark and jump-list motions, named marks and the info commands;
/// `Ok(None)` for another family's verb.
pub(super) fn decode_mark(parts: LineParts<'_>) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw: _,
        line,
    } = parts;
    Ok(Some(match verb {
        "goto_last_change" => Command::GotoLastChange,
        "goto_last_change_line" => Command::GotoLastChangeLine,
        "goto_change_mark_start" => Command::GotoChangeMarkStart,
        "goto_change_mark_end" => Command::GotoChangeMarkEnd,
        "goto_change_mark_start_line" => Command::GotoChangeMarkStartLine,
        "goto_change_mark_end_line" => Command::GotoChangeMarkEndLine,
        "goto_visual_mark_start" => Command::GotoVisualMarkStart,
        "goto_visual_mark_end" => Command::GotoVisualMarkEnd,
        "goto_visual_mark_start_line" => Command::GotoVisualMarkStartLine,
        "goto_visual_mark_end_line" => Command::GotoVisualMarkEndLine,
        "goto_context_mark" => Command::GotoContextMark,
        "goto_context_mark_line" => Command::GotoContextMarkLine,
        "goto_older_change" => Command::GotoOlderChange,
        "goto_newer_change" => Command::GotoNewerChange,
        "goto_older_jump" => Command::GotoOlderJump,
        "goto_newer_jump" => Command::GotoNewerJump,
        "insert_at_last_insert" => Command::InsertAtLastInsert,
        "ascii_info" => Command::AsciiInfo,
        "utf8_info" => Command::Utf8Info,
        "file_info" => Command::FileInfo,
        "cursor_info" => Command::CursorInfo,
        "set_named_mark" => Command::SetNamedMark(arg_char(arg, line)?),
        "goto_named_mark" => Command::GotoNamedMark(arg_char(arg, line)?),
        "goto_named_mark_line" => Command::GotoNamedMarkLine(arg_char(arg, line)?),
        "op_to_mark" => {
            let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let mut it = a.split_whitespace();
            let op = it
                .next()
                .and_then(mark_op_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let name = it
                .next()
                .and_then(|s| s.parse().ok())
                .and_then(char::from_u32)
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let linewise = match it.next() {
                Some("linewise") => true,
                Some("charwise") => false,
                _ => return Err(CommandParseError::BadArgument(a.to_string())),
            };
            Command::OpToMark { op, name, linewise }
        }
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the operator + motion commands up to `comment` (`d`, `c`, `y`, `>`,
/// `=`, `gc`); `Ok(None)` for another family's verb.
pub(super) fn decode_operator(parts: LineParts<'_>) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw: _,
        line,
    } = parts;
    Ok(Some(match verb {
        "break_undo" => Command::BreakUndo,
        "shift_right" => {
            let n = arg_u32(arg, line)?;
            Command::ShiftRight(n)
        }
        "shift_left" => {
            let n = arg_u32(arg, line)?;
            Command::ShiftLeft(n)
        }
        "move" => op_cmd(arg, Command::Move)?,
        "delete" => op_cmd(arg, Command::Delete)?,
        "change" => op_cmd(arg, Command::Change)?,
        "yank" => op_cmd(arg, Command::Yank)?,
        "shift_motion" => {
            // `shift_motion {left|right} {count} {motion}`
            let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let mut it = a.split_whitespace();
            let left = match it.next() {
                Some("left") => true,
                Some("right") => false,
                _ => return Err(CommandParseError::BadArgument(a.to_string())),
            };
            let count = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let motion = it
                .next()
                .and_then(motion_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            return Ok(Some(Command::ShiftMotion {
                left,
                count,
                motion,
            }));
        }
        "reindent" => op_cmd(arg, |count, motion| Command::Reindent { count, motion })?,
        "comment" => op_cmd(arg, |count, motion| Command::Comment { count, motion })?,
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the operators with extra arguments: `gq`/`gw`, indent sets, case,
/// forced-wise; `Ok(None)` for another family's verb.
pub(super) fn decode_operator_extra(
    parts: LineParts<'_>,
) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw: _,
        line,
    } = parts;
    Ok(Some(match verb {
        "format" => {
            // `format {count} {motion} {keep|move}`
            let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let mut it = a.split_whitespace();
            let count = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let motion = it
                .next()
                .and_then(motion_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let keep_cursor = matches!(it.next(), Some("keep"));
            return Ok(Some(Command::Format {
                count,
                motion,
                keep_cursor,
            }));
        }
        "set_indent_cols" => set_indent_cols_arg(arg, line)?,
        "insert_reindent" => Command::InsertReindent,
        "set_indents" => set_indents_arg(arg, line)?,
        "case_motion" => {
            // `case_motion {count} {motion} {case} [{wise}]` — no wise token is an unforced motion.
            let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let mut it = a.split_whitespace();
            let count = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let motion = it
                .next()
                .and_then(motion_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
            let case = arg_word_case(it.next(), line)?;
            let wise = match it.next() {
                None => None,
                Some(t) => Some(
                    forced_wise_from_token(t)
                        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?,
                ),
            };
            return Ok(Some(Command::CaseMotion {
                count,
                motion,
                case,
                wise,
            }));
        }
        "case_selection" => {
            return Ok(Some(Command::CaseSelection(arg_word_case(
                arg.map(str::trim),
                line,
            )?)));
        }
        "op_forced" => op_forced_arg(arg, line)?,
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the puts and register selection; `Ok(None)` for another family's
/// verb.
pub(super) fn decode_paste_register(
    parts: LineParts<'_>,
) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw,
        line,
    } = parts;
    Ok(Some(match verb {
        "paste_after" => Command::Paste {
            after: true,
            count: arg_u32(arg, line)?,
            move_after: false,
        },
        "paste_before" => Command::Paste {
            after: false,
            count: arg_u32(arg, line)?,
            move_after: false,
        },
        "paste_after_move" => Command::Paste {
            after: true,
            count: arg_u32(arg, line)?,
            move_after: true,
        },
        "paste_before_move" => Command::Paste {
            after: false,
            count: arg_u32(arg, line)?,
            move_after: true,
        },
        "paste_after_indent" => Command::PasteIndent {
            after: true,
            count: arg_u32(arg, line)?,
        },
        "paste_before_indent" => Command::PasteIndent {
            after: false,
            count: arg_u32(arg, line)?,
        },
        "paste_after_inner" => Command::PasteBlockInner {
            after: true,
            count: arg_u32(arg, line)?,
        },
        "paste_before_inner" => Command::PasteBlockInner {
            after: false,
            count: arg_u32(arg, line)?,
        },
        "paste_selection" => {
            let mut words = arg.unwrap_or("").split_whitespace();
            let swap = words.next() != Some("keep");
            let count = match words.next() {
                Some(n) => arg_u32(Some(n), line)?,
                None => 1,
            };
            Command::PasteSelection { swap, count }
        }
        "select_register" => {
            // No arg → the unnamed register; otherwise a decimal Unicode scalar for the register name.
            let name = match arg {
                None => None,
                Some(a) => {
                    let cp: u32 = a
                        .parse()
                        .map_err(|_| CommandParseError::BadArgument(line.to_string()))?;
                    Some(
                        char::from_u32(cp)
                            .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
                    )
                }
            };
            Command::SetRegister(name)
        }
        // The expression is the untrimmed remainder (may contain spaces), like `search_next`.
        "set_expr_register" => Command::SetExprRegister(raw.to_string()),
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the visual/Select mode entries and selection operators; `Ok(None)`
/// for another family's verb.
pub(super) fn decode_visual(parts: LineParts<'_>) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw: _,
        line,
    } = parts;
    Ok(Some(match verb {
        "enter_visual" => Command::EnterVisual {
            kind: SelectKind::Charwise,
        },
        "enter_visual_line" => Command::EnterVisual {
            kind: SelectKind::Linewise,
        },
        "enter_visual_block" => Command::EnterVisual {
            kind: SelectKind::Blockwise,
        },
        "enter_select" => Command::EnterSelect {
            kind: SelectKind::Charwise,
        },
        "enter_select_line" => Command::EnterSelect {
            kind: SelectKind::Linewise,
        },
        "enter_select_block" => Command::EnterSelect {
            kind: SelectKind::Blockwise,
        },
        "delete_selection" => Command::DeleteSelection,
        "yank_selection" => Command::YankSelection,
        "change_selection" => Command::ChangeSelection,
        "reselect_visual" => Command::ReselectVisual,
        "swap_selection_ends" => Command::SwapSelectionEnds,
        "replace_selection" => Command::ReplaceSelection(arg_char(arg, line)?),
        "visual_op" => Command::VisualOp(
            arg.and_then(visual_op_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
        ),
        "block_insert" => Command::BlockInsert(BlockInsertKind::Insert),
        "block_append" => Command::BlockInsert(BlockInsertKind::Append),
        "block_change" => Command::BlockInsert(BlockInsertKind::Change),
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the searches, `*`/`#`, substitute repeats and keyword lookups;
/// `Ok(None)` for another family's verb.
pub(super) fn decode_search(parts: LineParts<'_>) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg: _,
        raw,
        line,
    } = parts;
    Ok(Some(match verb {
        "search_next" => Command::SearchNext(raw.to_string()),
        "search_prev" => Command::SearchPrev(raw.to_string()),
        "search_word_under" => {
            let mut it = raw.split_whitespace();
            let forward = !matches!(it.next(), Some("bwd"));
            let whole_word = !matches!(it.next(), Some("any"));
            let op = match it.next() {
                None => SearchOp::Move,
                Some(t) => search_op_from_token(t)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
            };
            let count = match it.next() {
                None => 1,
                Some(c) => c
                    .parse()
                    .map_err(|_| CommandParseError::BadArgument(line.to_string()))?,
            };
            Command::SearchWordUnder {
                forward,
                whole_word,
                op,
                count,
            }
        }
        "repeat_substitute_global" => Command::RepeatSubstituteGlobal,
        "repeat_substitute_line" => Command::RepeatSubstituteLine,
        "goto_declaration" => Command::GotoDeclaration {
            global: matches!(raw.split_whitespace().next(), Some("global")),
        },
        "goto_first_match" => Command::GotoFirstMatch(raw.to_string()),
        "show_keyword_lines" => {
            let mut it = raw.split_whitespace();
            let above = !matches!(it.next(), Some("below"));
            let list = matches!(it.next(), Some("list"));
            let count = it.next().and_then(|c| c.parse().ok()).unwrap_or(1);
            Command::ShowKeywordLines { above, list, count }
        }
        "search" => {
            // `search {op} {count} {fwd|bwd} {offset} {pattern...}` — pattern is the untrimmed
            // remainder (may contain spaces); the offset token is whitespace-free.
            let mut parts = raw.splitn(5, ' ');
            let op = parts
                .next()
                .and_then(search_op_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let count: u32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let backward = matches!(parts.next(), Some("bwd"));
            let offset = parts
                .next()
                .and_then(SearchOffset::from_token)
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let pattern = parts.next().unwrap_or("").to_string();
            Command::Search {
                op,
                count,
                pattern,
                backward,
                offset,
            }
        }
        "search_object" => {
            // `search_object {op} {count} {fwd|bwd} {pattern...}` — pattern is the untrimmed remainder.
            let mut parts = raw.splitn(4, ' ');
            let op = parts
                .next()
                .and_then(search_op_from_token)
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let count: u32 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
            let backward = matches!(parts.next(), Some("bwd"));
            let pattern = parts.next().unwrap_or("").to_string();
            Command::SearchObject {
                op,
                count,
                pattern,
                backward,
            }
        }
        _ => return Ok(None),
    }))
}

/// The [`Command::from_line`] decoder for the undo/redo, save/quit and the Emacs region, kill and case
/// commands; `Ok(None)` for another family's verb.
pub(super) fn decode_session_emacs(
    parts: LineParts<'_>,
) -> Result<Option<Command>, CommandParseError> {
    let LineParts {
        verb,
        arg,
        raw: _,
        line,
    } = parts;
    Ok(Some(match verb {
        "undo" => Command::Undo,
        "redo" => Command::Redo,
        "undo_older" => Command::UndoOlder,
        "undo_newer" => Command::UndoNewer,
        "line_undo" => Command::LineUndo,
        "save" => Command::Save,
        "quit" => Command::Quit,
        "set_mark" => Command::SetMark,
        "kill_region" => Command::KillRegion,
        "copy_region" => Command::CopyRegion,
        "exchange_point_mark" => Command::ExchangePointMark,
        "emacs_kill_line" => Command::EmacsKillLine,
        "emacs_transpose_chars" => Command::EmacsTransposeChars,
        "emacs_transpose_words" => Command::EmacsTransposeWords,
        "emacs_kill_word" => {
            let count = arg_u32(arg, line)?;
            Command::EmacsKillWord { count }
        }
        "emacs_backward_kill_word" => {
            let count = arg_u32(arg, line)?;
            Command::EmacsBackwardKillWord { count }
        }
        "emacs_open_line" => Command::EmacsOpenLine,
        "emacs_mark_word" => Command::EmacsMarkWord,
        "emacs_kill_whole_line" => Command::EmacsKillWholeLine,
        "emacs_case_region" => Command::EmacsCaseRegion {
            case: arg_word_case(arg, line)?,
        },
        "emacs_horizontal_space" => match arg {
            Some("one") => Command::EmacsHorizontalSpace { keep_one: true },
            Some("none") => Command::EmacsHorizontalSpace { keep_one: false },
            _ => return Err(CommandParseError::BadArgument(line.to_string())),
        },
        "emacs_case_word" => Command::EmacsCaseWord {
            case: arg_word_case(arg, line)?,
        },
        "emacs_delete_indentation" => Command::EmacsDeleteIndentation,
        "emacs_yank" => {
            let count = arg_u32(arg, line)?;
            Command::EmacsYank { count }
        }
        "emacs_buffer_edge" => match arg {
            Some("start") => Command::EmacsBufferEdge { start: true },
            Some("end") => Command::EmacsBufferEdge { start: false },
            _ => return Err(CommandParseError::BadArgument(line.to_string())),
        },
        _ => return Ok(None),
    }))
}
