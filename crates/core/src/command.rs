//! Semantic editor commands (INV-CMD-SEMANTIC) — the granularity at which edits are recorded as a trace.
//!
//! A keymap resolves keys onto these (the input engine, C2); a [`crate::trace::Trace`] is a list of them, so
//! it survives keymap changes. Each command has a stable line form (`to_line`/`from_line`) — a dependency-
//! free, human-readable serialization used by the trace file.

use std::fmt::Write as _;

use crate::motion::Motion;

/// How a command-line search (`/pat`) folds into the editing grammar. A search is a MOTION (Vim: `/` is a
/// motion), so it can stand alone (`Move`) or be the range of an operator (`d/pat`, `c/pat`, `y/pat`). The
/// operator range is charwise-EXCLUSIVE — `[cursor, match)` — matching Vim's rule for `/` as a motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchOp {
    /// Bare `/pat` (and `{count}/pat`): move the cursor to the match.
    Move,
    /// `d/pat`: delete `[cursor, match)`.
    Delete,
    /// `c/pat`: delete `[cursor, match)` and enter Insert.
    Change,
    /// `y/pat`: yank `[cursor, match)`.
    Yank,
}

/// Which operator a forced-wise motion applies ([`Command::OpForced`]). The plain operator commands are
/// [`Command::Delete`]/[`Command::Change`]/[`Command::Yank`]; this enum names the same three for the rarer
/// forced-wise form so that form needs no parallel command per operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpKind {
    Delete,
    Change,
    Yank,
}

/// A motion's forced wise-ness (Vim `o_v` / `o_V` / `o_CTRL-V`): the operator applies the motion but
/// overrides whether the removed span is charwise, linewise, or blockwise. `Charwise` also toggles the
/// motion's exclusive/inclusive edge (Vim's rule for `v`). `Blockwise` treats the cursor and the motion
/// target as the two corners of a rectangle (`d<C-v>j` deletes one column across two rows).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForcedWise {
    Charwise,
    Linewise,
    Blockwise,
}

/// The SHAPE of a Visual/Select selection (F-025 c1: selection shape is namespace state, not a boolean).
/// Charwise (`v`) spans a contiguous byte range; linewise (`V`) whole lines; blockwise (`CTRL-V`) a
/// column-aligned rectangle across lines. `v`/`V`/`CTRL-V` switch the shape or leave the namespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SelectKind {
    #[default]
    Charwise,
    Linewise,
    Blockwise,
}

/// A blockwise-Visual insert command: type on the block's top row, then REPLICATE the typed text down
/// every row of the block on `<Esc>`. `Insert` (`I`) inserts at the block's left edge; `Append` (`A`)
/// appends at the right edge (padding short lines); `Change` (`c`/`s`) deletes the block first, then
/// inserts at the left edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockInsertKind {
    Insert,
    Append,
    Change,
}

