//! Dot-repeat (`.`) record type (D-025 / D-047 / F-023): the re-parameterizable change-intent the
//! engine captures and replays. Split out of `input/mod.rs` as a self-contained data definition; the
//! `InputEngine::record` builder that constructs it stays in the engine core.

use ruse_core::{Command, MarkOp, Motion, OpKind, SearchOp, VisualOp};

/// A counted repetition the frontend applies lazily (#683): `head`, then `unit` `times` times, then `tail`.
/// A count-prefixed insert (`999999999ix<Esc>`), `N.` of one, or an Emacs `C-u N` prefix expands to this
/// rather than to a materialised list, so a huge count allocates nothing up front; the frontend checks
/// for CTRL-C between repetitions ([`ruse_core::interrupt`]) and always applies `tail` (the `<Esc>` that
/// leaves Insert), as nvim leaves Insert when an interrupted count-insert stops.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Repeat {
    /// Applied once, first.
    pub head: Vec<Command>,
    /// Applied `times` times after `head`.
    pub unit: Vec<Command>,
    /// How many times `unit` is applied.
    pub times: u32,
    /// Applied once, last — even after an interrupt.
    pub tail: Vec<Command>,
}

impl Repeat {
    /// As a [`Feed`](super::Feed): a plain [`Feed::Replay`](super::Feed::Replay) when nothing repeats.
    #[must_use]
    pub fn into_feed(mut self) -> super::Feed {
        if self.times == 0 || self.unit.is_empty() {
            self.head.append(&mut self.tail);
            super::Feed::Replay(self.head)
        } else {
            super::Feed::Repeat(self)
        }
    }

    /// Every command in order, expanded (for tests and small repeats; the frontend iterates instead).
    #[must_use]
    pub fn expand(&self) -> Vec<Command> {
        let mut out = self.head.clone();
        for _ in 0..self.times {
            out.extend(self.unit.iter().cloned());
        }
        out.extend(self.tail.iter().cloned());
        out
    }
}

/// A recorded **change-intent** for Vim dot-repeat (D-025 / D-047): the buffer-modifying command that
/// began the change, plus — for changes that enter Insert — the exact commands typed until `<Esc>`.
///
/// This is the design's key move: `.` records the INTENT (a re-parameterizable command + text), not a
/// resolved byte range, so replaying it at a new cursor re-runs the motion there. `dw` recorded, then `.`
/// at the next word deletes THAT word; `ciwFOO<Esc>` recorded, then `.` re-does the change AND re-inserts
/// `FOO`. `.` never overwrites the record, so `..` repeats the same change.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ChangeIntent {
    /// The command that began the change: an operator (`dw`, `d2w`), a single-key edit (`x`, `~`, `>>`),
    /// or an insert-entry (`i`/`A`/`o`/`ciw`). Its count is the one `N.` overrides.
    pub(crate) lead: Command,
    /// The insert-session commands captured after an insert-entering `lead`, terminated by the
    /// `EnterNormal` that `<Esc>` produced. Empty for self-contained changes (`dw`, `x`, `>>`).
    pub(crate) insert: Vec<Command>,
    /// The register the change targeted (`"a` before it), replayed so `.` reuses the SAME register (Vim).
    /// `None` for an unregistered change; replay then omits the leading `SetRegister`.
    pub(crate) register: Option<char>,
    /// The expression of a `"=` register (`"=1+1<CR>p`): replay re-arms `"=` with it instead of selecting a
    /// register by name, as nvim's redo buffer holds `"=1+1<CR>` (#808).
    pub(crate) expr: Option<String>,
    /// The count typed before a PURE insert-entry (`3i`, `3o`, `3A`; VIM-CNT-INS): the number of times the
    /// typed text is inserted on `<Esc>`. `1` for a count-less entry and for every change-family entry
    /// (`c`/`s`/`ciw`, whose count applies to the MOTION, not to text repetition). Carried so `.` replays
    /// with the SAME count (Vim), while a leading `N.` overrides it.
    pub(crate) entry_count: u32,
    /// The screen-line motion (and its effective count) the lead's operator ran over (`dgj`, `3dgk`,
    /// `cg$`; #1539). The lead then holds only the span the frontend resolved at the time, so `.`
    /// hands the motion back to the frontend to resolve again where the cursor is now, as nvim's redo
    /// buffer holds the keys `dgj` and not the span. `None` for every other change.
    pub(crate) screen: Option<(super::ScreenMotion, u32)>,
}

