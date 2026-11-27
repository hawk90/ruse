//! Whole-line range operations: join, set indents, format, comment, reindent and shift, plus the
//! cursor a refused linewise operator leaves.
//!
//! Split out of `planner/mod.rs` (#1392); the items are as they were there.

use super::{beep, edit, hspace_end, indent_cols, mark_line_target, nop, one};
use crate::editor::{format, op_landing, want_col, MarkAt, MarkCol, OpMarks, MAXCOL};
use crate::motion::{self, line_end, line_start, next_grapheme};
use crate::{BytePos, Command, Edit, EditList, EditorState, GroupHint, Mode, Motion, Plan, Tabs};

/// Plan a `[count]J` / `[count]gJ` join (nvim's `do_join`). `count` joins `count-1` seams (Vim: `J`/`2J` =
/// one join, `3J` = two), leaving the cursor on the LAST join. `J` strips the next line's leading blanks
/// and inserts one space per seam (none before `)`, after a tab or a trailing space, or around an empty
/// line part; two after `.`/`?`/`!` under `'joinspaces'`; `'formatoptions'` `M`/`B` drop it around
/// multibyte characters); `gJ` removes only the newline. Under `'formatoptions'` `j` a line that
/// continues a comment loses its leader (`skip_comment`, #618). The seams are disjoint in original
/// coordinates, so they apply as one `EditList`; `removed` maps each seam's original offset to its
/// post-edit position for the cursor.
pub(super) fn plan_join(
    st: &EditorState,
    cur: usize,
    count: u32,
    mode: Mode,
    hint: GroupHint,
    no_space: bool,
) -> Plan {
    let b = st.bytes();
    let fc = &st.view.format;
    let remove_comments = fc.has('j');
    let parts = if remove_comments {
        format::parse_comments(&fc.comments)
    } else {
        Vec::new()
    };
    let insert_space = !no_space;
    let joins = count.max(2) - 1;
    let mut edits: Vec<Edit> = Vec::new();
    let mut cursor = cur;
    let mut removed: isize = 0;
    let mut le = line_end(b, cur);
    let first_ls = line_start(b, cur);
    let first_len = le - first_ls;
    // nvim's `sumsize != 0` (anything joined so far) and `endcurr1`/`endcurr2` (the last two characters of
    // the latest line part, none after an empty one). An empty line taking part adds nothing, so
    // `a`+``+`b` joins to `a b`, and `foo`+`` to `foo` (#677).
    let mut sum = le > first_ls;
    let last_two = crate::charclass::last_two_chars;
    let (mut end1, mut end2) = if insert_space {
        last_two(&b[first_ls..le])
    } else {
        (None, None)
    };
    let mut prev_was_comment =
        remove_comments && format::skip_comment(&b[first_ls..le], &parts, false, insert_space).1;
    for _ in 0..joins {
        // No next line to join. A final newline ends the last line; it is no seam, so `J` on the last line
        // of `a\nb\n` leaves it alone instead of turning it into a trailing space (#677).
        if le + 1 >= b.len() {
            break;
        }
        let next_ls = le + 1;
        let next_le = line_end(b, next_ls);
        let mut skip = 0usize;
        if remove_comments {
            let line = &b[next_ls..next_le];
            if prev_was_comment {
                let (n, c) = format::skip_comment(line, &parts, true, insert_space);
                skip = n;
                prev_was_comment = c;
            } else {
                prev_was_comment = format::skip_comment(line, &parts, false, insert_space).1;
            }
        }
        let content = if insert_space {
            hspace_end(b, next_ls + skip)
        } else {
            next_ls + skip
        };
        let mut sep: Vec<u8> = Vec::new();
        if insert_space && content < next_le && b[content] != b')' && sum && end1 != Some('\t') {
            let c0 = crate::charclass::cell_char(b, content).unwrap_or('\0');
            let e1 = end1.unwrap_or('\0');
            let narrow = |c: char| u32::from(c) < 0x100;
            let mbyte_ok = (!fc.has('M') || (narrow(c0) && narrow(e1)))
                && (!fc.has('B')
                    || (narrow(c0) && !format::eat_space(e1))
                    || (narrow(e1) && !format::eat_space(c0)));
            if mbyte_ok {
                let mut e = end1;
                if e == Some(' ') {
                    e = end2;
                } else {
                    sep.push(b' ');
                }
                if fc.joinspaces && matches!(e, Some('.' | '?' | '!')) {
                    sep.push(b' ');
                }
            }
        }
        let (del_start, del_len) = (le, content - le);
        if content < next_le {
            sum = true;
        }
        (end1, end2) = if insert_space {
            last_two(&b[content..next_le])
        } else {
            (None, None)
        };
        cursor = del_start.saturating_add_signed(-removed); // Vim rests the cursor on the join seam
        removed += del_len as isize - sep.len() as isize;
        // Joining onto an EMPTY line removes just its `\n`, which reads as that line deleted; take the next
        // line's first char along, so the empty line's marks stay on the joined line (nvim `do_join`,
        // #1125). An empty next line has no char to take; its marks are dropped there.
        let first = next_grapheme(b, del_start + 1);
        let onto_empty = del_len == 1 && sep.is_empty() && del_start == line_start(b, del_start);
        edits.push(
            if onto_empty && first > del_start + 1 && b[del_start + 1] != b'\n' {
                Edit::replace(
                    BytePos(del_start),
                    first - del_start,
                    b[del_start + 1..first].to_vec(),
                )
            } else {
                Edit::replace(BytePos(del_start), del_len, sep)
            },
        );
        le = next_le; // end of the next line to join (original coords)
    }
    if edits.is_empty() {
        // No line below to join: Vim's `nv_join` beeps on the last line, stopping a macro or a `:normal`
        // (#1111). A count past the end joins what is there instead.
        return beep(cur, mode);
    }
    // nvim's `do_join` marks (#1302): `'[` where the first line ended, `']` past the joined line's end
    // (`sumsize`), and `'.` at column 0 of the line after it: the `del_lines` of the joined lines calls
    // `changed_lines` last, so `'.` is past the buffer's end when the join reached the last line.
    let top = crate::pos::raw::line_of(b, first_ls);
    Plan {
        op_marks: Some(OpMarks {
            start: MarkAt::new(top, MarkCol::Byte(first_len)),
            end: MarkAt::new(top, MarkCol::LineEnd),
            dot: MarkAt::new(top + 1, MarkCol::Byte(0)),
        }),
        ..edit(
            EditList::new(edits).expect("join seams are disjoint and ordered"),
            cursor,
            mode,
            hint,
        )
    }
}