/// A semantic command — the granularity a trace records. Beyond single motions/edits it carries the editing
/// grammar: a counted move, and the `delete`/`change` operators over a motion range (`dw`, `d2w`, `cc`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    // motion
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveLineStart,
    MoveLineEnd,
    // mode / insert-entry
    EnterInsert,
    EnterInsertAfter,
    EnterNormal,
    /// `I` — insert before the first non-blank char of the line.
    InsertLineStart,
    /// `A` — append at the end of the line.
    AppendLineEnd,
    /// `o` — open a new line below and insert.
    OpenBelow,
    /// `O` — open a new line above and insert.
    OpenAbove,
    /// `R` — enter Replace mode (overwrite policy).
    EnterReplace,
    /// A printable key typed in Replace mode: overwrite the char under the cursor (or append at EOL).
    ReplaceType(char),
    /// `<BS>` in Replace mode: restore the last overwritten char (or delete the last appended one),
    /// moving the cursor back; at the session start it only moves the cursor left. Shared by Virtual Replace.
    ReplaceBackspace,
    /// `gR` — enter Virtual Replace mode (overwrite policy, tab-aware): like Replace but a `<Tab>` under the
    /// cursor is consumed one virtual column at a time rather than as a whole char.
    EnterVirtualReplace,
    /// A printable key typed in Virtual Replace mode (`gR`): overwrite the char under the cursor, but when
    /// that char is a `<Tab>` insert BEFORE it (shrinking the tab) until its last virtual column, then
    /// replace it; append at end-of-line. `<BS>` uses [`Command::ReplaceBackspace`].
    VirtualReplaceType(char),
    // edit
    InsertChar(char),
    InsertNewline,
    DeleteBack,
    /// `{count}x` — delete `count` chars from the cursor, clamped at end-of-line (Vim).
    DeleteUnder(u32),
    /// Emacs `delete-char` / `C-d` — delete `count` chars forward from point, WITHOUT writing to the kill
    /// ring (D-026: Emacs discards a `delete-char`, unlike Vim `x`). Clamped at buffer end and crosses
    /// newlines (Emacs is non-modal, so end-of-line is not a boundary), never touching the register.
    DeleteForward(u32),
    /// `{count}r{char}` — replace `count` chars with `char`; a no-op if fewer than `count` remain (Vim).
    ReplaceChar(u32, char),
    /// `{count}~` — toggle the case of `count` chars, clamped at EOL, then move past the last (Vim).
    ToggleCase(u32),
    /// `J` — join the current line with the next on a single space.
    JoinLines,
    /// `CTRL-G u` in Insert — break the undo sequence: the NEXT edit starts a fresh undo group, so a
    /// later `u` stops here instead of undoing the whole insert session. A nop that only clears the
    /// edit-continuation state (Vim `i_CTRL-G_u`). Undo-of-a-session is not observable via the parity
    /// oracle (its `set_lines` is not an undo boundary), so this is verified by a core unit test.
    BreakUndo,
    /// `{count}>>` — shift `count` lines one indent level to the right (Vim `>>`). Linewise: the count
    /// is a LINE count, not a motion. The indent unit comes from the editor's indent config.
    ShiftRight(u32),
    /// `{count}<<` — shift `count` lines one indent level to the left (Vim `<<`). Symmetric inverse of
    /// [`Command::ShiftRight`]: removes up to one indent level of leading whitespace, never past column 0.
    ShiftLeft(u32),
    // editing grammar: count + motion / operator (Phase D)
    Move(u32, Motion),
    Delete(u32, Motion),
    Change(u32, Motion),
    /// `{op}{v|V}{motion}` — an operator over a motion whose wise is FORCED (Vim `o_v`/`o_V`): `dvj`
    /// deletes charwise where `dj` is linewise; `dVe` deletes whole lines where `de` is charwise. Kept
    /// distinct from the plain operator commands so the common path stays a bare tuple; `count` multiplies
    /// the motion as usual.
    OpForced {
        op: OpKind,
        count: u32,
        motion: Motion,
        wise: ForcedWise,
    },
    // registers (v0 unnamed slot): yank a motion range; paste after (`p`) or before (`P`) the cursor
    Yank(u32, Motion),
    /// `{count}p` / `{count}P` — paste the unnamed register `count` times, after (`p`) or before (`P`)
    /// the cursor. `count` is the repetition count (Vim `2p` pastes twice); it defaults to 1.
    Paste {
        after: bool,
        count: u32,
    },
    /// `"x` — select the register `x` (`a`–`z`, or `A`–`Z` to append) for the FOLLOWING yank/delete/
    /// change/paste. `None` selects the unnamed register (the default). Emitted by the input engine on the
    /// register name key; the editor holds it as a one-shot pending register the next such command reads,
    /// then clears (D-026's named-slot expansion). Numbered/other register names are not modelled yet.
    SetRegister(Option<char>),
    // visual mode: enter a selection (charwise `v` / linewise `V` / blockwise `CTRL-V`); operate on it
    EnterVisual {
        kind: SelectKind,
    },
    /// `CTRL-G` from Visual — enter Select over the SAME selection (Visual↔Select toggle). Select
    /// shares Visual's anchor/shape and differs only in its unmatched-key policy (contracts/vim-style.yaml).
    EnterSelect {
        kind: SelectKind,
    },
    DeleteSelection,
    YankSelection,
    ChangeSelection,
    /// `gv` — re-select the LAST visual/select selection (its anchor, active end, and charwise/linewise
    /// kind), captured when that selection was last left. A no-op if there is no prior selection. This is
    /// the depth-1 degenerate of D-027's `` `< ``/`` `> `` selection history (one remembered `Selection`).
    ReselectVisual,
    /// Visual/Select `o` — swap the selection's two ends: the cursor jumps to the anchor and the anchor
    /// becomes the old cursor position. The SAME text stays selected, but subsequent motions now extend
    /// the OTHER end. Involutive (`oo` restores the original ends); a clean no-op outside a selection.
    SwapSelectionEnds,
    /// Select's `open/replace-selection` policy: a printable key deletes the selection, inserts the char,
    /// and enters Insert. The one behaviour that distinguishes Select from Visual over identical state.
    ReplaceSelection(char),
    /// Blockwise Visual `I`/`A`/`c`: enter Insert on the block's top row, arming a replicate session — on
    /// `<Esc>` the text typed on the top row is inserted at the same column on every other row of the block.
    BlockInsert(BlockInsertKind),
    // search (literal substring for v0; the pattern is carried so traces replay deterministically)
    SearchNext(String),
    SearchPrev(String),
    /// `{count}/{pattern}<CR>` as a MOTION: bare it moves to the `count`-th forward match; under an
    /// operator (`op`) it folds into a charwise-exclusive edit over `[cursor, match)` (`d/pat`, `c/pat`,
    /// `y/pat`). The pattern is literal (v0; C-REGEX later) and carried so traces replay deterministically.
    /// Backward search (`?`) is not wired yet, so this is forward-only. `n`/`N` still repeat via
    /// [`Command::SearchNext`]/[`Command::SearchPrev`].
    Search {
        op: SearchOp,
        count: u32,
        pattern: String,
    },
    // history / file / control
    Undo,
    Redo,
    /// `g-` — step to the chronologically older text state, across branches (Vim undo-time travel).
    UndoOlder,
    /// `g+` — step to the chronologically newer text state, across branches.
    UndoNewer,
    Save,
    Quit,
    // Emacs region (D-027 depth-1: a single per-buffer mark, the degenerate one-caret `Ring<Selection>`).
    /// `C-SPC` (set-mark-command) — set the mark at point. The region is the span between point and mark.
    SetMark,
    /// `C-w` (kill-region) — delete the region `[min(point,mark), max)` charwise into the unnamed register
    /// (the kill ring), leaving point and mark together at its start. A no-op when no mark is set.
    KillRegion,
    /// `M-w` (kill-ring-save) — copy the region into the register without deleting; point and mark are
    /// unchanged. A no-op when no mark is set.
    CopyRegion,
    /// `C-x C-x` (exchange-point-and-mark) — swap point and mark. A no-op when no mark is set.
    ExchangePointMark,
    /// `C-y` (yank) — paste the register before point AND set the mark at the insertion start, leaving point
    /// after the pasted text (D-051). Emacs yank differs from Vim `p`/`P` (which never touch the mark); the
    /// paste geometry itself is shared with [`Command::Paste`] `{after:false}` (gravity-aware cursor, D-050).
    EmacsYank {
        count: u32,
    },
    /// `M-<` / `M->` (beginning-of-buffer / end-of-buffer) — move point to the ABSOLUTE buffer start (`0`)
    /// or end (buffer length), and PUSH the mark at the old point (D-051). Distinct from Vim `gg`/`G`
    /// (`Move(GotoLine/LastLine)`), which land on a line's first non-blank and never set this mark.
    EmacsBufferEdge {
        start: bool,
    },
    /// `C-k` (kill-line) — kill from point to end of line into the register (the kill ring). Distinct from
    /// Vim `D` / `Delete(1, LineEnd)`: when point is already AT end of line, Emacs kills the terminating
    /// newline instead (joining the next line up), and at end-of-buffer it is inert. The count-less default
    /// (`kill-whole-line` nil, no prefix arg) — the binding ignores the prefix count (D-051 / RFC-0016:
    /// an Emacs op diverging from its Vim lookalike in more than caret gravity is its own command).
    EmacsKillLine,
    /// `C-t` (transpose-chars) — swap the character before point with the character at point, then advance
    /// point past the pair. At end of line it transposes the two characters that end the line (Emacs steps
    /// back one first). Inert when there is no pair to transpose (buffer/line start with fewer than two
    /// chars on the line). Emacs-only, no Vim lookalike (D-051); it does not touch the kill ring.
    EmacsTransposeChars,
    /// `M-d` / `kill-word` (Emacs kill-word) — kill the `EmacsWordFwd` span (the word only, not Vim `dw`'s
    /// trailing space) into the register, `count` words. Distinct from Vim `Delete(count, EmacsWordFwd)`
    /// ONLY in that it ACCUMULATES onto the current kill-ring entry when it follows another kill — but that
    /// alone earns its own command (D-051), so Vim deletes never accumulate.
    EmacsKillWord {
        count: u32,
    },
    /// `M-u` / `M-l` / `M-c` (upcase-word / downcase-word / capitalize-word) — recase the word from point to
    /// the end of the next word (the `forward-word` span) and leave point at that end. Emacs-only (D-051);
    /// the three keys differ only in the case operation, so they share one command carrying [`WordCase`].
    EmacsCaseWord {
        case: WordCase,
    },
}