impl ChangeIntent {
    /// The ordered commands `.` replays, as a [`Repeat`]: the lead (and register selection), the typed body,
    /// then its extra repeats. `count` — a leading `N` on the `.` — REPLACES the lead's count (Vim `3.`
    /// repeats with count 3); `None` keeps the recorded count. Insert text is replayed verbatim. For a PURE
    /// insert-entry (`3i`/`3o`/`3A`) the count is the number of TIMES the typed text is inserted
    /// (VIM-CNT-INS): `.` re-inserts it `entry_count` times, `N.` overrides that to `N`. The repeats are not
    /// expanded here, so a huge count costs no memory up front (#683).
    pub(crate) fn replay(&self, count: Option<u32>) -> Repeat {
        let lead = match count {
            Some(n) => with_count(&self.lead, n),
            None => self.lead.clone(),
        };
        // How many times to insert the typed body. Only a pure insert-entry repeats its text; a
        // change-family entry's count already rode `with_count` into the motion above, so it stays 1.
        let reps = if self.is_plain_insert_entry() {
            count.unwrap_or(self.entry_count).max(1)
        } else {
            1
        };
        let (body, had_enter) = self.body();
        let mut head =
            Vec::with_capacity(2 + self.insert.len() + usize::from(self.register.is_some()));
        // Re-select the register first, so the replayed change writes to the same slot (`"ax` then `.`).
        if let Some(expr) = &self.expr {
            head.push(Command::SetExprRegister(expr.clone()));
        } else if self.register.is_some() {
            head.push(Command::SetRegister(self.register));
        }
        head.push(lead);
        head.extend(body.iter().cloned());
        Repeat {
            head,
            unit: if reps > 1 {
                self.repeat_unit()
            } else {
                Vec::new()
            },
            times: reps - 1,
            tail: if had_enter {
                vec![Command::EnterNormal]
            } else {
                Vec::new()
            },
        }
    }

    /// The LIVE count-on-insert tail (VIM-CNT-INS) emitted on the terminating `<Esc>` when a count
    /// preceded a pure insert-entry (`3ihello<Esc>`). The lead and the FIRST typed body were already
    /// applied as the keys were typed, so this is the `(entry_count - 1)` extra repeats followed by the
    /// `EnterNormal` that leaves Insert (the original `<Esc>`'s `EnterNormal` is replaced by this list).
    /// `None` when there is nothing to replicate (count <= 1, or not a pure insert-entry).
    pub(crate) fn count_replay_tail(&self) -> Option<Repeat> {
        if self.entry_count <= 1 || !self.is_plain_insert_entry() {
            return None;
        }
        Some(Repeat {
            head: Vec::new(),
            unit: self.repeat_unit(),
            times: self.entry_count - 1,
            tail: vec![Command::EnterNormal],
        })
    }

    /// The typed insert body EXCLUDING the terminating `EnterNormal` — the exact commands `i_CTRL-A`
    /// (Vim's `".` register) re-inserts. Replaying these commands (which may include `DeleteBack`,
    /// `InsertTab`, …) reproduces the RESULTING text of the session, matching nvim's keystroke replay.
    pub(crate) fn insert_body(&self) -> &[Command] {
        self.body().0
    }

    /// The typed insert body — the session commands EXCLUDING the terminating `EnterNormal` — and whether
    /// that `EnterNormal` was present. `.` and the live count-replay both re-issue this body; the split
    /// keeps the `<Esc>` left-shift as the LAST command, after all repeats.
    fn body(&self) -> (&[Command], bool) {
        match self.insert.last() {
            Some(Command::EnterNormal) => (&self.insert[..self.insert.len() - 1], true),
            _ => (&self.insert[..], false),
        }
    }

    /// The commands run once per EXTRA repetition of a count-prefixed insert. For the line-opening entries
    /// (`o`/`O`) every repeat opens a NEW line and re-types the body — and Vim's `3O` repeats DOWNWARD, so
    /// its repeats open BELOW the insertion point exactly like `3o` (verified vs nvim). For the in-place
    /// entries (`i`/`a`/`I`/`A`) a repeat is just the typed body again at the caret.
    fn repeat_unit(&self) -> Vec<Command> {
        let (body, _) = self.body();
        let mut unit = Vec::with_capacity(body.len() + 1);
        if matches!(self.lead, Command::OpenBelow | Command::OpenAbove) {
            unit.push(Command::OpenBelow);
        }
        unit.extend(body.iter().cloned());
        unit
    }

    /// Whether the lead is a PURE insert-entry (`i`/`a`/`I`/`A`/`o`/`O`, and the Replace entries `R`/`gR`)
    /// — the only entries whose count repeats the typed text. A Replace repeat re-types the body in
    /// Replace mode, so it overwrites further on (`3Rxy<Esc>` on `abcdefghij` gives `xyxyxyghij`, nvim;
    /// #1155). Change-family entries (`c`/`C`/`s`/`cc`/`ciw`, `cgn`) are excluded: their count applies to
    /// the operator's motion, never to text repetition.
    fn is_plain_insert_entry(&self) -> bool {
        matches!(
            self.lead,
            Command::EnterInsert
                | Command::EnterInsertAfter
                | Command::InsertLineStart
                | Command::InsertColumnZero
                | Command::AppendLineEnd
                | Command::OpenBelow
                | Command::OpenAbove
                | Command::EnterReplace
                | Command::EnterVirtualReplace
        )
    }
}

