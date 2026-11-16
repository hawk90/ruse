//! The keys the session queues for itself and the undo steps around them (#1496): the split Alt chord's
//! key half, the macro replay queue and its one undo block, the undo sync before a typed key, and the
//! CTRL-C interrupt that ends a replay. Split from `session.rs`.

use crossterm::event::KeyEvent;
use ruse_core::{ExCmd, Mode};

use super::Session;
use crate::app::dispatch::{note_ex, place_cursor};

impl Session {
    /// Whether keys of a macro replay are still to run (the event loop paints less often meanwhile).
    pub(crate) fn replaying(&self) -> bool {
        self.macros.replay_pending()
    }

    /// The next key the session queued for itself, if any, with its `from_replay` flag: the key half of a
    /// split Alt chord (typed, #896), else the next key of a macro replay (`@{reg}`, `@:`). The caller runs
    /// a full frame before each one and feeds it to [`Session::on_key`] before reading the terminal.
    pub(crate) fn next_queued(&mut self) -> Option<(KeyEvent, bool)> {
        if let Some(k) = self.meta_tail.take() {
            return Some((k, false));
        }
        // CTRL-C between two replayed keys stops the replay (a runaway macro, #787 / #898).
        if self.macros.replay_pending() && ruse_core::interrupt::breakcheck() {
            self.settle_interrupt();
            return None;
        }
        // A linewise-Visual `@{reg}` runs on each line from its column 0, clamped to the last line.
        if let Some(line) = self.macros.next_line_run() {
            let b = self.ws.focused().doc.bytes();
            let lines = ruse_core::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
            let at = ruse_core::pos::nth_line_start(b, line.min(lines.saturating_sub(1))).0;
            place_cursor(&mut self.ws, &mut self.recorded, at);
        }
        let next = self.macros.next_replay();
        if next.is_none() {
            self.end_macro_undo();
        }
        next.map(|k| (k, true))
    }

    /// Close the undo block a macro replay opened ([`Session::macro_undo`]).
    pub(super) fn end_macro_undo(&mut self) {
        if let Some(doc) = self.macro_undo.take() {
            if let Some(d) = self.ws.document_mut(doc) {
                d.end_undo_block();
            }
            note_ex(ExCmd::End, &mut self.recorded);
        }
    }

    /// Vim's undo sync before a TYPED key (#678): a key read outside text entry, or by an `i_CTRL-O`
    /// one-shot, begins a new command, so its change is an undo step of its own; Insert keeps typing in one
    /// step, except the text typed after an `i_CTRL-O` command returns (a new insert). Keys that were not
    /// typed (a macro replay, `:normal`) never sync. The Emacs profile keeps its grouping (consecutive edits
    /// until a motion).
    pub(super) fn sync_undo_before_typed(&mut self) {
        if self.emacs_profile {
            return;
        }
        let mode = self.ws.focused().view.mode();
        let entry = matches!(mode, Mode::Insert | Mode::Replace | Mode::VirtualReplace);
        if !entry || self.engine.in_one_shot() || self.engine.insert_resumed() {
            // Recorded when it closes a group, so a replay's `u` undoes what it undid live (RFC-0020).
            if !self.ws.focused().doc.history().is_synced() {
                note_ex(ExCmd::UndoSync, &mut self.recorded);
            }
            self.ws.focused_doc_mut().sync_undo();
        }
    }

    /// After a key: if CTRL-C interrupted what it ran ([`ruse_core::interrupt`]), every loop on the way out
    /// has stopped; drop the keys still to replay (nvim flushes typeahead on `got_int`), close the replay's
    /// undo step, say so, and clear the interrupt for the next command.
    pub(super) fn settle_interrupt(&mut self) {
        if ruse_core::interrupt::take() {
            self.macros.interrupt_replay();
            self.end_macro_undo();
            self.status.info("Interrupted");
        }
    }
}