/// Apply explicit indent `levels` (one per line from `first_line`) to `[first_line, last_line]`: each
/// non-blank line's leading whitespace becomes `levels[i] × shiftwidth`; blank lines are emptied. One
/// transaction. The frontend supplies tree-derived levels (tree-aware `=`); the core stays view-free.
pub(in crate::editor) fn plan_set_indents(
    st: &EditorState,
    first_line: usize,
    last_line: usize,
    cols: &[Option<usize>],
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let last = last_line.min(line_starts.len().saturating_sub(1));
    let first = first_line.min(last);
    let mut edits: Vec<Edit> = Vec::new();
    let insert = st.view.mode == Mode::Insert;
    let cur = st.view.cursor;
    let mut first_cursor = if insert { cur } else { line_starts[first] };
    #[allow(clippy::needless_range_loop)]
    for li in first..=last {
        let ls = line_starts[li];
        let le = line_end(b, ls);
        let fnb = motion::first_non_blank(b, ls);
        let want: Vec<u8> = if fnb >= le && !insert {
            Vec::new() // blank line → no indent
        } else {
            match cols.get(li - first).copied().unwrap_or(Some(0)) {
                Some(c) => st.view.indent.rebuilt(&b[ls..fnb], c),
                None => b[ls..fnb].to_vec(), // keep it
            }
        };
        if insert {
            // Insert mode re-indents the caret's line (#1206): the caret keeps its place in the text, or
            // goes to the end of the new indent when it sat in the old one (nvim's `change_indent`).
            if (ls..=le).contains(&cur) {
                first_cursor = ls + want.len() + cur.saturating_sub(fnb);
            }
        } else if li == first {
            first_cursor = ls + reindented_cursor(st, &want, &b[fnb..le]);
        }
        if b[ls..fnb] != want[..] {
            edits.push(Edit::replace(BytePos(ls), fnb - ls, want));
        }
    }
    if edits.is_empty() {
        return nop(first_cursor.min(b.len()), st.view.mode);
    }
    let edits =
        EditList::new(edits).expect("indent edits sit at distinct line starts, so disjoint");
    edit(edits, first_cursor, st.mode(), hint)
}

