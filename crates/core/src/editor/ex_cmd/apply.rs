//! Running a resolved Ex edit on [`EditorState`]: each verb calls the method the live command calls.

use super::{ExCmd, ExOutcome, MAX_SHIFT_LEVELS};
use crate::editor::{EditorState, LineAddr, SubRange};
use crate::pos::BytePos;

fn lines(first: usize, last: usize) -> SubRange {
    SubRange::Lines(first, last)
}

impl EditorState {
    /// Run one resolved Ex edit (RFC-0020). The group markers are inert here: undo grouping across a group
    /// is the caller's ([`crate::trace::Trace::replay`] opens one undo block per outermost group).
    pub fn apply_ex(&mut self, ex: &ExCmd) -> ExOutcome {
        if ex.edits() && !self.doc.is_modifiable() {
            return ExOutcome::NotModifiable;
        }
        match ex {
            ExCmd::Substitute { .. } | ExCmd::Global { .. } => self.apply_ex_pattern(ex),
            ExCmd::Delete { .. }
            | ExCmd::Yank { .. }
            | ExCmd::Join { .. }
            | ExCmd::Shift { .. }
            | ExCmd::Move { .. }
            | ExCmd::Copy { .. }
            | ExCmd::Put { .. }
            | ExCmd::Sort { .. }
            | ExCmd::Retab { .. }
            | ExCmd::ReadText { .. }
            | ExCmd::FilterReplace { .. }
            | ExCmd::Edits { .. } => self.apply_ex_lines(ex),
            ExCmd::Mark {
                line,
                name,
                nonblank,
            } => self.apply_ex_mark(*line, *name, *nonblank),
            ExCmd::Set(_)
            | ExCmd::Place(_)
            | ExCmd::Want(_)
            | ExCmd::UndoSync
            | ExCmd::KeepJumps(_)
            | ExCmd::Modifiable(_)
            | ExCmd::Modified(_)
            | ExCmd::Register { .. }
            | ExCmd::Begin(_)
            | ExCmd::End => self.apply_ex_state(ex),
        }
    }

    /// The pattern-driven verbs: `:s` (with or without recorded confirm answers) and `:g`.
    fn apply_ex_pattern(&mut self, ex: &ExCmd) -> ExOutcome {
        match ex {
            ExCmd::Substitute {
                first,
                last,
                pattern,
                replacement,
                flags,
                answers,
            } => {
                let range = lines(*first, *last);
                let from = self.view.cursor;
                let result = match answers {
                    None => self
                        .substitute(range, pattern, replacement, *flags)
                        .inspect(|out| {
                            // A `:s` that matched is a jump (Vim sets the pcmark, #940).
                            if out.lines > 0 {
                                self.view.push_jump(from);
                            }
                        }),
                    Some(answers) => self
                        .substitute_preview(range, pattern, replacement, *flags)
                        .map(|subs| {
                            // A match is a jump before the first prompt (nvim's `do_sub`, #1119).
                            if !subs.is_empty() {
                                self.view.push_jump(from);
                            }
                            self.apply_confirmed(&subs, answers, range)
                        }),
                };
                match result {
                    Ok(out) => ExOutcome::Substituted(out),
                    Err(e) => ExOutcome::Failed(e),
                }
            }
            ExCmd::Global { marks, cmd } => {
                // A recorded mark set is ascending and in the buffer; a hand-edited one is made so.
                let n = super::super::line_spans(self.doc.bytes()).len();
                let mut marked: Vec<usize> = marks
                    .iter()
                    .filter(|&&m| (1..=n).contains(&m))
                    .map(|m| m - 1)
                    .collect();
                marked.sort_unstable();
                marked.dedup();
                match self.global_run(&marked, cmd) {
                    Ok(n) => ExOutcome::Lines(n),
                    Err(e) => ExOutcome::Failed(e),
                }
            }
            _ => unreachable!("apply_ex_pattern called on a non-pattern Ex command"),
        }
    }