/// How a completed command relates to the dot-repeat record.
pub(crate) enum ChangeKind {
    /// Enters Insert; the change is this command PLUS the text typed until `<Esc>`.
    InsertEntering,
    /// A complete buffer edit with no insert session (`dw`, `x`, `dd`, `>>`, `~`, `r`, `p`).
    Immediate,
    /// Not a change (pure motion, mode switch, yank, undo/redo, search) — `.` leaves the record intact.
    NotAChange,
}

/// Classify a completed command for dot-repeat. Per Vim, yank is NOT dot-repeatable; delete/change/put/
/// replace/shift/`~`/join and the insert-entries ARE.
///
/// The match is EXHAUSTIVE on purpose (#793): a wildcard silently filed every new `Command` as
/// `NotAChange`, so `.` replayed an OLDER change after `d/pat`, `` d`a ``, `R`, `gi`. Adding a variant
/// now forces a decision here. Each arm is one command family. The commands that carry an operator
/// (forced-wise, Visual, search, mark) are classified by that operator in [`operator_change_kind`].
///
/// Insert-entering commands are the change PLUS the text typed until `<Esc>`. `gi` is recorded as a plain
/// `i` (Vim's `nv_edit` rewrites `gi` to `i` after the jump), see `InputEngine::record`. `R` / `gR`: the
/// typed overwrites are the change body, replayed at the new cursor. Visual-block `I`/`A`/`c` (`CTRL-V`
/// then the key): the change is the block-insert entry plus the text typed until `<Esc>`. On replay the
/// block has no live selection, so the planner rebuilds an equivalent block (stored WIDTH x HEIGHT) at the
/// caret — nvim repeats a block insert over the same geometry positioned at the new cursor (verified vs
/// nvim v0.12.4). `[count].` is a no-op there (nvim ignores the count for block insert), which falls out
/// naturally: `BlockInsert` is not a plain insert-entry, so `replay` neither re-times the text nor (via
/// `with_count`) rewrites the command.
///
/// Not a change: yanks, motions and jumps, mode switches, searches, undo/redo, marks/registers, info
/// displays, `&`/`g&` (Vim does not dot-repeat them), and the insert-session-only commands — those reach
/// the recorder only inside Insert/Replace, where they are appended to the open change's body instead of
/// being classified here.
pub(crate) fn change_kind(cmd: &Command) -> ChangeKind {
    use ChangeKind as K;
    use Command as C;
    match cmd {
        // Operator-carrying: `dvj`, Visual `C`/`D`/`>`, `d/pat<CR>`, `cgn`, `c*`, `` d`a ``, ...
        C::OpForced { .. } | C::VisualOp(_) | C::OpToMark { .. } => operator_change_kind(cmd),
        C::Search { .. } | C::SearchObject { .. } | C::SearchWordUnder { .. } => {
            operator_change_kind(cmd)
        }
        // Insert-entering: `i`/`a`/`I`/`gI`/`A`/`o`/`O`, `gi`, `R`/`gR`, `c`, Visual `c`/`s`/`R`, block `I`.
        C::EnterInsert | C::EnterInsertAfter | C::InsertLineStart | C::InsertColumnZero => {
            K::InsertEntering
        }
        C::AppendLineEnd | C::OpenBelow | C::OpenAbove | C::InsertAtLastInsert => K::InsertEntering,
        C::EnterReplace | C::EnterVirtualReplace => K::InsertEntering,
        C::Change(..) | C::ChangeSelection | C::ReplaceSelection(_) => K::InsertEntering,
        C::BlockInsert(_) => K::InsertEntering,
        // Immediate Vim edits — dot-repeatable as a single command.
        C::Delete(..) | C::DeleteUnder(_) | C::DeleteForward(_) | C::DeleteBack => K::Immediate,
        C::ReplaceChar(..) | C::VirtualReplaceChar(..) | C::ReplaceSelectionChar(_) => K::Immediate,
        C::ToggleCase(_) | C::CaseMotion { .. } | C::CaseSelection(_) => K::Immediate,
        C::JoinLines(_) | C::JoinLinesNoSpace(_) => K::Immediate,
        C::IncrementNumber(_) | C::IncrementSelection { .. } => K::Immediate,
        C::ShiftRight(_) | C::ShiftLeft(_) | C::ShiftMotion { .. } => K::Immediate,
        C::Reindent { .. } | C::Format { .. } | C::Comment { .. } => K::Immediate,
        C::DeleteSelection => K::Immediate,
        // Immediate puts.
        C::Paste { .. } | C::PasteIndent { .. } | C::PasteBlockInner { .. } => K::Immediate,
        C::PasteSelection { .. } | C::EmacsYank { .. } => K::Immediate,
        // Immediate Emacs edits.
        C::EmacsKillLine | C::EmacsKillWord { .. } | C::EmacsBackwardKillWord { .. } => {
            K::Immediate
        }
        C::EmacsKillWholeLine | C::EmacsTransposeChars | C::EmacsTransposeWords => K::Immediate,
        C::EmacsCaseWord { .. } | C::EmacsCaseRegion { .. } => K::Immediate,
        C::EmacsDeleteIndentation | C::EmacsHorizontalSpace { .. } | C::EmacsOpenLine => {
            K::Immediate
        }
        // Not a change — yanks and motions.
        C::Yank(..) | C::YankSelection => K::NotAChange,
        C::Move(..) | C::MoveLeft | C::MoveRight | C::MoveUp | C::MoveDown => K::NotAChange,
        C::MoveLineStart | C::MoveLineEnd => K::NotAChange,
        // Not a change — mode switches and Visual selection state.
        C::EnterNormal | C::EnterTerminal | C::EnterTerminalNormal => K::NotAChange,
        C::EnterVisual { .. } | C::EnterSelect { .. } => K::NotAChange,
        C::ReselectVisual | C::SwapSelectionEnds => K::NotAChange,
        // Not a change — insert-session-only commands (appended to the open change's body).
        C::ReplaceType(_) | C::ReplaceBackspace | C::VirtualReplaceType(_) => K::NotAChange,
        C::InsertChar(_) | C::InsertRegister(_) | C::InsertEval(_) => K::NotAChange,
        C::InsertPaste(_) | C::ReplacePaste(_) | C::EmacsPaste(_) => K::NotAChange,
        C::InsertDeleteWordBack | C::InsertDeleteToLineStart | C::CompleteWord { .. } => {
            K::NotAChange
        }
        C::InsertIndent | C::InsertDedent => K::NotAChange,
        C::InsertDeleteIndent | C::InsertDeleteIndentRestore => K::NotAChange,
        C::InsertStartColumnLine { .. } | C::InsertCaretStep { .. } => K::NotAChange,
        C::InsertTab | C::InsertNewline | C::OpenLineIndent { .. } => K::NotAChange,
        C::InsertCloser { .. } | C::BreakUndo | C::InsertReindent => K::NotAChange,
        C::SetIndents { .. } | C::SetIndentCols { .. } => K::NotAChange,
        // Not a change — change-list, mark and jump-list jumps.
        C::GotoLastChange | C::GotoLastChangeLine => K::NotAChange,
        C::GotoChangeMarkStart | C::GotoChangeMarkEnd => K::NotAChange,
        C::GotoChangeMarkStartLine | C::GotoChangeMarkEndLine => K::NotAChange,
        C::GotoVisualMarkStart | C::GotoVisualMarkEnd => K::NotAChange,
        C::GotoVisualMarkStartLine | C::GotoVisualMarkEndLine => K::NotAChange,
        C::GotoContextMark | C::GotoContextMarkLine => K::NotAChange,
        C::GotoOlderChange | C::GotoNewerChange | C::GotoOlderJump | C::GotoNewerJump => {
            K::NotAChange
        }
        // Not a change — info displays, marks and registers.
        C::AsciiInfo | C::Utf8Info | C::FileInfo | C::CursorInfo => K::NotAChange,
        C::SetNamedMark(_) | C::GotoNamedMark(_) | C::GotoNamedMarkLine(_) => K::NotAChange,
        C::SetRegister(_) | C::SetExprRegister(_) => K::NotAChange,
        // Not a change — searches, `&`/`g&`, Ex, keyword lookups.
        C::SearchNext(_) | C::SearchPrev(_) => K::NotAChange,
        C::RepeatSubstituteGlobal | C::RepeatSubstituteLine | C::Ex(_) => K::NotAChange,
        C::GotoDeclaration { .. } | C::GotoFirstMatch(_) | C::ShowKeywordLines { .. } => {
            K::NotAChange
        }
        // Not a change — undo/redo, file and session.
        C::Undo | C::Redo | C::UndoOlder | C::UndoNewer | C::LineUndo => K::NotAChange,
        C::Save | C::Quit => K::NotAChange,
        // Not a change — Emacs mark and region commands.
        C::SetMark | C::KillRegion | C::CopyRegion | C::ExchangePointMark => K::NotAChange,
        C::EmacsBufferEdge { .. } | C::EmacsMarkWord => K::NotAChange,
    }
}