/// The formatting options a `gq` or an auto-wrap reads, from this View's (#618).
pub(super) fn fmt_opts(st: &EditorState) -> format::FmtOpts<'_> {
    let fc = &st.view.format;
    let listpat = if fc.has('n') && !fc.listpat.is_empty() {
        crate::pattern::Regex::compile(&fc.listpat, crate::pattern::Options::default()).ok()
    } else {
        None
    };
    format::FmtOpts {
        fo: &fc.options,
        parts: format::parse_comments(&fc.comments),
        ind: st.view.indent,
        joinspaces: fc.joinspaces,
        listpat,
    }
}

/// The line at `l` of `b` without its newline, if the buffer has one there.
fn buffer_line(b: &[u8], ls: usize) -> Vec<u8> {
    b[ls..line_end(b, ls)].to_vec()
}

/// Format the whole lines `[start, end)` (`start` a line start, `end` a line's content end) — the shared
/// tail of `gq`/`gw` over a motion, a search, a `gn` match or a mark, and of Visual `gq` (nvim's
/// `format_lines` through [`format::FmtBuf`], #618): each paragraph (blank lines, a change of comment
/// leader and, with `'formatoptions'` `n`, a list item end one) is joined and broken again at
/// `'textwidth'`, keeping its comment leader under `'formatoptions'` `q`. `below` is nvim's
/// `end_adjusted` (never set for Visual `gq`): the exclusive-linewise rule pulled the span's end off the
/// next line, so `gq` lands on the first non-blank of that next line instead of the last formatted one
/// (nvim `op_format`, #1071). `gw` ignores it.
pub(super) fn plan_format_lines(
    st: &EditorState,
    cur: usize,
    start: usize,
    end: usize,
    keep_cursor: bool,
    below: bool,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let lines: Vec<Vec<u8>> = b[start..end]
        .split(|&c| c == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    let n = lines.len();
    let before = (start > 0).then(|| buffer_line(b, line_start(b, start - 1)));
    let after = (end + 1 < b.len()).then(|| buffer_line(b, end + 1));
    let saved = keep_cursor.then(|| {
        let c = cur.clamp(start, end);
        let l = b[start..c].iter().filter(|&&x| x == b'\n').count();
        (l, c - line_start(b, c))
    });
    let mut fb = format::FmtBuf {
        lines,
        cur: (0, 0),
        saved,
        before,
        after,
        last_change: None,
    };
    let o = fmt_opts(st);
    fb.format_lines(n, st.view.text_width_for(true), &o);
    let new = fb.lines.join(&b'\n');
    let offset = |l: usize, col: usize| -> usize {
        start
            + fb.lines[..l].iter().map(|x| x.len() + 1).sum::<usize>()
            + col.min(fb.lines[l].len())
    };
    let new_len = new.len();
    // Where `gq` leaves the cursor, which is also where nvim's `op_format` puts `']` (`gw` puts it back).
    let last = if below && end + 1 < b.len() {
        // The line after the block, at its first non-blank.
        let fnb = motion::first_non_blank(b, end + 1);
        start + new_len + (fnb - end)
    } else {
        let l = fb.cur.0.min(fb.lines.len() - 1);
        let ws = fb.lines[l]
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count();
        offset(l, ws)
    };
    let cursor = if keep_cursor {
        let (l, col) = fb.saved.unwrap_or((0, 0));
        offset(l.min(fb.lines.len() - 1), col)
    } else {
        last
    };
    if new[..] == b[start..end] {
        return nop(cursor, Mode::Normal);
    }
    // nvim's `op_format` marks (#1302): `'[` on the first line (column 0 here; the callers move it to where
    // the operator started), `']` where the cursor ends, `'.` where the formatting's last change put it.
    let top = crate::pos::raw::line_of(b, start);
    let (end_line, end_col) = if last > start + new_len {
        // The line after the block: its first non-blank.
        (top + fb.lines.len(), last - (start + new_len) - 1)
    } else {
        let head = &new[..last - start];
        let line_start = head.iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        let lines = head.iter().filter(|&&c| c == b'\n').count();
        (top + lines, head.len() - line_start)
    };
    let (dl, dc) = fb.last_change.unwrap_or((0, 0));
    let op_marks = Some(OpMarks {
        start: MarkAt::new(top, MarkCol::Byte(0)),
        end: MarkAt::new(end_line, MarkCol::Byte(end_col)),
        dot: MarkAt::new(top + dl, MarkCol::Byte(dc)),
    });
    Plan {
        op_marks,
        ..edit(
            one(Edit::replace(BytePos(start), end - start, new)),
            cursor,
            Mode::Normal,
            hint,
        )
    }
}

/// `gc` over the lines `first..=last` (0-based): comment them out with `'commentstring'`, or uncomment
/// them when every non-blank one is commented ([`format::toggle_comment`], nvim's `vim._comment`). The
/// cursor goes to `at`'s line and column, clamped (the lines are replaced, as `nvim_buf_set_lines` does). An
/// empty `'commentstring'` changes nothing.
pub(super) fn plan_comment(
    st: &EditorState,
    first: usize,
    last: usize,
    at: usize,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let start = crate::pos::raw::nth_line_start(b, first);
    let end = line_end(b, crate::pos::raw::nth_line_start(b, last));
    let at_line = crate::pos::raw::line_of(b, at).clamp(first, last);
    let at_col = at - line_start(b, at);
    let lines: Vec<&[u8]> = b[start..end].split(|&c| c == b'\n').collect();
    let Some(new_lines) = format::toggle_comment(&lines, &st.view.format.commentstring) else {
        return nop(at, Mode::Normal);
    };
    let k = at_line - first;
    let cursor = start
        + new_lines[..k].iter().map(|l| l.len() + 1).sum::<usize>()
        + at_col.min(new_lines[k].len());
    let new = new_lines.join(&b'\n');
    if new[..] == b[start..end] {
        return nop(cursor, Mode::Normal);
    }
    edit(
        one(Edit::replace(BytePos(start), end - start, new)),
        cursor,
        Mode::Normal,
        hint,
    )
}

/// `=` over lines `[first_line, last_line]` (0-based, inclusive): the Lisp rules under `'lisp'`, else the C
/// rules (nvim's `=` runs `get_c_indent` whenever no 'indentexpr' is set), applied by `plan_set_indents`.
pub(super) fn plan_reindent(
    st: &EditorState,
    first_line: usize,
    last_line: usize,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let ind = st.view.indent;
    // `'lisp'`: the Lisp rules (#1163).
    let cols = if st.view.format.lisp {
        super::lisp::reindent(
            b,
            first_line,
            last_line,
            ind.tabs(),
            &st.view.format.lispwords,
            |c| ind.whitespace(c),
        )
    } else {
        // Else the C rules, with or without 'cindent': nvim's `=` runs `get_c_indent` whenever no
        // 'indentexpr' is set (#1163).
        let fc = &st.view.format;
        let e = super::cindent::CEnv {
            ts: ind.tabs(),
            cino: super::cindent::CinOptions::parse(&fc.cinoptions, ind.shift_width()),
            cinwords: &fc.cinwords,
            cinscopedecls: &fc.cinscopedecls,
            cinkeys: &fc.cinkeys,
            comments: &fc.comments,
        };
        super::cindent::reindent(b, first_line, last_line, &e)
    };
    plan_set_indents(st, first_line, last_line, &cols, hint)
}

/// Where `=` leaves the cursor on its first line, reindented to `indent` before `text` (the line from its
/// first non-blank): nvim's `op_reindent` ends with `beginline(BL_SOL | BL_FIX)`, which under
/// 'nostartofline' is `coladvance(curswant)`, as for `>>` (#1176): `j$==` stays at the end, `0==` at the
/// start. A blank line rests on its start.
fn reindented_cursor(st: &EditorState, indent: &[u8], text: &[u8]) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut line = indent.to_vec();
    line.extend_from_slice(text);
    line_col_cursor(&line, st.view.curswant.get(), st.view.indent.tabs())
}

