//! Dot-repeat (`.`) record type (D-025 / D-047 / F-023): the re-parameterizable change-intent the
//! engine captures and replays. Split out of `input/mod.rs` as a self-contained data definition; the
//! `InputEngine::record` builder that constructs it stays in the engine core.

use ruse_core::{Command, MarkOp, OpKind, SearchOp, VisualOp};

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
/// now forces a decision here.
pub(crate) fn change_kind(cmd: &Command) -> ChangeKind {
    use Command as C;
    match cmd {
        // Insert-entering: the change includes the text typed until `<Esc>`.
        C::EnterInsert
        | C::EnterInsertAfter
        | C::InsertLineStart
        | C::InsertColumnZero
        | C::AppendLineEnd
        | C::OpenBelow
        | C::OpenAbove
        // `gi` — recorded as a plain `i` (Vim's `nv_edit` rewrites `gi` to `i` after the jump), see
        // `InputEngine::record`.
        | C::InsertAtLastInsert
        // `R` / `gR`: the typed overwrites are the change body, replayed at the new cursor.
        | C::EnterReplace
        | C::EnterVirtualReplace
        | C::Change(..)
        | C::ChangeSelection
        | C::ReplaceSelection(_)
        // Visual `C`/`S`/`R` (linewise) and block `C` (to end of line): the change plus the typed text.
        | C::VisualOp(VisualOp::Lines(OpKind::Change) | VisualOp::BlockToEol(OpKind::Change))
        // Visual-block `I`/`A`/`c` (`CTRL-V` then the key): the change is the block-insert entry plus the
        // text typed until `<Esc>`. On replay the block has no live selection, so the planner rebuilds an
        // equivalent block (stored WIDTH x HEIGHT) at the caret — nvim repeats a block insert over the same
        // geometry positioned at the new cursor (verified vs nvim v0.12.4). `[count].` is a no-op here (nvim
        // ignores the count for block insert), which falls out naturally: `BlockInsert` is not a plain
        // insert-entry, so `replay` neither re-times the text nor (via `with_count`) rewrites the command.
        | C::BlockInsert(_)
        | C::OpForced {
            op: OpKind::Change, ..
        }
        // `c/pat<CR>`, `` c`a ``: the change re-searches / re-resolves the mark from the new cursor.
        | C::Search {
            op: SearchOp::Change,
            ..
        }
        | C::OpToMark {
            op: MarkOp::Change,
            ..
        }
        // `cgn`/`cgN` — the gn idiom: dot replays the change (re-searching from the new cursor), so `n.`
        // (or bare `.`) walks the change through every match. This is why gn matters.
        | C::SearchObject {
            op: SearchOp::Change,
            ..
        }
        // `c*` — like `c/pat`; the keyword is re-read at the new cursor (#1071).
        | C::SearchWordUnder {
            op: SearchOp::Change,
            ..
        } => ChangeKind::InsertEntering,
        // Self-contained buffer edits — dot-repeatable as a single command.
        C::Delete(..)
        | C::DeleteUnder(_)
        | C::DeleteForward(_)
        | C::DeleteBack
        | C::ReplaceChar(..)
        | C::VirtualReplaceChar(..)
        | C::ReplaceSelectionChar(_)
        | C::ToggleCase(_)
        | C::CaseMotion { .. }
        | C::JoinLines(_)
        | C::JoinLinesNoSpace(_)
        | C::CaseSelection(_)
        | C::IncrementNumber(_)
        | C::IncrementSelection { .. }
        | C::ShiftRight(_)
        | C::ShiftLeft(_)
        | C::ShiftMotion { .. }
        | C::Reindent { .. }
        | C::Format { .. }
        | C::Comment { .. }
        | C::Paste { .. }
        | C::PasteIndent { .. }
        | C::PasteBlockInner { .. }
        | C::EmacsYank { .. }
        | C::EmacsKillLine
        | C::EmacsKillWord { .. }
        | C::EmacsBackwardKillWord { .. }
        | C::EmacsKillWholeLine
        | C::EmacsTransposeChars
        | C::EmacsTransposeWords
        | C::EmacsCaseWord { .. }
        | C::EmacsCaseRegion { .. }
        | C::EmacsDeleteIndentation
        | C::EmacsHorizontalSpace { .. }
        | C::EmacsOpenLine
        | C::DeleteSelection
        | C::PasteSelection { .. }
        // Visual `D`/`X`, `>`/`<`, `J`/`gJ`, `=`, `gq` — the repeat re-covers the recorded amount of text.
        | C::VisualOp(
            VisualOp::Lines(OpKind::Delete)
            | VisualOp::BlockToEol(OpKind::Delete)
            | VisualOp::Shift { .. }
            | VisualOp::Join { .. }
            | VisualOp::Reindent
            | VisualOp::Format
            | VisualOp::Comment,
        )
        | C::OpForced {
            op: OpKind::Delete, ..
        }
        // `d/pat<CR>`, `gU/pat<CR>`, `>/pat<CR>` — the search motion is re-run from the new cursor.
        | C::Search {
            op:
                SearchOp::Delete
                | SearchOp::Case(_)
                | SearchOp::Shift { .. }
                | SearchOp::Reindent
                | SearchOp::Format { .. }
                | SearchOp::Comment,
            ..
        }
        // `` d`a ``/`d'a`, `` g~`a ``, `` >`a ``, `` =`a ``, `` gq`a `` — the mark is re-resolved on replay.
        | C::OpToMark {
            op:
                MarkOp::Delete
                | MarkOp::Case(_)
                | MarkOp::Shift { .. }
                | MarkOp::Reindent
                | MarkOp::Format { .. }
                | MarkOp::Comment,
            ..
        }
        // `dgn`/`dgN` — delete the match; dot-repeatable to sweep the next match. Likewise `gUgn`, `>gn`, ...
        | C::SearchObject {
            op:
                SearchOp::Delete
                | SearchOp::Case(_)
                | SearchOp::Shift { .. }
                | SearchOp::Reindent
                | SearchOp::Format { .. }
                | SearchOp::Comment,
            ..
        }
        // `d*`, `gU#`, `>g*` — re-searched for the keyword under the new cursor (nvim `d*.`, #1071).
        | C::SearchWordUnder {
            op:
                SearchOp::Delete
                | SearchOp::Case(_)
                | SearchOp::Shift { .. }
                | SearchOp::Reindent
                | SearchOp::Format { .. }
                | SearchOp::Comment,
            ..
        } => ChangeKind::Immediate,
        // Not a change: yanks (incl. forced, search and mark yanks), motions and jumps, mode switches,
        // searches, undo/redo, marks/registers, info displays, `&`/`g&` (Vim does not dot-repeat them),
        // and the insert-session-only commands — those reach the recorder only inside Insert/Replace,
        // where they are appended to the open change's body instead of being classified here.
        C::Yank(..)
        | C::YankSelection
        | C::VisualOp(
            VisualOp::Lines(OpKind::Yank)
            | VisualOp::BlockToEol(OpKind::Yank)
            | VisualOp::OtherCorner
            | VisualOp::YankTrim
            | VisualOp::LeaveAtStart,
        )
        | C::OpForced {
            op: OpKind::Yank, ..
        }
        | C::Search {
            op: SearchOp::Move | SearchOp::Yank,
            ..
        }
        | C::SearchObject {
            op: SearchOp::Move | SearchOp::Yank,
            ..
        }
        | C::SearchWordUnder {
            op: SearchOp::Move | SearchOp::Yank,
            ..
        }
        | C::OpToMark {
            op: MarkOp::Yank,
            ..
        }
        | C::Move(..)
        | C::MoveLeft
        | C::MoveRight
        | C::MoveUp
        | C::MoveDown
        | C::MoveLineStart
        | C::MoveLineEnd
        | C::EnterNormal
        | C::EnterTerminal
        | C::EnterTerminalNormal
        | C::EnterVisual { .. }
        | C::EnterSelect { .. }
        | C::ReselectVisual
        | C::SwapSelectionEnds
        | C::ReplaceType(_)
        | C::ReplaceBackspace
        | C::VirtualReplaceType(_)
        | C::InsertChar(_)
        | C::InsertRegister(_)
        | C::InsertEval(_)
        | C::InsertPaste(_)
        | C::ReplacePaste(_)
        | C::EmacsPaste(_)
        | C::InsertDeleteWordBack
        | C::InsertDeleteToLineStart
        | C::CompleteWord { .. }
        | C::InsertIndent
        | C::InsertDedent
        | C::InsertDeleteIndent
        | C::InsertDeleteIndentRestore
        | C::InsertStartColumnLine { .. }
        | C::InsertCaretStep { .. }
        | C::InsertTab
        | C::InsertNewline
        | C::OpenLineIndent { .. }
        | C::InsertCloser { .. }
        | C::BreakUndo
        | C::SetIndents { .. }
        | C::GotoLastChange
        | C::GotoLastChangeLine
        | C::GotoChangeMarkStart
        | C::GotoChangeMarkEnd
        | C::GotoChangeMarkStartLine
        | C::GotoChangeMarkEndLine
        | C::GotoVisualMarkStart
        | C::GotoVisualMarkEnd
        | C::GotoVisualMarkStartLine
        | C::GotoVisualMarkEndLine
        | C::GotoContextMark
        | C::GotoContextMarkLine
        | C::GotoOlderChange
        | C::GotoNewerChange
        | C::GotoOlderJump
        | C::GotoNewerJump
        | C::AsciiInfo
        | C::Utf8Info
        | C::FileInfo
        | C::CursorInfo
        | C::SetNamedMark(_)
        | C::GotoNamedMark(_)
        | C::GotoNamedMarkLine(_)
        | C::SetRegister(_)
        | C::SetExprRegister(_)
        | C::SearchNext(_)
        | C::SearchPrev(_)
        | C::RepeatSubstituteGlobal
        | C::RepeatSubstituteLine
        | C::Ex(_)
        | C::GotoDeclaration { .. }
        | C::GotoFirstMatch(_)
        | C::ShowKeywordLines { .. }
        | C::Undo
        | C::Redo
        | C::UndoOlder
        | C::UndoNewer
        | C::LineUndo
        | C::Save
        | C::Quit
        | C::SetMark
        | C::KillRegion
        | C::CopyRegion
        | C::ExchangePointMark
        | C::EmacsBufferEdge { .. }
        | C::EmacsMarkWord => ChangeKind::NotAChange,
    }
}