/// Which case operation [`Command::EmacsCaseWord`] applies over the word span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordCase {
    /// `M-u` upcase-word — every letter to upper case.
    Upcase,
    /// `M-l` downcase-word — every letter to lower case.
    Downcase,
    /// `M-c` capitalize-word — first letter of each word upper, the rest lower.
    Capitalize,
}

fn motion_token(m: Motion) -> String {
    let s = match m {
        Motion::Left => "left",
        Motion::Right => "right",
        Motion::Up => "up",
        Motion::Down => "down",
        Motion::LineStart => "line_start",
        Motion::LineEnd => "line_end",
        Motion::WordFwd => "word_fwd",
        Motion::WordBack => "word_back",
        Motion::WordEnd => "word_end",
        Motion::EmacsWordFwd => "emacs_word_fwd",
        Motion::BigWordFwd => "big_word_fwd",
        Motion::BigWordBack => "big_word_back",
        Motion::BigWordEnd => "big_word_end",
        Motion::InnerWord => "inner_word",
        Motion::AWord => "a_word",
        Motion::InnerBigWord => "inner_big_word",
        Motion::ABigWord => "a_big_word",
        Motion::InnerParagraph => "inner_paragraph",
        Motion::AParagraph => "a_paragraph",
        Motion::InnerSentence => "inner_sentence",
        Motion::ASentence => "a_sentence",
        Motion::Line => "line",
        Motion::GotoLine => "goto_line",
        Motion::LastLine => "last_line",
        Motion::MatchBracket => "match_bracket",
        Motion::ParagraphFwd => "paragraph_fwd",
        Motion::ParagraphBack => "paragraph_back",
        // A single whitespace-free token so the `<count> <motion>` split still works: the char is its decimal
        // scalar value; `f`/`t` and forward/back are flags. e.g. `find_char:120:1:0` = `fx`.
        Motion::FindChar { ch, forward, till } => {
            return format!("find_char:{}:{}:{}", ch as u32, forward as u8, till as u8)
        }
        // Delimiter/quote text objects carry their char(s) + around flag as a single whitespace-free token,
        // mirroring `find_char` so the `<count> <motion>` split still holds.
        Motion::Pair {
            open,
            close,
            around,
        } => return format!("pair:{}:{}:{}", open as u32, close as u32, around as u8),
        Motion::Quote { ch, around } => return format!("quote:{}:{}", ch as u32, around as u8),
    };
    s.to_string()
}

