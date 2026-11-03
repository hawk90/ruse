//! The state behind Vim's `U` (undo all latest changes on one line), kept per document like Vim's buffer.
//!
//! A change confined to one line saves that line's text the first time the line is changed (nvim v0.12.6
//! `u_saveline`): later changes to the SAME line keep the first copy, so `U` restores the line as it was
//! before the whole run of changes. `U` swaps the line with the saved copy, so a second `U` redoes; the swap
//! is an ordinary undoable change (`u` undoes a `U`). `u`/`CTRL-R` leave the saved copy alone.
//!
//! Saving is "armed" once per command, as nvim's `u_save` is: every Normal-mode command arms it, and inside
//! an Insert session only the first change does (and the first after an undo break such as `<Left>`). So
//! `Aab<CR>cd<Esc>U` does nothing: the `<CR>` dropped the saved line and `cd` is not a new command.
//!
//! Dropped (nothing for `U` to do): a delete of whole lines (`dd`, `:d`, `2cc`) and opening a line (`o`,
//! `O`, `<CR>` in Insert), as nvim does; and any change that splits, joins or removes the saved line.
//! Divergence from nvim: when lines are inserted or deleted ABOVE the saved line its number follows, where
//! nvim keeps the stale number and `U` then overwrites whichever line now sits there (`jxxkJU`, `jxxggyyPU`).

use crate::edit::EditList;

/// The saved line: its number (0-based), its text before the changes, and the cursor column `U` returns to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Saved {
    line: usize,
    text: Vec<u8>,
    col: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LineUndo {
    saved: Option<Saved>,
    /// The next one-line change saves its line (a new command started).
    armed: bool,
    /// The caret before the next change, set by the editor; the saved column comes from it.
    cursor: Option<usize>,
}

