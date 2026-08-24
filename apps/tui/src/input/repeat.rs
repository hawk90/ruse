//! Dot-repeat (`.`) record type (D-025 / D-047 / F-023): the re-parameterizable change-intent the
//! engine captures and replays. Split out of `input/mod.rs` as a self-contained data definition; the
//! `InputEngine::record` builder that constructs it stays in the engine core.

use ruse_core::{Command, OpKind, SearchOp};

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
    /// The count typed before a PURE insert-entry (`3i`, `3o`, `3A`; VIM-CNT-INS): the number of times the
    /// typed text is inserted on `<Esc>`. `1` for a count-less entry and for every change-family entry
    /// (`c`/`s`/`ciw`, whose count applies to the MOTION, not to text repetition). Carried so `.` replays
    /// with the SAME count (Vim), while a leading `N.` overrides it.
    pub(crate) entry_count: u32,
}

impl ChangeIntent {
    /// The ordered command list `.` replays. `count` — a leading `N` on the `.` — REPLACES the lead's
    /// count (Vim `3.` repeats with count 3); `None` keeps the recorded count. Insert text is replayed
    /// verbatim. For a PURE insert-entry (`3i`/`3o`/`3A`) the count is the number of TIMES the typed text
    /// is inserted (VIM-CNT-INS): `.` re-inserts it `entry_count` times, `N.` overrides that to `N`.
    pub(crate) fn replay(&self, count: Option<u32>) -> Vec<Command> {
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
        let mut cmds =
            Vec::with_capacity(1 + self.insert.len() + usize::from(self.register.is_some()));
        // Re-select the register first, so the replayed change writes to the same slot (`"ax` then `.`).
        if self.register.is_some() {
            cmds.push(Command::SetRegister(self.register));
        }
        cmds.push(lead);
        cmds.extend(body.iter().cloned());
        if reps > 1 {
            let unit = self.repeat_unit();
            for _ in 1..reps {
                cmds.extend(unit.iter().cloned());
            }
        }
        if had_enter {
            cmds.push(Command::EnterNormal);
        }
        cmds
    }

    /// The LIVE count-on-insert tail (VIM-CNT-INS) emitted on the terminating `<Esc>` when a count
    /// preceded a pure insert-entry (`3ihello<Esc>`). The lead and the FIRST typed body were already
    /// applied as the keys were typed, so this is the `(entry_count - 1)` extra repeats followed by the
    /// `EnterNormal` that leaves Insert (the original `<Esc>`'s `EnterNormal` is replaced by this list).
    /// `None` when there is nothing to replicate (count <= 1, or not a pure insert-entry).
    pub(crate) fn count_replay_tail(&self) -> Option<Vec<Command>> {
        if self.entry_count <= 1 || !self.is_plain_insert_entry() {
            return None;
        }
        let unit = self.repeat_unit();
        let mut cmds = Vec::with_capacity(unit.len() * (self.entry_count as usize - 1) + 1);
        for _ in 1..self.entry_count {
            cmds.extend(unit.iter().cloned());
        }
        cmds.push(Command::EnterNormal);
        Some(cmds)
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

    /// Whether the lead is a PURE insert-entry (`i`/`a`/`I`/`A`/`o`/`O`) — the only entries whose count
    /// repeats the typed text. Change-family entries (`c`/`C`/`s`/`cc`/`ciw`, `cgn`) are excluded: their
    /// count applies to the operator's motion, never to text repetition.
    fn is_plain_insert_entry(&self) -> bool {
        matches!(
            self.lead,
            Command::EnterInsert
                | Command::EnterInsertAfter
                | Command::InsertLineStart
                | Command::AppendLineEnd
                | Command::OpenBelow
                | Command::OpenAbove
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
pub(crate) fn change_kind(cmd: &Command) -> ChangeKind {
    use Command as C;
    match cmd {
        // Insert-entering: the change includes the text typed until `<Esc>`.
        C::EnterInsert
        | C::EnterInsertAfter
        | C::InsertLineStart
        | C::AppendLineEnd
        | C::OpenBelow
        | C::OpenAbove
        | C::Change(..)
        | C::ChangeSelection
        | C::ReplaceSelection(_)
        | C::OpForced {
            op: OpKind::Change, ..
        }
        // `cgn`/`cgN` — the gn idiom: dot replays the change (re-searching from the new cursor), so `n.`
        // (or bare `.`) walks the change through every match. This is why gn matters.
        | C::SearchObject {
            op: SearchOp::Change,
            ..
        } => ChangeKind::InsertEntering,
        // Self-contained buffer edits — dot-repeatable as a single command.
        C::Delete(..)
        | C::DeleteUnder(_)
        | C::DeleteForward(_)
        | C::DeleteBack
        | C::ReplaceChar(..)
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
        | C::Paste { .. }
        | C::PasteIndent { .. }
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
        | C::OpForced {
            op: OpKind::Delete, ..
        }
        // `dgn`/`dgN` — delete the match; dot-repeatable to sweep the next match.
        | C::SearchObject {
            op: SearchOp::Delete,
            ..
        } => ChangeKind::Immediate,
        // Everything else (motions, mode switches, yank incl. forced yank, search, undo/redo) is not a change.
        _ => ChangeKind::NotAChange,
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
        C::Paste {
            after, move_after, ..
        } => C::Paste {
            after: *after,
            count: n,
            move_after: *move_after,
        },
        other => other.clone(),
    }
}