/// [`change_kind`] of the operator-carrying family: each command is classified by the operator it carries,
/// in a helper whose own match is exhaustive over that operator enum.
fn operator_change_kind(cmd: &Command) -> ChangeKind {
    use Command as C;
    match cmd {
        C::OpForced { op, .. } => op_change_kind(*op),
        // Visual `C`/`S`/`R`, `D`/`X`, `>`/`<`, `J`/`gJ`, `=`, `gq`, ...
        C::VisualOp(op) => visual_op_change_kind(*op),
        // `c/pat<CR>`, `d/pat<CR>`, `cgn`, `dgn`, `c*`, `d*`, ... — re-searched from the new cursor.
        C::Search { op, .. } | C::SearchObject { op, .. } | C::SearchWordUnder { op, .. } => {
            search_op_change_kind(*op)
        }
        // `` c`a ``, `` d`a ``/`d'a`, `` g~`a ``, `` >`a ``, ... — the mark is re-resolved on replay.
        C::OpToMark { op, .. } => mark_op_change_kind(*op),
        _ => unreachable!("operator_change_kind called with a command that carries no operator"),
    }
}

/// [`change_kind`] of a forced-wise operator (`dvj`, `cVk`, `y<C-v>j`), and of the Visual linewise /
/// block-to-end-of-line forms, which carry the same three operators.
fn op_change_kind(op: OpKind) -> ChangeKind {
    match op {
        OpKind::Change => ChangeKind::InsertEntering,
        OpKind::Delete => ChangeKind::Immediate,
        OpKind::Yank => ChangeKind::NotAChange,
    }
}

