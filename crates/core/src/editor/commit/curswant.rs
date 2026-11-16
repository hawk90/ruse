//! The sticky wanted column (`curswant`) after a command: which failed commands keep it (#1171), and how
//! every other command sets, keeps or recomputes it. Split out of `editor/commit.rs` (#1504).

use super::super::{EditorState, Mode, Want, MAXCOL};
use crate::command::Command;
use crate::motion::{self, line_end, line_start, Motion};

/// Whether `cmd`, which FAILED (nothing moved, changed or switched mode; `beeped` when it beeped), leaves
/// the wanted column alone (#1171). nvim recomputes `w_curswant` lazily, only when a command sets
/// `w_set_curswant`; a failing command keeps the old column unless its handler sets that flag before (or
/// regardless of) the check that fails. These handlers bail out first and so keep it: `h`/`l` (`nv_left`,
/// `nv_right`), `f`/`t`/`;`/`,` (`nv_csearch`), `%` (`nv_percent`), `[(`/`])`/`[{`/`]}`/`[m`/`]M`
/// (`nv_bracket_block`), CTRL-O/CTRL-I/`g;`/`g,` at an end of their list (`nv_pcmark`), `CTRL-A`/`CTRL-X`
/// without a number (`op_addsub`), `J`/`gJ` (`nv_join`), `r` past the line's end (`nv_replace`) and `~` on
/// an empty line (`n_swapchar`); an operator over one of those motions is cancelled with it (`dfz`).
/// Everything else that fails resets the column to the cursor's, as nvim's handlers do: `/`/`?`/`n`/`N`/
/// `*`/`#` (`normal_search`), word, sentence, paragraph and section motions, text objects (`nv_object`),
/// mark jumps (`nv_mark_move_to`), undo and redo, and an operator that ran but failed (`"/dd`).
pub(super) fn keeps_curswant_on_failure(st: &EditorState, cmd: &Command, beeped: bool) -> bool {
    let keeps = |m: &Motion| {
        matches!(
            m,
            // `j`/`k` (`nv_down`/`nv_up`) and `{count}dd` past the end (`nv_lineop`) fail in `cursor_down`/
            // `cursor_up` before anything sets the column (#1176).
            Motion::Up
                | Motion::Down
                | Motion::Line
                | Motion::Left
                | Motion::Right
                | Motion::WrapLeft
                | Motion::WrapRight
                | Motion::FindChar { .. }
                | Motion::MatchBracket
                | Motion::GotoPercent
                | Motion::UnmatchedParenBack
                | Motion::UnmatchedParenFwd
                | Motion::UnmatchedBraceBack
                | Motion::UnmatchedBraceFwd
                | Motion::MethodStartFwd
                | Motion::MethodStartBack
                | Motion::MethodEndFwd
                | Motion::MethodEndBack
        )
    };
    match cmd {
        Command::Move(_, m)
        | Command::Delete(_, m)
        | Command::Change(_, m)
        | Command::Yank(_, m) => beeped && keeps(m),
        Command::OpForced { motion, .. }
        | Command::CaseMotion { motion, .. }
        | Command::ShiftMotion { motion, .. }
        | Command::Reindent { motion, .. }
        | Command::Format { motion, .. }
        | Command::Comment { motion, .. } => beeped && keeps(motion),
        Command::GotoOlderJump
        | Command::GotoNewerJump
        | Command::GotoOlderChange
        | Command::GotoNewerChange
        // `{count}>>` on the last line (`nv_lineop`'s `cursor_down`).
        | Command::ShiftRight(_)
        | Command::ShiftLeft(_) => beeped,
        // These always edit when they succeed.
        Command::IncrementNumber(_)
        | Command::JoinLines(_)
        | Command::JoinLinesNoSpace(_)
        | Command::ReplaceChar(..) => true,
        Command::ToggleCase(_) => {
            let b = st.doc.bytes();
            line_start(b, st.view.cursor) == line_end(b, st.view.cursor)
        }
        _ => false,
    }
}

