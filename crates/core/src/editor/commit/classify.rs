//! Read-only predicates over a [`Command`] that [`super::apply_command`] consults before planning: whether
//! it is a jumplist jump, whether it adds text to the buffer, and whether its delete fills `"1`. Split out
//! of `editor/commit.rs` (#1504).

use super::super::EditorState;
use crate::command::Command;
use crate::motion::Motion;

/// Whether `cmd` is a Vim JUMP — a command that sets the `''`/jumplist "before" mark (search, `G`/`gg`, `%`,
/// a mark jump, paragraph/sentence motion). A plain `h`/`j`/`k`/`l`/word motion is NOT a jump. `CTRL-O`/
/// `CTRL-I` themselves are excluded so navigating the jumplist does not record new entries.
pub(super) fn is_jump(cmd: &Command) -> bool {
    use crate::motion::Motion as M;
    match cmd {
        Command::SearchNext(_)
        | Command::SearchPrev(_)
        // A `/`/`?` search is a jump too, so `''` returns from it (#1088).
        | Command::Search {
            op: crate::SearchOp::Move,
            ..
        }
        | Command::SearchWordUnder {
            op: crate::SearchOp::Move,
            ..
        } => true,
        // `gd`/`gD` (and the `GotoFirstMatch` they resolve to) are jumps — `CTRL-O` returns.
        Command::GotoDeclaration { .. } | Command::GotoFirstMatch(_) => true,
        Command::GotoLastChange
        | Command::GotoLastChangeLine
        | Command::GotoChangeMarkStart
        | Command::GotoChangeMarkEnd
        | Command::GotoChangeMarkStartLine
        | Command::GotoChangeMarkEndLine
        | Command::GotoNamedMark(_)
        | Command::GotoNamedMarkLine(_)
        | Command::GotoContextMark
        | Command::GotoContextMarkLine => true,
        // An operator to a mark sets the context mark where it starts, as the jump does (nvim's
        // `nv_cursormark` calls `setpcmark` under an operator too, #1645): after `` y`a ``, `` `` `` returns.
        Command::OpToMark { .. } => true,
        Command::Move(_, m) => matches!(
            m,
            M::GotoLine
                | M::LastLine
                | M::MatchBracket
                | M::GotoPercent
                | M::ParagraphFwd
                | M::ParagraphBack
                | M::SentenceFwd
                | M::SentenceBack
                | M::SectionFwd
                | M::SectionBack
                | M::SectionEndFwd
                | M::SectionEndBack
        ),
        _ => false,
    }
}

/// Whether `cmd` adds text to the buffer: `o` and `O`, a put of a non-empty register (`p`, `P`, `gp`, `gP`,
/// `]p`, `[p`, `zp`, `zP`), and the text commands of Insert and Replace mode (typed, put or completed).
pub(super) fn adds_text(st: &EditorState, cmd: &Command) -> bool {
    let register = || {
        !st.view
            .registers
            .get(st.view.pending_register)
            .text()
            .is_empty()
    };
    match cmd {
        Command::OpenBelow | Command::OpenAbove | Command::OpenLineIndent { .. } => true,
        Command::Paste { .. }
        | Command::PasteIndent { .. }
        | Command::PasteBlockInner { .. }
        | Command::PasteSelection { .. } => register(),
        Command::InsertChar(_)
        | Command::InsertNewline
        | Command::InsertTab
        | Command::InsertRegister(_)
        | Command::InsertEval(_)
        | Command::InsertPaste(_)
        | Command::InsertCloser { .. }
        | Command::InsertAtLastInsert
        | Command::InsertIndent
        | Command::CompleteWord { .. }
        | Command::ReplaceType(_)
        | Command::VirtualReplaceType(_)
        | Command::VirtualReplaceChar(..)
        | Command::ReplacePaste(_)
        | Command::BlockInsert(_) => true,
        _ => false,
    }
}

/// Whether `cmd` deletes or changes over a motion that always fills `"1` (nvim's `use_reg_one`: `%`, `(`,
/// `)`, `{`, `}`, `` ` ``, `/`, `?`, `n`, `N`, `*`, `#`), even within one line (#749).
pub(super) fn uses_reg_one(cmd: &Command) -> bool {
    use crate::command::{MarkOp, SearchOp};
    let search_op = |op: &SearchOp| matches!(op, SearchOp::Delete | SearchOp::Change);
    match cmd {
        Command::Delete(_, m) | Command::Change(_, m) => matches!(
            m,
            Motion::MatchBracket
                | Motion::ParagraphFwd
                | Motion::ParagraphBack
                | Motion::SentenceFwd
                | Motion::SentenceBack
        ),
        Command::OpToMark { op, .. } => matches!(op, MarkOp::Delete | MarkOp::Change),
        Command::Search { op, .. } | Command::SearchWordUnder { op, .. } => search_op(op),
        _ => false,
    }
}