/// [`change_kind`] of a Visual operator. Visual `C`/`S`/`R` (linewise) and block `C` (to end of line) are
/// the change plus the typed text; `D`/`X`, `>`/`<`, `J`/`gJ`, `=`, `gq` re-cover the recorded amount of
/// text; the yanks and the cursor-only forms are not changes.
fn visual_op_change_kind(op: VisualOp) -> ChangeKind {
    match op {
        VisualOp::Lines(kind) | VisualOp::BlockToEol(kind) => op_change_kind(kind),
        VisualOp::Shift { .. }
        | VisualOp::Join { .. }
        | VisualOp::Reindent
        | VisualOp::Format
        | VisualOp::Comment => ChangeKind::Immediate,
        VisualOp::OtherCorner | VisualOp::YankTrim | VisualOp::LeaveAtStart => {
            ChangeKind::NotAChange
        }
    }
}

/// [`change_kind`] of an operator over a search (`c/pat<CR>`, `d/pat<CR>`, `gU/pat<CR>`, `>/pat<CR>`), a
/// `gn` match or a `*`/`#` keyword — all three are re-searched from the new cursor on replay. `cgn`/`cgN`
/// is the gn idiom: dot replays the change (re-searching from the new cursor), so `n.` (or bare `.`) walks
/// the change through every match; `dgn`/`dgN` likewise sweeps the next match. `c*`/`d*` re-read the
/// keyword under the new cursor (nvim `d*.`, #1071).
fn search_op_change_kind(op: SearchOp) -> ChangeKind {
    match op {
        SearchOp::Change => ChangeKind::InsertEntering,
        SearchOp::Delete
        | SearchOp::Case(_)
        | SearchOp::Shift { .. }
        | SearchOp::Reindent
        | SearchOp::Format { .. }
        | SearchOp::Comment => ChangeKind::Immediate,
        SearchOp::Move | SearchOp::Yank => ChangeKind::NotAChange,
    }
}

/// [`change_kind`] of an operator to a mark (`` c`a ``, `` d`a ``/`d'a`, `` g~`a ``, `` >`a ``, `` =`a ``,
/// `` gq`a ``) — the mark is re-resolved from the new cursor on replay.
fn mark_op_change_kind(op: MarkOp) -> ChangeKind {
    match op {
        MarkOp::Change => ChangeKind::InsertEntering,
        MarkOp::Delete
        | MarkOp::Case(_)
        | MarkOp::Shift { .. }
        | MarkOp::Reindent
        | MarkOp::Format { .. }
        | MarkOp::Comment => ChangeKind::Immediate,
        MarkOp::Yank => ChangeKind::NotAChange,
    }
}

/// Whether a failure of `cmd` (the core beeped) cancels it before nvim saves the redo buffer, so the
/// change it was about to replace stays the one `.` repeats (#1179, [`InputEngine::change_failed`]).
/// Every operator over a motion, search, `gn`, `*` or mark: its motion handler fails with `clearopbeep`
/// (or `clearop`) and `do_pending_operator`, where `prep_redo` runs, never sees the operator. `J`
/// (`nv_join`), `r` (`nv_replace`) and `~` (`n_swapchar`) bail out before their own `prep_redo`; a `J`
/// count past the last line is clamped first and saved (`x3J.` on one line repeats the `3J`, nvim). The
/// rest is saved before it can fail: `x` (`dl`, which never fails under an operator), `CTRL-A`
/// (`nv_addsub`), a put (`nv_put`), the Visual operators and the insert entries.
///
/// [`InputEngine::change_failed`]: super::InputEngine::change_failed
pub(crate) fn fails_before_redo(cmd: &Command) -> bool {
    use Command as C;
    match cmd {
        C::JoinLines(n) | C::JoinLinesNoSpace(n) => *n <= 2,
        C::Delete(..)
        | C::Change(..)
        | C::CaseMotion { .. }
        | C::ShiftMotion { .. }
        | C::Reindent { .. }
        | C::Format { .. }
        | C::Comment { .. }
        | C::OpForced { .. }
        | C::Search { .. }
        | C::SearchObject { .. }
        | C::SearchWordUnder { .. }
        | C::OpToMark { .. }
        | C::ReplaceChar(..)
        | C::VirtualReplaceChar(..)
        | C::ToggleCase(_) => true,
        _ => false,
    }
}