fn motion_from_token(s: &str) -> Option<Motion> {
    if let Some(rest) = s.strip_prefix("find_char:") {
        let mut it = rest.split(':');
        let ch = char::from_u32(it.next()?.parse().ok()?)?;
        let forward = it.next()? == "1";
        let till = it.next()? == "1";
        if it.next().is_some() {
            return None; // trailing garbage
        }
        return Some(Motion::FindChar { ch, forward, till });
    }
    if let Some(rest) = s.strip_prefix("pair:") {
        let mut it = rest.split(':');
        let open = char::from_u32(it.next()?.parse().ok()?)?;
        let close = char::from_u32(it.next()?.parse().ok()?)?;
        let around = it.next()? == "1";
        if it.next().is_some() {
            return None; // trailing garbage
        }
        return Some(Motion::Pair {
            open,
            close,
            around,
        });
    }
    if let Some(rest) = s.strip_prefix("quote:") {
        let mut it = rest.split(':');
        let ch = char::from_u32(it.next()?.parse().ok()?)?;
        let around = it.next()? == "1";
        if it.next().is_some() {
            return None; // trailing garbage
        }
        return Some(Motion::Quote { ch, around });
    }
    Some(match s {
        "left" => Motion::Left,
        "right" => Motion::Right,
        "up" => Motion::Up,
        "down" => Motion::Down,
        "line_start" => Motion::LineStart,
        "line_end" => Motion::LineEnd,
        "word_fwd" => Motion::WordFwd,
        "word_back" => Motion::WordBack,
        "word_end" => Motion::WordEnd,
        "emacs_word_fwd" => Motion::EmacsWordFwd,
        "big_word_fwd" => Motion::BigWordFwd,
        "big_word_back" => Motion::BigWordBack,
        "big_word_end" => Motion::BigWordEnd,
        "inner_word" => Motion::InnerWord,
        "a_word" => Motion::AWord,
        "inner_big_word" => Motion::InnerBigWord,
        "a_big_word" => Motion::ABigWord,
        "inner_paragraph" => Motion::InnerParagraph,
        "a_paragraph" => Motion::AParagraph,
        "inner_sentence" => Motion::InnerSentence,
        "a_sentence" => Motion::ASentence,
        "line" => Motion::Line,
        "goto_line" => Motion::GotoLine,
        "last_line" => Motion::LastLine,
        "match_bracket" => Motion::MatchBracket,
        "paragraph_fwd" => Motion::ParagraphFwd,
        "paragraph_back" => Motion::ParagraphBack,
        _ => return None,
    })
}