/// The byte offset in `line` (one line's bytes, no newline) of a Normal-mode cursor at the wanted column
/// `want` (Vim's `coladvance`): the char covering that virtual column, the last char past the end or at
/// [`MAXCOL`], `0` on an empty line.
fn line_col_cursor(line: &[u8], want: usize, ts: Tabs) -> usize {
    let last = || motion::prev_grapheme(line, line.len());
    if line.is_empty() {
        0
    } else if want == MAXCOL {
        last()
    } else {
        let at = want_col(line, 0, want, ts);
        if at >= line.len() {
            last()
        } else {
            at
        }
    }
}

/// What a refused operator was ([`refused_linewise_cursor`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::editor) enum RefusedOp {
    Delete,
    Change,
    Yank,
}

/// Where a LINEWISE operator `cmd` leaves the cursor when its register cannot be written (#1174): nvim has
/// already run `do_pending_operator`, which put the cursor on the START of the range (`oap->start`: the
/// earlier of the cursor and the motion's landing, a linewise Visual selection's first line at column 0);
/// `op_delete` then fails, and under 'nostartofline' a delete goes back to the old wanted column on that line.
/// `cc` lands on the first non-blank first (`nv_lineop`'s `beginline(BL_WHITE)`). `None` for a command whose
/// range start this does not know.
pub(in crate::editor) fn refused_linewise_cursor(
    st: &EditorState,
    cmd: &Command,
    op: RefusedOp,
) -> Option<usize> {
    let b = st.bytes();
    let cur = st.view.cursor;
    let ts = st.view.indent.tabs();
    let start = if st.view.mode.selection().is_some() {
        line_start(b, st.view.anchor?.min(cur))
    } else {
        let landing = match *cmd {
            Command::Delete(_, Motion::Line) | Command::Yank(_, Motion::Line) => cur,
            Command::Change(_, Motion::Line) => motion::first_non_blank(b, line_start(b, cur)),
            Command::Delete(n, m) | Command::Change(n, m) | Command::Yank(n, m) => {
                op_landing(b, cur, m, n, &st.view.curswant, ts, op == RefusedOp::Yank)
            }
            Command::OpToMark {
                name,
                linewise: true,
                ..
            } => mark_line_target(b, st.view.op_mark(name)?.0),
            _ => return None,
        };
        cur.min(landing)
    };
    if op != RefusedOp::Delete {
        return Some(start);
    }
    let ls = line_start(b, start);
    Some(ls + line_col_cursor(&b[ls..line_end(b, ls)], st.view.curswant.get(), ts))
}