/// Rewrite a command's count for `N.` (Vim replaces the change's count with `N`). Commands without a count
/// are returned unchanged.
///
/// Exhaustive like [`change_kind`] (#793): each counted family is rewritten by its own helper, and the
/// count-free commands are listed family by family.
pub(crate) fn with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::Move(..) | C::Delete(..) | C::Change(..) | C::Yank(..) | C::OpForced { .. } => {
            motion_op_with_count(cmd, n)
        }
        C::DeleteUnder(_)
        | C::DeleteForward(_)
        | C::ReplaceChar(..)
        | C::VirtualReplaceChar(..)
        | C::ToggleCase(_)
        | C::ShiftRight(_)
        | C::ShiftLeft(_)
        | C::JoinLines(_)
        | C::JoinLinesNoSpace(_)
        | C::IncrementNumber(_) => counted_edit_with_count(cmd, n),
        C::CaseMotion { .. }
        | C::ShiftMotion { .. }
        | C::Reindent { .. }
        | C::Comment { .. }
        | C::Format { .. } => line_op_with_count(cmd, n),
        C::Paste { .. } | C::PasteIndent { .. } | C::PasteBlockInner { .. } => {
            put_with_count(cmd, n)
        }
        C::Search { .. } | C::SearchObject { .. } | C::SearchWordUnder { .. } => {
            search_with_count(cmd, n)
        }
        // No count to rewrite. Listed (not a wildcard) so a new counted variant cannot silently drop
        // `N.` (#793). The Visual-selection changes are here by design: nvim ignores a count on `.` of a
        // Visual change (the repeat re-covers the recorded AMOUNT of text, `:help visual-repeat`).
        //
        // Count-free — uncounted motions, mode switches and Visual selection state.
        C::MoveLeft | C::MoveRight | C::MoveUp | C::MoveDown => cmd.clone(),
        C::MoveLineStart | C::MoveLineEnd => cmd.clone(),
        C::EnterNormal | C::EnterTerminal | C::EnterTerminalNormal => cmd.clone(),
        C::EnterVisual { .. } | C::EnterSelect { .. } => cmd.clone(),
        C::ReselectVisual | C::SwapSelectionEnds => cmd.clone(),
        // Count-free — insert entries.
        C::EnterInsert | C::EnterInsertAfter | C::InsertLineStart => cmd.clone(),
        C::InsertColumnZero | C::AppendLineEnd | C::OpenBelow | C::OpenAbove => cmd.clone(),
        C::EnterReplace | C::EnterVirtualReplace | C::InsertAtLastInsert => cmd.clone(),
        C::BlockInsert(..) => cmd.clone(),
        // Count-free — insert-session-only commands.
        C::ReplaceType(..) | C::ReplaceBackspace | C::VirtualReplaceType(..) => cmd.clone(),
        C::InsertChar(..) | C::InsertRegister(..) | C::InsertEval(..) => cmd.clone(),
        C::InsertPaste(..) | C::ReplacePaste(..) | C::EmacsPaste(..) => cmd.clone(),
        C::InsertDeleteWordBack | C::InsertDeleteToLineStart => cmd.clone(),
        C::CompleteWord { .. } | C::InsertIndent | C::InsertDedent => cmd.clone(),
        C::InsertDeleteIndent | C::InsertDeleteIndentRestore => cmd.clone(),
        C::InsertStartColumnLine { .. } | C::InsertCaretStep { .. } => cmd.clone(),
        C::InsertTab | C::InsertNewline | C::OpenLineIndent { .. } => cmd.clone(),
        C::InsertCloser { .. } | C::DeleteBack | C::BreakUndo | C::InsertReindent => cmd.clone(),
        C::SetIndents { .. } | C::SetIndentCols { .. } => cmd.clone(),
        // Count-free — Visual-selection changes (`:help visual-repeat`, above).
        C::ReplaceSelectionChar(..) | C::CaseSelection(..) => cmd.clone(),
        C::IncrementSelection { .. } | C::PasteSelection { .. } => cmd.clone(),
        C::DeleteSelection | C::YankSelection | C::ChangeSelection => cmd.clone(),
        C::ReplaceSelection(..) | C::VisualOp(..) => cmd.clone(),
        // Count-free — change-list, mark and jump-list jumps.
        C::GotoLastChange | C::GotoLastChangeLine => cmd.clone(),
        C::GotoChangeMarkStart | C::GotoChangeMarkEnd => cmd.clone(),
        C::GotoChangeMarkStartLine | C::GotoChangeMarkEndLine => cmd.clone(),
        C::GotoVisualMarkStart | C::GotoVisualMarkEnd => cmd.clone(),
        C::GotoVisualMarkStartLine | C::GotoVisualMarkEndLine => cmd.clone(),
        C::GotoContextMark | C::GotoContextMarkLine => cmd.clone(),
        C::GotoOlderChange | C::GotoNewerChange => cmd.clone(),
        C::GotoOlderJump | C::GotoNewerJump => cmd.clone(),
        // Count-free — info displays, marks and registers.
        C::AsciiInfo | C::Utf8Info | C::FileInfo | C::CursorInfo => cmd.clone(),
        C::SetNamedMark(..) | C::GotoNamedMark(..) | C::GotoNamedMarkLine(..) => cmd.clone(),
        C::OpToMark { .. } => cmd.clone(),
        C::SetRegister(..) | C::SetExprRegister(..) => cmd.clone(),
        // Count-free — `n`/`N`, `&`/`g&`, Ex, keyword lookups.
        C::SearchNext(..) | C::SearchPrev(..) => cmd.clone(),
        C::RepeatSubstituteGlobal | C::RepeatSubstituteLine | C::Ex(_) => cmd.clone(),
        C::GotoDeclaration { .. } | C::GotoFirstMatch(..) => cmd.clone(),
        C::ShowKeywordLines { .. } => cmd.clone(),
        // Count-free — undo/redo, file and session.
        C::Undo | C::Redo | C::UndoOlder | C::UndoNewer | C::LineUndo => cmd.clone(),
        C::Save | C::Quit => cmd.clone(),
        // Count-free — Emacs mark, region and edit commands.
        C::SetMark | C::KillRegion | C::CopyRegion | C::ExchangePointMark => cmd.clone(),
        C::EmacsYank { .. } | C::EmacsBufferEdge { .. } | C::EmacsMarkWord => cmd.clone(),
        C::EmacsKillLine | C::EmacsKillWholeLine => cmd.clone(),
        C::EmacsKillWord { .. } | C::EmacsBackwardKillWord { .. } => cmd.clone(),
        C::EmacsTransposeChars | C::EmacsTransposeWords => cmd.clone(),
        C::EmacsCaseWord { .. } | C::EmacsCaseRegion { .. } => cmd.clone(),
        C::EmacsHorizontalSpace { .. } | C::EmacsOpenLine => cmd.clone(),
        C::EmacsDeleteIndentation => cmd.clone(),
    }
}