/// Parse an operator/move argument `"<count> <motion>"` into a command via `ctor`.
fn op_kind_token(op: OpKind) -> &'static str {
    match op {
        OpKind::Delete => "delete",
        OpKind::Change => "change",
        OpKind::Yank => "yank",
    }
}

fn op_kind_from_token(s: &str) -> Option<OpKind> {
    Some(match s {
        "delete" => OpKind::Delete,
        "change" => OpKind::Change,
        "yank" => OpKind::Yank,
        _ => return None,
    })
}

fn forced_wise_token(w: ForcedWise) -> &'static str {
    match w {
        ForcedWise::Charwise => "charwise",
        ForcedWise::Linewise => "linewise",
        ForcedWise::Blockwise => "blockwise",
    }
}

fn forced_wise_from_token(s: &str) -> Option<ForcedWise> {
    Some(match s {
        "charwise" => ForcedWise::Charwise,
        "linewise" => ForcedWise::Linewise,
        "blockwise" => ForcedWise::Blockwise,
        _ => return None,
    })
}

fn search_op_token(op: SearchOp) -> &'static str {
    match op {
        SearchOp::Move => "move",
        SearchOp::Delete => "delete",
        SearchOp::Change => "change",
        SearchOp::Yank => "yank",
    }
}

fn search_op_from_token(s: &str) -> Option<SearchOp> {
    Some(match s {
        "move" => SearchOp::Move,
        "delete" => SearchOp::Delete,
        "change" => SearchOp::Change,
        "yank" => SearchOp::Yank,
        _ => return None,
    })
}

fn op_cmd(
    arg: Option<&str>,
    ctor: fn(u32, Motion) -> Command,
) -> Result<Command, CommandParseError> {
    let a = arg.ok_or_else(|| CommandParseError::BadArgument("missing count/motion".into()))?;
    let mut it = a.split_whitespace();
    let count: u32 = it
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
    let motion = it
        .next()
        .and_then(motion_from_token)
        .ok_or_else(|| CommandParseError::BadArgument(a.to_string()))?;
    Ok(ctor(count, motion))
}

/// Why a trace line could not be parsed into a [`Command`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommandParseError {
    Empty,
    UnknownVerb(String),
    BadArgument(String),
}