/// `[start, end)` of the line holding byte `pos` (`end` is its newline, or the buffer end).
fn bounds_at(text: &[u8], pos: usize) -> (usize, usize) {
    let pos = pos.min(text.len());
    let start = text[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let end = text[pos..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(text.len(), |i| pos + i);
    (start, end)
}

/// `[start, end)` of line `n` (0-based), or `None` past the last line.
fn bounds_of(text: &[u8], n: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    for _ in 0..n {
        start += text[start..].iter().position(|&b| b == b'\n')? + 1;
    }
    Some(bounds_at(text, start))
}

fn line_of(text: &[u8], pos: usize) -> usize {
    text[..pos.min(text.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

fn newlines(bytes: &[u8]) -> isize {
    bytes.iter().filter(|&&b| b == b'\n').count() as isize
}

/// Whether `list` inserts or deletes a newline (adds, removes, splits or joins lines).
fn changes_lines(pre: &[u8], list: &EditList) -> bool {
    list.edits()
        .iter()
        .any(|e| e.ins.contains(&b'\n') || pre[e.pos.0..e.end()].contains(&b'\n'))
}

impl LineUndo {
    /// Before a change: record the caret, and arm saving when a new command starts.
    pub(crate) fn prime(&mut self, cursor: usize, arm: bool) {
        self.cursor = Some(cursor);
        self.armed |= arm;
    }

    /// Drop the saved line (`o`, `O`, `<CR>`). `disarm` also stops the rest of the command from saving a
    /// line, as an Insert-mode `<CR>` does.
    pub(crate) fn clear(&mut self, disarm: bool) {
        self.saved = None;
        if disarm {
            self.armed = false;
        }
    }

    /// Track a fresh change from `pre` (the text before it). `fresh` is a change that starts a new undo
    /// group, which always begins a new command.
    pub(crate) fn on_edit(&mut self, pre: &[u8], list: &EditList, fresh: bool) {
        let cursor = self.cursor.take();
        self.armed |= fresh;
        let edits = list.edits();
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
            return;
        };
        let lo = first.pos.0;
        let one_line =
            !pre[lo..last.end()].contains(&b'\n') && !edits.iter().any(|e| e.ins.contains(&b'\n'));
        if one_line {
            if std::mem::take(&mut self.armed) {
                let line = line_of(pre, lo);
                if self.saved.as_ref().map(|s| s.line) != Some(line) {
                    let (s, e) = bounds_at(pre, lo);
                    let col = match cursor {
                        Some(c) if (s..=e).contains(&c) => c - s,
                        _ => lo - s,
                    };
                    self.saved = Some(Saved {
                        line,
                        text: pre[s..e].to_vec(),
                        col,
                    });
                }
            }
            return;
        }
        // Several lines changed but none added or removed (`>j`, `:%s/a/b/`): every line keeps its number,
        // and nvim keeps the saved copy even when the change reached that line.
        if !changes_lines(pre, list) {
            return;
        }
        // A delete of whole lines (it starts at a line start, or runs to the buffer end) drops the line.
        let whole_lines = edits.iter().any(|e| {
            let p = e.pos.0;
            pre[p..e.end()].contains(&b'\n')
                && (p == 0 || pre[p - 1] == b'\n' || e.end() == pre.len())
        });
        if whole_lines {
            self.saved = None;
            return;
        }
        self.follow(pre, list);
    }

    /// Track an undo/redo step: the saved copy stays, but its line follows inserted or deleted lines.
    pub(crate) fn on_reapply(&mut self, pre: &[u8], list: &EditList) {
        self.cursor = None;
        if changes_lines(pre, list) {
            self.follow(pre, list);
        }
    }

    /// Move the saved line's number past the lines `list` inserts or deletes before it; drop it when an
    /// edit changes, splits, joins or removes the line itself.
    fn follow(&mut self, pre: &[u8], list: &EditList) {
        let Some(saved) = self.saved.as_mut() else {
            return;
        };
        let Some((s, e)) = bounds_of(pre, saved.line) else {
            self.saved = None;
            return;
        };
        let mut shift: isize = 0;
        for ed in list.edits() {
            let (p, end, ins) = (ed.pos.0, ed.end(), ed.ins.as_slice());
            let survives = if p > e {
                true
            } else if p == e && ed.del == 0 && s < e {
                // Text added after the line: a new line keeps it whole, anything else appends to it.
                ins.first().is_none_or(|&b| b == b'\n')
            } else if end == s && (p < s || s < e || ed.del > 0) {
                // Before the line: it survives when it still starts a line afterwards.
                let starts_line = match ins.last() {
                    Some(&b) => b == b'\n',
                    None => p == 0 || pre[p - 1] == b'\n',
                };
                if starts_line {
                    shift += newlines(ins) - newlines(&pre[p..end]);
                }
                starts_line
            } else if p == s && s == e && ed.del == 0 {
                // An insert into an empty line: lines added before or after leave it whole.
                if ins.last() == Some(&b'\n') {
                    shift += newlines(ins);
                    true
                } else {
                    ins.first() == Some(&b'\n')
                }
            } else {
                end < s
            };
            if !survives {
                self.saved = None;
                return;
            }
            if end < s {
                shift += newlines(ins) - newlines(&pre[p..end]);
            }
        }
        saved.line = (saved.line as isize + shift).max(0) as usize;
    }

    /// Whether `U` has a line to restore in `text`.
    pub(crate) fn available(&self, text: &[u8]) -> bool {
        self.saved
            .as_ref()
            .is_some_and(|s| bounds_of(text, s.line).is_some())
    }

    /// Plan `U` over `text` with the caret at `cursor`: the line span to replace, the saved text, and the
    /// caret offset into the restored line. Swaps in the current line, so the next `U` redoes.
    pub(crate) fn swap(
        &mut self,
        text: &[u8],
        cursor: usize,
    ) -> Option<(usize, usize, Vec<u8>, usize)> {
        let saved = self.saved.as_mut()?;
        let Some((s, e)) = bounds_of(text, saved.line) else {
            self.saved = None;
            return None;
        };
        let col = saved.col;
        if (s..=e).contains(&cursor) {
            saved.col = cursor - s;
        }
        let restored = std::mem::replace(&mut saved.text, text[s..e].to_vec());
        self.cursor = None;
        Some((s, e, restored, col))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;

    fn list(edits: Vec<Edit>) -> EditList {
        EditList::new(edits).unwrap()
    }

    fn saved_line(u: &LineUndo) -> Option<usize> {
        u.saved.as_ref().map(|s| s.line)
    }

    const TEXT: &[u8] = b"abc\ndef\nghi\n";

    fn on_line(line: usize) -> LineUndo {
        let mut u = LineUndo::default();
        u.prime(4 * line, true);
        u.on_edit(TEXT, &list(vec![Edit::delete(4 * line, 1)]), false);
        u
    }

    #[test]
    fn a_one_line_change_saves_the_line_once() {
        let mut u = on_line(1);
        assert_eq!(saved_line(&u), Some(1));
        assert_eq!(u.saved.as_ref().unwrap().text, b"def");
        // A second change on the same line keeps the first copy.
        u.prime(4, true);
        u.on_edit(b"abc\nef\nghi\n", &list(vec![Edit::delete(4, 1)]), false);
        assert_eq!(u.saved.as_ref().unwrap().text, b"def");
    }

    #[test]
    fn an_unarmed_change_does_not_save() {
        let mut u = LineUndo::default();
        u.on_edit(TEXT, &list(vec![Edit::delete(0, 1)]), false);
        assert_eq!(saved_line(&u), None);
    }

    #[test]
    fn lines_added_or_removed_above_move_the_saved_line() {
        let mut u = on_line(2);
        // A new line above (`yyP` on line 0): the saved line moves down.
        u.on_edit(TEXT, &list(vec![Edit::insert(0, b"new\n".to_vec())]), true);
        assert_eq!(saved_line(&u), Some(3));
        // Joining the two lines above it (`J` on line 0) moves it back up.
        let mut u = on_line(2);
        u.on_edit(TEXT, &list(vec![Edit::replace(3, 1, b" ".to_vec())]), true);
        assert_eq!(saved_line(&u), Some(1));
    }

    #[test]
    fn splitting_joining_or_deleting_the_line_drops_it() {
        // `J` on the saved line joins the next one into it.
        let mut u = on_line(1);
        u.on_edit(TEXT, &list(vec![Edit::replace(7, 1, b" ".to_vec())]), true);
        assert_eq!(saved_line(&u), None);
        // `dd` anywhere drops it, as nvim does.
        let mut u = on_line(0);
        u.on_edit(TEXT, &list(vec![Edit::delete(8, 4)]), true);
        assert_eq!(saved_line(&u), None);
        // An undo that removes the line.
        let mut u = on_line(1);
        u.on_reapply(TEXT, &list(vec![Edit::delete(3, 4)]));
        assert_eq!(saved_line(&u), None);
    }

    #[test]
    fn swap_restores_and_keeps_the_current_text() {
        let mut u = on_line(1);
        let now = b"abc\nef\nghi\n";
        let (s, e, text, col) = u.swap(now, 4).unwrap();
        assert_eq!((s, e, text.as_slice(), col), (4, 6, b"def".as_slice(), 0));
        assert_eq!(u.saved.as_ref().unwrap().text, b"ef");
    }
}
