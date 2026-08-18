//! Dot-repeat (`.`) record type (D-025 / D-047 / F-023): the re-parameterizable change-intent the
//! engine captures and replays. Split out of `input/mod.rs` as a self-contained data definition; the
//! `InputEngine::record` builder that constructs it stays in the engine core.

use ruse_core::Command;

use super::with_count;

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
}

impl ChangeIntent {
    /// The ordered command list `.` replays. `count` — a leading `N` on the `.` — REPLACES the lead's
    /// count (Vim `3.` repeats with count 3); `None` keeps the recorded count. Insert text is replayed
    /// verbatim.
    pub(crate) fn replay(&self, count: Option<u32>) -> Vec<Command> {
        let lead = match count {
            Some(n) => with_count(&self.lead, n),
            None => self.lead.clone(),
        };
        let mut cmds =
            Vec::with_capacity(1 + self.insert.len() + usize::from(self.register.is_some()));
        // Re-select the register first, so the replayed change writes to the same slot (`"ax` then `.`).
        if self.register.is_some() {
            cmds.push(Command::SetRegister(self.register));
        }
        cmds.push(lead);
        cmds.extend(self.insert.iter().cloned());
        cmds
    }
}