/// Plan a linewise shift (`>>` / `<<`) over `count` lines from the cursor's line down: `right` adds one
/// indent level to each line, `!right` removes up to one, and a truly empty line is never indented (Vim).
/// The register is untouched; the cursor rule is in `plan_shift_times`.
pub(super) fn plan_shift(
    st: &EditorState,
    cur: usize,
    count: u32,
    right: bool,
    hint: GroupHint,
) -> Plan {
    plan_shift_times(st, cur, count, right, 1, hint)
}

/// [`plan_shift`] by `times` indent levels (Visual `3>` shifts the selected lines three levels).
pub(super) fn plan_shift_times(
    st: &EditorState,
    cur: usize,
    count: u32,
    right: bool,
    times: u32,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let first_ls = line_start(b, cur);
    let first_le = line_end(b, first_ls);

    // Each line's leading whitespace is REBUILT at its new width (`cols ± sw`, floored at 0) in the
    // configured style — Vim's `shift_line` → `set_indent`: under `noexpandtab` the result is tabs at the
    // tabstop plus a spaces remainder, under `expandtab` all spaces (a leading tab becomes spaces).
    let mut edits: Vec<Edit> = Vec::new();
    // The first line's rebuilt leading whitespace, for the cursor below.
    let mut first_new_ws: Vec<u8> = Vec::new();
    let mut ls = first_ls;
    // The last line the shift covers, for `']`.
    let mut last_ls = first_ls;
    for i in 0..count.max(1) {
        last_ls = ls;
        let le = line_end(b, ls);
        let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tabs());
        // Vim indents a whitespace-only line but never a truly EMPTY one (`ls == le`).
        let ind = st.view.indent;
        let old = &b[ls..ls + ws];
        let want = if right {
            (ls < le).then(|| ind.rebuilt(old, ind.shifted(cols, times as usize, false)))
        } else {
            Some(ind.rebuilt(old, ind.shifted(cols, times as usize, true)))
        };
        if let Some(want) = want {
            if i == 0 {
                first_new_ws.clone_from(&want);
            }
            if b[ls..ls + ws] != want[..] {
                edits.push(Edit::replace(BytePos(ls), ws, want));
            }
        }
        if le >= b.len() {
            break; // no more lines: a count past the end shifts the lines that exist (#1159).
        }
        ls = le + 1;
    }

    // Cursor: the FIRST shifted line at the wanted column, as nvim's `op_shift` → `beginline(BL_SOL | BL_FIX)`,
    // which under 'nostartofline' is `coladvance(curswant)` (#1176): `$>>` stays at the line's end, `w>>`
    // keeps its column. Computed against the POST-edit line (the rebuilt whitespace replaces the old); an
    // empty line got no indent, so the cursor stays at its start.
    let cursor = if right && first_ls >= first_le {
        first_ls
    } else {
        let (_, old_ws) = indent_cols(&b[first_ls..first_le], st.view.indent.tabs());
        let mut line = first_new_ws;
        line.extend_from_slice(&b[first_ls + old_ws..first_le]);
        first_ls + line_col_cursor(&line, st.view.curswant.get(), st.view.indent.tabs())
    };

    // nvim's `op_shift` marks the lines whatever it changed (#1302): `'[` on the first line (at column 0
    // here; the callers move it to where the operator started, `start_col_at`), `']` on the last line's last
    // byte, `'.` at column 0 of the first line (`changed_lines`).
    let top = crate::pos::raw::line_of(b, first_ls);
    let op_marks = Some(OpMarks {
        start: MarkAt::new(top, MarkCol::Byte(0)),
        end: MarkAt::new(crate::pos::raw::line_of(b, last_ls), MarkCol::LastByte),
        dot: MarkAt::new(top, MarkCol::Byte(0)),
    });
    if edits.is_empty() {
        // Nothing to indent/unindent (e.g. `<<` at column 0, or `>>` on an empty line): a pure cursor move.
        return Plan {
            op_marks,
            ..nop(cursor, st.mode())
        };
    }
    let edits = EditList::new(edits)
        .expect("shift edits sit at distinct line starts, so they are disjoint");
    Plan {
        op_marks,
        ..edit(edits, cursor, st.mode(), hint)
    }
}