    /// The verbs that edit or copy whole lines (or apply recorded raw edits).
    fn apply_ex_lines(&mut self, ex: &ExCmd) -> ExOutcome {
        match ex {
            ExCmd::Delete { first, last, reg } => {
                // `:d` is a jump from where the cursor stood (nvim's `ex_operators`, #1119).
                self.view.push_jump(self.view.cursor);
                ExOutcome::Lines(self.delete_lines_into(lines(*first, *last), *reg))
            }
            ExCmd::Yank { first, last, reg } => {
                ExOutcome::Lines(self.yank_lines_into(lines(*first, *last), *reg))
            }
            ExCmd::Join {
                first,
                last,
                no_space,
            } => ExOutcome::Lines(self.join_lines(lines(*first, *last), *no_space)),
            ExCmd::Shift {
                first,
                last,
                left,
                levels,
            } => {
                self.view.push_jump(self.view.cursor);
                let levels = (*levels).min(MAX_SHIFT_LEVELS);
                ExOutcome::Lines(self.shift_lines(lines(*first, *last), *left, levels))
            }
            ExCmd::Move { first, last, dest } => {
                ExOutcome::Relocated(self.move_lines(lines(*first, *last), LineAddr::Line(*dest)))
            }
            ExCmd::Copy { first, last, dest } => {
                ExOutcome::Relocated(self.copy_lines(lines(*first, *last), LineAddr::Line(*dest)))
            }
            ExCmd::Put { dest, reg } => {
                ExOutcome::Lines(self.put_lines(LineAddr::Line(*dest), *reg))
            }
            ExCmd::Sort { first, last, opts } => {
                match self.sort_lines(lines(*first, *last), opts) {
                    Ok(n) => ExOutcome::Lines(n),
                    Err(e) => ExOutcome::Failed(e),
                }
            }
            ExCmd::Retab {
                first,
                last,
                bang,
                tabstop,
            } => ExOutcome::Retabbed(self.retab(lines(*first, *last), *bang, *tabstop)),
            ExCmd::ReadText {
                dest,
                cursor_on_last,
                bytes,
            } => ExOutcome::Lines(self.read_lines(LineAddr::Line(*dest), bytes, *cursor_on_last)),
            ExCmd::FilterReplace { first, last, bytes } => {
                ExOutcome::Lines(self.filter_lines(lines(*first, *last), bytes))
            }
            ExCmd::Edits { origin, edits } => ExOutcome::Applied(self.apply_edits(edits, *origin)),
            _ => unreachable!("apply_ex_lines called on a non-line Ex command"),
        }
    }

    /// `:mark` / `:k`: set mark `name` on `line` (clamped into the buffer), at its first non-blank when
    /// `nonblank` (the `'` / `` ` `` jump-mark path) or at its start (a named mark).
    fn apply_ex_mark(&mut self, line: usize, name: char, nonblank: bool) -> ExOutcome {
        let b = self.doc.bytes();
        let n = crate::pos::count_newlines(b) + usize::from(b.last() != Some(&b'\n'));
        let start = crate::pos::raw::nth_line_start(b, line.clamp(1, n.max(1)) - 1);
        let at = if nonblank {
            // `beginline(BL_WHITE | BL_FIX)`: the first non-blank, or a blank line's last char.
            let nb = crate::motion::first_non_blank(b, start);
            let end = crate::pos::raw::line_end(b, start);
            if nb == end && end > start {
                end - 1
            } else {
                nb
            }
        } else {
            start
        };
        if nonblank {
            self.view.set_mark(name, at);
        } else {
            self.view.set_named_mark(name, at);
        }
        ExOutcome::Done
    }

    /// The verbs that set editor, view, buffer or register state without editing text, plus the inert
    /// group markers. Each reports [`ExOutcome::Done`].
    fn apply_ex_state(&mut self, ex: &ExCmd) -> ExOutcome {
        match ex {
            ExCmd::Set(opt) => self.set_option(opt.clone()),
            ExCmd::Place(at) => {
                let b = self.doc.bytes();
                let at = crate::motion::snap(b, at.0.min(b.len()));
                self.view.set_cursor(BytePos(at));
            }
            ExCmd::Want(col) => self.view.curswant = super::super::Want::col(*col),
            ExCmd::UndoSync => self.doc.sync_undo(),
            ExCmd::KeepJumps(on) => self.view.keep_jumps = *on,
            ExCmd::Modifiable(on) => self.doc.set_modifiable(*on),
            ExCmd::Modified(on) => {
                if *on {
                    self.doc.mark_unsaved();
                } else {
                    self.doc.mark_saved();
                }
            }
            ExCmd::Register { name, value } => {
                self.view.registers.set_slot(*name, value.clone());
            }
            ExCmd::Begin(_) | ExCmd::End => {}
            _ => unreachable!("apply_ex_state called on a non-state Ex command"),
        }
        ExOutcome::Done
    }
}

#[cfg(test)]
mod tests;
