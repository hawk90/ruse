//! Where `u` / `CTRL-R` / `g-` / `g+` leave the cursor — Vim's `u_undoredo` placement (#1063).
//!
//! Vim saves each change as line-granular undo ENTRIES (`ue_top`, the lines below it, and their text) plus
//! the cursor at the start of the change (`uh_cursor`). Undo/redo then put the cursor:
//!
//! 1. at `uh_cursor` when its line lies in (or next to) a re-applied entry, else on the first line an entry
//!    actually changed;
//! 2. one line up when that is the line just below `uh_cursor`'s (the `o` case);
//! 3. at `uh_cursor`'s column when on `uh_cursor`'s line, else at the sticky column (`nostartofline`).
//!
//! ruse records byte edits, not line entries, so [`entries`] derives the entry an edit would have been in
//! Vim: a whole-line edit (it starts at a line start and inserts and deletes only whole lines) covers just
//! those lines, any other edit covers every line it touches.

use crate::edit::EditList;
use crate::pos::{count_newlines, line_of, line_start};

/// One Vim-style undo entry, in the coordinates of the text AFTER its list was applied.
pub(crate) struct Entry {
    /// The number of lines above the entry (Vim's `ue_top`).
    top: usize,
    /// How many lines the entry holds after the step (Vim's `ue_size`).
    newsize: usize,
    /// How many of those leading lines the step left as they were.
    same: usize,
}

/// Append the entries one applied list contributes: `list` turned `old` into `new`.
pub(crate) fn entries(old: &[u8], new: &[u8], list: &EditList, out: &mut Vec<Entry>) {
    let mut scan = 0;
    let mut old_line = 0;
    let mut byte_delta: isize = 0;
    let mut line_delta: isize = 0;
    for e in list.edits() {
        let pos = e.pos.0;
        let end = pos + e.del;
        old_line += count_newlines(&old[scan..pos]);
        scan = pos;
        let del_nl = count_newlines(&old[pos..end]);
        let ins_nl = count_newlines(&e.ins);
        let whole_lines = (pos == 0 || old[pos - 1] == b'\n')
            && (e.del == 0 || old[end - 1] == b'\n')
            && e.ins.last().is_none_or(|&c| c == b'\n');
        let (oldsize, newsize) = if whole_lines {
            (del_nl, ins_nl)
        } else {
            (del_nl + 1, ins_nl + 1)
        };
        let new_pos = pos.saturating_add_signed(byte_delta);
        let top = old_line.saturating_add_signed(line_delta);
        let same = same_lines(
            old,
            line_start(old, pos),
            new,
            line_start(new, new_pos),
            oldsize.min(newsize),
        );
        out.push(Entry { top, newsize, same });
        byte_delta += e.ins.len() as isize - e.del as isize;
        line_delta += ins_nl as isize - del_nl as isize;
    }
}

/// How many of the first `n` lines starting at `a` in `old` and at `b` in `new` are equal.
fn same_lines(old: &[u8], mut a: usize, new: &[u8], mut b: usize, n: usize) -> usize {
    for i in 0..n {
        let ae = old[a..].iter().position(|&c| c == b'\n').map(|k| a + k);
        let be = new[b..].iter().position(|&c| c == b'\n').map(|k| b + k);
        let la = &old[a..ae.unwrap_or(old.len())];
        let lb = &new[b..be.unwrap_or(new.len())];
        if la != lb {
            return i;
        }
        match (ae, be) {
            (Some(x), Some(y)) => (a, b) = (x + 1, y + 1),
            _ => return i + 1,
        }
    }
    n
}

/// Where the cursor lands, as a 0-based line of the text after the step and either a byte column on it or
/// `None` for the sticky column (Vim `beginline(BL_SOL | BL_FIX)` under `nostartofline`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UndoCursor {
    /// 0-based line of the text after the step.
    pub line: usize,
    /// Byte column on that line (clamp it to the line), or `None` for the sticky column.
    pub col: Option<usize>,
}