/// [`with_count`] of a motion and of the `d`/`c`/`y` operators over one, plain or forced-wise.
fn motion_op_with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::Move(_, m) => C::Move(n, *m),
        C::Delete(_, m) => C::Delete(n, *m),
        C::Change(_, m) => C::Change(n, *m),
        C::Yank(_, m) => C::Yank(n, *m),
        C::OpForced {
            op, motion, wise, ..
        } => C::OpForced {
            op: *op,
            count: n,
            motion: *motion,
            wise: *wise,
        },
        _ => unreachable!("with_count routes only motions and d/c/y operators here"),
    }
}

/// The operator of `cmd` over motion `m` with count `n` instead of its own: the lead of a screen-line
/// change ([`ChangeIntent::screen`]) with the span the frontend resolved for `.` (#1539). Only the
/// operators [`InputEngine::op_over_motion`](super::InputEngine) builds a change from carry a motion
/// here; anything else is returned unchanged.
pub(crate) fn with_motion(cmd: &Command, m: Motion, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::Delete(..) => C::Delete(n, m),
        C::Change(..) => C::Change(n, m),
        C::OpForced { op, wise, .. } => C::OpForced {
            op: *op,
            count: n,
            motion: m,
            wise: *wise,
        },
        C::CaseMotion { case, .. } => C::CaseMotion {
            count: n,
            motion: m,
            case: *case,
        },
        C::ShiftMotion { left, .. } => C::ShiftMotion {
            left: *left,
            count: n,
            motion: m,
        },
        C::Reindent { .. } => C::Reindent {
            count: n,
            motion: m,
        },
        C::Comment { .. } => C::Comment {
            count: n,
            motion: m,
        },
        C::Format { keep_cursor, .. } => C::Format {
            count: n,
            motion: m,
            keep_cursor: *keep_cursor,
        },
        other => other.clone(),
    }
}

