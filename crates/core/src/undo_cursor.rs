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
//! The same entries give the `[`/`]`/`.` marks the step leaves (`change_marks`, #1168).
//!
//! ruse records byte edits, not line entries, so [`entries`] derives the entry an edit would have been in
//! Vim: a whole-line edit (it starts at a line start and inserts and deletes only whole lines) covers just
//! those lines, any other edit covers every line it touches. A change whose Vim operator saved all its
//! lines at once (a blockwise operator) says so with an [`UndoEntry`], and its edits make one entry (#1612).

use crate::edit::EditList;
use crate::pos::count_newlines;
use crate::pos::raw::{line_of, line_start};

/// One Vim-style undo entry, in the coordinates of the text AFTER its list was applied.
pub(crate) struct Entry {
    /// The number of lines above the entry (Vim's `ue_top`).
    top: usize,
    /// How many lines the entry held before the step.
    oldsize: usize,
    /// How many lines the entry holds after the step (Vim's `ue_size`).
    newsize: usize,
    /// How many of those leading lines the step left as they were.
    same: usize,
}

/// How a change's edits map onto Vim undo entries (#1612). Vim's operators `u_save` their lines before
/// they change them, so the entries follow the saves, not the edits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum UndoEntry {
    /// One entry per edit (see [`entries`]): what an edit that saved only its own lines leaves.
    #[default]
    PerEdit,
    /// One entry from the first edited line to the last: a blockwise operator, whose `op_shift` /
    /// `op_delete` / `op_tilde` / `op_replace` / `block_insert` saves the block's lines at once.
    One,
    /// No entry: the lines are in an earlier change of the same undo step, which saved them already. A
    /// block change copying its text down the block (nvim's `op_change` saves nothing for that copy; its
    /// `op_delete` saved the block's lines).
    Covered,
}

/// Append the entries one applied list contributes: `list` turned `old` into `new`, its edits making
/// entries as `shape` says.
pub(crate) fn entries(
    old: &[u8],
    new: &[u8],
    list: &EditList,
    shape: UndoEntry,
    out: &mut Vec<Entry>,
) {
    if shape == UndoEntry::Covered {
        return;
    }
    let first = out.len();
    edit_entries(old, new, list, out);
    if shape == UndoEntry::One && out.len() > first + 1 {
        let (top, end) = out[first..].iter().fold((usize::MAX, 0), |(top, end), e| {
            (top.min(e.top), end.max(e.top + e.newsize))
        });
        let grown: isize = out[first..]
            .iter()
            .map(|e| e.newsize as isize - e.oldsize as isize)
            .sum();
        let newsize = end - top;
        let oldsize = newsize.saturating_add_signed(-grown);
        let same = same_lines(
            old,
            nth_line_start(old, top),
            new,
            nth_line_start(new, top),
            oldsize.min(newsize),
        );
        out.truncate(first);
        out.push(Entry {
            top,
            oldsize,
            newsize,
            same,
        });
    }
}

/// The byte offset of 0-based line `n` in `text`, or its length when it has fewer lines.
fn nth_line_start(text: &[u8], n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    text.iter()
        .enumerate()
        .filter(|&(_, &c)| c == b'\n')
        .nth(n - 1)
        .map_or(text.len(), |(i, _)| i + 1)
}

/// Append one entry per edit of `list`, which turned `old` into `new`.
fn edit_entries(old: &[u8], new: &[u8], list: &EditList, out: &mut Vec<Entry>) {
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
        // Whole lines below an unterminated last line: they go with the newline before them (`dd` or a
        // linewise put there), so the edit runs from that line's end to the end of the text. Vim's entry
        // holds only those lines, below the line the edit starts on.
        let tail_lines = !whole_lines
            && end == old.len()
            && pos > 0
            && old[pos - 1] != b'\n'
            && (e.del > 0 || !e.ins.is_empty())
            && (e.del == 0 || old[pos] == b'\n')
            && (e.ins.is_empty() || (e.ins[0] == b'\n' && e.ins.last() != Some(&b'\n')));
        let (oldsize, newsize) = if whole_lines || tail_lines {
            (del_nl, ins_nl)
        } else {
            (del_nl + 1, ins_nl + 1)
        };
        let new_pos = pos.saturating_add_signed(byte_delta);
        let top = old_line.saturating_add_signed(line_delta) + usize::from(tail_lines);
        let (old_from, new_from) = if tail_lines {
            (pos + 1, new_pos + 1)
        } else {
            (line_start(old, pos), line_start(new, new_pos))
        };
        let same = same_lines(old, old_from, new, new_from, oldsize.min(newsize));
        out.push(Entry {
            top,
            oldsize,
            newsize,
            same,
        });
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
    /// The lines the step leaves the `[`/`]`/`.` marks on.
    pub marks: ChangeMarks,
}

