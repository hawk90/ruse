//! The dot-repeat recorder (D-025 / F-023): [`InputEngine::record`] folds each outcome into the `.`
//! record, [`InputEngine::change_failed`] restores it when the change fails (#1179), and the `".`
//! register text. The record type itself lives in [`super::repeat`]. Moved out of `input/mod.rs` (#1493).

use ruse_core::{Command, Mode};

use super::{change_kind, repeat, ChangeIntent, ChangeKind, Feed, InputEngine};

impl InputEngine {
    /// The text of the most recent insert session (Vim's `".` register, `:help quote_.`), reconstructed
    /// from the recorded insert body — the same commands `i_CTRL-A` replays. `None` when nothing has been
    /// inserted yet. The body is replayed over an empty string: `InsertChar`/`InsertNewline`/`InsertTab`
    /// append; `DeleteBack` removes the last char (so `abX<BS>cd` yields the net `abcd`, matching what a
    /// `i_CTRL-A` replay produces). Uncommon in-insert edits (`i_CTRL-W`/`i_CTRL-U`/`i_CTRL-R`) are not
    /// modelled here — a documented, minor divergence from nvim's raw-keystroke capture for those cases.
    #[must_use]
    pub fn last_inserted_text(&self) -> Option<String> {
        if self.last_inserted.is_empty() {
            return None;
        }
        let mut out = String::new();
        for cmd in &self.last_inserted {
            match cmd {
                Command::InsertChar('\r') | Command::InsertNewline => out.push('\n'),
                Command::InsertChar(c) => out.push(*c),
                Command::InsertTab => out.push('\t'),
                Command::InsertPaste(t) | Command::ReplacePaste(t) => out.push_str(t),
                Command::DeleteBack => {
                    // `<BS>` deleted a whole cluster (#909), so the record drops one too.
                    let keep = unicode_segmentation::UnicodeSegmentation::grapheme_indices(
                        out.as_str(),
                        true,
                    )
                    .next_back()
                    .map_or(0, |(i, _)| i);
                    out.truncate(keep);
                }
                _ => {}
            }
        }
        Some(out)
    }

    /// Dispatch a PURE insert-entry (`i`/`a`/`I`/`A`/`o`/`O`, `R`/`gR`), capturing the leading count for
    /// count-on-insert repetition (VIM-CNT-INS) BEFORE [`action`](Self::action) resets the axes. The
    /// captured count is stashed in `pending_insert_count`; [`record`](Self::record) folds it into the
    /// [`ChangeIntent`] this entry opens, and the terminating `<Esc>` replays the typed text that many
    /// times. Change-family entries (`c`/`s`/…) do NOT come through here, so their count never repeats text.
    pub(super) fn insert_entry(&mut self, cmd: Command) -> Feed {
        self.pending_insert_count = self.mcount();
        self.action(cmd)
    }