/// Vim's placement over the step's last-group `entries`. `uh` is `uh_cursor` as a 0-based (line, byte
/// column); `cur_line` is the cursor's 0-based line before the step; `lines` is the line count after it.
pub(crate) fn place(
    entries: &[Entry],
    uh: (usize, usize),
    cur_line: usize,
    lines: usize,
) -> UndoCursor {
    // Vim works in 1-based line numbers; `top` is already "the line above", so it needs no shift.
    let uh_lnum = uh.0 + 1;
    let mut lnum = cur_line + 1;
    let mut newlnum: Option<usize> = None;
    let mut pinned = false;
    for (k, e) in entries.iter().enumerate() {
        if uh_lnum >= e.top && uh_lnum <= e.top + e.newsize + 1 {
            lnum = uh_lnum;
            pinned = true;
        } else if !pinned && newlnum.is_none_or(|n| e.top < n) {
            if e.same == e.newsize && newlnum.is_none() && k + 1 == entries.len() {
                newlnum = Some(e.top);
                lnum = e.top + 1;
            } else if e.same < e.newsize {
                newlnum = Some(e.top + e.same);
                lnum = e.top + e.same + 1;
            }
        }
    }
    let lines = lines.max(1);
    lnum = lnum.clamp(1, lines);
    if uh_lnum + 1 == lnum && lnum > 1 {
        lnum -= 1;
    }
    UndoCursor {
        line: lnum - 1,
        col: (uh_lnum == lnum).then_some(uh.1),
    }
}

/// `offset` in `text` as a 0-based (line, byte column).
pub(crate) fn line_col(text: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    (line_of(text, offset), offset - line_start(text, offset))
}

/// Vim's line count: a final `\n` ends the last line rather than starting another.
pub(crate) fn line_count(text: &[u8]) -> usize {
    let n = count_newlines(text) + 1;
    if text.last() == Some(&b'\n') {
        n - 1
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;

    fn run(old: &str, edits: Vec<Edit>) -> Vec<(usize, usize, usize)> {
        let list = EditList::new(edits).expect("disjoint");
        let new = list.apply_to(old.as_bytes());
        let mut out = Vec::new();
        entries(old.as_bytes(), &new, &list, &mut out);
        out.iter().map(|e| (e.top, e.newsize, e.same)).collect()
    }

    #[test]
    fn a_charwise_edit_covers_the_lines_it_touches() {
        // `x` on line 2: one line, changed.
        assert_eq!(run("ab\ncd\nef", vec![Edit::delete(3, 1)]), [(1, 1, 0)]);
        // Inserting `x\ny` mid-line 1 leaves two lines where there was one.
        assert_eq!(
            run("ab\ncd", vec![Edit::insert(1, b"x\ny".to_vec())]),
            [(0, 2, 0)]
        );
        // A join: two lines become one.
        assert_eq!(
            run("ab\ncd", vec![Edit::replace(2, 1, b" ".to_vec())]),
            [(0, 1, 0)]
        );
    }

    #[test]
    fn a_whole_line_edit_covers_only_those_lines() {
        // Deleting line 2 (`dd`): nothing is left in the entry, which sits under line 1.
        assert_eq!(run("ab\ncd\nef", vec![Edit::delete(3, 3)]), [(1, 0, 0)]);
        // Putting a line above line 1 (`P`).
        assert_eq!(
            run("ab\ncd", vec![Edit::insert(0, b"xy\n".to_vec())]),
            [(0, 1, 0)]
        );
        // Lines further down are numbered after the earlier edits of the same list.
        assert_eq!(
            run(
                "ab\ncd\nef",
                vec![Edit::insert(0, b"x\ny\n".to_vec()), Edit::delete(6, 1)]
            ),
            [(0, 2, 0), (4, 1, 0)]
        );
    }

    #[test]
    fn unchanged_leading_lines_are_skipped() {
        // Replacing two lines where the first stays the same: the change starts one line down.
        assert_eq!(
            run("ab\ncd\nef", vec![Edit::replace(0, 5, b"ab\nXY".to_vec())]),
            [(0, 2, 1)]
        );
    }

    #[test]
    fn placement_follows_vim() {
        let e = |top, newsize, same| Entry { top, newsize, same };
        // `uh_cursor` inside the entry: back there, column and all.
        assert_eq!(
            place(&[e(0, 1, 0)], (0, 4), 1, 2),
            UndoCursor {
                line: 0,
                col: Some(4)
            }
        );
        // Far from the change: the first changed line, sticky column.
        assert_eq!(
            place(&[e(4, 1, 0)], (0, 4), 0, 9),
            UndoCursor { line: 4, col: None }
        );
        // Undoing a deletion at the end: the line under the entry, clamped to the buffer.
        assert_eq!(
            place(&[e(3, 0, 0)], (0, 0), 0, 3),
            UndoCursor { line: 2, col: None }
        );
        // `uh_cursor` just under the entry still counts as in it (undoing `o`).
        assert_eq!(
            place(&[e(5, 0, 0)], (5, 2), 0, 9),
            UndoCursor {
                line: 5,
                col: Some(2)
            }
        );
    }

    #[test]
    fn line_count_ignores_a_final_newline() {
        assert_eq!(line_count(b""), 1);
        assert_eq!(line_count(b"a"), 1);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"\n"), 1);
    }
}