/// Maintain [`EditorState::curswant`] after a command (Vim's curswant rule): a vertical move KEEPS the
/// wanted column (ride it through short lines); `$`/`<End>`/`A` (append) set it to [`MAXCOL`] so subsequent
/// `j`/`k` — and an Insert caret — stay at the line's end; a failed command whose nvim handler never sets
/// the column keeps it (`kept_col`, [`keeps_curswant_on_failure`]); every other command recomputes it from
/// the cursor's new char column.
///
/// Insert-mode append column (Vim `i_CTRL-O` at EOL): while `curswant == MAXCOL` in Insert, the caret rests
/// past the line's last char (the append position): a one-shot `CTRL-O` command that leaves the cursor at
/// the line's end PRESERVES that append intent (e.g. `dd`, so `A<C-o>ddX` appends `X` at the end of the line
/// `dd` left behind, and `i<C-o>$X` appends at end), and a cursor resting ON the last char steps past it
/// (nvim's `restart_edit` rule in `edit()`); one landing mid-line (`` A<C-o>`a ``) stays there. Only a
/// command run FROM Insert keeps the intent: `$cb` or `$gi` (Normal to Insert) put the caret where the
/// command did (#702). Called from [`apply_command`], the single plan+commit driver, so `cmd` is always in
/// scope.
pub(super) fn update_curswant(
    st: &mut EditorState,
    cmd: &Command,
    entry_mode: Mode,
    kept_col: bool,
) {
    let insert = matches!(st.view.mode, Mode::Insert);
    let from_insert = insert && matches!(entry_mode, Mode::Insert);
    // Whether the cursor rests on the line's last char or past it.
    let at_line_end = |st: &EditorState| {
        let b = st.doc.bytes();
        let cur = st.view.cursor; // told by the bytes, not the line (#1231)
        motion::at_nul(b, cur) || motion::at_nul(b, motion::next_grapheme(b, cur))
    };
    match cmd {
        // A failed command whose nvim handler never sets `w_set_curswant` (`keeps_curswant_on_failure`).
        _ if kept_col => {}
        Command::Move(_, Motion::Up)
        | Command::Move(_, Motion::Down)
        // `gg`/`G`/`{count}%` move to the wanted column without changing it (`'nostartofline'`).
        | Command::Move(_, Motion::GotoLine | Motion::LastLine | Motion::GotoPercent)
        | Command::MoveUp
        | Command::MoveDown
        // An undo break moves nothing: `i_<Down><Down>` (a break before each move) keeps the column.
        | Command::BreakUndo
        // Starting (or switching) Visual moves nothing: `$v` keeps the end-of-line column, so `$V>` shifts
        // with the cursor at the line's end (nvim's `n_start_visual_mode` leaves `w_curswant` alone, #1176).
        | Command::EnterVisual { .. }
        // A `"x` prefix (or the register a failed `p` spends) moves nothing either (nvim 0.12.6:
        // `/a<CR>:1t$<CR>"add` keeps the search's column).
        | Command::SetRegister(_) => {} // keep the sticky column
        // `$`/`<End>` and `A` (append) want the line's end.
        Command::Move(_, Motion::LineEnd) | Command::MoveLineEnd | Command::AppendLineEnd => {
            st.view.curswant = Want::col(MAXCOL)
        }
        // `{count}|` wants the column it names, not the start of the char it lands on (nvim's `nv_pipe`):
        // `5|` onto a tab or an `<ff>` that covers column 5 keeps 5 for a following `j`/`k`.
        Command::Move(n, Motion::Column) => {
            st.view.curswant = Want::col((*n).max(1) as usize - 1);
        }
        // Commands that establish a definite column: horizontal moves, the Insert-native keys, and every
        // insert-ENTRY except `A` (which is the append/MAXCOL case above) reset the wanted column — they
        // never preserve a stale append intent from an earlier `$`/`A`.
        Command::Move(_, _)
        | Command::MoveLeft
        | Command::MoveRight
        | Command::MoveLineStart
        | Command::InsertChar(_)
        | Command::InsertNewline
        | Command::DeleteBack
        | Command::EnterInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::InsertColumnZero
        | Command::OpenBelow
        | Command::OpenAbove
        | Command::EnterReplace
        | Command::EnterVirtualReplace
        // A `/`/`?` search is a definite motion/edit: it establishes the match column (and, for `c?`,
        // the deletion point where Insert begins). It must never preserve a stale `$`/`A` append intent
        // — `$c?pat` inserts at the match, not at the line end.
        | Command::Search { .. } => st.view.curswant = st.wanted_col(),
        // Everything else (edits like `dd`/`x`, mode changes): a one-shot `CTRL-O` command run while
        // `curswant == MAXCOL` that leaves the cursor at the line's end PRESERVES MAXCOL; otherwise recompute
        // the column.
        _ => {
            if !(from_insert && st.view.curswant == MAXCOL && at_line_end(st)) {
                st.view.curswant = st.wanted_col();
            }
        }
    }
    // In Insert, a MAXCOL wanted column parks a caret on the last char at the append position (end of line).
    if insert && st.view.curswant == MAXCOL && at_line_end(st) {
        let b = st.doc.bytes();
        st.view.cursor = line_end(b, st.view.cursor);
    }
}