    /// Fold a just-produced outcome into the dot-repeat record. In Insert mode, extend the in-flight change
    /// until `<Esc>` closes it; in Normal/Visual, an insert-entering command opens a recording, a
    /// self-contained edit becomes the record outright, and anything else leaves the record intact.
    /// `Pending`/`Ignored`/`Replay`/`Open*` never touch it — which is what makes `..` repeat one change.
    pub(super) fn record(&mut self, out: &Feed, mode: Mode) {
        let Feed::Cmd(cmd) = out else {
            return;
        };
        self.replaced_change = None;
        // Replace / Virtual Replace (`R`/`gR`) are insert sessions too: their overwrites are the body.
        if matches!(mode, Mode::Insert | Mode::Replace | Mode::VirtualReplace) {
            // Insert resumed after an `i_CTRL-O` one-shot: the first edit opens a FRESH `i` change (Vim's
            // `stop_arrow` resets the redo buffer to `1i`); `<Esc>` with nothing typed keeps the old one.
            if std::mem::take(&mut self.insert.resumed)
                && self.recording.is_none()
                && *cmd != Command::EnterNormal
            {
                self.recording = Some(ChangeIntent {
                    lead: Command::EnterInsert,
                    insert: Vec::new(),
                    register: None,
                    expr: None,
                    entry_count: 1,
                    screen: None,
                });
            }
            if let Some(rec) = self.recording.as_mut() {
                // A caret jump in Replace restarts the record as a plain `i` (nvim's `stop_arrow` writes
                // "1i"), so the keys typed after it replay as Insert keys: they insert (#727). A body that
                // switched into Replace itself (`i_<Insert>`) replays its overwrites as they are.
                let as_insert = |rec: &ChangeIntent| {
                    rec.lead == Command::EnterInsert
                        && !rec.insert.iter().any(|c| {
                            matches!(c, Command::EnterReplace | Command::EnterVirtualReplace)
                        })
                };
                let body = match cmd {
                    Command::ReplaceType(c) | Command::VirtualReplaceType(c) if as_insert(rec) => {
                        Command::InsertChar(*c)
                    }
                    Command::ReplaceBackspace if as_insert(rec) => Command::DeleteBack,
                    _ => cmd.clone(),
                };
                rec.insert.push(body);
                // The `<Esc>` that leaves Insert (recorded here so replay leaves Insert too) closes the change.
                if *cmd == Command::EnterNormal {
                    let closed = self.recording.take();
                    // Count-on-insert (VIM-CNT-INS): if a count preceded the entry, stage the extra repeats
                    // so `feed` returns them as ONE undo-grouped `Feed::Repeat` in place of this `<Esc>`.
                    if let Some(rec) = &closed {
                        self.pending_insert_replay = rec.count_replay_tail();
                        // Remember the typed text for `i_CTRL-A` / `i_CTRL-@` (Vim's `".` register). Unlike
                        // `last_change`, this survives a later non-insert change (`x`, `dw`) — verified vs
                        // nvim — so it is captured here on close and never touched by an Immediate change.
                        self.last_inserted = rec.insert_body().to_vec();
                    }
                    self.last_change = closed;
                }
            }
            return;
        }
        // `"x` selects the register the NEXT recorded change should carry — remembered, not itself a change.
        if let Command::SetRegister(name) = cmd {
            self.pending_record_register = *name;
            self.pending_record_expr = None;
            return;
        }
        // `"={expr}<CR>` is the register selection of the change that follows (#808).
        if let Command::SetExprRegister(expr) = cmd {
            self.pending_record_register = Some('=');
            self.pending_record_expr = Some(expr.clone());
            return;
        }
        // A search with no previous pattern (`dn`, `cn` before any search) fails before its operator runs, so
        // it is no change for `.` (nvim's `normal_search` → `clearopbeep`, #1173).
        let failing_search = matches!(cmd, Command::Search { pattern, .. } if pattern.is_empty());
        match if failing_search {
            ChangeKind::NotAChange
        } else {
            change_kind(cmd)
        } {
            ChangeKind::InsertEntering => {
                self.replaced_change = Some((self.last_change.clone(), self.recording.take()));
                self.recording = Some(ChangeIntent {
                    // `gi` repeats as a plain `i` at the NEW cursor (Vim's `nv_edit` turns `gi` into `i`
                    // after jumping to the `'^` mark), not as another jump to the last-insert spot.
                    lead: match cmd {
                        Command::InsertAtLastInsert => Command::EnterInsert,
                        other => other.clone(),
                    },
                    insert: Vec::new(),
                    register: self.pending_record_register.take(),
                    expr: self.pending_record_expr.take(),
                    // The count a pure insert-entry captured (`3i`); `0` (→ 1) for a count-less or
                    // change-family entry, so only `i`/`a`/`I`/`A`/`o`/`O` ever repeat their text.
                    entry_count: std::mem::take(&mut self.pending_insert_count).max(1),
                    screen: None,
                });
            }
            ChangeKind::Immediate => {
                self.replaced_change = Some((self.last_change.take(), self.recording.take()));
                let mut register = self.pending_record_register.take();
                let mut expr = self.pending_record_expr.take();
                let lead = match repeat::visual_put_redo(cmd, register, mode) {
                    Some((lead, reg)) => {
                        (register, expr) = (reg, None);
                        lead
                    }
                    None => cmd.clone(),
                };
                self.last_change = Some(ChangeIntent {
                    lead,
                    insert: Vec::new(),
                    register,
                    expr,
                    entry_count: 1,
                    screen: None,
                });
            }
            // A non-change (motion, mode switch, yank) forgets a dangling register selection.
            ChangeKind::NotAChange => {
                self.pending_record_register = None;
                self.pending_record_expr = None;
            }
        }
    }

    /// The command the engine just produced failed (the core beeped). When nvim cancels that command before
    /// it saves the redo buffer, the change it replaced comes back, so `.` still repeats the earlier change
    /// (#1179): a motion that fails under an operator (`d/zzz<CR>`, `dfz`, `d%`, `d'a` on an unset mark,
    /// `dk` on the first line) is cleared with `clearopbeep` before `do_pending_operator` runs `prep_redo`,
    /// and `J` on the last line, `r` past the end and `~` on an empty line bail out before theirs. A
    /// failure nvim reaches only after saving the redo (`CTRL-A` with no number, a put of an empty
    /// register, an operator refused by its register: `"/dd`) stays the change; `refused` says the core
    /// refused the operator for its register ([`ruse_core::Effect::RegisterRefused`]), which a motion
    /// failing first never reaches (`"/dfz` with no `z` keeps the earlier change, #1184). Call it only
    /// right after running the command `cmd` the engine returned, before the next key.
    pub fn change_failed(&mut self, cmd: &Command, refused: bool) {
        if refused || !repeat::fails_before_redo(cmd) {
            return;
        }
        let Some((last, recording)) = self.replaced_change.take() else {
            return;
        };
        self.last_change = last;
        self.recording = recording;
    }
}