/// Where `u` / `CTRL-R` leave the `[`, `]` and `.` marks: column 0 of these 0-based lines of the text after
/// the step (#1168). Vim's `u_undoredo` brackets the lines its entries re-applied (`b_op_start` /
/// `b_op_end`), and `changed_lines` puts `.` on the top line of the last entry it re-applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ChangeMarks {
    /// The `[` mark's line.
    pub start_line: usize,
    /// The `]` mark's line.
    pub end_line: usize,
    /// The `.` mark's line, or `None` when no entry changed a line.
    pub last_change_line: Option<usize>,
}

/// Vim's `[`/`]`/`.` bookkeeping in `u_undoredo` over one group's `entries`, in the order they were
/// re-applied. `lines_before` and `lines_after` are the line counts before and after the group: `[` starts
/// at the old line count and only ever moves up, so undoing the deletion of the last line leaves it there,
/// one line above the restored line (nvim v0.12.6).
pub(crate) fn change_marks(
    entries: &[Entry],
    lines_before: usize,
    lines_after: usize,
) -> ChangeMarks {
    // 1-based line numbers, as in Vim; `top` is the line above the entry.
    let mut start = lines_before as isize;
    let mut end: isize = 0;
    let mut dot = None;
    for e in entries {
        let (top, old, new) = (e.top as isize, e.oldsize as isize, e.newsize as isize);
        if old != new {
            // `mark_adjust` for the marks set by the entries before this one.
            if start > top + old {
                start += new - old;
            }
            if end > top + old {
                end += new - old;
            }
        }
        if old > 0 || new > 0 {
            dot = Some(top + 1);
        }
        start = start.min(top + 1);
        if new == 0 && top + 1 > end {
            end = top + 1;
        } else if top + new > end {
            end = top + new;
        }
    }
    let lines = lines_after.max(1) as isize;
    let line = |n: isize| (n.clamp(1, lines) - 1) as usize;
    ChangeMarks {
        start_line: line(start),
        end_line: line(end),
        last_change_line: dot.map(line),
    }
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
        marks: ChangeMarks::default(),
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
    use crate::pos::BytePos;

    fn run(old: &str, edits: Vec<Edit>) -> Vec<(usize, usize, usize)> {
        run_as(old, edits, UndoEntry::PerEdit)
    }

    fn run_as(old: &str, edits: Vec<Edit>, shape: UndoEntry) -> Vec<(usize, usize, usize)> {
        let list = EditList::new(edits).expect("disjoint");
        let new = list.apply_to(old.as_bytes());
        let mut out = Vec::new();
        entries(old.as_bytes(), &new, &list, shape, &mut out);
        out.iter().map(|e| (e.top, e.newsize, e.same)).collect()
    }

    #[test]
    fn a_one_entry_list_is_one_entry_over_all_its_lines() {
        // A block `x` on lines 2 to 4: nvim's `op_delete` saves the three lines as one entry (#1612).
        let x = || {
            vec![
                Edit::delete(BytePos(4), 1),
                Edit::delete(BytePos(7), 1),
                Edit::delete(BytePos(10), 1),
            ]
        };
        assert_eq!(
            run_as("ab\ncd\nef\ngh", x(), UndoEntry::PerEdit),
            [(1, 1, 0), (2, 1, 0), (3, 1, 0)]
        );
        assert_eq!(run_as("ab\ncd\nef\ngh", x(), UndoEntry::One), [(1, 3, 0)]);
        // A block change's copy down the block is in its delete's entry already.
        assert_eq!(run_as("ab\ncd\nef\ngh", x(), UndoEntry::Covered), []);
        // A row the block skipped (a short line a `>` leaves alone) is inside the entry, unchanged.
        assert_eq!(
            run_as(
                "ab\n\ncd",
                vec![
                    Edit::insert(BytePos(1), b"  ".to_vec()),
                    Edit::insert(BytePos(5), b"  ".to_vec()),
                ],
                UndoEntry::One
            ),
            [(0, 3, 0)]
        );
        // A block change whose Insert took a `<CR>`: the entry holds the lines it added.
        assert_eq!(
            run_as(
                "ab\ncd",
                vec![
                    Edit::replace(BytePos(1), 1, b"x\ny".to_vec()),
                    Edit::replace(BytePos(4), 1, b"x".to_vec()),
                ],
                UndoEntry::One
            ),
            [(0, 3, 0)]
        );
        // The `[`/`]`/`.` marks undoing it leaves: `.` on the entry's top line, not its last.
        let list = EditList::new(x()).expect("disjoint");
        let new = list.apply_to(b"ab\ncd\nef\ngh");
        let mut out = Vec::new();
        entries(b"ab\ncd\nef\ngh", &new, &list, UndoEntry::One, &mut out);
        assert_eq!(change_marks(&out, 4, 4).last_change_line, Some(1));
    }

    #[test]
    fn a_charwise_edit_covers_the_lines_it_touches() {
        // `x` on line 2: one line, changed.
        assert_eq!(
            run("ab\ncd\nef", vec![Edit::delete(BytePos(3), 1)]),
            [(1, 1, 0)]
        );
        // Inserting `x\ny` mid-line 1 leaves two lines where there was one.
        assert_eq!(
            run("ab\ncd", vec![Edit::insert(BytePos(1), b"x\ny".to_vec())]),
            [(0, 2, 0)]
        );
        // A join: two lines become one.
        assert_eq!(
            run("ab\ncd", vec![Edit::replace(BytePos(2), 1, b" ".to_vec())]),
            [(0, 1, 0)]
        );
    }

    #[test]
    fn a_whole_line_edit_covers_only_those_lines() {
        // Deleting line 2 (`dd`): nothing is left in the entry, which sits under line 1.
        assert_eq!(
            run("ab\ncd\nef", vec![Edit::delete(BytePos(3), 3)]),
            [(1, 0, 0)]
        );
        // Putting a line above line 1 (`P`).
        assert_eq!(
            run("ab\ncd", vec![Edit::insert(BytePos(0), b"xy\n".to_vec())]),
            [(0, 1, 0)]
        );
        // Lines further down are numbered after the earlier edits of the same list.
        assert_eq!(
            run(
                "ab\ncd\nef",
                vec![
                    Edit::insert(BytePos(0), b"x\ny\n".to_vec()),
                    Edit::delete(BytePos(6), 1)
                ]
            ),
            [(0, 2, 0), (4, 1, 0)]
        );
    }

    #[test]
    fn unchanged_leading_lines_are_skipped() {
        // Replacing two lines where the first stays the same: the change starts one line down.
        assert_eq!(
            run(
                "ab\ncd\nef",
                vec![Edit::replace(BytePos(0), 5, b"ab\nXY".to_vec())]
            ),
            [(0, 2, 1)]
        );
    }

    #[test]
    fn placement_follows_vim() {
        let e = |top, newsize, same| Entry {
            top,
            oldsize: newsize,
            newsize,
            same,
        };
        let at = |c: UndoCursor| (c.line, c.col);
        // `uh_cursor` inside the entry: back there, column and all.
        assert_eq!(at(place(&[e(0, 1, 0)], (0, 4), 1, 2)), (0, Some(4)));
        // Far from the change: the first changed line, sticky column.
        assert_eq!(at(place(&[e(4, 1, 0)], (0, 4), 0, 9)), (4, None));
        // Undoing a deletion at the end: the line under the entry, clamped to the buffer.
        assert_eq!(at(place(&[e(3, 0, 0)], (0, 0), 0, 3)), (2, None));
        // `uh_cursor` just under the entry still counts as in it (undoing `o`).
        assert_eq!(at(place(&[e(5, 0, 0)], (5, 2), 0, 9)), (5, Some(2)));
    }

    #[test]
    fn a_line_put_below_an_unterminated_last_line_is_an_entry_of_its_own() {
        // `dd` on the unterminated last line takes the newline before it: the entry is the line alone.
        assert_eq!(
            run("ab\ncd", vec![Edit::delete(BytePos(2), 3)]),
            [(1, 0, 0)]
        );
        // Undoing it puts the line back below line 1.
        assert_eq!(
            run("ab", vec![Edit::insert(BytePos(2), b"\ncd".to_vec())]),
            [(1, 1, 0)]
        );
        // A charwise edit at the end of the text is not one: it changes the last line.
        assert_eq!(
            run("ab", vec![Edit::insert(BytePos(2), b"x".to_vec())]),
            [(0, 1, 0)]
        );
    }

    #[test]
    fn change_marks_follow_vim() {
        let e = |top, oldsize, newsize| Entry {
            top,
            oldsize,
            newsize,
            same: 0,
        };
        let m = |start_line, end_line, last_change_line| ChangeMarks {
            start_line,
            end_line,
            last_change_line,
        };
        // Undoing `dw` on line 2: `[`, `]` and `.` all on line 2.
        assert_eq!(change_marks(&[e(1, 1, 1)], 2, 2), m(1, 1, Some(1)));
        // Undoing a join of lines 2 and 3: `]` on line 3.
        assert_eq!(change_marks(&[e(1, 1, 2)], 2, 3), m(1, 2, Some(1)));
        // Undoing `dd` of the last line: `[` stays at the old last line, one above the restored one.
        assert_eq!(change_marks(&[e(2, 0, 1)], 2, 3), m(1, 2, Some(2)));
        // Undoing `P` of line 2: the deleted line's place; `.` past the end is clamped to the last line.
        assert_eq!(change_marks(&[e(1, 1, 0)], 4, 3), m(1, 1, Some(1)));
        assert_eq!(change_marks(&[e(3, 1, 0)], 4, 3), m(2, 2, Some(2)));
        // A later entry above an earlier one moves the earlier one's marks down by its line change.
        assert_eq!(
            change_marks(&[e(4, 1, 1), e(0, 0, 2)], 6, 8),
            m(0, 6, Some(0))
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

    proptest::proptest! {
        #![proptest_config(crate::test_cases(256))]

        /// #1612: a block operator's edits (at most one per row, inside the row) as ONE entry: it runs from
        /// the first edited row to the last, holds every per-edit entry's lines, and undoing it leaves `'.`
        /// on its top row whichever rows between were edited.
        #[test]
        fn a_one_entry_list_spans_its_rows(
            rows in proptest::collection::vec(
                ("[a-c]{0,4}", proptest::option::of((0usize..5, 0usize..3, "[xy]{0,2}"))),
                1..8,
            ),
        ) {
            let mut old = Vec::new();
            let mut edits = Vec::new();
            let mut edited = Vec::new();
            for (i, (line, edit)) in rows.iter().enumerate() {
                let ls = old.len();
                old.extend_from_slice(line.as_bytes());
                old.push(b'\n');
                if let Some((at, del, ins)) = edit {
                    let at = (*at).min(line.len());
                    let del = (*del).min(line.len() - at);
                    if del > 0 || !ins.is_empty() {
                        edits.push(Edit::replace(BytePos(ls + at), del, ins.as_bytes().to_vec()));
                        edited.push(i);
                    }
                }
            }
            let (Some(&first), Some(&last)) = (edited.first(), edited.last()) else {
                return Ok(());
            };
            let list = EditList::new(edits).expect("one edit per row: disjoint");
            let new = list.apply_to(&old);
            let (mut one, mut per) = (Vec::new(), Vec::new());
            entries(&old, &new, &list, UndoEntry::One, &mut one);
            entries(&old, &new, &list, UndoEntry::PerEdit, &mut per);
            proptest::prop_assert_eq!(one.len(), 1);
            let e = &one[0];
            proptest::prop_assert_eq!((e.top, e.oldsize, e.newsize), (first, last - first + 1, last - first + 1));
            for p in &per {
                proptest::prop_assert!(e.top <= p.top && p.top + p.newsize <= e.top + e.newsize);
            }
            let lines = line_count(&old);
            proptest::prop_assert_eq!(change_marks(&one, lines, lines).last_change_line, Some(first));
        }
    }
}