/// Rewrite a command's count for `N.` (Vim replaces the change's count with `N`). Commands without a count
/// are returned unchanged.
pub(crate) fn with_count(cmd: &Command, n: u32) -> Command {
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
        C::DeleteUnder(_) => C::DeleteUnder(n),
        C::DeleteForward(_) => C::DeleteForward(n),
        C::ReplaceChar(_, c) => C::ReplaceChar(n, *c),
        C::VirtualReplaceChar(_, c) => C::VirtualReplaceChar(n, *c),
        C::ToggleCase(_) => C::ToggleCase(n),
        C::CaseMotion { motion, case, .. } => C::CaseMotion {
            count: n,
            motion: *motion,
            case: *case,
        },
        C::ShiftRight(_) => C::ShiftRight(n),
        C::ShiftLeft(_) => C::ShiftLeft(n),
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
        C::Paste {
            after, move_after, ..
        } => C::Paste {
            after: *after,
            count: n,
            move_after: *move_after,
        },
        C::JoinLines(_) => C::JoinLines(n),
        C::JoinLinesNoSpace(_) => C::JoinLinesNoSpace(n),
        // `CTRL-A`/`CTRL-X` carry the count as a signed delta; `5.` keeps the direction.
        C::IncrementNumber(d) => {
            C::IncrementNumber(if *d < 0 { -i64::from(n) } else { i64::from(n) })
        }
        C::PasteIndent { after, .. } => C::PasteIndent {
            after: *after,
            count: n,
        },
        C::PasteBlockInner { after, .. } => C::PasteBlockInner {
            after: *after,
            count: n,
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
        // No count to rewrite. Listed (not a wildcard) so a new counted variant cannot silently drop
        // `N.` (#793). The Visual-selection changes are here by design: nvim ignores a count on `.` of a
        // Visual change (the repeat re-covers the recorded AMOUNT of text, `:help visual-repeat`).
        C::MoveLeft
        | C::MoveRight
        | C::MoveUp
        | C::MoveDown
        | C::MoveLineStart
        | C::MoveLineEnd
        | C::EnterInsert
        | C::EnterInsertAfter
        | C::EnterNormal
        | C::EnterTerminal
        | C::EnterTerminalNormal
        | C::InsertLineStart
        | C::InsertColumnZero
        | C::AppendLineEnd
        | C::OpenBelow
        | C::OpenAbove
        | C::EnterReplace
        | C::ReplaceType(..)
        | C::ReplaceBackspace
        | C::EnterVirtualReplace
        | C::VirtualReplaceType(..)
        | C::InsertChar(..)
        | C::InsertRegister(..)
        | C::InsertEval(..)
        | C::InsertPaste(..)
        | C::ReplacePaste(..)
        | C::EmacsPaste(..)
        | C::InsertDeleteWordBack
        | C::InsertDeleteToLineStart
        | C::CompleteWord { .. }
        | C::InsertIndent
        | C::InsertDedent
        | C::InsertDeleteIndent
        | C::InsertDeleteIndentRestore
        | C::InsertStartColumnLine { .. }
        | C::InsertCaretStep { .. }
        | C::InsertTab
        | C::InsertNewline
        | C::OpenLineIndent { .. }
        | C::InsertCloser { .. }
        | C::DeleteBack
        | C::ReplaceSelectionChar(..)
        | C::CaseSelection(..)
        | C::IncrementSelection { .. }
        | C::GotoLastChange
        | C::GotoLastChangeLine
        | C::GotoChangeMarkStart
        | C::GotoChangeMarkEnd
        | C::GotoChangeMarkStartLine
        | C::GotoChangeMarkEndLine
        | C::GotoVisualMarkStart
        | C::GotoVisualMarkEnd
        | C::GotoVisualMarkStartLine
        | C::GotoVisualMarkEndLine
        | C::GotoContextMark
        | C::GotoContextMarkLine
        | C::GotoOlderChange
        | C::GotoNewerChange
        | C::GotoOlderJump
        | C::GotoNewerJump
        | C::InsertAtLastInsert
        | C::AsciiInfo
        | C::Utf8Info
        | C::FileInfo
        | C::CursorInfo
        | C::SetNamedMark(..)
        | C::GotoNamedMark(..)
        | C::GotoNamedMarkLine(..)
        | C::OpToMark { .. }
        | C::BreakUndo
        | C::SetIndents { .. }
        | C::PasteSelection { .. }
        | C::SetRegister(..)
        | C::SetExprRegister(..)
        | C::EnterVisual { .. }
        | C::EnterSelect { .. }
        | C::DeleteSelection
        | C::YankSelection
        | C::ChangeSelection
        | C::ReselectVisual
        | C::SwapSelectionEnds
        | C::ReplaceSelection(..)
        | C::VisualOp(..)
        | C::BlockInsert(..)
        | C::SearchNext(..)
        | C::SearchPrev(..)
        | C::RepeatSubstituteGlobal
        | C::RepeatSubstituteLine
        | C::Ex(_)
        | C::GotoDeclaration { .. }
        | C::GotoFirstMatch(..)
        | C::ShowKeywordLines { .. }
        | C::Undo
        | C::Redo
        | C::UndoOlder
        | C::UndoNewer
        | C::LineUndo
        | C::Save
        | C::Quit
        | C::SetMark
        | C::KillRegion
        | C::CopyRegion
        | C::ExchangePointMark
        | C::EmacsYank { .. }
        | C::EmacsBufferEdge { .. }
        | C::EmacsKillLine
        | C::EmacsTransposeChars
        | C::EmacsTransposeWords
        | C::EmacsKillWord { .. }
        | C::EmacsBackwardKillWord { .. }
        | C::EmacsCaseWord { .. }
        | C::EmacsHorizontalSpace { .. }
        | C::EmacsOpenLine
        | C::EmacsMarkWord
        | C::EmacsKillWholeLine
        | C::EmacsCaseRegion { .. }
        | C::EmacsDeleteIndentation => cmd.clone(),
    }
}