impl Command {
    /// The stable single-line serialization of this command (used by the trace format). A `char` argument
    /// is encoded as its decimal Unicode scalar value so whitespace/newlines never need escaping.
    #[must_use]
    pub fn to_line(&self) -> String {
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
            Command::InsertLineStart => "insert_line_start".into(),
            Command::AppendLineEnd => "append_line_end".into(),
            Command::OpenBelow => "open_below".into(),
            Command::OpenAbove => "open_above".into(),
            Command::EnterReplace => "enter_replace".into(),
            Command::ReplaceType(c) => format!("replace_type {}", *c as u32),
            Command::ReplaceBackspace => "replace_backspace".into(),
            Command::EnterVirtualReplace => "enter_virtual_replace".into(),
            Command::VirtualReplaceType(c) => format!("virtual_replace_type {}", *c as u32),
            Command::InsertChar(c) => {
                let mut s = String::from("insert_char ");
                let _ = write!(s, "{}", *c as u32);
                s
            }
            Command::InsertNewline => "insert_newline".into(),
            Command::DeleteBack => "delete_back".into(),
            Command::DeleteUnder(n) => format!("delete_under {n}"),
            Command::DeleteForward(n) => format!("delete_forward {n}"),
            Command::ReplaceChar(n, c) => format!("replace_char {n} {}", *c as u32),
            Command::ToggleCase(n) => format!("toggle_case {n}"),
            Command::JoinLines => "join_lines".into(),
            Command::BreakUndo => "break_undo".into(),
            Command::ShiftRight(n) => format!("shift_right {n}"),
            Command::ShiftLeft(n) => format!("shift_left {n}"),
            Command::Move(n, m) => format!("move {n} {}", motion_token(*m)),
            Command::Delete(n, m) => format!("delete {n} {}", motion_token(*m)),
            Command::Change(n, m) => format!("change {n} {}", motion_token(*m)),
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
            Command::Paste { after, count } => {
                if *after {
                    format!("paste_after {count}")
                } else {
                    format!("paste_before {count}")
                }
            }
            Command::SetRegister(name) => match name {
                Some(c) => format!("select_register {}", *c as u32),
                None => "select_register".into(),
            },
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
            Command::BlockInsert(kind) => match kind {
                BlockInsertKind::Insert => "block_insert".into(),
                BlockInsertKind::Append => "block_append".into(),
                BlockInsertKind::Change => "block_change".into(),
            },
            Command::SearchNext(p) => format!("search_next {p}"),
            Command::SearchPrev(p) => format!("search_prev {p}"),
            // Pattern LAST so it may contain spaces (parsed as the untrimmed remainder, like search_next).
            Command::Search { op, count, pattern } => {
                format!("search {} {count} {pattern}", search_op_token(*op))
            }
            Command::Undo => "undo".into(),
            Command::Redo => "redo".into(),
            Command::UndoOlder => "undo_older".into(),
            Command::UndoNewer => "undo_newer".into(),
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
            Command::EmacsKillWord { count } => format!("emacs_kill_word {count}"),
            Command::EmacsCaseWord { case } => format!(
                "emacs_case_word {}",
                match case {
                    WordCase::Upcase => "upcase",
                    WordCase::Downcase => "downcase",
                    WordCase::Capitalize => "capitalize",
                }
            ),
        }
    }

    /// Parse one trace line back into a [`Command`].
    pub fn from_line(line: &str) -> Result<Command, CommandParseError> {
        let line = line.trim();
        if line.is_empty() {
            return Err(CommandParseError::Empty);
        }
        let (verb, arg) = match line.split_once(' ') {
            Some((v, a)) => (v, Some(a.trim())),
            None => (line, None),
        };
        // The raw remainder (untrimmed) — search patterns may contain spaces.
        let raw = line.split_once(' ').map(|(_, r)| r).unwrap_or("");
        Ok(match verb {
            "move_left" => Command::MoveLeft,
            "move_right" => Command::MoveRight,
            "move_up" => Command::MoveUp,
            "move_down" => Command::MoveDown,
            "move_line_start" => Command::MoveLineStart,
            "move_line_end" => Command::MoveLineEnd,
            "enter_insert" => Command::EnterInsert,
            "enter_insert_after" => Command::EnterInsertAfter,
            "enter_normal" => Command::EnterNormal,
            "insert_line_start" => Command::InsertLineStart,
            "append_line_end" => Command::AppendLineEnd,
            "open_below" => Command::OpenBelow,
            "open_above" => Command::OpenAbove,
            "enter_replace" => Command::EnterReplace,
            "replace_type" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ReplaceType(c)
            }
            "replace_backspace" => Command::ReplaceBackspace,
            "enter_virtual_replace" => Command::EnterVirtualReplace,
            "virtual_replace_type" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::VirtualReplaceType(c)
            }
            "insert_char" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::InsertChar(c)
            }
            "insert_newline" => Command::InsertNewline,
            "delete_back" => Command::DeleteBack,
            "delete_under" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::DeleteUnder(n)
            }
            "delete_forward" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::DeleteForward(n)
            }
            "replace_char" => {
                let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let mut it = a.split_whitespace();
                let n: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let cp: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ReplaceChar(n, c)
            }
            "toggle_case" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ToggleCase(n)
            }
            "join_lines" => Command::JoinLines,
            "break_undo" => Command::BreakUndo,
            "shift_right" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ShiftRight(n)
            }
            "shift_left" => {
                let n: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ShiftLeft(n)
            }
            "move" => return op_cmd(arg, Command::Move),
            "delete" => return op_cmd(arg, Command::Delete),
            "change" => return op_cmd(arg, Command::Change),
            "yank" => return op_cmd(arg, Command::Yank),
            "op_forced" => {
                // `op_forced {op} {wise} {count} {motion}`
                let a = arg.ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let mut it = a.split_whitespace();
                let op = it
                    .next()
                    .and_then(op_kind_from_token)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let wise = it
                    .next()
                    .and_then(forced_wise_from_token)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let count: u32 = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let motion = it
                    .next()
                    .and_then(motion_from_token)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::OpForced {
                    op,
                    count,
                    motion,
                    wise,
                }
            }
            "paste_after" => Command::Paste {
                after: true,
                count: arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
            },
            "paste_before" => Command::Paste {
                after: false,
                count: arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?,
            },
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
            "replace_selection" => {
                let cp: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let c = char::from_u32(cp)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::ReplaceSelection(c)
            }
            "block_insert" => Command::BlockInsert(BlockInsertKind::Insert),
            "block_append" => Command::BlockInsert(BlockInsertKind::Append),
            "block_change" => Command::BlockInsert(BlockInsertKind::Change),
            "search_next" => Command::SearchNext(raw.to_string()),
            "search_prev" => Command::SearchPrev(raw.to_string()),
            "search" => {
                // `search {op} {count} {pattern...}` — pattern is the untrimmed remainder (may hold spaces).
                let mut parts = raw.splitn(3, ' ');
                let op = parts
                    .next()
                    .and_then(search_op_from_token)
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let count: u32 = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                let pattern = parts.next().unwrap_or("").to_string();
                Command::Search { op, count, pattern }
            }
            "undo" => Command::Undo,
            "redo" => Command::Redo,
            "undo_older" => Command::UndoOlder,
            "undo_newer" => Command::UndoNewer,
            "save" => Command::Save,
            "quit" => Command::Quit,
            "set_mark" => Command::SetMark,
            "kill_region" => Command::KillRegion,
            "copy_region" => Command::CopyRegion,
            "exchange_point_mark" => Command::ExchangePointMark,
            "emacs_kill_line" => Command::EmacsKillLine,
            "emacs_transpose_chars" => Command::EmacsTransposeChars,
            "emacs_kill_word" => {
                let count: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::EmacsKillWord { count }
            }
            "emacs_case_word" => match arg {
                Some("upcase") => Command::EmacsCaseWord {
                    case: WordCase::Upcase,
                },
                Some("downcase") => Command::EmacsCaseWord {
                    case: WordCase::Downcase,
                },
                Some("capitalize") => Command::EmacsCaseWord {
                    case: WordCase::Capitalize,
                },
                _ => return Err(CommandParseError::BadArgument(line.to_string())),
            },
            "emacs_yank" => {
                let count: u32 = arg
                    .and_then(|a| a.parse().ok())
                    .ok_or_else(|| CommandParseError::BadArgument(line.to_string()))?;
                Command::EmacsYank { count }
            }
            "emacs_buffer_edge" => match arg {
                Some("start") => Command::EmacsBufferEdge { start: true },
                Some("end") => Command::EmacsBufferEdge { start: false },
                _ => return Err(CommandParseError::BadArgument(line.to_string())),
            },
            other => return Err(CommandParseError::UnknownVerb(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_variant() {
        let cases = [
            Command::MoveLeft,
            Command::MoveRight,
            Command::MoveUp,
            Command::MoveDown,
            Command::MoveLineStart,
            Command::MoveLineEnd,
            Command::EnterInsert,
            Command::EnterInsertAfter,
            Command::EnterNormal,
            Command::InsertLineStart,
            Command::AppendLineEnd,
            Command::OpenBelow,
            Command::OpenAbove,
            Command::EnterReplace,
            Command::ReplaceType('z'),
            Command::ReplaceType('가'),
            Command::ReplaceBackspace,
            Command::EnterVirtualReplace,
            Command::VirtualReplaceType('z'),
            Command::VirtualReplaceType('가'),
            Command::InsertChar('h'),
            Command::ReplaceChar(1, 'z'),
            Command::ReplaceChar(3, 'z'),
            Command::ReplaceChar(1, '가'),
            Command::ToggleCase(1),
            Command::ToggleCase(4),
            Command::JoinLines,
            Command::BreakUndo,
            Command::ShiftRight(1),
            Command::ShiftRight(3),
            Command::ShiftLeft(1),
            Command::ShiftLeft(2),
            Command::InsertChar('🎉'),
            Command::InsertChar(' '),
            Command::InsertNewline,
            Command::DeleteBack,
            Command::DeleteUnder(1),
            Command::DeleteUnder(3),
            Command::DeleteForward(1),
            Command::DeleteForward(2),
            Command::Move(2, Motion::WordFwd),
            Command::Move(1, Motion::EmacsWordFwd),
            Command::Delete(1, Motion::EmacsWordFwd),
            Command::Move(1, Motion::BigWordFwd),
            Command::Delete(1, Motion::BigWordBack),
            Command::Change(2, Motion::BigWordEnd),
            Command::Delete(1, Motion::Line),
            Command::Delete(1, Motion::InnerWord),
            Command::Change(1, Motion::AWord),
            Command::Delete(1, Motion::InnerBigWord),
            Command::Change(1, Motion::ABigWord),
            Command::Delete(1, Motion::InnerParagraph),
            Command::Yank(1, Motion::AParagraph),
            Command::Delete(1, Motion::InnerSentence),
            Command::Change(1, Motion::ASentence),
            Command::Delete(
                1,
                Motion::Pair {
                    open: '(',
                    close: ')',
                    around: false,
                },
            ),
            Command::Change(
                2,
                Motion::Pair {
                    open: '{',
                    close: '}',
                    around: true,
                },
            ),
            Command::Yank(
                1,
                Motion::Pair {
                    open: '<',
                    close: '>',
                    around: false,
                },
            ),
            Command::Delete(
                1,
                Motion::Quote {
                    ch: '"',
                    around: true,
                },
            ),
            Command::Change(
                1,
                Motion::Quote {
                    ch: '`',
                    around: false,
                },
            ),
            Command::Change(3, Motion::WordEnd),
            Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Charwise,
            },
            Command::OpForced {
                op: OpKind::Yank,
                count: 2,
                motion: Motion::WordEnd,
                wise: ForcedWise::Linewise,
            },
            Command::OpForced {
                op: OpKind::Change,
                count: 1,
                motion: Motion::WordFwd,
                wise: ForcedWise::Charwise,
            },
            Command::OpForced {
                op: OpKind::Delete,
                count: 1,
                motion: Motion::Down,
                wise: ForcedWise::Blockwise,
            },
            Command::Move(
                1,
                Motion::FindChar {
                    ch: 'x',
                    forward: true,
                    till: false,
                },
            ),
            Command::Delete(
                2,
                Motion::FindChar {
                    ch: ')',
                    forward: true,
                    till: true,
                },
            ),
            Command::Move(
                1,
                Motion::FindChar {
                    ch: '가',
                    forward: false,
                    till: false,
                },
            ),
            Command::Move(1, Motion::LastLine),
            Command::Move(5, Motion::GotoLine),
            Command::Delete(1, Motion::GotoLine),
            Command::Move(1, Motion::MatchBracket),
            Command::Delete(1, Motion::MatchBracket),
            Command::Move(1, Motion::ParagraphFwd),
            Command::Move(2, Motion::ParagraphBack),
            Command::Delete(1, Motion::ParagraphFwd),
            Command::Yank(1, Motion::ParagraphBack),
            Command::Yank(1, Motion::Line),
            Command::Yank(2, Motion::WordFwd),
            Command::Paste {
                after: true,
                count: 1,
            },
            Command::Paste {
                after: false,
                count: 1,
            },
            Command::Paste {
                after: true,
                count: 2,
            },
            Command::Paste {
                after: false,
                count: 5,
            },
            Command::SetRegister(None),
            Command::SetRegister(Some('a')),
            Command::SetRegister(Some('Z')),
            Command::EnterVisual {
                kind: SelectKind::Charwise,
            },
            Command::EnterVisual {
                kind: SelectKind::Linewise,
            },
            Command::EnterVisual {
                kind: SelectKind::Blockwise,
            },
            Command::EnterSelect {
                kind: SelectKind::Charwise,
            },
            Command::EnterSelect {
                kind: SelectKind::Linewise,
            },
            Command::EnterSelect {
                kind: SelectKind::Blockwise,
            },
            Command::DeleteSelection,
            Command::YankSelection,
            Command::ChangeSelection,
            Command::SwapSelectionEnds,
            Command::ReselectVisual,
            Command::ReplaceSelection('z'),
            Command::ReplaceSelection('가'),
            Command::ReplaceSelection('🎉'),
            Command::BlockInsert(BlockInsertKind::Insert),
            Command::BlockInsert(BlockInsertKind::Append),
            Command::BlockInsert(BlockInsertKind::Change),
            Command::SearchNext("foo bar".into()),
            Command::SearchPrev("x".into()),
            Command::Search {
                op: SearchOp::Move,
                count: 2,
                pattern: "foo".into(),
            },
            Command::Search {
                op: SearchOp::Delete,
                count: 1,
                pattern: "world foo".into(),
            },
            Command::Search {
                op: SearchOp::Change,
                count: 1,
                pattern: "x".into(),
            },
            Command::Search {
                op: SearchOp::Yank,
                count: 3,
                pattern: "a b".into(),
            },
            Command::Undo,
            Command::Redo,
            Command::Save,
            Command::Quit,
            Command::EmacsYank { count: 1 },
            Command::EmacsYank { count: 4 },
            Command::EmacsBufferEdge { start: true },
            Command::EmacsBufferEdge { start: false },
            Command::EmacsKillLine,
            Command::EmacsTransposeChars,
            Command::EmacsKillWord { count: 1 },
            Command::EmacsKillWord { count: 3 },
            Command::EmacsCaseWord {
                case: WordCase::Upcase,
            },
            Command::EmacsCaseWord {
                case: WordCase::Downcase,
            },
            Command::EmacsCaseWord {
                case: WordCase::Capitalize,
            },
        ];
        for c in cases {
            assert_eq!(Command::from_line(&c.to_line()), Ok(c.clone()), "{c:?}");
        }
    }

    #[test]
    fn rejects_unknown_and_bad() {
        assert_eq!(Command::from_line(""), Err(CommandParseError::Empty));
        assert!(matches!(
            Command::from_line("frobnicate"),
            Err(CommandParseError::UnknownVerb(_))
        ));
        assert!(matches!(
            Command::from_line("insert_char zzz"),
            Err(CommandParseError::BadArgument(_))
        ));
    }
}