/// `cmd` over its exclusive charwise motion made inclusive (#1576), as Vim's `o_v` toggles it: `d`, `c`
/// and `y` gain a forced charwise wise, and a forced charwise one loses it (`dvgk` after `$` is
/// exclusive in nvim). The backward `gk` after `$` is inclusive, so it also takes the cursor's char,
/// which no single exclusive target can name. Other commands are returned unchanged.
pub(crate) fn toggle_inclusive(cmd: &Command) -> Command {
    use ruse_core::ForcedWise;
    use Command as C;
    let forced = |op, count: &u32, motion: &Motion| C::OpForced {
        op,
        count: *count,
        motion: *motion,
        wise: ForcedWise::Charwise,
    };
    match cmd {
        C::Delete(n, m) => forced(OpKind::Delete, n, m),
        C::Change(n, m) => forced(OpKind::Change, n, m),
        C::Yank(n, m) => forced(OpKind::Yank, n, m),
        C::OpForced {
            op,
            count,
            motion,
            wise: ForcedWise::Charwise,
        } => match op {
            OpKind::Delete => C::Delete(*count, *motion),
            OpKind::Change => C::Change(*count, *motion),
            OpKind::Yank => C::Yank(*count, *motion),
        },
        other => other.clone(),
    }
}

/// [`with_count`] of a counted edit with no motion (`x`, `X`, `r`, `gr`, `~`, `>>`, `<<`, `J`, `gJ`,
/// `CTRL-A`/`CTRL-X`).
fn counted_edit_with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::DeleteUnder(_) => C::DeleteUnder(n),
        C::DeleteForward(_) => C::DeleteForward(n),
        C::ReplaceChar(_, c) => C::ReplaceChar(n, *c),
        C::VirtualReplaceChar(_, c) => C::VirtualReplaceChar(n, *c),
        C::ToggleCase(_) => C::ToggleCase(n),
        C::ShiftRight(_) => C::ShiftRight(n),
        C::ShiftLeft(_) => C::ShiftLeft(n),
        C::JoinLines(_) => C::JoinLines(n),
        C::JoinLinesNoSpace(_) => C::JoinLinesNoSpace(n),
        // `CTRL-A`/`CTRL-X` carry the count as a signed delta; `5.` keeps the direction.
        C::IncrementNumber(d) => {
            C::IncrementNumber(if *d < 0 { -i64::from(n) } else { i64::from(n) })
        }
        _ => unreachable!("with_count routes only motionless counted edits here"),
    }
}

/// [`with_count`] of a case, shift, reindent, comment or format operator over a motion.
fn line_op_with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::CaseMotion { motion, case, .. } => C::CaseMotion {
            count: n,
            motion: *motion,
            case: *case,
        },
        C::ShiftMotion { left, motion, .. } => C::ShiftMotion {
            left: *left,
            count: n,
            motion: *motion,
        },
        C::Reindent { motion, .. } => C::Reindent {
            count: n,
            motion: *motion,
        },
        C::Comment { motion, .. } => C::Comment {
            count: n,
            motion: *motion,
        },
        C::Format {
            motion,
            keep_cursor,
            ..
        } => C::Format {
            count: n,
            motion: *motion,
            keep_cursor: *keep_cursor,
        },
        _ => {
            unreachable!("with_count routes only case/shift/reindent/comment/format operators here")
        }
    }
}

/// [`with_count`] of a put (`p`/`P`, `gp`/`gP`, `]p`/`[p`, and the block-inner put).
fn put_with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::Paste {
            after, move_after, ..
        } => C::Paste {
            after: *after,
            count: n,
            move_after: *move_after,
        },
        C::PasteIndent { after, .. } => C::PasteIndent {
            after: *after,
            count: n,
        },
        C::PasteBlockInner { after, .. } => C::PasteBlockInner {
            after: *after,
            count: n,
        },
        _ => unreachable!("with_count routes only puts here"),
    }
}

/// [`with_count`] of a search motion or an operator over one: `/pat`, `gn`, `*`/`#`.
fn search_with_count(cmd: &Command, n: u32) -> Command {
    use Command as C;
    match cmd {
        C::Search {
            op,
            pattern,
            backward,
            offset,
            ..
        } => C::Search {
            op: *op,
            count: n,
            pattern: pattern.clone(),
            backward: *backward,
            offset: *offset,
        },
        C::SearchObject {
            op,
            pattern,
            backward,
            ..
        } => C::SearchObject {
            op: *op,
            count: n,
            pattern: pattern.clone(),
            backward: *backward,
        },
        C::SearchWordUnder {
            forward,
            whole_word,
            op,
            ..
        } => C::SearchWordUnder {
            forward: *forward,
            whole_word: *whole_word,
            op: *op,
            count: n,
        },
        _ => unreachable!("with_count routes only searches here"),
    }
}
