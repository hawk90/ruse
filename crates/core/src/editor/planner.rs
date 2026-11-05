use super::*;
use crate::RegKind;

/// The most bytes one counted repeat of text may produce (`{count}p`, `{count}gr`): 64 MiB is far beyond
/// any interactive intent, and clamping the count keeps a hostile one (a digit run that saturates) from
/// requesting a multi-gigabyte allocation (#683).
const MAX_PUT_BYTES: usize = 1 << 26;

/// Enter a blockwise insert-replicate session (`CTRL-V` `I`/`A`/`c`): compute the top-row insert point
/// and, for `c`, the block-delete edits + captured blockwise register. The typed text is replicated down
/// the block by [`block_replicate`] when the session's `<Esc>` closes it. `seed` is text already typed into
/// the session: blockwise Select's replace-selection policy (`g CTRL-H` then a printable key) is a block `c`
/// whose first char is the key, so it replicates with the rest of the run.
fn plan_block_insert(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    kind: &BlockInsertKind,
    seed: &[u8],
) -> Plan {
    let append = matches!(kind, BlockInsertKind::Append);
    let change = matches!(kind, BlockInsertKind::Change);
    let ts = st.view.indent.tab_width;
    let block = if let Some(anchor) = st.view.anchor {
        // Live block insert from a Visual-block selection: the two corners are `anchor` and `cur`. `$`
        // (curswant at MAXCOL) makes the block ragged: `$A` appends at EACH row's own line end.
        block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL)
    } else if let Some(geom) = st.view.last_block_geom {
        // Dot-repeat (`.`): no live selection. Rebuild an equivalent block of the stored WIDTH x HEIGHT with
        // its top-left at the caret — nvim positions the repeat at the cursor and reuses the original geometry
        // (verified vs nvim v0.12.4). The recorded command carries the kind (I/A/c); `geom` carries the size
        // and the `$`-ragged flag (only meaningful with `A`). Rows clamp at end-of-buffer like the live path.
        block_at(b, cur, geom.width, geom.height, ts, geom.to_eol)
    } else {
        // Neither a live selection nor a prior block insert to repeat — degrade to a plain Insert entry.
        return nop(cur, Mode::Insert);
    };
    let to_eol = append && block.hi == MAXCOL;
    let width = block.width().max(1);
    let lo = block.lo;
    let target_col = if append { lo + width } else { lo };
    let top = block.rows[0];
    let top_ls = top.ls;
    let rows_below = block.rows.len().saturating_sub(1);

    let mut edits: Vec<Edit> = Vec::new();
    let mut reg: Option<Register> = None;
    // `c`: delete the block on every row first (capture it blockwise), then insert at the left edge.
    let (insert_start, top_left) = if change {
        reg = Some(Register::blockwise_width(block.yank_text(b), width));
        for (i, r) in block.rows.iter().enumerate() {
            let mut keep = vec![b' '; r.out_l + r.out_r];
            if i == 0 {
                keep.splice(r.out_l..r.out_l, seed.iter().copied());
            }
            if r.de > r.ds || !keep.is_empty() {
                edits.push(Edit::replace(BytePos(r.ds), r.de - r.ds, keep));
            }
        }
        (top.ds + top.out_l, top.ds + top.out_l)
    } else if to_eol {
        (top.le, top.ds) // `$A`: append at the top row's own end (ragged), no padding
    } else {
        // `I` at the left edge, `A` past the right edge; a short `A` row is padded out to the column. On
        // this row the caret cannot sit inside a `<Tab>` or wide char crossing the column, so the insert goes
        // before it (`I`) or after it (`A`), as in nvim; the other rows split it (`block_replicate`).
        let (at, _, pre, _) = block_insert_point(b, top_ls, target_col, ts, append);
        let at = if at < top.le && pre > 0 {
            if append {
                cluster_cells(b, at, top.le, 0, ts).0
            } else {
                at
            }
        } else {
            if pre > 0 {
                edits.push(Edit::insert(BytePos(at), vec![b' '; pre]));
            }
            at + pre
        };
        (at, if append { top.ds } else { at })
    };

    // The top row's length once a change has deleted its part of the block, before anything is typed.
    let top_len = if change {
        (top.le - top.ls) - (top.de - top.ds) + top.out_l + top.out_r
    } else {
        top.le - top.ls
    };
    let session = BlockInsert {
        insert_start,
        top_left,
        top_line_start: top_ls,
        top_len,
        target_col,
        rows_below,
        append,
        to_eol,
        change,
        width,
    };
    let list = EditList::new(edits).expect("block-insert enter edits are disjoint (one per line)");
    let is_edit = !list.is_empty();
    Plan {
        action: Action::BlockInsertArm {
            edits: list,
            hint,
            session,
        },
        cursor: insert_start + seed.len(),
        mode: Mode::Insert,
        is_edit,
        effects: Vec::new(),
        set_register: reg.map(RegWrite::Edit),
        set_anchor: None,
        set_mark: None,
    }
}

/// Visual `d`/`c`/`y` over a charwise (`line == false`) or linewise selection `anchor`..`cur`; also the
/// linewise uppercase forms `D`/`X`/`Y`/`C`/`S`/`R` ([`VisualOp::Lines`]), which force `line`.
fn plan_selection_span(
    st: &EditorState,
    b: &[u8],
    anchor: usize,
    cur: usize,
    op: OpKind,
    line: bool,
    hint: GroupHint,
) -> Plan {
    let (s, e) = selection_range(b, anchor, cur, line);
    // Visual-LINEWISE change (`V…c`) behaves exactly like `cc` over the selected line range:
    // keep the first line's indent, replace the rest with ONE empty line, and — crucially — KEEP
    // the trailing separator to the following line (never merge the next line in). Route it through
    // the shared cc-logic; `selection_range` gives whole lines incl. the trailing '\n', so drop
    // that one byte to get the CONTENT end the helper expects. Charwise/blockwise change stays the
    // delete-then-Insert path below.
    if line && op == OpKind::Change && s < e {
        let content_end = if e > s && b[e - 1] == b'\n' { e - 1 } else { e };
        return plan_linewise_change(b, s, content_end, st.view.indent.autoindent, hint);
    }
    // A charwise Visual `d`/`c` cannot take the final newline: Vim has no line after the last one to join,
    // so `v}d` at the end of `a\nb\n` leaves one empty line rather than an empty buffer (#677).
    let e = if !line && op != OpKind::Yank && st.view.caret == CaretGravity::OnChar {
        e.min(motion::content_end(b)).max(s)
    } else {
        e
    };
    let reg = captured(b, s, e, line);
    match op {
        // Yank leaves the buffer unchanged, cursor at the selection start (Vim), back to Normal: on a char,
        // so a start on a line's break (`$vk` reaches past the end, #1176) is its last char.
        OpKind::Yank => Plan {
            action: Action::Nop,
            cursor: {
                let (ls, le) = (line_start(b, s), line_end(b, s));
                if s >= le && le > ls && st.view.caret == CaretGravity::OnChar {
                    prev_grapheme(b, le)
                } else {
                    s
                }
            },
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
            set_register: Some(RegWrite::Yank(reg, (s, e))),
            set_anchor: None,
            set_mark: None,
        },
        OpKind::Delete if s < e => {
            // A linewise delete keeps the wanted column on the line that moves up under the cursor (nvim's
            // `nostartofline`: `lVjd` lands on column 1); `at_col` clamps it to that line's end.
            // Lines through the end of the buffer also take the newline before them (no blank line is left
            // behind) and the cursor moves up onto the new last line; in a newline-terminated buffer only
            // the empty slot after the final newline is left below, which is no line (#677, #1167).
            let (s, cursor) = if !line {
                (s, s)
            } else if e < b.len() {
                (
                    s,
                    want_col(b, e, st.view.curswant, st.view.indent.tab_width) - (e - s),
                )
            } else if s > 0 {
                (
                    if b[e - 1] == b'\n' { s } else { s - 1 },
                    want_col(
                        b,
                        line_start(b, s - 1),
                        st.view.curswant,
                        st.view.indent.tab_width,
                    ),
                )
            } else {
                (s, s)
            };
            edit_yank(
                one(Edit::delete(BytePos(s), e - s)),
                cursor,
                Mode::Normal,
                hint,
                reg,
            )
        }
        OpKind::Change if s < e => edit_yank(
            one(Edit::delete(BytePos(s), e - s)),
            s,
            Mode::Insert,
            hint,
            reg,
        ),
        // Empty selection: just leave Visual (Change still opens Insert).
        OpKind::Change => nop(s, Mode::Insert),
        OpKind::Delete => nop(s, Mode::Normal),
    }
}

/// [`Command::VisualOp`]: a line-oriented Visual operator over the live selection, leaving it (to Normal,
/// or Insert for the changes). Outside a selection it is a clean no-op back to Normal.
fn plan_visual_op(st: &EditorState, b: &[u8], cur: usize, op: VisualOp, hint: GroupHint) -> Plan {
    let (Some(kind), Some(anchor)) = (st.view.mode.selection(), st.view.anchor) else {
        return nop(cur, Mode::Normal);
    };
    let (lo, hi) = (anchor.min(cur), anchor.max(cur));
    let (first, last) = (
        crate::pos::raw::line_of(b, lo),
        crate::pos::raw::line_of(b, hi),
    );
    let lines = (last - first + 1) as u32;
    // The shared line planners keep the mode they were called in; a Visual operator ends the selection.
    let ts = st.view.indent.tab_width;
    let normal = |p: Plan| Plan {
        mode: Mode::Normal,
        ..p
    };
    match op {
        VisualOp::Lines(k) => plan_selection_span(st, b, anchor, cur, k, true, hint),
        VisualOp::BlockToEol(k) if kind == SelectKind::Blockwise => {
            block_op(b, &block_geom(b, anchor, cur, ts, true), k, hint)
        }
        VisualOp::BlockToEol(k) => plan_selection_span(st, b, anchor, cur, k, true, hint),
        VisualOp::Shift { left, count } if kind == SelectKind::Blockwise => {
            normal(plan_block_shift(
                st,
                b,
                &block_geom(b, anchor, cur, ts, false),
                left,
                count,
                hint,
            ))
        }
        VisualOp::Shift { left, count } => normal(plan_shift_times(
            st,
            line_start(b, lo),
            lines,
            !left,
            count.max(1),
            hint,
        )),
        VisualOp::Join { spaces } => plan_join(st, lo, lines.max(2), Mode::Normal, hint, !spaces),
        VisualOp::Reindent => normal(plan_reindent(st, first, last, hint)),
        VisualOp::Comment => normal(plan_comment(st, first, last, lo, hint)),
        VisualOp::Format => normal(plan_format_lines(
            st,
            cur,
            line_start(b, lo),
            line_end(b, hi),
            false,
            false,
            hint,
        )),
        // `zy`: a block yank drops each row's trailing white space; any other shape yanks as `y` does.
        VisualOp::YankTrim if kind == SelectKind::Blockwise => {
            let mut p = block_op(
                b,
                &block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL),
                OpKind::Yank,
                hint,
            );
            if let Some(RegWrite::Yank(reg, span)) = p.set_register.take() {
                let rows: Vec<&[u8]> = reg
                    .text()
                    .split(|&c| c == b'\n')
                    .map(|row| row.trim_ascii_end())
                    .collect();
                let reg = Register::blockwise_width(rows.join(&b'\n'), reg.block_width());
                p.set_register = Some(RegWrite::Yank(reg, span));
            }
            p
        }
        VisualOp::YankTrim => plan_selection_span(
            st,
            b,
            anchor,
            cur,
            OpKind::Yank,
            kind == SelectKind::Linewise,
            hint,
        ),
        // Block `O`: the cursor takes the anchor's column on its own row, the anchor the cursor's column on
        // its row — the other corner of the same rectangle, horizontally. Charwise/linewise `O` is `o`.
        // A corner past a row's last char (a `$` block, #1207) rests past it on the other row too, as
        // nvim's Visual `coladvance` may go one past the end.
        VisualOp::OtherCorner if kind == SelectKind::Blockwise => {
            let (a_col, c_col) = (vcols_at(b, anchor, ts).0, vcols_at(b, cur, ts).0);
            let corner = |p: usize, col: usize| {
                let ls = line_start(b, p);
                if col >= range::line_cells(b, ls, ts) {
                    line_end(b, ls)
                } else {
                    at_vcol(b, ls, col, ts)
                }
            };
            Plan {
                set_anchor: Some(corner(anchor, c_col)),
                ..nop(corner(cur, a_col), st.view.mode)
            }
        }
        VisualOp::OtherCorner => Plan {
            set_anchor: Some(cur),
            ..nop(anchor, st.view.mode)
        },
        // `:`/`!`: Vim's colon operator leaves the cursor at the start of the area — a block's top-left
        // corner; for lines, the first line's column 0, unless the cursor is the selection's start already,
        // where it stays (nvim v0.12.6). `'<`/`'>` keep the selection as it was.
        VisualOp::LeaveAtStart => {
            let at = match kind {
                SelectKind::Charwise => lo,
                SelectKind::Blockwise => {
                    let col = |p: usize| col_of(b, line_start(b, p), p);
                    at_col(b, line_start(b, lo), col(anchor).min(col(cur)))
                }
                SelectKind::Linewise if cur < anchor => cur,
                SelectKind::Linewise => line_start(b, anchor),
            };
            nop(at, Mode::Normal)
        }
    }
}

/// The shared `cc`-style linewise-change body over a whole-line span `[ls, content_end)` where
/// `content_end` is the CONTENT end (the trailing newline EXCLUDED). Vim keeps the first line's leading
/// indent, deletes the rest of the content, KEEPS the trailing newline (so exactly one empty indented line
/// remains — the separator to the following line is never eaten), captures the whole lines linewise
/// (indent + trailing newline included), and enters Insert at the end of the kept indent. Shared by
/// `cc`/`S`, the paragraph objects, the linewise inner block ([`plan_change`]), AND visual-linewise change
/// (`V…c` — [`Command::ChangeSelection`] over a linewise selection), so all four agree with nvim.
fn plan_linewise_change(
    b: &[u8],
    ls: usize,
    content_end: usize,
    keep_indent: bool,
    hint: GroupHint,
) -> Plan {
    // Without `'autoindent'` the indent goes too (nvim `op_change`).
    let indent_end = if keep_indent {
        motion::first_non_blank(b, ls).min(content_end)
    } else {
        ls
    };
    // Register span: whole lines including the terminating newline where one is present.
    let reg_end = if content_end < b.len() && b[content_end] == b'\n' {
        content_end + 1
    } else {
        content_end
    };
    let reg = captured(b, ls, reg_end, true);
    if indent_end >= content_end {
        // Nothing after the indent to delete (empty/blank line): keep the buffer, but still capture the
        // register linewise and drop into Insert at the indent end.
        Plan {
            action: Action::Nop,
            cursor: indent_end,
            mode: Mode::Insert,
            is_edit: false,
            effects: Vec::new(),
            set_register: Some(RegWrite::Edit(reg)),
            set_anchor: None,
            set_mark: None,
        }
    } else {
        edit_yank(
            one(Edit::delete(BytePos(indent_end), content_end - indent_end)),
            indent_end,
            Mode::Insert,
            hint,
            reg,
        )
    }
}

/// `c{motion}` / `cc`: delete the change span, capture it to a register, and enter Insert. A linewise span
/// (`cc`/`S`, `cj`, `c}` reduced by the exclusive-linewise rule, `cip`, a linewise inner block) keeps the
/// first line's indent and the trailing newline ([`plan_linewise_change`]); a failed linewise motion stays
/// in Normal.
fn plan_change(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    m: &Motion,
    hint: GroupHint,
) -> Plan {
    let sp = change_span(b, cur, *m, count);
    let landing = op_landing(
        b,
        cur,
        *m,
        count,
        st.view.curswant,
        st.view.indent.tab_width,
        false,
    );
    apply_op(st, b, cur, MarkOp::Change, sp, landing, hint)
}

/// `[op]/pat` (forward) or `[op]?pat` (backward) — search `count` times, then apply the pending operator
/// (move/delete/change/yank) over the charwise-EXCLUSIVE span between the cursor and the match: `[cur, m)`
/// forward, `[m, cur)` backward. An operator that finds no match on the correct side of the cursor is a
/// no-op (Vim rings the bell). Matches nvim v0.12.4 (`d?pat` geometry verified via the oracle harness).
#[allow(clippy::too_many_arguments)]
fn plan_search(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    backward: bool,
    offset: SearchOffset,
    hint: GroupHint,
) -> Plan {
    // An empty pattern is a search with no previous pattern to repeat (`n` before any search, #1173): it
    // fails before it searches (E35), an operator with it.
    if pattern.is_empty() {
        return beep(cur, st.view.mode);
    }
    match offset {
        // A LINE offset (`/pat/+1`) reshapes the whole motion into a linewise one; the others (none /
        // `s` / `e`) stay charwise. Split so each keeps its own, simpler geometry.
        SearchOffset::Line(n) => {
            plan_search_line(st, b, cur, op, count, pattern, backward, n, hint)
        }
        _ => plan_search_char(st, b, cur, cur, op, count, pattern, backward, offset, hint),
    }
}

/// The operator span of a charwise search motion (`d/pat`, `dn`, `d*`), after Vim's exclusive-linewise rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SearchSpan {
    /// The landing is the cursor itself (`d*` on a word with no other match): an empty exclusive span.
    Empty,
    /// The charwise span `[lo, hi)`.
    Chars { lo: usize, hi: usize },
    /// Whole lines `first..=last` (the exclusive-linewise promotion).
    Lines { first: usize, last: usize },
}

impl SearchSpan {
    /// The 0-based inclusive LINE range the span covers; an empty span covers the cursor's line (`!*` on a
    /// lone word opens `:.!`, verified against nvim 0.12.6).
    fn lines(self, b: &[u8], cur: usize) -> (usize, usize) {
        match self {
            SearchSpan::Empty => {
                let l = crate::pos::raw::line_of(b, cur);
                (l, l)
            }
            SearchSpan::Chars { lo, hi } => (
                crate::pos::raw::line_of(b, lo),
                crate::pos::raw::line_of(b, hi - 1),
            ),
            SearchSpan::Lines { first, last } => (first, last),
        }
    }
}

/// The offset-applied landing of the `count`-th charwise search match from `origin` (see
/// [`plan_search_char`] for the rule), or `None` when the pattern has no match.
#[allow(clippy::too_many_arguments)]
fn search_char_landing(
    st: &EditorState,
    b: &[u8],
    origin: usize,
    count: u32,
    pattern: &str,
    backward: bool,
    offset: SearchOffset,
) -> Option<usize> {
    // Every match a search can land on, empty ones included (`/^$`, `/\<`, `/foo\zs`, #705).
    let spans = search_spans(b, pattern, st.view.search_options());
    if spans.is_empty() {
        return None;
    }
    // The offset-applied cursor landing for a match `[s, e)`.
    let landing = |(s, e): (usize, usize)| -> usize {
        match offset {
            SearchOffset::None | SearchOffset::Start(0) => s,
            SearchOffset::Start(k) => step_char(b, s, k),
            // `e` is relative to the match's LAST char (`e` = end-exclusive `e`, so back up one boundary).
            SearchOffset::End(k) => step_char(b, motion::prev_grapheme(b, e), k),
            SearchOffset::Line(_) => unreachable!("line offsets take plan_search_line"),
        }
    };
    // A landing at the end of a line is compared one character back, where the cursor is put (#737).
    let key = |lp: usize| landing_key(b, lp);
    // The distinct landings in order. `key` never decreases along them, so the next landing after a
    // position is a binary search away.
    let mut lps: Vec<usize> = spans.iter().map(|&sp| landing(sp)).collect();
    lps.sort_unstable();
    lps.dedup();
    let (first, last) = (lps[0], lps[lps.len() - 1]);
    // One `n` (or `N`): the nearest landing strictly past the cursor's key, wrapping around the buffer.
    let step = |pos: usize| -> Option<usize> {
        let k = key(pos);
        Some(if backward {
            match lps.partition_point(|&lp| key(lp) < k) {
                0 => last,
                i => lps[i - 1],
            }
        } else {
            lps.get(lps.partition_point(|&lp| key(lp) <= k))
                .copied()
                .unwrap_or(first)
        })
    };
    walk_count(origin, count, step)
}

/// The operator span between the cursor and a charwise search `target`, plus whether Vim's
/// exclusive-linewise rule ADJUSTED its end (`end_adjusted` in nvim's `do_pending_operator`, which moves the
/// `gq` cursor one line further, #1071). `[cursor, target)` when the landing is after the cursor, else
/// `[target, cursor)`; an `e` offset makes it inclusive through the offset char and exempts it from the rule.
fn search_char_span(
    b: &[u8],
    cur: usize,
    target: usize,
    offset: SearchOffset,
) -> (SearchSpan, bool) {
    // The span runs between the cursor and the landing whichever way the search went: a search that
    // WRAPPED past the buffer's end lands before the cursor (`Gd/foo` deletes back to `foo`, #710).
    let (lo, mut hi) = (cur.min(target), cur.max(target));
    if matches!(offset, SearchOffset::End(_)) {
        hi = motion::next_grapheme(b, hi);
        return if hi <= lo {
            (SearchSpan::Empty, false)
        } else {
            (SearchSpan::Chars { lo, hi }, false)
        };
    }
    if hi <= lo {
        return (SearchSpan::Empty, false);
    }
    // Vim's exclusive-linewise rule (`:help exclusive-linewise`): an exclusive span ending at column 0 of a
    // later line becomes linewise when it starts at/before its first non-blank, else it pulls back one byte
    // (`d/foo` and `dn` landing on a line start).
    let (lo2, hi2, linewise) = exclusive_linewise(b, lo, hi);
    let adjusted = linewise || hi2 != hi;
    if linewise {
        let (first, last) = (
            crate::pos::raw::line_of(b, lo2),
            crate::pos::raw::line_of(b, hi2 - 1),
        );
        return (SearchSpan::Lines { first, last }, adjusted);
    }
    if hi2 <= lo2 {
        return (SearchSpan::Empty, adjusted);
    }
    (SearchSpan::Chars { lo: lo2, hi: hi2 }, adjusted)
}

/// The keyword search `*`/`#` (`whole_word`) or `g*`/`g#` runs from the cursor: the keyword's START (Vim moves
/// there before searching) and its pattern (`\<word\>` for the whole-word form). `None` when the line has
/// no keyword at or after the cursor.
///
/// A line with no keyword but other non-blank text searches that text instead (`...`), as nvim's
/// `find_ident_under_cursor` falls back to the non-blank string: its magic characters are escaped and it
/// gets no `\<`/`\>` (#1087). `None` only when the rest of the line is blank (E348).
pub(crate) fn word_search(b: &[u8], cur: usize, whole_word: bool) -> Option<(usize, String)> {
    let Some((s, e)) = motion::word_under_cursor(b, cur) else {
        let (s, e) = motion::cword_under_cursor(b, cur, false)?;
        let text = std::str::from_utf8(&b[s..e]).ok()?;
        // nvim's `nv_ident` escapes `/.*~[^$\` (`?` too for `#`, which its `?` search then unescapes).
        let mut pattern = String::with_capacity(2 * text.len());
        for c in text.chars() {
            if "/.*~[^$\\".contains(c) {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        return Some((s, pattern));
    };
    let word = std::str::from_utf8(&b[s..e]).ok()?;
    // Keyword chars (alnum/`_`/non-ASCII) are regex-safe; `\<…\>` = whole word.
    let pattern = if whole_word {
        format!("\\<{word}\\>")
    } else {
        word.to_string()
    };
    Some((s, pattern))
}

/// The target line of a line-offset search (`/pat/+N`): the `count`-th match's line moved by `n`, clamped
/// to the buffer. `None` when a step finds no match.
#[allow(clippy::too_many_arguments)]
fn search_line_target(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    pattern: &str,
    backward: bool,
    n: i32,
) -> Option<usize> {
    // The match starts `search_fwd`/`search_bwd` step through, found once for every step (#682).
    let mut starts: Vec<usize> = search_spans(b, pattern, st.view.search_options())
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    starts.sort_unstable();
    starts.dedup();
    let (&first, &last) = (starts.first()?, starts.last()?);
    let key = |s: usize| landing_key(b, s);
    let step = |pos: usize| -> Option<usize> {
        Some(if backward {
            match starts.partition_point(|&s| key(s) < pos) {
                0 => last,
                i => starts[i - 1],
            }
        } else {
            starts
                .get(starts.partition_point(|&s| key(s) <= pos))
                .copied()
                .unwrap_or(first)
        })
    };
    let pos = walk_count(cur, count, step)?;
    let last_line = crate::pos::raw::line_of(b, b.len());
    let match_line = crate::pos::raw::line_of(b, pos);
    Some((match_line as i64 + i64::from(n)).clamp(0, last_line as i64) as usize)
}

/// The 0-based inclusive LINE range a span command's operator would cover — a search (`/`, `?`, `n`, `N`,
/// `*`, `#`, `g*`, `g#`), a `gn` match or a mark — or `None` when it has no target (no match, unset mark).
/// The command's operator is ignored. This is how the `!` filter over those motions numbers its
/// `:{range}!` line (#800): the frontend gets the command from the engine and asks the planner, so the
/// filter and every other operator share one span geometry.
pub(crate) fn span_lines(st: &EditorState, cmd: &Command) -> Option<(usize, usize)> {
    let b = st.bytes();
    let cur = st.view.cursor;
    match cmd {
        // No previous pattern (#1173): no target.
        Command::Search { pattern, .. } if pattern.is_empty() => None,
        Command::Search {
            count,
            pattern,
            backward,
            offset: SearchOffset::Line(n),
            ..
        } => {
            let target = search_line_target(st, b, cur, *count, pattern, *backward, *n)?;
            let cur_line = crate::pos::raw::line_of(b, cur);
            Some((cur_line.min(target), cur_line.max(target)))
        }
        Command::Search {
            count,
            pattern,
            backward,
            offset,
            ..
        } => {
            let target = search_char_landing(st, b, cur, *count, pattern, *backward, *offset)?;
            Some(search_char_span(b, cur, target, *offset).0.lines(b, cur))
        }
        Command::SearchWordUnder {
            forward,
            whole_word,
            count,
            ..
        } => {
            let (origin, pattern) = word_search(b, cur, *whole_word)?;
            let target = search_char_landing(
                st,
                b,
                origin,
                *count,
                &pattern,
                !forward,
                SearchOffset::None,
            )?;
            Some(
                search_char_span(b, cur, target, SearchOffset::None)
                    .0
                    .lines(b, cur),
            )
        }
        Command::SearchObject {
            count,
            pattern,
            backward,
            ..
        } => {
            let (s, e) = search_object_match(st, b, cur, *count, pattern, *backward)?;
            Some((
                crate::pos::raw::line_of(b, s),
                crate::pos::raw::line_of(b, prev_boundary(b, e)),
            ))
        }
        Command::OpToMark { name, linewise, .. } => {
            let mark = st.view.named_mark(*name)?.0.min(b.len());
            let (lo, hi) = (cur.min(mark), cur.max(mark));
            let lines = if *linewise {
                (
                    crate::pos::raw::line_of(b, lo),
                    crate::pos::raw::line_of(b, hi),
                )
            } else if lo >= hi {
                SearchSpan::Empty.lines(b, cur)
            } else {
                SearchSpan::Chars { lo, hi }.lines(b, cur)
            };
            Some(lines)
        }
        _ => None,
    }
}

/// A charwise search motion with no offset or an `s`/`e`/`b` character offset (`:help search-offset`).
///
/// The landing is the offset-applied position of the count-th match found in the search direction. The
/// match is picked by comparing OFFSET-APPLIED positions against the cursor (`> cursor` forward,
/// `< cursor` backward, wrapping): this single rule reproduces every nvim case — fresh searches, `n`/`N`,
/// and retyping — because after any offset search the cursor already sits at the previous landing, so the
/// same comparison advances past it (even when the offset, e.g. `s-1`, lands the cursor *before* the
/// match). Under an operator the span is `[cursor, target)` EXCLUSIVE, except an `e` (End) offset makes it
/// INCLUSIVE through the offset char (Vim: only `e` turns the motion inclusive). The search runs from
/// `origin` — the cursor, except `*`/`#`, which start at the keyword's start — while the operator span
/// always starts at the cursor.
#[allow(clippy::too_many_arguments)]
fn plan_search_char(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    origin: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    backward: bool,
    offset: SearchOffset,
    hint: GroupHint,
) -> Plan {
    let Some(target) = search_char_landing(st, b, origin, count, pattern, backward, offset) else {
        return beep(cur, st.view.mode);
    };
    // A `$` match is past the line's last character; outside Visual mode the cursor is put back on that
    // character, also as an operator's end (nvim: `d/$` on "ab cd" from `b` deletes "b c", #737). Visual
    // mode keeps the real landing, so the selection takes in the line break.
    let target = if st.view.mode == Mode::Normal {
        landing_key(b, target)
    } else {
        target
    };
    if matches!(op, SearchOp::Move) {
        return nop(target, st.view.mode);
    }
    let (lo, hi, adjusted) = match search_char_span(b, cur, target, offset) {
        (SearchSpan::Empty, _) => return nop(cur, st.view.mode),
        (SearchSpan::Lines { first, last }, adjusted) => {
            return plan_search_lines(st, b, cur, op, first, last, adjusted, target, hint);
        }
        (SearchSpan::Chars { lo, hi }, adjusted) => {
            if let Some(span_op) = op.span_op() {
                return plan_op_over_span(
                    st, b, cur, span_op, lo, hi, false, adjusted, target, hint,
                );
            }
            (lo, hi, adjusted)
        }
    };
    let sp = OpSpan {
        s: lo,
        e: hi,
        linewise: false,
        adjusted,
        failed: false,
    };
    match op {
        SearchOp::Delete => apply_op(
            st,
            b,
            cur,
            MarkOp::Delete,
            delete_quirk(b, sp, false),
            target,
            hint,
        ),
        SearchOp::Change => apply_op(st, b, cur, MarkOp::Change, sp, target, hint),
        SearchOp::Yank => apply_op(st, b, cur, MarkOp::Yank, sp, target, hint),
        SearchOp::Move
        | SearchOp::Case(_)
        | SearchOp::Shift { .. }
        | SearchOp::Reindent
        | SearchOp::Format { .. }
        | SearchOp::Comment => unreachable!("handled above"),
    }
}

/// A search motion with a LINE offset (`/pat/+N`, `/pat/-N`, `/pat/N`). The match is found by the plain
/// rule (count-th match with start beyond the cursor in the search direction, wrapping) — the line offset
/// only reshapes the RESULT. As a bare motion (`Move`) the cursor lands on COLUMN 0 of the target line
/// (Vim "column 1" — NOT the first non-blank; verified against nvim v0.12.4). Under an operator the motion
/// is LINEWISE over the lines between the cursor's line and the target line.
#[allow(clippy::too_many_arguments)]
fn plan_search_line(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    backward: bool,
    n: i32,
    hint: GroupHint,
) -> Plan {
    let Some(target_line) = search_line_target(st, b, cur, count, pattern, backward, n) else {
        return beep(cur, st.view.mode);
    };
    if matches!(op, SearchOp::Move) {
        // Column 0 of the target line (snapped/clamped in `commit`).
        return nop(
            crate::pos::raw::nth_line_start(b, target_line),
            st.view.mode,
        );
    }
    // Linewise over the cursor's line .. target line (inclusive), regardless of search direction.
    let cur_line = crate::pos::raw::line_of(b, cur);
    let (first, last) = (cur_line.min(target_line), cur_line.max(target_line));
    let landing = crate::pos::raw::nth_line_start(b, target_line);
    plan_search_lines(st, b, cur, op, first, last, false, landing, hint)
}

/// A search operator applied LINEWISE over lines `first..=last` — a line-offset search (`d/pat/+1`) and an
/// exclusive search span promoted by Vim's exclusive-linewise rule (`dn` landing at column 0, #799).
/// `end_adjusted` is that promotion (see [`search_char_span`]); `landing` is where the search moved the
/// cursor.
#[allow(clippy::too_many_arguments)]
fn plan_search_lines(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    first: usize,
    last: usize,
    end_adjusted: bool,
    landing: usize,
    hint: GroupHint,
) -> Plan {
    let first_ls = crate::pos::raw::nth_line_start(b, first);
    let last_ls = crate::pos::raw::nth_line_start(b, last);
    if let Some(span_op) = op.span_op() {
        return plan_op_over_span(
            st,
            b,
            cur,
            span_op,
            first_ls,
            last_ls,
            true,
            end_adjusted,
            landing,
            hint,
        );
    }
    let op = match op {
        SearchOp::Delete => MarkOp::Delete,
        SearchOp::Change => MarkOp::Change,
        SearchOp::Yank => MarkOp::Yank,
        SearchOp::Move
        | SearchOp::Case(_)
        | SearchOp::Shift { .. }
        | SearchOp::Reindent
        | SearchOp::Format { .. }
        | SearchOp::Comment => unreachable!("handled above"),
    };
    plan_op_over_span(
        st,
        b,
        cur,
        op,
        first_ls,
        last_ls,
        true,
        end_adjusted,
        landing,
        hint,
    )
}

/// One delete / change / yank / recase over a resolved operator span: the tail every operator path shares (a
/// motion, a forced motion, a mark, a search), so their register, mode and cursor rules agree with nvim
/// (#693, #712, #1077). `landing` is where the motion left the cursor (see [`op_landing`]).
/// - Delete: a linewise delete rests on the line that moves up, at the wanted column (`'nostartofline'`,
///   nvim's `old_col`), or on its first non-blank when the exclusive-linewise rule made the span linewise
///   (`sp.adjusted`). A linewise delete reaching an unterminated last line also takes the newline before it.
/// - Change: a linewise span is `cc` over those lines ([`plan_linewise_change`]); a FAILED linewise motion
///   (`cj` on the last line) stays in Normal.
/// - Yank / recase: a linewise span rests on the earlier of the cursor and the landing, keeping its column
///   (`wyy`, `jwyk`, `wgUj`); a charwise one on the span start.
fn apply_op(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: MarkOp,
    sp: OpSpan,
    landing: usize,
    hint: GroupHint,
) -> Plan {
    let OpSpan {
        s,
        e,
        linewise,
        adjusted,
        failed,
    } = sp;
    let mode = st.view.mode;
    let rest = if linewise { cur.min(landing) } else { s };
    // An empty buffer is nvim's "--No lines in buffer--" (a new or empty file, or every line deleted):
    // `yy` there yanks one empty line (nvim v0.12.6 sets `""` and `"0` to a linewise "\n"), while `dd` has
    // no line to delete and fails, leaving the registers alone (#906).
    if linewise && s >= e && b.is_empty() && op == MarkOp::Yank {
        let mut plan = nop(cur, mode);
        plan.set_register = Some(RegWrite::Yank(Register::linewise(Vec::new()), (s, e)));
        return plan;
    }
    match op {
        MarkOp::Delete | MarkOp::Yank | MarkOp::Case(_) if s >= e => nop(cur, mode),
        MarkOp::Delete if !linewise => {
            let reg = captured(b, s, e, false);
            edit_yank(one(Edit::delete(BytePos(s), e - s)), s, mode, hint, reg)
        }
        MarkOp::Delete => {
            let reg = captured(b, s, e, true);
            // The line that ends up under the cursor, as `(its start before the delete, the bytes removed
            // before it)`: the line below the span, or — when the span runs to an unterminated last line,
            // whose preceding newline goes too — the line above it. A span that runs to the end of a
            // newline-terminated buffer leaves only the empty slot after the final newline below it, which
            // is no line (#677): the cursor takes the line above too, at the wanted column (#1167).
            let (del_start, line, shift) = if e == b.len() && s > 0 && b[e - 1] != b'\n' {
                (s - 1, line_start(b, s - 1), 0)
            } else if e == b.len() && s > 0 {
                (s, line_start(b, s - 1), 0)
            } else {
                (s, e, e - s)
            };
            let at = if adjusted {
                motion::first_non_blank(b, line)
            } else {
                want_col(b, line, st.view.curswant, st.view.indent.tab_width)
            };
            edit_yank(
                one(Edit::delete(BytePos(del_start), e - del_start)),
                at - shift,
                mode,
                hint,
                reg,
            )
        }
        MarkOp::Change if linewise && failed => nop(cur, Mode::Normal),
        MarkOp::Change if linewise => {
            let content_end = if e > s && b[e - 1] == b'\n' { e - 1 } else { e };
            plan_linewise_change(b, s, content_end, st.view.indent.autoindent, hint)
        }
        MarkOp::Change if s >= e => nop(s, Mode::Insert),
        MarkOp::Change => {
            // The register captures the removed content charwise (a partial-line change like `c$` pastes
            // inline).
            let reg = captured(b, s, e, false);
            edit_yank(
                one(Edit::delete(BytePos(s), e - s)),
                s,
                Mode::Insert,
                hint,
                reg,
            )
        }
        MarkOp::Yank => Plan {
            action: Action::Nop,
            cursor: rest,
            mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: Some(RegWrite::Yank(captured(b, s, e, linewise), (s, e))),
            set_anchor: None,
            set_mark: None,
        },
        // Recase the span; an all-non-letter span is just a cursor move (Vim). Invalid UTF-8 bytes are kept.
        MarkOp::Case(case) => {
            let recased = recase_bytes(&b[s..e], case);
            // The resting point after the edit: its offset inside the span shifts with the recased prefix.
            let at = if rest > s && rest <= e {
                s + recase_bytes(&b[s..rest], case).len()
            } else {
                rest
            };
            if recased == b[s..e] {
                nop(rest, mode)
            } else {
                edit(
                    one(Edit::replace(BytePos(s), e - s, recased)),
                    at,
                    mode,
                    hint,
                )
            }
        }
        MarkOp::Shift { .. } | MarkOp::Reindent | MarkOp::Format { .. } | MarkOp::Comment => {
            unreachable!("the line operators take plan_op_over_span")
        }
    }
}

/// Recase `bytes`, keeping any invalid UTF-8 byte as it is: a file with stray bytes recases its letters
/// instead of panicking (#706).
fn recase_bytes(bytes: &[u8], case: WordCase) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.extend_from_slice(recase(chunk.valid(), case).as_bytes());
        out.extend_from_slice(chunk.invalid());
    }
    out
}

/// The `MarkOp` form of a delete / change / yank.
fn mark_op(op: OpKind) -> MarkOp {
    match op {
        OpKind::Delete => MarkOp::Delete,
        OpKind::Change => MarkOp::Change,
        OpKind::Yank => MarkOp::Yank,
    }
}

/// Vi's `d` quirk (nvim `op_delete`) on a plain charwise delete: over several lines, starting in the indent
/// and leaving only blanks after its end, it deletes whole lines (`2d$`, `daw` on an empty line). Objects
/// take [`crate::textobj::delete_reshape`], every other motion
/// [`crate::editor::range::delete_promotes_linewise`]. Forced motions and Visual deletes never take it.
fn delete_quirk(b: &[u8], sp: OpSpan, object: bool) -> OpSpan {
    if sp.linewise {
        return sp;
    }
    let (s, e, linewise) = if object {
        crate::textobj::delete_reshape(b, sp.s, sp.e)
    } else {
        match crate::editor::range::delete_promotes_linewise(b, sp.s, sp.e) {
            Some((ls, le)) => (ls, le, true),
            None => (sp.s, sp.e, false),
        }
    };
    OpSpan {
        s,
        e,
        linewise,
        ..sp
    }
}

/// Apply a span operator over `[lo, hi)` — the cursor..mark span of [`Command::OpToMark`], and the case /
/// shift / reindent / format forms of a search ([`SearchOp::span_op`]) and of `gn` (#800). `linewise` widens
/// the span to the whole lines of `lo` and `hi` (the `'` mark form, a line-offset search); otherwise it is
/// charwise-EXCLUSIVE, so for the line operators an end resting at column 0 of the far line does NOT pull that
/// line in (Vim's exclusive-motion rule — the same reduction `ShiftMotion`/`Reindent` apply to a motion).
/// `end_adjusted` says the exclusive-linewise rule moved the span's end (nvim's `oap->end_adjusted`): `gq`
/// then leaves the cursor on the line BELOW the formatted block (#1071). `landing` is where the motion left
/// the cursor ([`apply_op`]).
#[allow(clippy::too_many_arguments)]
fn plan_op_over_span(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: MarkOp,
    lo: usize,
    hi: usize,
    linewise: bool,
    end_adjusted: bool,
    landing: usize,
    hint: GroupHint,
) -> Plan {
    // Shift (`>`), reindent (`=`) and format (`gq`/`gw`) operate on whole LINES regardless of the charwise /
    // linewise form (Vim: they are linewise even over a charwise motion), so dispatch them on the touched line
    // range before the span split the others share. They reuse the SAME planners as over a motion.
    if matches!(
        op,
        MarkOp::Shift { .. } | MarkOp::Reindent | MarkOp::Format { .. } | MarkOp::Comment
    ) {
        let (first, last) = if linewise {
            (
                crate::pos::raw::line_of(b, lo),
                crate::pos::raw::line_of(b, hi),
            )
        } else if lo >= hi {
            return nop(cur, st.view.mode);
        } else {
            (
                crate::pos::raw::line_of(b, lo),
                crate::pos::raw::line_of(b, hi - 1),
            )
        };
        return match op {
            MarkOp::Shift { left } => {
                let lines = (last - first + 1) as u32;
                plan_shift(st, line_start(b, lo), lines, !left, hint)
            }
            MarkOp::Reindent => plan_reindent(st, first, last, hint),
            MarkOp::Format { keep } => {
                let start = crate::pos::raw::nth_line_start(b, first);
                let end = line_end(b, crate::pos::raw::nth_line_start(b, last));
                plan_format_lines(st, cur, start, end, keep, end_adjusted, hint)
            }
            MarkOp::Comment => plan_comment(st, first, last, cur.min(lo), hint),
            _ => unreachable!("guarded by matches! above"),
        };
    }
    let sp = if linewise {
        let le = line_end(b, hi);
        OpSpan {
            s: line_start(b, lo),
            e: if le < b.len() { le + 1 } else { le },
            linewise: true,
            adjusted: end_adjusted,
            failed: false,
        }
    } else {
        OpSpan {
            s: lo,
            e: hi,
            linewise: false,
            adjusted: end_adjusted,
            failed: false,
        }
    };
    apply_op(st, b, cur, op, sp, landing, hint)
}

/// The match `[start, end)` `gn`/`gN` selects (see [`plan_search_object`]), or `None` with no match.
fn search_object_match(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    pattern: &str,
    backward: bool,
) -> Option<(usize, usize)> {
    let spans = match_spans(b, pattern, st.view.search_options());
    if spans.is_empty() {
        return None;
    }
    let len = spans.len();
    // The match to start from: forward, the first whose end is past the cursor (the one containing it, or
    // the next), wrapping to the first; backward, the one containing the cursor, else the last that starts
    // before it, wrapping to the last.
    let start_idx = if backward {
        spans
            .iter()
            .rposition(|&(s, e)| s <= cur && cur < e)
            .or_else(|| spans.iter().rposition(|&(s, _)| s < cur))
            .unwrap_or(len - 1)
    } else {
        spans.iter().position(|&(_, e)| e > cur).unwrap_or(0)
    };
    // Advance `count-1` further matches in the direction of travel, wrapping around the document.
    let steps = i64::from(count.max(1)) - 1;
    let delta = if backward { -steps } else { steps };
    let idx = (start_idx as i64 + delta).rem_euclid(len as i64) as usize;
    Some(spans[idx])
}

/// `gn` / `gN` (the search-match text object, Vim `:help gn`). Selects/operates on the WHOLE match under the
/// cursor, or — when the cursor is not on a match — the next (`gn`) or previous (`gN`) one; `count` advances
/// that many matches (wrapping). The bare form ([`SearchOp::Move`]) enters charwise Visual with the match
/// selected; `Delete`/`Change`/`Yank` operate on the match span `[start, end)` directly.
#[allow(clippy::too_many_arguments)]
fn plan_search_object(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    op: &SearchOp,
    count: u32,
    pattern: &str,
    backward: bool,
    hint: GroupHint,
) -> Plan {
    let found = (!pattern.is_empty())
        .then(|| search_object_match(st, b, cur, count, pattern, backward))
        .flatten();
    let Some((s, e)) = found else {
        // No match, or no previous pattern (empty, #1184): nvim's `current_search` fails (`clearopbeep`),
        // so `dgn` is no change for `.` (#1179).
        return beep(cur, st.view.mode);
    };
    match op {
        // Bare `gn`/`gN`: enter charwise Visual with the match selected (anchor at its start, cursor on its
        // last char). `commit` applies `set_anchor` after its enter-selection default, so this span wins.
        SearchOp::Move => Plan {
            action: Action::Nop,
            cursor: motion::prev_grapheme(b, e),
            mode: Mode::Visual {
                kind: SelectKind::Charwise,
            },
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: Some(s),
            set_mark: None,
        },
        SearchOp::Delete => {
            let reg = captured(b, s, e, false);
            edit_yank(
                one(Edit::delete(BytePos(s), e - s)),
                s,
                Mode::Normal,
                hint,
                reg,
            )
        }
        SearchOp::Change => {
            let reg = captured(b, s, e, false);
            edit_yank(
                one(Edit::delete(BytePos(s), e - s)),
                s,
                Mode::Insert,
                hint,
                reg,
            )
        }
        SearchOp::Yank => {
            let reg = captured(b, s, e, false);
            Plan {
                action: Action::Nop,
                cursor: s,
                mode: st.view.mode,
                is_edit: false,
                effects: Vec::new(),
                set_register: Some(RegWrite::Yank(reg, (s, e))),
                set_anchor: None,
                set_mark: None,
            }
        }
        // `gUgn`, `>gn`, `gqgn`, ...: the shared span-operator planner over the match.
        SearchOp::Case(_)
        | SearchOp::Shift { .. }
        | SearchOp::Reindent
        | SearchOp::Format { .. }
        | SearchOp::Comment => {
            let span_op = op.span_op().expect("a span op");
            plan_op_over_span(st, b, cur, span_op, s, e, false, false, s, hint)
        }
    }
}

/// `gR`-mode typing (Virtual Replace): tab-aware overwrite — over a multi-column tab, insert before it
/// (it shrinks) until its last column, then replace it; at end-of-line, append; else overwrite one char,
/// remembering the original for `<BS>`.
fn plan_virtual_replace_type(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    c: char,
    hint: GroupHint,
) -> Plan {
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes().to_vec();
    let tn = typed.len();
    let le = line_end(b, cur);
    let (edits, push, cursor) = if cur >= le {
        // End-of-line: nothing to overwrite — append (backspace deletes it).
        (one(Edit::insert(BytePos(cur), typed)), Some(None), cur + tn)
    } else if b[cur] == b'\t' {
        // Over a TAB: it spans `w` virtual columns to the next tabstop. While more than one column
        // remains, INSERT before the tab (it shrinks, backspace regrows it); on the LAST column, replace
        // the tab with the typed char (backspace restores the tab).
        let ls = line_start(b, cur);
        let vcol = motion::vcol_of(b, ls, cur, st.view.indent.tab_width);
        let w = st.view.indent.tab_width.max(1) - (vcol % st.view.indent.tab_width.max(1));
        if w > 1 {
            (one(Edit::insert(BytePos(cur), typed)), Some(None), cur + tn)
        } else {
            (
                one(Edit::replace(BytePos(cur), 1, typed)),
                Some(Some(vec![b'\t'])),
                cur + tn,
            )
        }
    } else {
        // Over a normal char: overwrite it (remember the original for `<BS>`), like Replace.
        let nb = next_boundary(b, cur);
        (
            one(Edit::replace(BytePos(cur), nb - cur, typed)),
            Some(Some(b[cur..nb].to_vec())),
            cur + tn,
        )
    };
    Plan {
        action: Action::ReplaceTxn {
            edits,
            hint,
            push,
            pop: false,
        },
        cursor,
        mode: Mode::VirtualReplace,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// `{count}gr{char}` — CLASSIC-Vim one-shot virtual replace: overwrite `count` chars with `char` using the
/// tab-aware [`plan_virtual_replace_type`] policy, then return to Normal (the one-shot form of `gR`, as `r`
/// is to `R`). Because a single typed char is never `\n`, every step stays on the cursor's own line, so the
/// whole change is one replace of `[cur, line_end)` computed by SIMULATING the `gR`-typing loop on an owned
/// copy of the line. Over a `<Tab>` this shrinks/inserts to preserve the following text's column; when
/// `count` overruns the line it APPENDS the surplus past end-of-line (Vim), unlike `r`. The cursor lands on
/// the last replaced char (Vim leaves Replace with a one-boundary left nudge).
fn plan_virtual_replace_char(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    count: u32,
    c: char,
    hint: GroupHint,
) -> Plan {
    let tabw = st.view.indent.tab_width.max(1);
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    // Work on an owned copy of the whole line so tab virtual-column math sees the evolving text; positions
    // are relative to `ls`. Only bytes from the cursor onward are ever touched.
    let mut line: Vec<u8> = b[ls..le].to_vec();
    let start = cur - ls; // cursor offset within `line`
    let mut p = start;
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes();
    let tn = typed.len();
    for done in 0..count {
        if p >= line.len() {
            // Past end-of-line: append the whole surplus at once (virtual Replace grows the line here),
            // bounded like every put ([`MAX_PUT_BYTES`]) so a hostile count cannot exhaust memory (#683).
            let rest = ((count - done) as usize).min((MAX_PUT_BYTES / tn).max(1));
            line.reserve(rest * tn);
            for _ in 0..rest {
                line.extend_from_slice(typed);
            }
            p += rest * tn;
            break;
        } else if line[p] == b'\t' {
            // Over a TAB spanning `w` virtual columns: insert before it (shrinking it) while more than one
            // column remains; on its last column replace the tab itself — preserving the next char's column.
            let vcol = motion::vcol_of(&line, 0, p, tabw);
            let w = tabw - (vcol % tabw);
            if w > 1 {
                line.splice(p..p, typed.iter().copied());
            } else {
                line.splice(p..p + 1, typed.iter().copied());
            }
        } else {
            // Over a normal char: overwrite exactly one grapheme boundary's worth.
            let nb = next_boundary(&line, p);
            line.splice(p..nb, typed.iter().copied());
        }
        p += tn;
    }
    // One replace of the line tail from the cursor; everything before `start` is unchanged.
    let new_tail = line[start..].to_vec();
    // Vim leaves Replace nudging left one boundary → the cursor rests on the LAST typed char at `ls + p - tn`
    // (p advanced by at least one `tn` per iteration, so this never precedes the line start).
    let cursor = ls + p - tn;
    edit(
        one(Edit::replace(BytePos(cur), le - cur, new_tail)),
        cursor,
        Mode::Normal,
        hint,
    )
}

/// A Vim-profile motion that fails ([`motion::fails`]), bare in Normal or Visual or under an operator: the
/// command does nothing and beeps, so the frontend flushes the keys that were not typed (#1085). The
/// operator is cancelled whole (`dfz`, `cj` on the last line stays in Normal).
fn failed_motion(st: &EditorState, b: &[u8], cur: usize, cmd: &Command) -> Option<Plan> {
    if st.view.caret != CaretGravity::OnChar
        || !matches!(st.view.mode, Mode::Normal | Mode::Visual { .. })
    {
        return None;
    }
    let (count, m, op) = match *cmd {
        Command::Move(count, m) => (count, m, false),
        Command::Delete(count, m) | Command::Change(count, m) | Command::Yank(count, m) => {
            (count, m, true)
        }
        Command::OpForced { count, motion, .. }
        | Command::CaseMotion { count, motion, .. }
        | Command::ShiftMotion { count, motion, .. }
        | Command::Reindent { count, motion }
        | Command::Format { count, motion, .. }
        | Command::Comment { count, motion } => (count, motion, true),
        _ => return None,
    };
    let visual = st.view.mode.selection().is_some();
    if op && visual {
        return None;
    }
    // A failed word object leaves the cursor where Vim's scan stopped (`daw` on blanks: their end). In
    // Visual the anchor stays, so the selection reaches there (`vaw` on a blank line, #1111).
    if op || visual {
        let anchor = visual.then(|| st.view.anchor.unwrap_or(cur));
        if let Some(at) = crate::textobj::word_object_fails(b, cur, anchor, m, count) {
            let at = motion::off_phantom_at(b, at);
            let (ls, le) = (line_start(b, at), line_end(b, at));
            let at = if at >= le && le > ls {
                prev_grapheme(b, le)
            } else {
                at
            };
            return Some(beep(at, st.view.mode));
        }
    }
    // A block, quote or sentence object with nothing to select fails under an operator (nvim's `nv_object`
    // → `clearopbeep`), so `di(` without a `(` is no change for `.` and stops a macro (#1179).
    if op && crate::textobj::object_fails(b, cur, m, count) {
        return Some(beep(cur, st.view.mode));
    }
    // A bare `{count}e` that runs out still moves as far as it can (handled in the `Move` arm).
    if !op && matches!(m, Motion::WordEnd | Motion::BigWordEnd) {
        return None;
    }
    motion::fails(b, cur, m, count, op, visual).then(|| beep(cur, st.view.mode))
}

/// A failed command: no change, the cursor at `cursor`, and a beep ([`Effect::Beep`], #1085).
fn beep(cursor: usize, mode: Mode) -> Plan {
    Plan {
        effects: vec![Effect::Beep],
        ..nop(cursor, mode)
    }
}

fn nop(cursor: usize, mode: Mode) -> Plan {
    Plan {
        action: Action::Nop,
        cursor,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// A non-editing move (like [`nop`]) that also SETS the Emacs mark at `mark`. Shared by the mark commands
/// (`SetMark` / `ExchangePointMark` / `EmacsMarkWord` / `EmacsBufferEdge`).
fn nop_mark(cursor: usize, mode: Mode, mark: usize) -> Plan {
    Plan {
        set_mark: Some(MarkWrite::Set(mark)),
        ..nop(cursor, mode)
    }
}

/// A non-editing plan carrying an arbitrary `action` — the jumplist / changelist / named-mark steps whose
/// resolution happens in `commit` (the planner supplies a placeholder cursor the action then overrides).
/// Same field shape as [`nop`] but the action is not forced to `Action::Nop`.
fn moved(action: Action, cursor: usize, mode: Mode) -> Plan {
    Plan {
        action,
        ..nop(cursor, mode)
    }
}

fn edit(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// An `edit` that also captures the removed span into a register (`d`/`c`/`x` fill the unnamed slot).
fn edit_yank(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: Some(RegWrite::Edit(reg)),
        set_anchor: None,
        set_mark: None,
    }
}

fn one(e: Edit) -> EditList {
    EditList::new(vec![e]).expect("single edit is always valid")
}

/// Add `delta` to the first number at or after `from` on the line `ls..le`, matching Neovim `nrformats`
/// (decimal, and `0x`/`0b`/`0o` based literals). Returns `(start, old_len, new_bytes, cursor)` — an
/// absolute-offset replacement plus the offset of its last digit (where Vim leaves the caret) — or `None`
/// when the rest of the line holds no number. This is the shared engine for `CTRL-A`/`CTRL-X` (one call at
/// the cursor) and Visual increment (one call per selected line).
fn incr_number(
    b: &[u8],
    ls: usize,
    le: usize,
    from: usize,
    delta: i64,
) -> Option<(usize, usize, Vec<u8>, usize)> {
    // First decimal digit at or after `from` (Vim searches forward). A based literal always has one (its
    // `0`), so this anchors every base.
    let mut d = from.max(ls);
    while d < le && !b[d].is_ascii_digit() {
        d += 1;
    }
    if d >= le {
        return None; // no number on the rest of the line
    }
    // Detect a based literal `0x`/`0X` (hex), `0b`/`0B` (binary), or `0o`/`0O` (octal). Two ways `d` (the
    // first decimal digit) can land on one: the `0` prefix starts AT `d`, or `d` sits inside the digit run
    // (walk left over base-max hex digits — the widest alphabet — to a base letter, and check the `0`).
    let radix_of = |c: u8| match c {
        b'x' | b'X' => Some(16u32),
        b'o' | b'O' => Some(8),
        b'b' | b'B' => Some(2),
        _ => None,
    };
    let is_digit_of = |c: u8, radix: u32| (c as char).is_digit(radix);
    let based_at_d = b[d] == b'0'
        && b.get(d + 1)
            .copied()
            .and_then(radix_of)
            .is_some_and(|r| b.get(d + 2).copied().is_some_and(|c| is_digit_of(c, r)));
    let mut hleft = d;
    while hleft > ls && b[hleft - 1].is_ascii_hexdigit() {
        hleft -= 1;
    }
    let based_inside = hleft >= ls + 2 && b[hleft - 2] == b'0' && radix_of(b[hleft - 1]).is_some();
    if based_at_d || based_inside {
        let prefix = if based_at_d { d } else { hleft - 2 };
        let radix = radix_of(b[prefix + 1]).unwrap_or(16);
        let letter = b[prefix + 1]; // keep the original `x`/`X`/`b`/`B`/`o`/`O` case (Vim does)
        let mut end = prefix + 2;
        while end < le && is_digit_of(b[end], radix) {
            end += 1;
        }
        let val: i128 = std::str::from_utf8(&b[prefix + 2..end])
            .ok()
            .and_then(|s| i128::from_str_radix(s, radix).ok())
            .unwrap_or(0);
        // Based literals stay non-negative (Vim wraps at 0 for the default unsigned view); the numeric body
        // is rendered in its own base (lowercase digits), keeping the original `0{letter}` prefix.
        let n = (val + i128::from(delta)).max(0);
        let body = match radix {
            2 => format!("{n:b}"),
            8 => format!("{n:o}"),
            _ => format!("{n:x}"),
        };
        let new_text = format!("0{}{body}", letter as char);
        let bytes = new_text.into_bytes();
        let cursor = prefix + bytes.len().saturating_sub(1);
        return Some((prefix, end - prefix, bytes, cursor));
    }
    // Decimal: the maximal digit run around `d`, plus a leading `-` sign if directly before it.
    let mut start = d;
    while start > ls && b[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let mut end = d;
    while end < le && b[end].is_ascii_digit() {
        end += 1;
    }
    let num_start = if start > ls && b[start - 1] == b'-' {
        start - 1
    } else {
        start
    };
    // Parse (i128 to absorb any i64 span), add the delta, and re-render. An unparseable/overflowing run is
    // left untouched (returned as a no-op replacement of itself).
    let val: i128 = std::str::from_utf8(&b[num_start..end])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let result = val + i128::from(delta);
    // Vim preserves a leading-zero field WIDTH: `007`→`008`, `099`→`100` (grows only on carry), `-007`→
    // `-006`. Only when the original magnitude (digits, excluding sign) had a leading zero — a plain `42`
    // is never padded. The width is measured on the magnitude and re-applied after sign handling.
    let orig_mag_width = end - start;
    let had_leading_zero = orig_mag_width > 1 && b[start] == b'0';
    let mag = result.unsigned_abs().to_string();
    let mag = if had_leading_zero && mag.len() < orig_mag_width {
        format!("{mag:0>orig_mag_width$}")
    } else {
        mag
    };
    let new_text = if result < 0 { format!("-{mag}") } else { mag };
    let bytes = new_text.into_bytes();
    let cursor = num_start + bytes.len().saturating_sub(1); // land on the last digit (Vim)
    Some((num_start, end - num_start, bytes, cursor))
}

/// Resolve the current Visual/Select selection into a NON-EMPTY byte span `(s, e)` plus its linewise flag
/// (charwise/linewise; blockwise is handled separately by its callers). Shared by the in-place selection
/// operators (`CaseSelection`, `ReplaceSelectionChar`, `PasteSelection`) whose only difference is what they
/// do with the span. On failure it returns the cursor those arms should drop back to Normal at — `cur` when
/// there is no anchor, `s` on an empty span (the collapse point) — matching each arm's prior inline `nop`.
fn visual_span(st: &EditorState, b: &[u8], cur: usize) -> Result<(usize, usize, bool), usize> {
    let anchor = st.view.anchor.ok_or(cur)?;
    let line = st.view.mode.selection() == Some(SelectKind::Linewise);
    let (s, e) = selection_range(b, anchor, cur, line);
    if s >= e {
        return Err(s);
    }
    Ok((s, e, line))
}

/// The live `CTRL-V` selection's [`Block`], or `None` when the selection is not blockwise. `$` (curswant at
/// MAXCOL) makes it ragged, each row running to its own line end.
fn selection_block(st: &EditorState, b: &[u8], cur: usize) -> Option<Block> {
    if st.view.mode.selection() != Some(SelectKind::Blockwise) {
        return None;
    }
    let anchor = st.view.anchor?;
    let to_eol = st.view.curswant == MAXCOL;
    Some(block_geom(b, anchor, cur, st.view.indent.tab_width, to_eol))
}

/// Block `r{char}` (Vim `op_replace`): every cell of the block on each row becomes `c` — a `<Tab>` or a
/// wide character inside turns into one `c` per cell, a straddler's outside cells stay as spaces, and a
/// double-width `c` fills two cells each (an odd cell left over becomes a space). Rows the block does not
/// reach are left alone. The cursor goes to the block's top-left.
fn block_replace(block: &Block, c: char, hint: GroupHint) -> Plan {
    use unicode_width::UnicodeWidthChar;
    let cw = c.width().unwrap_or(1).max(1);
    let mut buf = [0u8; 4];
    let cb = c.encode_utf8(&mut buf).as_bytes();
    let edits: Vec<Edit> = block
        .rows
        .iter()
        .filter(|r| r.de > r.ds)
        .map(|r| {
            let short = block.hi == MAXCOL || r.cells <= block.hi;
            let end = if block.hi == MAXCOL {
                r.cells
            } else {
                (block.hi + 1).min(r.cells)
            };
            let cells = end.saturating_sub(block.lo);
            let extra = if short { 0 } else { cells % cw };
            let mut ins = vec![b' '; r.out_l];
            for _ in 0..cells / cw {
                ins.extend_from_slice(cb);
            }
            if !short {
                ins.extend(std::iter::repeat_n(b' ', r.out_r + extra));
            }
            Edit::replace(BytePos(r.ds), r.de - r.ds, ins)
        })
        .collect();
    let at = block.rows[0].ds;
    match EditList::new(edits) {
        Ok(list) if !list.is_empty() => edit(list, at, Mode::Normal, hint),
        _ => nop(at, Mode::Normal),
    }
}

/// Visual `p`/`P` (nvim `nv_put_opt`): delete the selection, then put the register where it was, shaped by
/// the selection and register kinds:
/// - a linewise selection puts any register as whole lines (a charwise or block register becomes lines);
/// - a charwise selection with a linewise register splits the line and puts the lines between the halves;
/// - a block selection with a one-line charwise register puts that text on every row of the block (rows
///   shorter than the block's left edge are skipped); with a linewise register `p` puts the lines below the
///   block and `P` above it;
/// - anything else is an ordinary put at the selection's start.
///
/// The put goes before the cursor, or after it when the delete left the cursor on the last character of
/// the line or file (Vim's "cursor at the end" test). The register is read before the delete, so `p` of the
/// unnamed register still puts the old text; `p` then leaves the deleted text in the unnamed register, `P`
/// leaves the registers as they were. The whole replacement is one edit and one undo step.
fn plan_paste_selection(st: &EditorState, b: &[u8], cur: usize, swap: bool, count: u32) -> Plan {
    let (Some(kind), Some(anchor)) = (st.view.mode.selection(), st.view.anchor) else {
        return nop(cur, Mode::Normal);
    };
    let ts = st.view.indent.tab_width;
    let reg = st.view.registers.get(st.view.pending_register).clone();
    // 1. The delete: its edits, the captured text, and where the cursor sits afterwards.
    let (dels, deleted, mut c2, block_lines) = match kind {
        SelectKind::Blockwise => {
            let block = block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL);
            let dels: Vec<Edit> = block
                .rows
                .iter()
                .filter(|r| r.de > r.ds)
                .map(|r| Edit::replace(BytePos(r.ds), r.de - r.ds, vec![b' '; r.out_l + r.out_r]))
                .collect();
            let reg = Register::blockwise_width(block.yank_text(b), block.width());
            let top = block.rows[0];
            let first = crate::pos::raw::line_of(b, top.ls);
            (
                dels,
                reg,
                top.ds + top.out_l,
                Some((first, first + block.rows.len() - 1)),
            )
        }
        SelectKind::Linewise => {
            let (s, e) = selection_range(b, anchor, cur, true);
            // Lines through the end of an unterminated buffer also take the newline before them (as `Vd`).
            let s0 = if e == b.len() && b.last() != Some(&b'\n') && s > 0 {
                s - 1
            } else {
                s
            };
            (
                vec![Edit::delete(BytePos(s0), e - s0)],
                captured(b, s, e, true),
                s0,
                None,
            )
        }
        SelectKind::Charwise => {
            let (s, e) = selection_range(b, anchor, cur, false);
            if s >= e {
                return nop(s, Mode::Normal);
            }
            (
                vec![Edit::delete(BytePos(s), e - s)],
                captured(b, s, e, false),
                s,
                None,
            )
        }
    };
    let b2 = EditList::new(dels)
        .expect("selection deletes are disjoint (one per line)")
        .apply_to(b);
    // 2. Normal mode cannot rest past a line's last character: a delete that emptied the end of its line (or
    //    took the last lines) leaves the cursor on the character (line) before, and the put goes after it.
    let mut forward = false;
    if kind == SelectKind::Linewise {
        if c2 >= b2.len() && !b2.is_empty() {
            c2 = line_start(&b2, b2.len() - usize::from(b2.ends_with(b"\n")));
            forward = true;
        } else {
            c2 = line_start(&b2, c2);
        }
    } else if c2 > line_start(&b2, c2) && c2 >= line_end(&b2, c2) {
        c2 = prev_boundary(&b2, c2);
        forward = true;
    }
    let caret = st.view.caret;
    let put = |reg: &Register, at: usize, after: bool| {
        paste(
            &b2,
            at,
            Mode::Normal,
            reg,
            after,
            count.max(1),
            caret,
            false,
            true,
            ts,
        )
    };
    let applied = |p: Plan| -> (Vec<u8>, usize) {
        match p.action {
            Action::Txn { edits, .. } => (edits.apply_to(&b2), p.cursor),
            _ => (b2.clone(), p.cursor),
        }
    };
    // 3. The put, on the post-delete text.
    let (mut b3, cursor) = if reg.is_empty() {
        (b2.clone(), c2.min(b2.len()))
    } else {
        match (kind, reg.kind()) {
            (SelectKind::Linewise, RegKind::Linewise) => applied(put(&reg, c2, forward)),
            (SelectKind::Linewise, _) => {
                applied(put(&Register::linewise(reg.text().to_vec()), c2, forward))
            }
            (SelectKind::Charwise, RegKind::Linewise) => {
                let sp = if forward { next_boundary(&b2, c2) } else { c2 };
                let mut out = b2[..sp].to_vec();
                out.push(b'\n');
                // Bounded like every other put ([`MAX_PUT_BYTES`]), so a hostile count cannot exhaust
                // memory.
                let reps =
                    (count.max(1) as usize).min((MAX_PUT_BYTES / reg.text().len().max(1)).max(1));
                for _ in 0..reps {
                    out.extend_from_slice(reg.text());
                }
                out.extend_from_slice(&b2[sp..]);
                let fnb = reg
                    .text()
                    .iter()
                    .take_while(|&&c| c == b' ' || c == b'\t')
                    .count();
                (out, sp + 1 + fnb)
            }
            (SelectKind::Blockwise, RegKind::Linewise) if swap => {
                let (_, last) = block_lines.expect("a block selection records its lines");
                let at = crate::pos::raw::nth_line_start(&b2, last).min(b2.len());
                applied(put(&reg, at, true))
            }
            (SelectKind::Blockwise, RegKind::Charwise) if !reg.text().contains(&b'\n') => {
                let (first, last) = block_lines.expect("a block selection records its lines");
                let (v0, v1) = vcols_at(&b2, c2, ts);
                let vcol = if forward { v1 + 1 } else { v0 };
                let mut edits = Vec::new();
                let mut cursor = c2;
                for line in first..=last {
                    let ls = crate::pos::raw::nth_line_start(&b2, line);
                    if ls > b2.len() || line_cells(&b2, ls, ts) < vcol {
                        continue;
                    }
                    let le = line_end(&b2, ls);
                    let at = if vcol >= line_cells(&b2, ls, ts) {
                        le
                    } else {
                        at_vcol(&b2, ls, vcol, ts)
                    };
                    if line == first {
                        cursor = at + reg.text().len() - 1;
                    }
                    edits.push(Edit::insert(BytePos(at), reg.text().to_vec()));
                }
                let list = EditList::new(edits).expect("one insert per line");
                (list.apply_to(&b2), cursor)
            }
            _ => applied(put(&reg, c2, forward)),
        }
    };
    // Putting lines into a buffer the delete emptied leaves no extra empty line behind (Vim deletes it).
    if kind == SelectKind::Linewise
        && b2.is_empty()
        && b.last() != Some(&b'\n')
        && b3.ends_with(b"\n")
    {
        b3.pop();
    }
    // 4. One edit from the old text to the new: the changed middle between the common ends, kept on char
    //    boundaries.
    let max = b.len().min(b3.len());
    let mut p = b
        .iter()
        .zip(&b3)
        .take_while(|(x, y)| x == y)
        .count()
        .min(max);
    while p > 0 && p < b.len() && (b[p] & 0xC0) == 0x80 {
        p -= 1;
    }
    let mut q = b
        .iter()
        .rev()
        .zip(b3.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
        .min(max - p);
    while q > 0 && (b[b.len() - q] & 0xC0) == 0x80 {
        q -= 1;
    }
    // The replaced text goes to the unnamed register, not the one being put (nvim deletes with no register
    // named, #707).
    let set_register = swap.then_some(RegWrite::Replaced(deleted));
    if b3 == b {
        return Plan {
            set_register,
            ..nop(cursor.min(b.len()), Mode::Normal)
        };
    }
    Plan {
        action: Action::Txn {
            edits: one(Edit::replace(
                BytePos(p),
                b.len() - p - q,
                b3[p..b3.len() - q].to_vec(),
            )),
            hint: GroupHint::BreakBefore,
        },
        cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register,
        set_anchor: None,
        set_mark: None,
    }
}

/// Like [`edit_yank`] but routes the captured span through [`RegWrite::KillAppend`] — an Emacs kill that
/// accumulates onto the current unnamed entry when it follows another kill (kill-ring behaviour).
/// Select's `open/replace-selection` over a charwise or linewise selection, shared by a typed key
/// ([`Command::ReplaceSelection`]) and a paste ([`Command::InsertPaste`] in a selection): delete the
/// selection into the unnamed register, insert `ins` where it began, and enter Insert after it.
fn replace_selection_with(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    ins: Vec<u8>,
) -> Plan {
    let line = matches!(
        st.view.mode,
        Mode::Visual {
            kind: SelectKind::Linewise
        } | Mode::Select {
            kind: SelectKind::Linewise
        }
    );
    let n = ins.len();
    match st.view.anchor {
        Some(anchor) => {
            let (s, e) = selection_range(b, anchor, cur, line);
            if s < e {
                // The removed span fills the unnamed register, as a Visual/Normal delete does —
                // charwise even for whole lines, as nvim stores it (`gHX` leaves `"abc\n"`, `v`).
                let reg = captured(b, s, e, false);
                edit_yank(
                    one(Edit::replace(BytePos(s), e - s, ins)),
                    s + n,
                    Mode::Insert,
                    hint,
                    reg,
                )
            } else {
                edit(
                    one(Edit::insert(BytePos(s), ins)),
                    s + n,
                    Mode::Insert,
                    hint,
                )
            }
        }
        // No anchor (not really in a selection): degrade to a plain insert-and-enter-Insert.
        None => edit(
            one(Edit::insert(BytePos(cur), ins)),
            cur + n,
            Mode::Insert,
            hint,
        ),
    }
}

fn edit_kill(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: Some(RegWrite::KillAppend(reg)),
        set_anchor: None,
        set_mark: None,
    }
}

/// Recase `text` for a Vim case operator (`gU` `gu` `g~` `~` `g?`): nvim's tables, one character to one
/// (`ß` → `ẞ`), composing characters kept ([`crate::casemap::vim_recase`]). `Capitalize` is not a Vim
/// operator; it takes the Emacs rules.
fn recase(text: &str, case: WordCase) -> String {
    use crate::casemap::{vim_recase, VimCase};
    match case {
        WordCase::Upcase => vim_recase(text, VimCase::Upper),
        WordCase::Downcase => vim_recase(text, VimCase::Lower),
        WordCase::Toggle => vim_recase(text, VimCase::Toggle),
        WordCase::Rot13 => vim_recase(text, VimCase::Rot13),
        WordCase::Capitalize => recase_emacs(text, case),
    }
}

/// Recase `text` for an Emacs case command (`M-u` `M-l` `M-c`, the region commands): Emacs 31.1's
/// casefiddle rules, special casing (`ß` → `SS`) and final sigma included
/// ([`crate::casemap::emacs_recase`]). `Toggle` / `Rot13` are not Emacs commands; they take the Vim rules.
fn recase_emacs(text: &str, case: WordCase) -> String {
    use crate::casemap::{emacs_recase, EmacsCase};
    match case {
        WordCase::Upcase => emacs_recase(text, EmacsCase::Up),
        WordCase::Downcase => emacs_recase(text, EmacsCase::Down),
        WordCase::Capitalize => emacs_recase(text, EmacsCase::Capitalize),
        WordCase::Toggle | WordCase::Rot13 => recase(text, case),
    }
}

/// Capture `b[s..e]` as a register value with the given paste geometry.
fn captured(b: &[u8], s: usize, e: usize, linewise: bool) -> Register {
    let bytes = b[s..e].to_vec();
    if linewise {
        Register::linewise(bytes)
    } else {
        Register::charwise(bytes)
    }
}

/// Whether `byte` is horizontal whitespace (a space or tab) — never a newline. The shared predicate for
/// the whitespace-fixup commands (`J`, `just-one-space`, `delete-horizontal-space`, `delete-indentation`).
fn is_hspace(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// The first index `<= from` such that `b[i..from]` is all horizontal whitespace (scan left).
fn hspace_start(b: &[u8], from: usize) -> usize {
    let mut s = from;
    while s > 0 && is_hspace(b[s - 1]) {
        s -= 1;
    }
    s
}

/// The first index `>= from` that is not horizontal whitespace (scan right).
fn hspace_end(b: &[u8], from: usize) -> usize {
    let mut e = from;
    while e < b.len() && is_hspace(b[e]) {
        e += 1;
    }
    e
}

/// The apostrophe-mark (`'{a-z}` / `'.`) target for a mark at `pos`: the first non-blank of that position's
/// LINE (Vim's linewise mark jump), clamped into range.
fn mark_line_target(b: &[u8], pos: usize) -> usize {
    let p = pos.min(b.len());
    motion::first_non_blank(b, crate::pos::raw::line_start(b, p))
}

/// A charwise mark jump target: snap `pos` to a char boundary, then apply Vim's Normal-mode end-of-line
/// clamp — a mark that lands on (or past) the line's newline pulls back onto the line's last char. This is
/// what makes `` `] `` land on the last inserted/changed char even when the stored mark is the exclusive
/// insert end-caret or the trailing '\n' of a linewise span (matching Neovim's on-jump clamp).
fn mark_char_target(b: &[u8], pos: usize) -> usize {
    let p = motion::snap(b, pos.min(b.len()));
    let le = line_end(b, p);
    let ls = line_start(b, p);
    if p >= le && ls < le {
        prev_grapheme(b, le)
    } else {
        p
    }
}

/// The active Emacs region as an ordered `[s, e)` byte span, or `None` when no mark is set or the mark
/// coincides with point (a degenerate region — the region commands are inert then). Shared by
/// `KillRegion` / `CopyRegion` / `EmacsCaseRegion`.
fn active_region(st: &EditorState, cur: usize) -> Option<(usize, usize)> {
    match st.view.mark {
        Some(m) if m != cur => Some((cur.min(m), cur.max(m))),
        _ => None,
    }
}

/// Recase `b[s..e]` with `case` by `rules` ([`recase`] for Vim, [`recase_emacs`]), or `None` when the
/// span is empty or not valid UTF-8 (a grapheme-boundary span always is). Shared by `EmacsCaseWord`
/// (forward-word span), `EmacsCaseRegion` (the region) and a Visual-block `CaseSelection` (each row).
fn recase_span(
    b: &[u8],
    s: usize,
    e: usize,
    case: WordCase,
    rules: fn(&str, WordCase) -> String,
) -> Option<Vec<u8>> {
    if e <= s {
        return None;
    }
    std::str::from_utf8(&b[s..e])
        .ok()
        .map(|span| rules(span, case).into_bytes())
}

/// Plan a `[count]J` / `[count]gJ` join (nvim's `do_join`). `count` joins `count-1` seams (Vim: `J`/`2J` =
/// one join, `3J` = two), leaving the cursor on the LAST join. `J` strips the next line's leading blanks
/// and inserts one space per seam (none before `)`, after a tab or a trailing space, or around an empty
/// line part; two after `.`/`?`/`!` under `'joinspaces'`; `'formatoptions'` `M`/`B` drop it around
/// multibyte characters); `gJ` removes only the newline. Under `'formatoptions'` `j` a line that
/// continues a comment loses its leader (`skip_comment`, #618). The seams are disjoint in original
/// coordinates, so they apply as one `EditList`; `removed` maps each seam's original offset to its
/// post-edit position for the cursor.
fn plan_join(
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
    // nvim's `sumsize != 0` (anything joined so far) and `endcurr1`/`endcurr2` (the last two characters of
    // the latest line part, none after an empty one). An empty line taking part adds nothing, so
    // `a`+``+`b` joins to `a b`, and `foo`+`` to `foo` (#677).
    let mut sum = le > first_ls;
    let last_two = |s: &[u8]| -> (Option<char>, Option<char>) {
        let t = std::str::from_utf8(s).unwrap_or("");
        let mut it = t.chars().rev();
        (it.next(), it.next())
    };
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
            let c0 = std::str::from_utf8(&b[content..next_le])
                .ok()
                .and_then(|t| t.chars().next())
                .unwrap_or('\0');
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
    edit(
        EditList::new(edits).expect("join seams are disjoint and ordered"),
        cursor,
        mode,
        hint,
    )
}

/// The line a comment-continuing open makes (#618): `o`/`O` under `'formatoptions'` `o` and an Insert
/// `<CR>` under `r` on a line that starts with a `'comments'` leader (nvim's `open_line` with
/// `OPENLINE_DO_COM`). The new line's leading text (indent and leader) and, for `<CR>`, how far the text
/// after the cursor starts (`'autoindent'` drops the blanks there). `None` when the line is no comment, so
/// the plain open applies; also for a finished C comment under `'autoindent'`, which lines up with the
/// line that opened it.
pub(super) fn open_comment_line(st: &EditorState, cmd: &Command) -> Option<(Vec<u8>, bool)> {
    open_comment_line_in(&st.view, st.bytes(), cmd)
}

/// [`open_comment_line`] over a View and its buffer's bytes.
pub(crate) fn open_comment_line_in(
    view: &View,
    b: &[u8],
    cmd: &Command,
) -> Option<(Vec<u8>, bool)> {
    if view.caret != CaretGravity::OnChar {
        return None;
    }
    let fc = &view.format;
    let cur = view.cursor;
    let (forward, split) = match cmd {
        Command::OpenBelow if fc.has('o') => (true, false),
        Command::OpenAbove if fc.has('o') => (false, false),
        Command::InsertNewline if fc.has('r') && view.mode == Mode::Insert => (true, true),
        _ => return None,
    };
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let saved = &b[ls..if split { cur } else { le }];
    let ind = view.indent;
    let indent_on = ind.autoindent || ind.smartindent;
    let parts = format::parse_comments(&fc.comments);
    let lead = format::open_leader(
        saved,
        &parts,
        forward,
        split.then(|| cur - ls),
        indent_on,
        ind.tab_width,
    );
    let (old_cols, old_len) = indent_cols(saved, ind.tab_width);
    let mut leader = lead.leader;
    let newindent = if leader.is_empty() {
        // A finished C comment: line up with the line holding its `/*`.
        let ce = lead.comment_end?;
        if !(indent_on && saved[ce..].starts_with(b"*/")) {
            return None;
        }
        let open = b[..ls + ce].windows(2).rposition(|w| w == b"/*")?;
        let ols = line_start(b, open);
        indent_cols(&b[ols..line_end(b, ols)], ind.tab_width).0
    } else if indent_on {
        lead.newindent.unwrap_or(old_cols)
    } else {
        0
    };
    if newindent > 0 {
        let n = leader
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count();
        leader.drain(..n);
    }
    let mut line = if newindent == 0 {
        Vec::new()
    } else if ind.copyindent {
        ind.keeping(&saved[..old_len], newindent)
    } else {
        ind.whitespace(newindent)
    };
    let has_leader = !leader.is_empty();
    line.extend_from_slice(&leader);
    Some((line, has_leader))
}

/// The comment-end character a comment-continuing open arms ([`View::end_comment`]): the last byte of
/// the end part of the three-part comment the source line is in, when that part has the `x` flag.
pub(super) fn open_end_comment(st: &EditorState, cmd: &Command) -> Option<u8> {
    let forward = match cmd {
        Command::OpenBelow | Command::InsertNewline => true,
        Command::OpenAbove => false,
        _ => return None,
    };
    let b = st.bytes();
    let cur = st.view.cursor;
    let ls = line_start(b, cur);
    let end = if matches!(cmd, Command::InsertNewline) {
        cur
    } else {
        line_end(b, cur)
    };
    let parts = format::parse_comments(&st.view.format.comments);
    let (n, k) = format::leader_len(&b[ls..end], &parts, !forward, true);
    let k = k.filter(|_| n > 0)?;
    let flags = parts[k].flags.as_bytes();
    let end_part = flags.iter().find_map(|&f| match f {
        b's' => Some(k + 2),
        b'm' => Some(k + 1),
        b'e' if !forward => Some(k),
        b'e' | b'f' => Some(usize::MAX),
        _ => None,
    })?;
    let part = parts.get(end_part)?;
    if part.flags.contains('x') {
        part.text.last().copied()
    } else {
        None
    }
}

/// Typing the armed comment-end character first on a line opened inside a three-part comment replaces the
/// middle leader by the end one (nvim's `insertchar` with `end_comment_pending`): ` * ` + `/` is ` */`.
fn plan_end_comment(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    c: char,
    hint: GroupHint,
) -> Option<Plan> {
    let ch = u8::try_from(c).ok()?;
    if !st.view.auto_indent_pending
        || st.view.end_comment != Some(ch)
        || st.view.mode != Mode::Insert
    {
        return None;
    }
    let ls = line_start(b, cur);
    let line = &b[ls..line_end(b, cur)];
    let parts = format::parse_comments(&st.view.format.comments);
    let (n, k) = format::leader_len(line, &parts, false, true);
    let k = k.filter(|&k| n > 0 && parts[k].flags.contains('m'))?;
    let mut middle = parts[k].text.as_slice();
    while middle.last().is_some_and(|&x| x == b' ' || x == b'\t') {
        middle = &middle[..middle.len() - 1];
    }
    let end = &parts.get(k + 1)?.text;
    let mut i = cur - ls;
    while i > 0 && matches!(line[i - 1], b' ' | b'\t') {
        i -= 1;
    }
    let i = i.checked_sub(middle.len())?;
    if end.last() != Some(&ch) {
        return None;
    }
    Some(edit(
        one(Edit::replace(BytePos(ls + i), cur - (ls + i), end.clone())),
        ls + i + end.len(),
        Mode::Insert,
        hint,
    ))
}

/// Plan an open that continues a comment ([`open_comment_line`]), or `None`.
fn plan_open_comment(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    cmd: &Command,
    hint: GroupHint,
) -> Option<Plan> {
    let (prefix, _) = open_comment_line(st, cmd)?;
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let plan = match cmd {
        Command::OpenBelow => {
            let mut ins = vec![b'\n'];
            ins.extend_from_slice(&prefix);
            edit(
                one(Edit::insert(BytePos(le), ins)),
                le + 1 + prefix.len(),
                Mode::Insert,
                hint,
            )
        }
        Command::OpenAbove => {
            let mut ins = prefix.clone();
            ins.push(b'\n');
            edit(
                one(Edit::insert(BytePos(ls), ins)),
                ls + prefix.len(),
                Mode::Insert,
                hint,
            )
        }
        _ => {
            // Insert `<CR>`: an untouched autoindent's blanks before the cursor go (nvim's `trunc_line`), and
            // under 'autoindent' the blanks after it do not move to the new line.
            let mut cut = cur;
            if st.view.auto_indent_pending {
                while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
                    cut -= 1;
                }
            }
            let mut after = cur;
            if st.view.indent.autoindent {
                while after < le && matches!(b[after], b' ' | b'\t') {
                    after += 1;
                }
            }
            let mut ins = vec![b'\n'];
            ins.extend_from_slice(&prefix);
            let cursor = cut + ins.len();
            edit(
                one(Edit::replace(BytePos(cut), after - cut, ins)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
    };
    Some(plan)
}

/// A caret key that takes an Insert cursor off a line it opened with an untouched autoindent (or comment
/// leader) removes that line's trailing blanks first (nvim's `start_arrow` → `stop_insert` with `did_ai`,
/// #617): `o<Up>` leaves an empty line, not one holding the indent.
fn leave_autoindent(st: &EditorState, cur: usize, p: Plan, hint: GroupHint) -> Plan {
    let b = st.bytes();
    if st.view.mode != Mode::Insert
        || !st.view.auto_indent_pending
        || st.view.caret != CaretGravity::OnChar
        || p.is_edit
        || !matches!(p.action, Action::Nop)
        || p.mode != Mode::Insert
        || line_start(b, p.cursor) == line_start(b, cur)
    {
        return p;
    }
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let mut cut = le;
    while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
        cut -= 1;
    }
    if cut == le || cur != le {
        return p;
    }
    let cursor = if p.cursor >= le {
        p.cursor - (le - cut)
    } else {
        p.cursor
    };
    edit(
        one(Edit::delete(BytePos(cut), le - cut)),
        cursor,
        Mode::Insert,
        hint,
    )
}

pub fn plan(st: &EditorState, cmd: &Command) -> Plan {
    let hint = if st.view.last_was_edit {
        GroupHint::Continue
    } else {
        GroupHint::BreakBefore
    };
    let p = plan_cmd(st, cmd);
    leave_autoindent(st, st.view.cursor, p, hint)
}

fn plan_cmd(st: &EditorState, cmd: &Command) -> Plan {
    let b = st.bytes();
    let cur = st.view.cursor;
    // Undo/redo restore DOCUMENT state, not mode. The engine only reaches them from Normal or from
    // Insert via `i_CTRL-O` (Vim's `u` in Visual is not an undo), so preserving Insert makes
    // `i_CTRL-O u` return to Insert — the one-shot's resume (KL-OBL-5) — while everything else
    // collapses to Normal exactly as before.
    let undo_resume_mode = if st.view.mode == Mode::Insert {
        Mode::Insert
    } else {
        Mode::Normal
    };
    let hint = if st.view.last_was_edit {
        GroupHint::Continue
    } else {
        GroupHint::BreakBefore
    };
    if let Some(p) = failed_motion(st, b, cur, cmd) {
        return p;
    }
    if let Some(p) = plan_open_comment(st, b, cur, cmd, hint) {
        return p;
    }

    match cmd {
        // `h`/`l` move by whole grapheme cluster so the cursor never lands mid-emoji/ZWJ/combining
        // (F-002 #2). Operator/selection internals below still step by char boundary — grapheme-aware
        // deletion is a follow-up; this slice makes the CURSOR stay synced to user-perceived chars.
        Command::MoveLeft => nop(prev_grapheme(b, cur), st.view.mode),
        Command::MoveRight => nop(next_grapheme(b, cur), st.view.mode),
        Command::MoveLineStart => nop(line_start(b, cur), st.view.mode),
        Command::MoveLineEnd => nop(line_end(b, cur), st.view.mode),
        Command::MoveUp => nop(
            motion::vmove(b, cur, 1, false, st.view.curswant, st.view.indent.tab_width),
            st.view.mode,
        ),
        Command::MoveDown => nop(
            motion::vmove(b, cur, 1, true, st.view.curswant, st.view.indent.tab_width),
            st.view.mode,
        ),
        Command::EnterInsert => nop(cur, Mode::Insert),
        // `gi` — resume Insert at the last-insert position (snapped), or the buffer start if none yet.
        Command::InsertAtLastInsert => {
            let at = st
                .view
                .last_insert()
                .map_or(0, |p| motion::snap(b, p.min(b.len())));
            nop(at, Mode::Insert)
        }
        // `a`: Insert after the cursor's char — on an empty line (or past the last char) there is none, so
        // the caret stays rather than crossing into the next line (#701).
        Command::EnterInsertAfter => {
            let at = if cur < line_end(b, cur) {
                next_grapheme(b, cur)
            } else {
                cur
            };
            nop(at, Mode::Insert)
        }
        Command::InsertLineStart => nop(motion::first_non_blank(b, cur), Mode::Insert),
        // `gI` — insert at byte column 0 (line start), BEFORE all indentation, unlike `I` (first non-blank).
        Command::InsertColumnZero => nop(line_start(b, cur), Mode::Insert),
        Command::AppendLineEnd => nop(line_end(b, cur), Mode::Insert),
        // `o`/`O` under `'autoindent'`/`'smartindent'` (Vim profile): the new line takes the indent nvim's
        // `open_line` gives it ([`IndentConfig::open_indent`]).
        Command::OpenBelow | Command::OpenAbove if auto_indents(st) => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let below = matches!(cmd, Command::OpenBelow);
            let (pad, _) = st
                .view
                .indent
                .open_indent(b, ls, le, below, &st.view.format.cinwords);
            let mut ins = Vec::with_capacity(pad.len() + 1);
            if below && le == ls && le < b.len() {
                // An empty line: the new line goes in after its `\n`, so marks stay on it (#1125).
                ins.extend_from_slice(&pad);
                ins.push(b'\n');
                edit(
                    one(Edit::insert(BytePos(le + 1), ins)),
                    le + 1 + pad.len(),
                    Mode::Insert,
                    hint,
                )
            } else if below {
                ins.push(b'\n');
                ins.extend_from_slice(&pad);
                edit(
                    one(Edit::insert(BytePos(le), ins)),
                    le + 1 + pad.len(),
                    Mode::Insert,
                    hint,
                )
            } else {
                ins.extend_from_slice(&pad);
                ins.push(b'\n');
                edit(
                    one(Edit::insert(BytePos(ls), ins)),
                    ls + pad.len(),
                    Mode::Insert,
                    hint,
                )
            }
        }
        Command::OpenBelow => {
            let le = line_end(b, cur);
            // On an empty line the new line goes in after its `\n`, the same bytes as before it, so its
            // marks read as staying on it (nvim `o`; `O` there moves them down, #1125).
            let at = if le == line_start(b, cur) && le < b.len() {
                le + 1
            } else {
                le
            };
            edit(
                one(Edit::insert(BytePos(at), b"\n".to_vec())),
                le + 1,
                Mode::Insert,
                hint,
            )
        }
        Command::OpenAbove => {
            let ls = line_start(b, cur);
            edit(
                one(Edit::insert(BytePos(ls), b"\n".to_vec())),
                ls,
                Mode::Insert,
                hint,
            )
        }
        Command::EnterNormal => {
            // `<Esc>` closing a blockwise insert-replicate session replicates the top row's typed text down
            // the block (see `block_replicate`); the session is cleared in `commit` as Insert is left.
            if st.view.mode == Mode::Insert {
                if let Some(session) = st.view.block_insert {
                    return block_replicate(b, cur, session, hint, st.view.indent.tab_width);
                }
                // Autoindent cleanup (Vim): if this Insert session opened a line with auto-indent and
                // nothing non-blank was typed on it, leaving Insert removes the indent so no trailing
                // whitespace is left. Guarded to ONLY a line that is entirely blank with the caret at its
                // end — exactly the auto-indent-leftover shape — so user-typed content is never touched.
                if st.view.auto_indent_pending {
                    // nvim's `stop_insert` with `did_ai`: the blanks before the caret at the line end go,
                    // the whole indent of a line holding nothing else, or the blank after a comment
                    // leader (`o<Esc>` on `// a` leaves `//`, #618).
                    let ls = line_start(b, cur);
                    let le = line_end(b, cur);
                    let mut cut = le;
                    while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
                        cut -= 1;
                    }
                    if cur == le && cut < le {
                        return edit(
                            one(Edit::delete(BytePos(cut), le - cut)),
                            cut,
                            Mode::Normal,
                            hint,
                        );
                    }
                }
            }
            // Vim: leaving Insert OR Replace nudges the cursor left one, but never before the line start.
            // Leaving Visual (Esc) just collapses the selection in place — no nudge.
            if matches!(
                st.view.mode,
                Mode::Insert | Mode::Replace | Mode::VirtualReplace
            ) {
                let ls = line_start(b, cur);
                let c = if cur > ls { prev_grapheme(b, cur) } else { cur };
                nop(c, Mode::Normal)
            } else if st.view.mode.selection().is_some() && st.view.caret == CaretGravity::OnChar {
                // A Visual cursor past the line's last char (`v$`, `<C-v>$`, `$vj`) comes back onto it, as
                // Normal mode never rests on the newline (#1207).
                let (ls, le) = (line_start(b, cur), line_end(b, cur));
                let c = if cur == le && le > ls {
                    prev_grapheme(b, cur)
                } else {
                    cur
                };
                nop(c, Mode::Normal)
            } else {
                nop(cur, Mode::Normal)
            }
        }
        // F-011: mode-only transitions; the frontend owns the PTY + scrollback (the core never edits a
        // terminal buffer). Cursor is unchanged — a terminal buffer's document is an empty placeholder.
        Command::EnterTerminal => nop(cur, Mode::Terminal),
        Command::EnterTerminalNormal => nop(cur, Mode::TerminalNormal),
        Command::EnterReplace => nop(cur, Mode::Replace),
        Command::ReplaceType(c) => {
            let mut buf = [0u8; 4];
            let typed = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let tn = typed.len();
            let le = line_end(b, cur);
            let (edits, push) = if cur < le {
                // Overwrite the char under the cursor; remember its original bytes for `<BS>` restore.
                let nb = next_boundary(b, cur);
                (
                    one(Edit::replace(BytePos(cur), nb - cur, typed)),
                    Some(Some(b[cur..nb].to_vec())),
                )
            } else {
                // At end-of-line there is nothing to overwrite: append, remembered as `None` (backspace deletes).
                (one(Edit::insert(BytePos(cur), typed)), Some(None))
            };
            Plan {
                action: Action::ReplaceTxn {
                    edits,
                    hint,
                    push,
                    pop: false,
                },
                cursor: cur + tn,
                mode: Mode::Replace,
                is_edit: true,
                effects: Vec::new(),
                set_register: None,
                set_anchor: None,
                set_mark: None,
            }
        }
        // Shared by Replace (`R`) and Virtual Replace (`gR`) — the restore stack is identical; the mode is
        // preserved so `<BS>` stays in whichever replace mode is active.
        Command::ReplaceBackspace => match st.view.replace_stack.last() {
            // At the session start `<BS>` only moves the cursor left (Vim does not restore past the start).
            None => {
                let ls = line_start(b, cur);
                let c = if cur > ls { prev_grapheme(b, cur) } else { cur };
                nop(c, st.view.mode)
            }
            Some(entry) => {
                let start = prev_boundary(b, cur); // the last typed char occupies [start, cur)
                let edits = match entry {
                    Some(orig) => one(Edit::replace(BytePos(start), cur - start, orig.clone())),
                    None => one(Edit::delete(BytePos(start), cur - start)),
                };
                Plan {
                    action: Action::ReplaceTxn {
                        edits,
                        hint,
                        push: None,
                        pop: true,
                    },
                    cursor: start,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: None,
                    set_mark: None,
                }
            }
        },
        Command::EnterVirtualReplace => nop(cur, Mode::VirtualReplace),
        Command::VirtualReplaceType(c) => plan_virtual_replace_type(st, b, cur, *c, hint),
        Command::VirtualReplaceChar(count, c) => {
            plan_virtual_replace_char(st, b, cur, *count, *c, hint)
        }
        Command::InsertChar(c) => {
            let mut buf = [0u8; 4];
            let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = bytes.len();
            // `'smartindent'`: a `{`, `}` or `#` typed first on a line just opened realigns it.
            let ls = line_start(b, cur);
            let si = st.view.si_state;
            if auto_indents(st)
                && st.view.indent.smartindent
                && si.armed()
                && st.view.mode == Mode::Insert
                && b[ls..cur].iter().all(|&x| x == b' ' || x == b'\t')
            {
                if let Some(pad) = st.view.indent.typed_indent(b, ls, cur, si, *c) {
                    let cursor = ls + pad.len() + n;
                    let mut repl = pad;
                    repl.extend_from_slice(&bytes);
                    return edit(
                        one(Edit::replace(BytePos(ls), cur - ls, repl)),
                        cursor,
                        Mode::Insert,
                        hint,
                    );
                }
            }
            if let Some(p) = plan_end_comment(st, b, cur, *c, hint) {
                return p;
            }
            if let Some(p) = auto_wrap(st, b, cur, *c, hint) {
                return p;
            }
            edit(
                one(Edit::insert(BytePos(cur), bytes)),
                cur + n,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-R{reg}` — splice the named register's raw bytes at the caret, staying in Insert with the
        // cursor after the inserted text. An empty register is a no-op. `"` reads the unnamed register (via
        // `get`'s fallback). The bytes are inserted verbatim (linewise registers keep their trailing `\n`).
        Command::InsertRegister(name) => {
            let bytes = st.view.registers.get(Some(*name)).text().to_vec();
            if bytes.is_empty() {
                return nop(cur, Mode::Insert);
            }
            let n = bytes.len();
            edit(
                one(Edit::insert(BytePos(cur), bytes)),
                cur + n,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-R=<expr><CR>` — evaluate the expression (a minimal-honest arithmetic/string calculator,
        // NOT full Vimscript) and splice the formatted result at the caret, staying in Insert. A malformed
        // or unsupported expression evaluates to the empty string and inserts nothing (Vim's degrade).
        Command::InsertEval(e) => {
            let bytes = crate::expr::eval_or_empty(e).into_bytes();
            if bytes.is_empty() {
                return nop(cur, Mode::Insert);
            }
            let n = bytes.len();
            edit(
                one(Edit::insert(BytePos(cur), bytes)),
                cur + n,
                Mode::Insert,
                hint,
            )
        }
        // A bracketed paste (#600): one literal splice at the caret, mode unchanged.
        Command::InsertPaste(text) => {
            if text.is_empty() {
                return nop(cur, st.view.mode);
            }
            // Over a charwise/linewise selection a paste replaces it, as a typed Select key does
            // (`vim.paste`: `<Del>` the selection, then put the text where it was), entering Insert.
            if matches!(
                st.view.mode.selection(),
                Some(SelectKind::Charwise | SelectKind::Linewise)
            ) {
                return replace_selection_with(st, b, cur, hint, text.as_bytes().to_vec());
            }
            let n = text.len();
            edit(
                one(Edit::insert(BytePos(cur), text.as_bytes().to_vec())),
                cur + n,
                st.view.mode,
                hint,
            )
        }
        // A Replace-mode paste: overwrite one char of the current line per pasted char (newlines excluded,
        // as nvim's `vim.paste` counts them), then splice the text. nvim counts BYTES and can split a
        // multi-byte char; this counts characters.
        Command::ReplacePaste(text) => {
            if text.is_empty() {
                return nop(cur, st.view.mode);
            }
            let nchars = text.chars().filter(|&c| c != '\n').count();
            let le = line_end(b, cur);
            let end = std::str::from_utf8(&b[cur..le]).map_or(le, |rest| {
                rest.char_indices().nth(nchars).map_or(le, |(i, _)| cur + i)
            });
            let n = text.len();
            edit(
                one(Edit::replace(
                    BytePos(cur),
                    end - cur,
                    text.as_bytes().to_vec(),
                )),
                cur + n,
                st.view.mode,
                hint,
            )
        }
        // Emacs `xterm-paste`: kill-new the text, then yank it (point after, mark at the start).
        Command::EmacsPaste(text) => {
            if text.is_empty() {
                return nop(cur, st.view.mode);
            }
            let n = text.len();
            Plan {
                set_register: Some(RegWrite::Write(Register::charwise(
                    text.as_bytes().to_vec(),
                ))),
                set_mark: Some(MarkWrite::Set(cur)),
                ..edit(
                    one(Edit::insert(BytePos(cur), text.as_bytes().to_vec())),
                    cur + n,
                    st.view.mode,
                    hint,
                )
            }
        }
        // `i_CTRL-W` — delete the word before the caret (within the current line), staying in Insert.
        Command::InsertDeleteWordBack => {
            let ls = line_start(b, cur);
            if cur <= ls {
                return nop(cur, Mode::Insert);
            }
            let wb = motion::target(b, cur, Motion::WordBack, 1).max(ls);
            // If the word motion did not move (e.g. only whitespace precedes), delete one grapheme instead.
            let start = if wb < cur { wb } else { prev_boundary(b, cur) };
            edit(
                one(Edit::delete(BytePos(start), cur - start)),
                start,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-U` (nvim v0.12.6, default 'backspace' "indent,eol,start" and 'autoindent'): delete the text
        // entered before the caret on this line; with none, delete back to the first non-blank, and from
        // there the indent too. At the line start it joins the line to the one above, as `<BS>` does.
        Command::InsertDeleteToLineStart => {
            let ls = line_start(b, cur);
            if cur <= ls {
                if ls == 0 {
                    return nop(cur, Mode::Insert);
                }
                return edit(
                    one(Edit::delete(BytePos(ls - 1), 1)),
                    ls - 1,
                    Mode::Insert,
                    hint,
                );
            }
            let mut fnb = ls;
            while fnb < cur && matches!(b[fnb], b' ' | b'\t') {
                fnb += 1;
            }
            let floor = if cur > fnb { fnb } else { ls };
            // Where this Insert began (Vim's `Insstart_orig`): a caret key inside it does not move it.
            let insstart = st.view.insstart.unwrap_or(cur);
            let start = if (ls..cur).contains(&insstart) {
                insstart.max(floor)
            } else {
                floor
            };
            edit(
                one(Edit::delete(BytePos(start), cur - start)),
                start,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-N` / `i_CTRL-P` — one keyword-completion cycle step: replace the `back` CHARACTERS before
        // the caret (the current completion region) with the candidate `text`, staying in Insert with the
        // caret after it. `back` walks char boundaries so multibyte keywords delete cleanly; it is clamped
        // at the buffer start. Applying repeated steps in one session chains cleanly (each deletes the last
        // applied text and inserts the next), so `.` reproduces the final accepted text.
        Command::CompleteWord { back, text } => {
            let mut start = cur;
            for _ in 0..*back {
                if start == 0 {
                    break;
                }
                start = prev_boundary(b, start);
            }
            let bytes = text.as_bytes().to_vec();
            let n = bytes.len();
            edit(
                one(Edit::replace(BytePos(start), cur - start, bytes)),
                start + n,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-T` — indent the current line to the NEXT multiple of the shiftwidth (Vim rounds: 2 cols at
        // sw=4 → 4); the caret rides right with the text. The leading whitespace is rebuilt at the new width
        // in the configured style (Vim's `change_indent`).
        Command::InsertIndent => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tab_width);
            let sw = st.shift_width();
            let want = st
                .view
                .indent
                .rebuilt(&b[ls..ls + ws], (cols / sw + 1) * sw);
            let cursor = reindented_caret(cur, ls, ws, want.len());
            edit(
                one(Edit::replace(BytePos(ls), ws, want)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-D` — dedent the current line by one shiftwidth; the caret rides left (never before the line
        // start). A no-op when there is no leading whitespace.
        Command::InsertDedent => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tab_width);
            if ws == 0 {
                return nop(cur, Mode::Insert);
            }
            // Down to the PREVIOUS multiple of the shiftwidth (Vim rounds: 6 cols at sw=4 → 4).
            let sw = st.shift_width();
            let want = st
                .view
                .indent
                .rebuilt(&b[ls..ls + ws], cols.saturating_sub(1) / sw * sw);
            let cursor = reindented_caret(cur, ls, ws, want.len());
            edit(
                one(Edit::replace(BytePos(ls), ws, want)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // `i_0_CTRL-D` / `i_^_CTRL-D` — delete all leading whitespace of the line; the caret rides left.
        Command::InsertDeleteIndent | Command::InsertDeleteIndentRestore => {
            let ls = line_start(b, cur);
            let le = line_end(b, ls);
            let remove = b[ls..le]
                .iter()
                .take_while(|c| matches!(c, b' ' | b'\t'))
                .count();
            if remove == 0 {
                return nop(cur, Mode::Insert);
            }
            let cursor = cur.saturating_sub(remove).max(ls);
            edit(
                one(Edit::delete(BytePos(ls), remove)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // `i_CTRL-G_j` / `i_CTRL-G_k` — the adjacent line, at the column the insert started in (char columns,
        // like the rest of the motion model). No adjacent line: stay put (nvim beeps).
        Command::InsertStartColumnLine { down } => {
            let ls = line_start(b, cur);
            let target = if *down {
                let le = line_end(b, cur);
                (le < crate::motion::content_end(b)).then(|| le + 1)
            } else {
                (ls > 0).then(|| line_start(b, ls - 1))
            };
            let to = target.map_or(cur, |t| at_col(b, t, st.view.insert_col));
            nop(to, st.view.mode)
        }
        // `i_CTRL-G_U` + `<Left>`/`<Right>`: one char within the line (the undo-group keep is in
        // `apply_command`).
        Command::InsertCaretStep { right } => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let to = if *right {
                if cur < le {
                    next_boundary(b, cur)
                } else {
                    cur
                }
            } else if cur > ls {
                prev_boundary(b, cur)
            } else {
                cur
            };
            nop(to, st.view.mode)
        }
        // `<Tab>` in Insert: insert whitespace at the caret to the next stop. Space style (`expandtab`)
        // inserts spaces up to the next tabstop column even when it started mid-line; tab style inserts one
        // `\t`. Within the line's leading whitespace the stop is the SHIFTWIDTH instead ('smarttab', on by
        // default in nvim): spaces under `expandtab`, else the leading whitespace is rebuilt as tabs + spaces
        // at the new width. With `ts == sw` both rules agree. Unlike `i_CTRL-T` this is caret-relative.
        Command::InsertTab => {
            let ls = line_start(b, cur);
            let (from, run) = st.view.indent.tab(b, ls, cur);
            let cursor = from + run.len();
            edit(
                one(Edit::replace(BytePos(from), cur - from, run)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // An Insert `<CR>` under `'autoindent'`/`'smartindent'` (Vim profile): the blanks after the caret
        // go, the new line takes the indent of the text before it, and a line holding only the indent the
        // last open gave it is emptied (nvim's `open_line` with `did_ai`).
        Command::InsertNewline if auto_indents(st) && st.view.mode == Mode::Insert => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let (mut pad, _) =
                st.view
                    .indent
                    .open_indent(b, ls, cur, true, &st.view.format.cinwords);
            // After `^ CTRL-D` an unindented line hands the deleted indent to the next one (`old_indent`).
            if pad.is_empty() && st.view.old_indent > 0 && st.view.indent.autoindent {
                pad = st.view.indent.whitespace(st.view.old_indent);
            }
            let mut cut = cur;
            if st.view.auto_indent_pending {
                while cut > ls && matches!(b[cut - 1], b' ' | b'\t') {
                    cut -= 1;
                }
            }
            let mut after = cur;
            while after < le && matches!(b[after], b' ' | b'\t') {
                after += 1;
            }
            let mut ins = Vec::with_capacity(pad.len() + 1);
            ins.push(b'\n');
            ins.extend_from_slice(&pad);
            let cursor = cut + ins.len();
            edit(
                one(Edit::replace(BytePos(cut), after - cut, ins)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        Command::InsertNewline => edit(
            one(Edit::insert(BytePos(cur), b"\n".to_vec())),
            cur + 1,
            Mode::Insert,
            hint,
        ),
        // `o`/`O`/`<CR>` with a tree-suggested indent (F-015 Phase 2): open a line whose leading whitespace
        // is `level × unit`, cursor after it. `Above` inserts `<indent>\n` before the line (cursor on the new
        // line above); `Below`/`Split` insert `\n<indent>` (cursor on the new line below / after the split).
        // `level: 0` degrades to a plain open, so this also serves as the non-tree fallback.
        Command::OpenLineIndent { kind, level } => {
            let pad = st.indent_for_level(*level);
            let (at, mut ins, cursor) = match kind {
                OpenKind::Above => {
                    let ls = line_start(b, cur);
                    (ls, Vec::with_capacity(pad.len() + 1), ls + pad.len())
                }
                OpenKind::Below => {
                    let le = line_end(b, cur);
                    (le, Vec::with_capacity(pad.len() + 1), le + 1 + pad.len())
                }
                OpenKind::Split => (cur, Vec::with_capacity(pad.len() + 1), cur + 1 + pad.len()),
            };
            if matches!(kind, OpenKind::Above) {
                ins.extend_from_slice(&pad);
                ins.push(b'\n');
            } else {
                ins.push(b'\n');
                ins.extend_from_slice(&pad);
            }
            edit(
                one(Edit::insert(BytePos(at), ins)),
                cursor,
                Mode::Insert,
                hint,
            )
        }
        // A closer (`}`/`)`/`]`) typed as the line's sole leading content realigns the line to its matching
        // opener's indent, then inserts the closer — smartindent-like (F-015 Phase 3a). Deterministic bytes
        // bracket-match; with content already before the cursor, or no matching opener, it is a plain insert.
        Command::InsertCloser { ch } => {
            let mut buf = [0u8; 4];
            let typed = ch.encode_utf8(&mut buf).as_bytes().to_vec();
            let n = typed.len();
            let ls = line_start(b, cur);
            let all_ws = b[ls..cur].iter().all(|&x| x == b' ' || x == b'\t');
            let pair = match ch {
                '}' => Some((b'{', b'}')),
                ')' => Some((b'(', b')')),
                ']' => Some((b'[', b']')),
                _ => None,
            };
            let pad = if all_ws {
                pair.and_then(|(open, close)| matching_opener_pad(b, cur, open, close))
            } else {
                None
            };
            match pad {
                Some(pad) => {
                    // One edit: replace the leading whitespace [ls, cur) with `pad ++ closer`, so any content
                    // after the cursor is untouched and the whole thing is a single insert-coalesced group.
                    let cursor = ls + pad.len() + n;
                    let mut repl = pad;
                    repl.extend_from_slice(&typed);
                    edit(
                        one(Edit::replace(BytePos(ls), cur - ls, repl)),
                        cursor,
                        Mode::Insert,
                        hint,
                    )
                }
                None => edit(
                    one(Edit::insert(BytePos(cur), typed)),
                    cur + n,
                    Mode::Insert,
                    hint,
                ),
            }
        }
        Command::DeleteBack => {
            let ls = line_start(b, cur);
            let stepped = (st.view.caret == CaretGravity::OnChar && st.view.mode == Mode::Insert)
                .then(|| st.view.indent.backspace(b, ls, cur, st.view.typed_space))
                .flatten();
            if let Some((from, pad)) = stepped {
                // `'smarttab'` in the indent / `'softtabstop'`: back to the previous stop (nvim's `ins_bs`).
                return edit(
                    one(Edit::replace(BytePos(from), cur - from, vec![b' '; pad])),
                    from + pad,
                    Mode::Insert,
                    hint,
                );
            }
            if cur == 0 {
                nop(cur, st.view.mode)
            } else {
                // Vim's `<BS>` deletes a whole character with its composing marks, and nvim v0.12.6 a whole
                // cluster (a flag, a ZWJ sequence), so the cursor never lands inside one (#909). The Emacs
                // profile keeps deleting one code point.
                let p = if st.view.caret == CaretGravity::OnChar {
                    prev_grapheme(b, cur)
                } else {
                    prev_boundary(b, cur)
                };
                edit(
                    one(Edit::delete(BytePos(p), cur - p)),
                    p,
                    st.view.mode,
                    hint,
                )
            }
        }
        Command::DeleteUnder(count) => {
            // `{count}x`: delete `count` chars from the cursor, clamped at end-of-line (Vim). Fewer than
            // `count` chars left deletes to EOL. The removed span fills the unnamed register (charwise). A
            // char is a grapheme cluster: a base with its composing marks, a ZWJ emoji sequence (#696).
            let le = line_end(b, cur);
            let (end, _) = advance_graphemes(b, cur, *count, le);
            if end <= cur {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, cur, end, false);
                edit_yank(
                    one(Edit::delete(BytePos(cur), end - cur)),
                    cur,
                    st.view.mode,
                    hint,
                    reg,
                )
            }
        }
        Command::DeleteForward(count) => {
            // Emacs `delete-char`: delete `count` chars forward from point, clamped at BUFFER end (crosses
            // newlines — Emacs has no end-of-line boundary), and WITHOUT writing to the kill ring (D-026).
            // Uses the no-register `edit` path, unlike Vim `x` (`DeleteUnder`, which yanks via `edit_yank`).
            let end = advance_n(b, cur, *count, b.len());
            if end <= cur {
                nop(cur, st.view.mode)
            } else {
                edit(
                    one(Edit::delete(BytePos(cur), end - cur)),
                    cur,
                    st.view.mode,
                    hint,
                )
            }
        }
        Command::ReplaceChar(count, c) => {
            // `{count}r{ch}`: replace `count` chars with `ch`. Per Vim it is a NO-OP if fewer than
            // `count` chars remain on the line (never a partial replace). Cursor lands on the last one.
            let le = line_end(b, cur);
            let (end, reached) = advance_graphemes(b, cur, *count, le);
            if !reached {
                // Vim beeps (`nv_replace`), so a macro or `:normal` stops here (#1111).
                beep(cur, st.view.mode)
            } else if *c == '\n' {
                // `{count}r<CR>`: replace the count chars with a SINGLE line break (Vim splits the line),
                // leaving the cursor on the first char of the new line — never `count` newlines.
                edit(
                    one(Edit::replace(BytePos(cur), end - cur, b"\n".to_vec())),
                    cur + 1,
                    st.view.mode,
                    hint,
                )
            } else {
                let mut buf = [0u8; 4];
                let one_ch = c.encode_utf8(&mut buf).as_bytes();
                let mut bytes = Vec::with_capacity(one_ch.len() * *count as usize);
                for _ in 0..*count {
                    bytes.extend_from_slice(one_ch);
                }
                let last = cur + one_ch.len() * (*count as usize - 1);
                edit(
                    one(Edit::replace(BytePos(cur), end - cur, bytes)),
                    last,
                    st.view.mode,
                    hint,
                )
            }
        }
        Command::ToggleCase(count) => {
            // `{count}~`: toggle the case of `count` chars, clamped at EOL, then leave the cursor past the
            // last toggled char (a Normal-mode edit, so `commit` clamps it back onto the last char at EOL).
            // Case-toggle by Unicode scalar (uppercase→lowercase, else lowercase→uppercase), so non-ASCII
            // letters flip too (`~` on "αβ" → "Αβ"); non-letters are consumed but left unchanged. The
            // toggled UTF-8 may differ in byte length from the source, so the cursor lands at `cur +
            // flipped.len()`, not the source end.
            let le = line_end(b, cur);
            let (end, _) = advance_graphemes(b, cur, *count, le);
            if end <= cur {
                // An empty line: nvim's `n_swapchar` fails (`clearopbeep`), so it is no change for `.` (#1179).
                beep(cur, st.view.mode)
            } else {
                // Invalid UTF-8 bytes are kept as they are (#706).
                let flipped = recase_bytes(&b[cur..end], WordCase::Toggle);
                if flipped == b[cur..end] {
                    nop(end, st.view.mode) // nothing was a letter: `~` just moves right
                } else {
                    let cursor = cur + flipped.len();
                    edit(
                        one(Edit::replace(BytePos(cur), end - cur, flipped)),
                        cursor,
                        st.view.mode,
                        hint,
                    )
                }
            }
        }
        Command::JoinLines(count) => plan_join(st, cur, *count, st.view.mode, hint, false),
        Command::JoinLinesNoSpace(count) => plan_join(st, cur, *count, st.view.mode, hint, true),
        Command::IncrementNumber(delta) => {
            // `CTRL-A`/`CTRL-X`: adjust the number at or after the cursor on the current line. A `0x`-prefixed
            // hex literal increments in hex (`0x1f`→`0x20`); otherwise a decimal (with optional `-` sign).
            let ls = crate::pos::raw::line_start(b, cur);
            let le = line_end(b, cur);
            match incr_number(b, ls, le, cur, *delta) {
                Some((start, old_len, bytes, cursor)) => edit(
                    one(Edit::replace(BytePos(start), old_len, bytes)),
                    cursor,
                    st.view.mode,
                    hint,
                ),
                None => nop(cur, st.view.mode), // no number on the rest of the line
            }
        }
        // Visual `CTRL-A`/`CTRL-X` (and `g CTRL-A`/`g CTRL-X`): increment the first number per selected line.
        // Plain form adds `delta` to every line; `sequential` (the `g` form) adds `delta`, `2·delta`, `3·delta`…
        // to successive numbered lines — the classic "turn a column of 1s into 1,2,3…".
        //
        // The unifying rule across all three shapes: on each line, search from the SELECTION'S LEFT EDGE
        // (`incr_number` finds the first number at/after it, expanding to the whole number). This makes a
        // column of numbers that are NOT the first on their line increment correctly — the whole point of the
        // feature — for both `CTRL-V` (block left column) and `v` (the start column on the first line):
        //   - BLOCKWISE (`CTRL-V`): the first character each row touches (`block_geom`).
        //   - LINEWISE (`V`): the line start (whole lines).
        //   - CHARWISE (`v`): the selection start on the first line, the line start on continuation lines.
        // (Charwise does not yet CLIP a number sitting past the selection end on the last line — a rare edge,
        // noted as a follow-up.)
        Command::IncrementSelection { delta, sequential } => {
            let Some(anchor) = st.view.anchor else {
                return nop(cur, Mode::Normal);
            };
            // Per selected line: (line_start, search_from).
            let lines: Vec<(usize, usize)> = match st.view.mode.selection() {
                Some(SelectKind::Blockwise) => {
                    block_geom(b, anchor, cur, st.view.indent.tab_width, false)
                        .rows
                        .iter()
                        .map(|r| (r.ls, r.ds))
                        .collect()
                }
                kind => {
                    let linewise = kind == Some(SelectKind::Linewise);
                    let (s, e) = selection_range(b, anchor, cur, linewise);
                    let mut ls = line_start(b, s.min(b.len()));
                    let mut out = Vec::new();
                    // Charwise starts its FIRST line at the selection start `s`; every other line (and every
                    // linewise line) starts at its own line start.
                    let mut from = if linewise { ls } else { s.max(ls) };
                    while ls < e && ls <= b.len() {
                        out.push((ls, from));
                        let le = line_end(b, ls);
                        if le >= b.len() {
                            break;
                        }
                        ls = le + 1;
                        from = ls;
                    }
                    out
                }
            };
            let mut edits: Vec<Edit> = Vec::new();
            // Vim leaves the caret on the FIRST SELECTED line at the selection's LEFT-EDGE column — the
            // first line's search-`from` (linewise: column 0; charwise: the selection start; blockwise: the
            // block's left column). It is INDEPENDENT of which line changed (or whether any did) and of the
            // number's own last digit — so a multi-digit result (`007`→`008`) or a numberless first line
            // (`abc` above the numbers) still homes here. Verified against nvim v0.12.4 (parity fixtures).
            let caret = lines.first().map_or(cur, |&(_ls, from)| from);
            let mut steps: i64 = 0; // numbered lines seen so far (the sequence multiplier)
            for (ls, from) in lines {
                let le = line_end(b, ls);
                steps += 1;
                let this = if *sequential { delta * steps } else { *delta };
                if let Some((start, old_len, bytes, _cursor)) = incr_number(b, ls, le, from, this) {
                    edits.push(Edit::replace(BytePos(start), old_len, bytes));
                } else {
                    steps -= 1; // a line with no number does not advance the sequence
                }
            }
            if edits.is_empty() {
                return nop(caret, Mode::Normal);
            }
            let list = EditList::new(edits).expect("one replacement per line ⇒ disjoint");
            edit(list, caret, Mode::Normal, hint)
        }
        Command::GotoLastChange => {
            // `` `. `` — move to the last change position (snapped into range). No-op before any edit.
            match st.view.last_change() {
                Some(BytePos(pos)) => nop(motion::snap(b, pos), st.view.mode),
                None => beep(cur, st.view.mode),
            }
        }
        // `'.` — LINEWISE to the first non-blank of the last change's line. No-op before any edit.
        Command::GotoLastChangeLine => match st.view.last_change() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `[ `` — to the FIRST char of the last changed/yanked text. No-op before any change/yank.
        Command::GotoChangeMarkStart => match st.view.change_mark_start() {
            Some(BytePos(pos)) => nop(mark_char_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `] `` — to the LAST char of the last changed/yanked text (EOL-clamped). No-op if unset.
        Command::GotoChangeMarkEnd => match st.view.change_mark_end() {
            Some(BytePos(pos)) => nop(mark_char_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `'[` — LINEWISE to the first non-blank of the `[` mark's line. No-op if unset.
        Command::GotoChangeMarkStartLine => match st.view.change_mark_start() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `']` — LINEWISE to the first non-blank of the `]` mark's line. No-op if unset.
        Command::GotoChangeMarkEndLine => match st.view.change_mark_end() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `< `` — to the START of the last visual selection. Charwise/blockwise: the byte-min end (the
        // block's top corner is that end). Linewise: column 0 of the first selected line (Vim). No-op if
        // no visual selection has ended. Verified against nvim v0.12.4 (parity fixtures).
        Command::GotoVisualMarkStart => match st.view.last_visual_bounds() {
            Some((BytePos(s), _e, kind)) => {
                let pos = if kind == SelectKind::Linewise {
                    line_start(b, s)
                } else {
                    s
                };
                nop(mark_char_target(b, pos), st.view.mode)
            }
            None => beep(cur, st.view.mode),
        },
        // `` `> `` — to the END of the last visual selection. Charwise/blockwise: the byte-max end. Linewise:
        // the last char of the last selected line (Vim clamps its MAXCOL `>` mark onto the last char). No-op
        // if unset. Verified against nvim v0.12.4.
        Command::GotoVisualMarkEnd => match st.view.last_visual_bounds() {
            Some((_s, BytePos(e), kind)) => {
                let pos = if kind == SelectKind::Linewise {
                    line_end(b, e)
                } else {
                    e
                };
                nop(mark_char_target(b, pos), st.view.mode)
            }
            None => beep(cur, st.view.mode),
        },
        // `'<` — LINEWISE to the first non-blank of the FIRST selected line. No-op if unset.
        Command::GotoVisualMarkStartLine => match st.view.last_visual_bounds() {
            Some((BytePos(s), _e, _kind)) => nop(mark_line_target(b, s), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `'>` — LINEWISE to the first non-blank of the LAST selected line. No-op if unset.
        Command::GotoVisualMarkEndLine => match st.view.last_visual_bounds() {
            Some((_s, BytePos(e), _kind)) => nop(mark_line_target(b, e), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` `` `` — jump to the context mark (position before the latest jump). No-op before any jump. It is
        // itself a jump (`apply_command` records the leave position AFTER commit), so a repeat toggles.
        Command::GotoContextMark => match st.view.context_mark() {
            Some(BytePos(pos)) => nop(motion::snap(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `''` — LINEWISE to the first non-blank of the context mark's line. No-op before any jump.
        Command::GotoContextMarkLine => match st.view.context_mark() {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `g;`/`g,` — step the change list. The cursor here is a placeholder; `commit` steps `change_idx`
        // (a mutation the pure planner cannot make) and overrides it with the resolved change position.
        Command::GotoOlderChange => moved(Action::JumpChange { older: true }, cur, st.view.mode),
        Command::GotoNewerChange => moved(Action::JumpChange { older: false }, cur, st.view.mode),
        // `CTRL-O`/`CTRL-I` — step the jumplist. `commit` mutates it (the planner is pure); the placeholder
        // cursor is overridden with the resolved jump position.
        Command::GotoOlderJump => moved(Action::JumpList { older: true }, cur, st.view.mode),
        Command::GotoNewerJump => moved(Action::JumpList { older: false }, cur, st.view.mode),
        // `m{a-z}` — install a named mark at the cursor. The cursor stays; `commit` writes the mark table.
        Command::SetNamedMark(ch) => moved(Action::SetNamedMark { ch: *ch }, cur, st.view.mode),
        // `` `{a-z} `` — jump to a named mark (snapped). No-op if unset.
        // A mark past a line's last char (`` `^ `` after `A`) lands on that char, as Vim's `check_cursor`.
        Command::GotoNamedMark(ch) => match st.view.named_mark(*ch) {
            Some(BytePos(pos)) => nop(mark_char_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `'{a-z}` — LINEWISE to the first non-blank of a named mark's line. No-op if unset.
        Command::GotoNamedMarkLine(ch) => match st.view.named_mark(*ch) {
            Some(BytePos(pos)) => nop(mark_line_target(b, pos), st.view.mode),
            None => beep(cur, st.view.mode),
        },
        // `` d`{a-z} `` / `d'{a-z}` (and `c`/`y`): operate from the cursor to a named mark. `` ` `` is
        // exclusive charwise; `'` is linewise over the line range. No-op if the mark is unset.
        Command::OpToMark { op, name, linewise } => {
            let Some(BytePos(mark)) = st.view.named_mark(*name) else {
                return beep(cur, st.view.mode);
            };
            let mark = motion::snap(b, mark.min(b.len()));
            let (lo, hi) = (cur.min(mark), cur.max(mark));
            if *linewise {
                // `'a` lands on the first non-blank of the mark's line.
                let landing = mark_line_target(b, mark);
                return plan_op_over_span(st, b, cur, *op, lo, hi, true, false, landing, hint);
            }
            // `` `a `` is an exclusive charwise motion: Vim's exclusive-linewise rule (a mark at column 0 of
            // a later line pulls the end back, linewise from the indent, #710), then — for `d` — Vi's
            // delete quirk. The line operators keep the plain span and take only the adjusted flag.
            let sp = exclusive_op_span(b, lo, hi);
            if matches!(
                op,
                MarkOp::Shift { .. } | MarkOp::Reindent | MarkOp::Format { .. } | MarkOp::Comment
            ) {
                return plan_op_over_span(st, b, cur, *op, lo, hi, false, sp.adjusted, mark, hint);
            }
            let sp = if *op == MarkOp::Delete {
                delete_quirk(b, sp, false)
            } else {
                sp
            };
            apply_op(st, b, cur, *op, sp, mark, hint)
        }
        // `CTRL-G u`: break the undo group. A pure nop (is_edit = false), so `commit` sets
        // `last_was_edit = false` and the NEXT edit's `GroupHint` becomes `BreakBefore` — a fresh undo
        // group starts here mid-insert-session (Vim `i_CTRL-G_u`). Cursor and mode are untouched.
        Command::BreakUndo => nop(cur, st.view.mode),
        Command::Move(count, m) => {
            // A text object issued in a selection mode (`viw`, `vi(`) sets BOTH ends: anchor at the object's
            // start, cursor on its last char (inclusive selection). A bare motion only moves the cursor.
            if st.view.mode.selection().is_some() && crate::textobj::owns(*m) {
                // Word, sentence, bracket and quote objects follow Vim's Visual rules: on a one-char area
                // they select the object, on a wider one they EXTEND it (`viwiw`, `va(a(`, `visis`). They
                // turn a linewise selection charwise; bracket and sentence objects any selection.
                let anchor = st.view.anchor.unwrap_or(cur);
                let Some((a, c)) = crate::textobj::visual(b, anchor, cur, *m, *count) else {
                    return nop(cur, st.view.mode);
                };
                let to_charwise = st.view.mode.selection() == Some(SelectKind::Linewise)
                    || matches!(
                        m,
                        Motion::Pair { .. } | Motion::InnerSentence | Motion::ASentence
                    );
                let mode = match st.view.mode {
                    Mode::Select { .. } if to_charwise => Mode::Select {
                        kind: SelectKind::Charwise,
                    },
                    _ if to_charwise => Mode::Visual {
                        kind: SelectKind::Charwise,
                    },
                    mode => mode,
                };
                return Plan {
                    action: Action::Nop,
                    cursor: c,
                    mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: Some(a),
                    set_mark: None,
                };
            }
            if st.view.mode.selection().is_some() && is_text_object(*m) {
                let (s, e) = motion::char_span(b, cur, *m, *count);
                if s >= e {
                    return nop(cur, st.view.mode);
                }
                // `ip`/`ap` are LINEWISE objects: selecting one in Visual switches the selection to
                // linewise (Vim), so a following `d`/`y` gets linewise register geometry. Other objects
                // (`iw`, `i(`, `is`, …) keep the current visual kind.
                let obj_mode = if matches!(m, Motion::InnerParagraph | Motion::AParagraph) {
                    match st.view.mode {
                        Mode::Select { .. } => Mode::Select {
                            kind: SelectKind::Linewise,
                        },
                        _ => Mode::Visual {
                            kind: SelectKind::Linewise,
                        },
                    }
                } else {
                    st.view.mode
                };
                return Plan {
                    action: Action::Nop,
                    cursor: motion::prev_grapheme(b, e),
                    mode: obj_mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: Some(s),
                    set_mark: None,
                };
            }
            // Vertical motions honour the sticky desired column (curswant) so `j`/`k` keep the wanted
            // column through shorter interior lines; every other motion computes its own landing.
            let ts = st.view.indent.tab_width;
            let target = match m {
                Motion::Up => motion::vmove(b, cur, *count, false, st.view.curswant, ts),
                Motion::Down => motion::vmove(b, cur, *count, true, st.view.curswant, ts),
                // `gg`/`G`/`{count}%` keep the wanted column too (`'nostartofline'`, nvim's
                // `beginline(BL_SOL)`).
                Motion::GotoLine | Motion::LastLine | Motion::GotoPercent => want_col(
                    b,
                    line_start(b, motion::target(b, cur, *m, *count)),
                    st.view.curswant,
                    ts,
                ),
                // In charwise Visual `$` goes just past the line's last char, so the selection takes the
                // line break (`v$d` joins the lines, #697). In blockwise Visual too (nvim's `coladvance`
                // puts a Visual `$` one past the end): a motion from there starts past the last char, so
                // `<C-v>$h` rests ON it and the block still ends there, and `ge` finds that word (#1207).
                Motion::LineEnd
                    if matches!(
                        st.view.mode,
                        Mode::Visual {
                            kind: SelectKind::Charwise | SelectKind::Blockwise
                        } | Mode::Select {
                            kind: SelectKind::Charwise | SelectKind::Blockwise
                        }
                    ) =>
                {
                    line_end(b, motion::target(b, cur, *m, *count))
                }
                _ => motion::target(b, cur, *m, *count),
            };
            // Normal mode never rests on a non-empty line's trailing newline, whatever the motion: `j`/`k`
            // overshooting a short line (or `$`'s MAXCOL), `w`/`W` past the last word, `l`, `go` onto a
            // newline byte and `^` on an all-blank line (Vim's `BL_FIX`) all pull back onto the last char.
            // The phantom slot after the final newline is handled for every mode in `commit` (#677).
            // Visual may reach the newline (`vl` at the end selects it, as nvim does), Insert rests after the
            // last char, and the Emacs profile (between-char gravity) is never clamped.
            let target =
                if matches!(st.view.mode, Mode::Normal) && st.view.caret == CaretGravity::OnChar {
                    let ls = line_start(b, target);
                    let le = line_end(b, target);
                    if target == le && le > ls {
                        prev_grapheme(b, le)
                    } else {
                        target
                    }
                } else {
                    target
                };
            // `{count}e` past the last word moves to the end and still fails (nvim's `end_word`).
            if matches!(m, Motion::WordEnd | Motion::BigWordEnd)
                && st.view.caret == CaretGravity::OnChar
                && matches!(st.view.mode, Mode::Normal | Mode::Visual { .. })
                && motion::fails(
                    b,
                    cur,
                    *m,
                    *count,
                    false,
                    st.view.mode.selection().is_some(),
                )
            {
                return beep(target, st.view.mode);
            }
            nop(target, st.view.mode)
        }
        Command::Delete(count, m) => {
            // Vi's "d" quirk (nvim `op_delete`): a charwise delete over several lines that starts in the
            // indent and leaves only blanks after its end goes linewise (`daw` on an empty line, `das` on
            // a whole-line `dat` tag block, `2d$` over whole lines, #747). Visual deletes never take it.
            let sp = delete_quirk(b, op_span_full(b, cur, *m, *count), is_text_object(*m));
            if sp.s >= sp.e && crate::textobj::owns(*m) {
                // An object that selects nothing (`di(` on `(\n)`) still leaves the cursor at its start.
                return nop(sp.s, st.view.mode);
            }
            let landing = op_landing(
                b,
                cur,
                *m,
                *count,
                st.view.curswant,
                st.view.indent.tab_width,
                false,
            );
            apply_op(st, b, cur, MarkOp::Delete, sp, landing, hint)
        }
        Command::Change(count, m) => plan_change(st, b, cur, *count, m, hint),
        Command::Yank(count, m) => {
            let sp = op_span_full(b, cur, *m, *count);
            let landing = op_landing(
                b,
                cur,
                *m,
                *count,
                st.view.curswant,
                st.view.indent.tab_width,
                true,
            );
            apply_op(st, b, cur, MarkOp::Yank, sp, landing, hint)
        }
        Command::CaseMotion {
            count,
            motion,
            case,
        } => {
            let sp = op_span_full(b, cur, *motion, *count);
            let landing = op_landing(
                b,
                cur,
                *motion,
                *count,
                st.view.curswant,
                st.view.indent.tab_width,
                false,
            );
            apply_op(st, b, cur, MarkOp::Case(*case), sp, landing, hint)
        }
        // Forced-wise operator (`dvj`, `dVe`, `yv}`): reshape the span (see `forced_span`), then apply the
        // operator like its plain form.
        Command::OpForced {
            op,
            count,
            motion,
            wise,
        } => {
            // Forced blockwise (`d<C-v>j`): the cursor and the motion target are the block's two corners.
            if *wise == ForcedWise::Blockwise {
                let target = motion::target(b, cur, *motion, *count);
                let block = block_geom(b, cur, target, st.view.indent.tab_width, false);
                return block_op(b, &block, *op, hint);
            }
            let sp = forced_span(
                b,
                cur,
                *motion,
                *count,
                *wise,
                st.view.curswant,
                st.view.indent.tab_width,
            );
            if sp.s >= sp.e && *op == OpKind::Change && !sp.linewise {
                // An empty charwise change still drops into Insert (Vim).
                return nop(sp.s, Mode::Insert);
            }
            let landing = op_landing(
                b,
                cur,
                *motion,
                *count,
                st.view.curswant,
                st.view.indent.tab_width,
                *op == OpKind::Yank,
            );
            apply_op(st, b, cur, mark_op(*op), sp, landing, hint)
        }
        // `{count}>>` moves `count - 1` lines down first (nvim `nv_lineop` → `cursor_down`), so on the
        // last line a count above 1 fails like `3dd` there, shifting nothing (#1159); a count past the
        // end from a line above still shifts the lines that exist.
        Command::ShiftRight(count) | Command::ShiftLeft(count)
            if *count > 1
                && st.view.caret == CaretGravity::OnChar
                && st.view.mode == Mode::Normal
                && line_end(b, cur) >= crate::motion::content_end(b) =>
        {
            beep(cur, st.view.mode)
        }
        Command::ShiftRight(count) => plan_shift(st, cur, *count, true, hint),
        Command::ShiftLeft(count) => plan_shift(st, cur, *count, false, hint),
        Command::ShiftMotion {
            left,
            count,
            motion,
        } => {
            // Resolve the motion's byte span, then shift every LINE it touches (Vim `>` is always linewise).
            let (s, e, _) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let first_line = crate::pos::raw::line_of(b, s);
                let last_line = crate::pos::raw::line_of(b, e - 1);
                let lines = (last_line - first_line + 1) as u32;
                plan_shift(st, line_start(b, s), lines, !*left, hint)
            }
        }
        Command::Reindent { count, motion } => {
            // `=` over a motion — reindent every LINE it touches (always linewise, like `>`).
            let (s, e, _) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let first = crate::pos::raw::line_of(b, s);
                let last = crate::pos::raw::line_of(b, e - 1);
                plan_reindent(st, first, last, hint)
            }
        }
        Command::Format {
            count,
            motion,
            keep_cursor,
        } => plan_format(st, cur, *count, *motion, *keep_cursor, hint),
        Command::Comment { count, motion } => {
            // `gc` over a motion: every LINE it touches; the cursor goes to the start of the range
            // (`g@`'s `'[`), keeping its column over a linewise motion.
            let (s, e, linewise) = op_span(b, cur, *motion, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let landing = op_landing(
                    b,
                    cur,
                    *motion,
                    *count,
                    st.view.curswant,
                    st.view.indent.tab_width,
                    true,
                );
                let at = if linewise { cur.min(landing) } else { s };
                let first = crate::pos::raw::line_of(b, s);
                let last = crate::pos::raw::line_of(b, e - 1);
                plan_comment(st, first, last, at, hint)
            }
        }
        Command::SetIndents {
            first_line,
            last_line,
            levels,
        } => {
            let sw = st.view.indent.shift_width();
            let cols: Vec<Option<usize>> = levels.iter().map(|l| Some(l * sw)).collect();
            plan_set_indents(st, *first_line, *last_line, &cols, hint)
        }
        Command::SetIndentCols {
            first_line,
            last_line,
            cols,
        } => plan_set_indents(st, *first_line, *last_line, cols, hint),
        // Resolved by the frontend from the buffer's indenter; unresolved it does nothing (nvim without one).
        Command::InsertReindent => nop(cur, st.view.mode),
        // Paste reads the pending register (`"xp`) or the unnamed slot; `commit` clears the pending slot.
        Command::Paste {
            after,
            count,
            move_after,
        } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
            *move_after,
            true,
            st.view.indent.tab_width,
        ),
        // `zp` / `zP` — `p` / `P`, but a block's short rows get no trailing spaces (nvim `PUT_BLOCK_INNER`).
        Command::PasteBlockInner { after, count } => paste(
            b,
            cur,
            st.view.mode,
            st.view.registers.get(st.view.pending_register),
            *after,
            *count,
            st.view.caret,
            false,
            false,
            st.view.indent.tab_width,
        ),
        // `]p`/`[p` — indent-adjusting paste. For a linewise register, reindent its lines to the current
        // line's indent before pasting; a charwise/blockwise register pastes unchanged (Vim `]p` = `p`).
        Command::PasteIndent { after, count } => {
            let reg = st.view.registers.get(st.view.pending_register);
            if reg.is_linewise() {
                let (target_cols, _) = indent_cols(
                    &b[line_start(b, cur)..line_end(b, cur)],
                    st.view.indent.tab_width,
                );
                let adjusted = reindent_register(
                    reg.text(),
                    target_cols,
                    st.view.indent.tab_width,
                    st.view.indent.style,
                );
                let tmp = Register::linewise(adjusted);
                paste(
                    b,
                    cur,
                    st.view.mode,
                    &tmp,
                    *after,
                    *count,
                    st.view.caret,
                    false,
                    true,
                    st.view.indent.tab_width,
                )
            } else {
                paste(
                    b,
                    cur,
                    st.view.mode,
                    reg,
                    *after,
                    *count,
                    st.view.caret,
                    false,
                    true,
                    st.view.indent.tab_width,
                )
            }
        }
        // `"x` — install the one-shot pending register. A pure state set: no edit, no cursor/mode change.
        Command::SetRegister(name) => Plan {
            action: Action::SetPending(*name),
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        // `"=<expr><CR>` — evaluate the expression, store the formatted result in the `"=` slot, and arm `=`
        // as the pending register so the following `p`/`P` pastes it. An empty result (malformed/unsupported
        // expression) makes that paste a no-op (Vim's degrade). Like `SetRegister`, a pure state set.
        Command::SetExprRegister(e) => Plan {
            action: Action::SetExprPending(crate::expr::eval_or_empty(e)),
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::EnterVisual { kind } => nop(cur, Mode::Visual { kind: *kind }),
        // CTRL-G toggle: enter Select over the same selection. The anchor is preserved because both are
        // selection modes, so `commit` keeps it (see the (true, true) arm there). Outside a selection it is
        // inert: `gh` enters Visual first, and the `v_CTRL-O` return lands after a command that may have
        // ended the selection (nvim returns to Select only while Visual is still active).
        Command::EnterSelect { kind } if st.view.mode.selection().is_some() => {
            nop(cur, Mode::Select { kind: *kind })
        }
        Command::EnterSelect { .. } => nop(cur, st.view.mode),
        Command::ReselectVisual => match st.view.last_visual {
            // Restore the remembered selection: re-enter Visual with the stored kind, put the cursor on the
            // active end, and install the stored anchor (via `set_anchor`, which `commit` applies after its
            // enter-selection bookkeeping). No prior selection → a clean no-op (Vim rings the bell).
            Some((anchor, active, kind)) => Plan {
                action: Action::Nop,
                cursor: active,
                mode: Mode::Visual { kind },
                is_edit: false,
                effects: Vec::new(),
                set_register: None,
                set_anchor: Some(anchor),
                set_mark: None,
            },
            None => nop(cur, st.view.mode),
        },
        Command::ReplaceSelection(c) => {
            // Select's `open/replace-selection`: delete the selection, insert the char, enter Insert.
            let mut buf = [0u8; 4];
            // A block is a block `c` whose first typed char is this one, so `<Esc>` replicates the whole run
            // down the block (nvim: `g CTRL-H` + `<S-Down>` + `X` leaves `X` on both rows).
            if st.view.mode.selection() == Some(SelectKind::Blockwise) && st.view.anchor.is_some() {
                let seed = c.encode_utf8(&mut buf).as_bytes();
                return plan_block_insert(st, b, cur, hint, &BlockInsertKind::Change, seed);
            }
            let ins = c.encode_utf8(&mut buf).as_bytes().to_vec();
            replace_selection_with(st, b, cur, hint, ins)
        }
        Command::BlockInsert(kind) => plan_block_insert(st, b, cur, hint, kind, &[]),
        Command::SwapSelectionEnds => {
            // Visual/Select `o`: exchange the two ends. The cursor jumps to the anchor; the anchor becomes
            // the old cursor (`set_anchor`, which `commit` installs). The SAME text stays selected, but a
            // later bare motion now extends the OTHER end (it re-plans against the new anchor). Involutive.
            // Outside a selection (no anchor) it is a clean no-op.
            match (st.view.mode.selection(), st.view.anchor) {
                (Some(_), Some(anchor)) => Plan {
                    set_anchor: Some(cur),
                    ..nop(anchor, st.view.mode)
                },
                _ => nop(cur, st.view.mode),
            }
        }
        Command::YankSelection | Command::DeleteSelection | Command::ChangeSelection => {
            let Some(anchor) = st.view.anchor else {
                // Not in a selection (or no anchor) — drop back to Normal, do nothing.
                return nop(cur, Mode::Normal);
            };
            // Blockwise (`CTRL-V`) selections operate on a rectangle of per-row slices, not one span.
            if st.view.mode.selection() == Some(SelectKind::Blockwise) {
                let op = match cmd {
                    Command::YankSelection => OpKind::Yank,
                    Command::ChangeSelection => OpKind::Change,
                    _ => OpKind::Delete,
                };
                let ts = st.view.indent.tab_width;
                let block = block_geom(b, anchor, cur, ts, st.view.curswant == MAXCOL);
                return block_op(b, &block, op, hint);
            }
            let line = st.view.mode.selection() == Some(SelectKind::Linewise);
            let op = match cmd {
                Command::YankSelection => OpKind::Yank,
                Command::ChangeSelection => OpKind::Change,
                _ => OpKind::Delete,
            };
            plan_selection_span(st, b, anchor, cur, op, line, hint)
        }
        Command::VisualOp(op) => plan_visual_op(st, b, cur, *op, hint),
        // Visual `u`/`U`/`~`/`g?` — recase the selection in place, cursor at its start, back to Normal. A
        // block recases each row's characters wholly inside it (Vim `op_tilde` over `block_prep`).
        Command::CaseSelection(case) => {
            if let Some(block) = selection_block(st, b, cur) {
                let edits: Vec<Edit> = block
                    .rows
                    .iter()
                    .filter_map(|r| recase_span(b, r.s, r.e, *case, recase).map(|new| (r, new)))
                    .filter(|(r, new)| new[..] != b[r.s..r.e])
                    .map(|(r, new)| Edit::replace(BytePos(r.s), r.e - r.s, new))
                    .collect();
                let at = block.rows[0].ds;
                return match EditList::new(edits) {
                    Ok(list) if !list.is_empty() => edit(list, at, Mode::Normal, hint),
                    _ => nop(at, Mode::Normal),
                };
            }
            let (s, e, _line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            match std::str::from_utf8(&b[s..e]) {
                Ok(text) => {
                    let new = recase(text, *case).into_bytes();
                    edit(
                        one(Edit::replace(BytePos(s), e - s, new)),
                        s,
                        Mode::Normal,
                        hint,
                    )
                }
                Err(_) => nop(s, Mode::Normal),
            }
        }
        // Visual `r{char}` — overwrite every non-newline char in the selection with `c`, keeping line
        // breaks and the byte length's char count; cursor to the span start, back to Normal. A block fills
        // each row's cells with `c` (Vim `op_replace`), so a `<Tab>` or wide char inside becomes several.
        Command::ReplaceSelectionChar(c) => {
            if let Some(block) = selection_block(st, b, cur) {
                return block_replace(&block, *c, hint);
            }
            let (s, e, _line) = match visual_span(st, b, cur) {
                Ok(v) => v,
                Err(c) => return nop(c, Mode::Normal),
            };
            match std::str::from_utf8(&b[s..e]) {
                Ok(text) => {
                    let new: String = text
                        .chars()
                        .map(|ch| if ch == '\n' { '\n' } else { *c })
                        .collect();
                    edit(
                        one(Edit::replace(BytePos(s), e - s, new.into_bytes())),
                        s,
                        Mode::Normal,
                        hint,
                    )
                }
                Err(_) => nop(s, Mode::Normal),
            }
        }
        // Visual `p`/`P` — replace the selection with the register; `swap` puts the deleted text into the
        // unnamed register (Vim `p`), `P` preserves the register (so it can overwrite successive selections).
        Command::PasteSelection { swap, count } => plan_paste_selection(st, b, cur, *swap, *count),
        Command::SearchNext(pat) => {
            let m = search_fwd(b, pat, cur + 1, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        Command::SearchPrev(pat) => {
            let m = search_bwd(b, pat, cur, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        // `/pat` / `?pat` as a motion: step to the `count`-th match in the search direction (each step
        // resumes from just past/before the last), then either move there (`Move`) or fold the exclusive
        // span into an edit (`d/pat`, `c?pat`, `y?pat`). No match on the operative side aborts (a clean
        // no-op, never a reversed/empty edit). See `plan_search` for the direction geometry.
        Command::Search {
            op,
            count,
            pattern,
            backward,
            offset,
        } => plan_search(st, b, cur, op, *count, pattern, *backward, *offset, hint),
        Command::SearchObject {
            op,
            count,
            pattern,
            backward,
        } => plan_search_object(st, b, cur, op, *count, pattern, *backward, hint),
        // `*`/`#`/`g*`/`g#`: a charwise search for the keyword under the cursor, run from the keyword's
        // start, operator-aware like `/pat` (`d*`, #1071). No keyword on the line → no-op (E348).
        Command::SearchWordUnder {
            forward,
            whole_word,
            op,
            count,
        } => match word_search(b, cur, *whole_word) {
            Some((origin, pattern)) => plan_search_char(
                st,
                b,
                cur,
                origin,
                op,
                *count,
                &pattern,
                !*forward,
                SearchOffset::None,
                hint,
            ),
            None => nop(cur, st.view.mode),
        },
        // `gd`/`gD` are resolved by the frontend (it reads the keyword under the cursor from the buffer and
        // rewrites this to a concrete `GotoFirstMatch`), so the pure core never acts on it directly.
        Command::GotoDeclaration { .. } => nop(cur, st.view.mode),
        // `[i`/`]i`/`[I`/`]I` are resolved by the frontend (it reads the keyword under the cursor and scans
        // the buffer lines, echoing or listing the matches); a pure display command — never mutates.
        Command::ShowKeywordLines { .. } => nop(cur, st.view.mode),
        // The resolved `gd`/`gD` jump: land on the FIRST whole-word match scanning from the TOP of the file
        // (offset 0 — matching nvim v0.12.4's `gd`/`gD`), else keep the cursor. `is_jump` records the leave.
        Command::GotoFirstMatch(pat) => {
            let m = search_fwd(b, pat, 0, st.view.search_options()).unwrap_or(cur);
            nop(m, st.view.mode)
        }
        // `ga`/`:ascii`, `CTRL-G`, `g CTRL-G` — INFO commands resolved by the frontend (it reads the buffer
        // bytes + cursor and sets the status line); the pure core never mutates for them.
        Command::AsciiInfo | Command::Utf8Info | Command::FileInfo | Command::CursorInfo => {
            nop(cur, st.view.mode)
        }
        // `g&` — resolved in the frontend against its last-substitute state; a no-op in the pure core.
        Command::RepeatSubstituteGlobal => nop(cur, st.view.mode),
        // `&` — resolved in the frontend (current-line, flagless repeat); a no-op in the pure core.
        Command::RepeatSubstituteLine => nop(cur, st.view.mode),
        // A resolved Ex edit never reaches the planner: `apply_command` runs it through
        // `EditorState::apply_ex` (RFC-0020). Planned alone it changes nothing.
        Command::Ex(_) => nop(cur, st.view.mode),
        // Nothing to undo or redo: Vim's `u_doit` beeps ("Already at oldest change"), which stops a macro
        // replay or a `:normal` (#1111).
        Command::Undo if !st.doc.can_undo() => beep(cur, st.view.mode),
        Command::Redo if !st.doc.can_redo() => beep(cur, st.view.mode),
        Command::Undo => Plan {
            action: Action::Undo,
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::Redo => Plan {
            action: Action::Redo,
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::UndoOlder => Plan {
            action: Action::UndoChrono { older: true },
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        // `U` — a change in its own right (`u` undoes it), so it is an edit; nothing to restore is a no-op.
        Command::LineUndo if st.doc.can_line_undo() => Plan {
            action: Action::LineUndo,
            cursor: cur,
            mode: Mode::Normal,
            is_edit: true,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::LineUndo => nop(cur, st.view.mode),
        Command::UndoNewer => Plan {
            action: Action::UndoChrono { older: false },
            cursor: cur,
            mode: undo_resume_mode,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::Save => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: vec![Effect::Save],
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        Command::Quit => Plan {
            action: Action::Nop,
            cursor: cur,
            mode: st.view.mode,
            is_edit: false,
            effects: vec![Effect::Quit],
            set_register: None,
            set_anchor: None,
            set_mark: None,
        },
        // Emacs-profile commands: the depth-1 region ops (D-027) and the distinct Emacs commands
        // (D-051), grouped into `plan_emacs` to keep this match readable.
        Command::SetMark
        | Command::ExchangePointMark
        | Command::CopyRegion
        | Command::KillRegion
        | Command::EmacsYank { .. }
        | Command::EmacsKillLine
        | Command::EmacsKillWord { .. }
        | Command::EmacsBackwardKillWord { .. }
        | Command::EmacsTransposeChars
        | Command::EmacsTransposeWords
        | Command::EmacsCaseWord { .. }
        | Command::EmacsHorizontalSpace { .. }
        | Command::EmacsOpenLine
        | Command::EmacsMarkWord
        | Command::EmacsKillWholeLine
        | Command::EmacsCaseRegion { .. }
        | Command::EmacsDeleteIndentation
        | Command::EmacsBufferEdge { .. } => plan_emacs(st, b, cur, hint, cmd),
    }
}

/// Plan the Emacs-profile commands split out of [`plan`]: the depth-1 region ops (D-027 —
/// SetMark / KillRegion / CopyRegion / ExchangePointMark) and the distinct Emacs commands (D-051).
/// The core stays profile-agnostic; the profile decides which of these its keymap resolves to.
fn plan_emacs(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
        // Emacs region (D-027 depth-1). `C-SPC` drops the mark at point.
        Command::SetMark => nop_mark(cur, st.view.mode, cur),
        // `C-x C-x` swaps point and mark; the mark takes the old point. No mark set → inert.
        Command::ExchangePointMark => match st.view.mark {
            Some(m) => nop_mark(m, st.view.mode, cur),
            None => nop(cur, st.view.mode),
        },
        // `M-w` copies the region `[min,max)` charwise into the register; point and mark are untouched. An
        // empty region or no mark is inert.
        Command::CopyRegion => match active_region(st, cur) {
            Some((s, e)) => {
                let reg = captured(b, s, e, false);
                Plan {
                    action: Action::Nop,
                    cursor: cur,
                    mode: st.view.mode,
                    is_edit: false,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::Yank(reg, (s, e))),
                    set_anchor: None,
                    set_mark: None,
                }
            }
            None => nop(cur, st.view.mode),
        },
        // `C-w` kills the region into the register (the kill ring) and leaves point and mark together at its
        // start — Emacs keeps the mark at the region's lower bound `s` (where point also lands after the
        // deletion collapses the span), it does NOT clear it. An empty region or no mark is inert.
        Command::KillRegion => match active_region(st, cur) {
            Some((s, e)) => {
                let reg = captured(b, s, e, false);
                Plan {
                    action: Action::Txn {
                        edits: one(Edit::delete(BytePos(s), e - s)),
                        hint,
                    },
                    cursor: s,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    // A kill: accumulates onto the current unnamed entry when it follows another kill.
                    set_register: Some(RegWrite::KillAppend(reg)),
                    set_anchor: None,
                    set_mark: Some(MarkWrite::Set(s)),
                }
            }
            None => nop(cur, st.view.mode),
        },
        // `C-y` (Emacs yank, D-051): the same gravity-aware charwise paste as `Paste{after:false}`, plus it
        // SETS the mark at the insertion start (point before the paste). An empty register is inert (no mark
        // write either — `paste` returns a Nop).
        Command::EmacsYank { count } => {
            let mut p = paste(
                b,
                cur,
                st.view.mode,
                st.view.registers.get(st.view.pending_register),
                false,
                *count,
                st.view.caret,
                false, // Emacs yank is not `gp` — normal cursor placement
                true,
                st.view.indent.tab_width,
            );
            if !matches!(p.action, Action::Nop) {
                p.set_mark = Some(MarkWrite::Set(cur));
            }
            p
        }
        // `C-k` (Emacs kill-line, D-051): kill from point into the register. With text before the line end,
        // kill that text (NOT the newline). With point already at the line end, kill the terminating newline
        // instead — joining the next line up. At end-of-buffer (no newline to take) it is inert. This is the
        // no-prefix `kill-whole-line`-nil default; unlike Vim `D` (`Delete(1, LineEnd)`) it never no-ops at EOL.
        Command::EmacsKillLine => {
            let le = line_end(b, cur);
            if cur < le {
                let reg = captured(b, cur, le, false);
                edit_kill(
                    one(Edit::delete(BytePos(cur), le - cur)),
                    cur,
                    st.view.mode,
                    hint,
                    reg,
                )
            } else if le < b.len() {
                // Point at EOL: kill the single newline byte, joining the following line.
                let reg = captured(b, le, le + 1, false);
                edit_kill(
                    one(Edit::delete(BytePos(le), 1)),
                    le,
                    st.view.mode,
                    hint,
                    reg,
                )
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `M-d` / `kill-word` (Emacs kill-word, D-051): kill the `EmacsWordFwd` span (the word only, not Vim
        // `dw`'s trailing space) into the register, accumulating like every Emacs kill. Distinct from Vim
        // `Delete(count, EmacsWordFwd)` ONLY in that accumulation — but that difference is enough to warrant
        // its own command (D-051), so Vim deletes never accumulate.
        Command::EmacsKillWord { count } => {
            let (s, e, _) = op_span(b, cur, Motion::EmacsWordFwd, *count);
            if s >= e {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, s, e, false);
                edit_kill(
                    one(Edit::delete(BytePos(s), e - s)),
                    s,
                    st.view.mode,
                    hint,
                    reg,
                )
            }
        }
        // `M-DEL` (Emacs backward-kill-word, D-051): kill `count` words backward (the `WordBack` span) into
        // the register. A BACKWARD kill, so on a kill run it PREPENDS onto the current entry (via
        // `RegWrite::KillPrepend`). Distinct from Vim `db` (no accumulation).
        Command::EmacsBackwardKillWord { count } => {
            let target = motion::target(b, cur, Motion::EmacsWordBack, *count);
            if target >= cur {
                nop(cur, st.view.mode)
            } else {
                let reg = captured(b, target, cur, false);
                Plan {
                    action: Action::Txn {
                        edits: one(Edit::delete(BytePos(target), cur - target)),
                        hint,
                    },
                    cursor: target,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: Some(RegWrite::KillPrepend(reg)),
                    set_anchor: None,
                    set_mark: None,
                }
            }
        }
        // `C-t` (Emacs transpose-chars, D-051): swap the char before point with the char at point, then
        // advance point past the pair. `j` is the right char (it slides left), `i = j - 1` the left char;
        // at end of line Emacs steps back one so the two chars ENDING the line are transposed. Inert when the
        // line has no pair to transpose (point at buffer/line start, or a one-char line). No kill-ring write.
        Command::EmacsTransposeChars => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let j = if cur >= le {
                le.checked_sub(1).filter(|_| le >= ls + 2)
            } else if cur > ls {
                Some(cur)
            } else {
                None
            };
            match j {
                Some(j) if j > ls && j < le => Plan {
                    action: Action::Txn {
                        edits: one(Edit::replace(BytePos(j - 1), 2, vec![b[j], b[j - 1]])),
                        hint,
                    },
                    cursor: j + 1,
                    mode: st.view.mode,
                    is_edit: true,
                    effects: Vec::new(),
                    set_register: None,
                    set_anchor: None,
                    set_mark: None,
                },
                _ => nop(cur, st.view.mode),
            }
        }
        // `M-t` (Emacs transpose-words, D-051): swap the word before/around point with the following word,
        // preserving the separator, and leave point past the moved second word. Mirrors Emacs `transpose-subr`
        // for `forward-word`: `start1` = backward-word, `end1` = forward-word from there, `end2` =
        // forward-word again, `start2` = backward-word from there. Inert without a real pair. No kill write.
        Command::EmacsTransposeWords => {
            let start1 = motion::target(b, cur, Motion::EmacsWordBack, 1);
            let end1 = motion::target(b, start1, Motion::EmacsWordFwd, 1);
            let end2 = motion::target(b, end1, Motion::EmacsWordFwd, 1);
            let start2 = motion::target(b, end2, Motion::EmacsWordBack, 1);
            if start1 < end1 && end1 <= start2 && start2 < end2 {
                let mut new = Vec::with_capacity(end2 - start1);
                new.extend_from_slice(&b[start2..end2]); // second word moves to the front
                new.extend_from_slice(&b[end1..start2]); // the original separator
                new.extend_from_slice(&b[start1..end1]); // first word moves to the back
                edit(
                    one(Edit::replace(BytePos(start1), end2 - start1, new)),
                    end2,
                    st.view.mode,
                    hint,
                )
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `M-u`/`M-l`/`M-c` (Emacs upcase-/downcase-/capitalize-word, D-051): recase the `forward-word` span
        // (point to the end of the next word) and leave point at that end. Inert with no word ahead; no
        // kill-ring write. The span sits on grapheme boundaries (EmacsWordFwd lands on one), so it is valid
        // UTF-8 to recase; point lands at `cur + new_len` in case the recased bytes changed length.
        Command::EmacsCaseWord { case } => {
            let end = motion::target(b, cur, Motion::EmacsWordFwd, 1);
            match recase_span(b, cur, end, *case, recase_emacs) {
                Some(recased) => {
                    let cursor = cur + recased.len();
                    Plan {
                        action: Action::Txn {
                            edits: one(Edit::replace(BytePos(cur), end - cur, recased)),
                            hint,
                        },
                        cursor,
                        mode: st.view.mode,
                        is_edit: true,
                        effects: Vec::new(),
                        set_register: None,
                        set_anchor: None,
                        set_mark: None,
                    }
                }
                None => nop(cur, st.view.mode),
            }
        }
        // `M-SPC` (just-one-space) / `M-\` (delete-horizontal-space, D-051): collapse the run of spaces/tabs
        // around point (never crossing a newline). `keep_one` leaves exactly one space with point after it
        // (inserting one when there was none); otherwise it deletes them all. No kill-ring write.
        Command::EmacsHorizontalSpace { keep_one } => {
            let (s, e) = (hspace_start(b, cur), hspace_end(b, cur));
            if *keep_one {
                // Already exactly a single space? Leave the buffer, rest point after it (a pure move).
                if e - s == 1 && b[s] == b' ' {
                    nop(s + 1, st.view.mode)
                } else {
                    edit(
                        one(Edit::replace(BytePos(s), e - s, b" ".to_vec())),
                        s + 1,
                        st.view.mode,
                        hint,
                    )
                }
            } else if e > s {
                edit(one(Edit::delete(BytePos(s), e - s)), s, st.view.mode, hint)
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `C-o` (Emacs open-line, D-051): insert a newline at point but leave point BEFORE it (opens a blank
        // line below without moving onto it). Unlike Vim `o`, no mode change and no register write.
        Command::EmacsOpenLine => edit(
            one(Edit::insert(BytePos(cur), b"\n".to_vec())),
            cur,
            st.view.mode,
            hint,
        ),
        // `M-@` (Emacs mark-word, D-051): set the mark at the end of the next word (`forward-word`) without
        // moving point, activating the region point→word-end. No edit, no kill-ring write.
        Command::EmacsMarkWord => nop_mark(
            cur,
            st.view.mode,
            motion::target(b, cur, Motion::EmacsWordFwd, 1),
        ),
        // `C-S-DEL` (Emacs kill-whole-line, D-051): kill the whole line INCLUDING its trailing newline,
        // regardless of point column, into the register (accumulating), leaving point at the line start.
        Command::EmacsKillWholeLine => {
            let ls = line_start(b, cur);
            let le = line_end(b, cur);
            let e = if le < b.len() { le + 1 } else { le }; // include the terminating newline when present
            if e > ls {
                let reg = captured(b, ls, e, false);
                edit_kill(
                    one(Edit::delete(BytePos(ls), e - ls)),
                    ls,
                    st.view.mode,
                    hint,
                    reg,
                )
            } else {
                nop(cur, st.view.mode)
            }
        }
        // `C-x C-u` / `C-x C-l` (Emacs upcase-/downcase-/capitalize-region, D-051): recase the active region
        // `[min(point,mark), max)`. When the recasing changes the length (`ß` → `SS`), point at the region
        // end moves with the text (casefiddle.c adjusts it), while the mark keeps its CHARACTER offset
        // (`replace_range_2` leaves markers alone): Emacs 31.1 upcases "straße" with point at 0 and the
        // mark at 6 to "STRASSE" with the mark still at 6. Inert without a mark. No kill write.
        Command::EmacsCaseRegion { case } => match active_region(st, cur) {
            Some((s, e)) => match recase_span(b, s, e, *case, recase_emacs) {
                Some(recased) => {
                    let new_end = s + recased.len();
                    let follow = |p: usize| if p >= e { p - e + new_end } else { p };
                    // The byte offset in the recased buffer of the character the mark's offset counts to.
                    let keep_chars = |m: usize| {
                        if m <= s {
                            return m;
                        }
                        let n = String::from_utf8_lossy(&b[s..m]).chars().count();
                        let new = String::from_utf8_lossy(&recased);
                        match new.char_indices().nth(n) {
                            Some((i, _)) => s + i,
                            None => {
                                let past = n - new.chars().count();
                                let tail = String::from_utf8_lossy(&b[e..]);
                                new_end
                                    + tail.char_indices().nth(past).map_or(tail.len(), |(i, _)| i)
                            }
                        }
                    };
                    let set_mark = st
                        .view
                        .mark
                        .map(keep_chars)
                        .filter(|&m| Some(m) != st.view.mark)
                        .map(MarkWrite::Set);
                    Plan {
                        action: Action::Txn {
                            edits: one(Edit::replace(BytePos(s), e - s, recased)),
                            hint,
                        },
                        cursor: follow(cur),
                        mode: st.view.mode,
                        is_edit: true,
                        effects: Vec::new(),
                        set_register: None,
                        set_anchor: None,
                        set_mark,
                    }
                }
                None => nop(cur, st.view.mode),
            },
            None => nop(cur, st.view.mode),
        },
        // `M-^` (Emacs delete-indentation / join-line, D-051): join this line to the previous one — delete
        // the preceding newline plus the previous line's trailing whitespace and this line's leading
        // indentation, then fix up whitespace to ONE space, or NONE when the join lands at beginning-of-line
        // (an empty previous line). Point rests at the join. No kill-ring write. Distinct from Vim `J`.
        Command::EmacsDeleteIndentation => {
            let ls = line_start(b, cur);
            if ls == 0 {
                nop(cur, st.view.mode) // no previous line to join to
            } else {
                // Eat the previous line's trailing whitespace (back from the newline) and this line's
                // leading indentation (forward from its start).
                let s = hspace_start(b, ls - 1);
                let e = hspace_end(b, ls);
                // fixup-whitespace: one space, unless the join sits at the start of its line (empty prev).
                let repl: Vec<u8> = if s == line_start(b, s) {
                    Vec::new()
                } else {
                    vec![b' ']
                };
                edit(
                    one(Edit::replace(BytePos(s), e - s, repl)),
                    s,
                    st.view.mode,
                    hint,
                )
            }
        }
        // `M-<` / `M->` (Emacs beginning/end-of-buffer, D-051): move point to the ABSOLUTE buffer start/end
        // (not Vim `gg`/`G`'s first-non-blank line) and PUSH the mark at the old point.
        Command::EmacsBufferEdge { start } => {
            nop_mark(if *start { 0 } else { b.len() }, st.view.mode, cur)
        }
        _ => unreachable!("plan_emacs handles only Emacs-profile commands"),
    }
}

/// Plan a linewise shift (`>>` / `<<`) over `count` lines from the cursor's line down. `right` adds one
/// indent level to each line; `!right` removes up to one. Empty lines are never indented (Vim); the cursor
/// lands on the first non-blank of the cursor's (first) line, exactly as Vim leaves it. The register is
/// untouched. Edits are one-per-line at distinct line starts, so the [`EditList`] is disjoint by construction.
/// `=` reindent (bracket-depth model): set each line in `[first_line, last_line]` (0-based, inclusive) to
/// `depth × shiftwidth` of indent, where `depth` is the net unclosed `([{` before the line; a line whose first
/// non-blank is a closer dedents one level, and blank lines are left empty. Structural / language-agnostic:
/// brackets inside strings or comments are NOT excluded (documented). One transaction (edits are disjoint,
/// ascending line starts). Cursor → the first reindented line's new first-non-blank.
/// The leading whitespace of the line holding the opener that matches the closer at `cur` (F-015 Phase 3a).
/// Scans backwards from `cur` counting the `(open, close)` pair; when the nesting depth returns to zero the
/// matching opener is found and its line's leading whitespace is returned as the closer's target indent.
/// `None` when no matching opener exists. Deterministic; not string/comment-aware (as with the `=` fallback).
fn matching_opener_pad(b: &[u8], cur: usize, open: u8, close: u8) -> Option<Vec<u8>> {
    let mut depth = 1i32;
    let mut i = cur;
    while i > 0 {
        i -= 1;
        let x = b[i];
        if x == close {
            depth += 1;
        } else if x == open {
            depth -= 1;
            if depth == 0 {
                let ls = line_start(b, i);
                let fnb = motion::first_non_blank(b, ls);
                return Some(b[ls..fnb].to_vec());
            }
        }
    }
    None
}

/// Apply explicit indent `levels` (one per line from `first_line`) to `[first_line, last_line]`: each
/// non-blank line's leading whitespace becomes `levels[i] × shiftwidth`; blank lines are emptied. One
/// transaction. The frontend supplies tree-derived levels (tree-aware `=`); the core stays view-free.
pub(super) fn plan_set_indents(
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
    Plan {
        action: Action::Txn { edits, hint },
        cursor: first_cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// The formatting options a `gq` or an auto-wrap reads, from this View's (#618).
fn fmt_opts(st: &EditorState) -> format::FmtOpts<'_> {
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

/// Auto-wrap while typing (nvim's `insertchar` → `internal_format`, #618): typing the non-blank `c` at
/// `cur` in Insert past `'textwidth'` (or the `'wrapmargin'` width) breaks the line at a blank before the
/// cursor, the new line taking the indent and, with `'formatoptions'` `c`, the comment leader; then `c`
/// goes in. `None` when nothing wraps.
fn auto_wrap(st: &EditorState, b: &[u8], cur: usize, c: char, hint: GroupHint) -> Option<Plan> {
    if st.view.caret != CaretGravity::OnChar
        || st.view.mode != Mode::Insert
        || c == ' '
        || c == '\t'
    {
        return None;
    }
    let tw = st.view.text_width_for(false);
    if tw == 0 {
        return None;
    }
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    // 'formatoptions' `l`: a line already longer than 'textwidth' when this insert began is left alone.
    if st.view.format.has('l') {
        if let Some((line, len)) = st.view.insstart_line {
            if line == crate::pos::raw::line_of(b, cur) && len > tw {
                return None;
            }
        }
    }
    let o = fmt_opts(st);
    let mut fb = format::FmtBuf {
        lines: vec![b[ls..le].to_vec()],
        cur: (0, cur - ls),
        saved: None,
        before: None,
        after: None,
    };
    fb.internal_format(tw, -1, false, false, false, Some(c), &o);
    if fb.lines.len() == 1 {
        return None;
    }
    let (l, col) = fb.cur;
    let at: usize = fb.lines[..l].iter().map(|x| x.len() + 1).sum::<usize>() + col;
    let mut text = fb.lines.join(&b'\n');
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes();
    text.splice(at..at, typed.iter().copied());
    Some(edit(
        one(Edit::replace(BytePos(ls), le - ls, text)),
        ls + at + typed.len(),
        Mode::Insert,
        hint,
    ))
}

/// `gq`/`gw` {motion} — format the motion's whole lines (nvim's `op_format`). `gw` (`keep_cursor`) keeps
/// the caret on its text; `gq` leaves it on the first non-blank of the last formatted line.
fn plan_format(
    st: &EditorState,
    cur: usize,
    count: u32,
    motion: Motion,
    keep_cursor: bool,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    let (s, e, _) = op_span(b, cur, motion, count);
    if s >= e {
        return nop(cur, st.view.mode);
    }
    // Through the last touched line (exclusive of its trailing newline).
    plan_format_lines(
        st,
        cur,
        line_start(b, s),
        line_end(b, e - 1),
        keep_cursor,
        exclusive_end_adjusted(b, cur, motion, count),
        hint,
    )
}

/// Format the whole lines `[start, end)` (`start` a line start, `end` a line's content end) — the shared
/// tail of `gq`/`gw` over a motion, a search, a `gn` match or a mark, and of Visual `gq` (nvim's
/// `format_lines` through [`format::FmtBuf`], #618): each paragraph (blank lines, a change of comment
/// leader and, with `'formatoptions'` `n`, a list item end one) is joined and broken again at
/// `'textwidth'`, keeping its comment leader under `'formatoptions'` `q`. `below` is nvim's
/// `end_adjusted` (never set for Visual `gq`): the exclusive-linewise rule pulled the span's end off the
/// next line, so `gq` lands on the first non-blank of that next line instead of the last formatted one
/// (nvim `op_format`, #1071). `gw` ignores it.
fn plan_format_lines(
    st: &EditorState,
    cur: usize,
    start: usize,
    end: usize,
    keep_cursor: bool,
    below: bool,
    hint: GroupHint,
) -> Plan {
    let b = st.bytes();
    if std::str::from_utf8(&b[start..end]).is_err() {
        return nop(cur, Mode::Normal);
    }
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
    let cursor = if keep_cursor {
        let (l, col) = fb.saved.unwrap_or((0, 0));
        offset(l.min(fb.lines.len() - 1), col)
    } else if below && end + 1 < b.len() {
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

/// `gc` over the lines `first..=last` (0-based): comment them out with `'commentstring'`, or uncomment
/// them when every non-blank one is commented ([`format::toggle_comment`], nvim's `vim._comment`). The
/// cursor goes to `at`'s line and column, clamped (the lines are replaced, as `nvim_buf_set_lines` does). An
/// empty `'commentstring'` changes nothing.
fn plan_comment(st: &EditorState, first: usize, last: usize, at: usize, hint: GroupHint) -> Plan {
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

fn plan_reindent(st: &EditorState, first_line: usize, last_line: usize, hint: GroupHint) -> Plan {
    let b = st.bytes();
    // `'lisp'`: the Lisp rules instead (#1163).
    if st.view.format.lisp {
        let ind = st.view.indent;
        let cols = super::lisp::reindent(
            b,
            first_line,
            last_line,
            ind.tab_width,
            &st.view.format.lispwords,
            |c| ind.whitespace(c),
        );
        return plan_set_indents(st, first_line, last_line, &cols, hint);
    }
    // Per-line start offset, and the net bracket depth AT each line's start.
    let mut line_starts: Vec<usize> = Vec::new();
    let mut depth_at: Vec<i32> = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0usize;
    line_starts.push(0);
    depth_at.push(0);
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.max(1) - 1,
            b'\n' => {
                line_starts.push(i + 1);
                depth_at.push(depth);
            }
            _ => {}
        }
        i += 1;
    }

    // Target indent LEVEL for line `li`, from depth (a closer-first line dedents one).
    let indent_level = |li: usize, fnb: usize, ls: usize| -> usize {
        let closer = ls < b.len() && fnb < b.len() && matches!(b[fnb], b')' | b']' | b'}');
        (depth_at[li] - i32::from(closer)).max(0) as usize
    };

    let mut edits: Vec<Edit> = Vec::new();
    let last = last_line.min(line_starts.len().saturating_sub(1));
    let mut first_cursor = line_starts[first_line];
    // `li` indexes the two parallel vecs (`line_starts` + `depth_at`) and marks the first line, so a
    // range loop is the clear form here.
    #[allow(clippy::needless_range_loop)]
    for li in first_line..=last {
        let ls = line_starts[li];
        let le = line_end(b, ls);
        let fnb = motion::first_non_blank(b, ls);
        let want: Vec<u8> = if fnb >= le {
            Vec::new() // blank line → no indent
        } else {
            // nvim's `set_indent`: under 'preserveindent' the line keeps its indent characters as far
            // as they fit (#617).
            let cols = indent_level(li, fnb, ls) * st.view.indent.shift_width();
            st.view.indent.rebuilt(&b[ls..fnb], cols)
        };
        if li == first_line {
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
        EditList::new(edits).expect("reindent edits sit at distinct line starts, so disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor: first_cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
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
    line_col_cursor(&line, st.view.curswant, st.view.indent.tab_width)
}

/// The byte offset in `line` (one line's bytes, no newline) of a Normal-mode cursor at the wanted column
/// `want` (Vim's `coladvance`): the char covering that virtual column, the last char past the end or at
/// [`MAXCOL`], `0` on an empty line.
fn line_col_cursor(line: &[u8], want: usize, ts: usize) -> usize {
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
pub(super) enum RefusedOp {
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
pub(super) fn refused_linewise_cursor(
    st: &EditorState,
    cmd: &Command,
    op: RefusedOp,
) -> Option<usize> {
    let b = st.bytes();
    let cur = st.view.cursor;
    let ts = st.view.indent.tab_width;
    let start = if st.view.mode.selection().is_some() {
        line_start(b, st.view.anchor?.min(cur))
    } else {
        let landing = match *cmd {
            Command::Delete(_, Motion::Line) | Command::Yank(_, Motion::Line) => cur,
            Command::Change(_, Motion::Line) => motion::first_non_blank(b, line_start(b, cur)),
            Command::Delete(n, m) | Command::Change(n, m) | Command::Yank(n, m) => {
                op_landing(b, cur, m, n, st.view.curswant, ts, op == RefusedOp::Yank)
            }
            Command::OpToMark {
                name,
                linewise: true,
                ..
            } => mark_line_target(b, st.view.named_mark(name)?.0),
            _ => return None,
        };
        cur.min(landing)
    };
    if op != RefusedOp::Delete {
        return Some(start);
    }
    let ls = line_start(b, start);
    Some(ls + line_col_cursor(&b[ls..line_end(b, ls)], st.view.curswant, ts))
}

fn plan_shift(st: &EditorState, cur: usize, count: u32, right: bool, hint: GroupHint) -> Plan {
    plan_shift_times(st, cur, count, right, 1, hint)
}

/// [`plan_shift`] by `times` indent levels (Visual `3>` shifts the selected lines three levels).
fn plan_shift_times(
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
    for i in 0..count.max(1) {
        let le = line_end(b, ls);
        let (cols, ws) = indent_cols(&b[ls..le], st.view.indent.tab_width);
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
        let (_, old_ws) = indent_cols(&b[first_ls..first_le], st.view.indent.tab_width);
        let mut line = first_new_ws;
        line.extend_from_slice(&b[first_ls + old_ws..first_le]);
        first_ls + line_col_cursor(&line, st.view.curswant, st.view.indent.tab_width)
    };

    if edits.is_empty() {
        // Nothing to indent/unindent (e.g. `<<` at column 0, or `>>` on an empty line): a pure cursor move.
        return Plan {
            action: Action::Nop,
            cursor,
            mode: st.mode(),
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        };
    }
    let edits = EditList::new(edits)
        .expect("shift edits sit at distinct line starts, so they are disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Visual-block `>`/`<` (`:help v_b_>`, `v_b_<`, #1180): each row is shifted from the block's LEFT column
/// by `count` shiftwidths, as nvim's `shift_block`; the text left of the block and the block's right edge
/// play no part. A row that ends before the block, or an empty one, is left alone.
///
/// - `>`: the white space just left of the block (a `<Tab>` the left edge cuts included) and the white
///   space from the left edge up to the first non-blank are rebuilt as one run, `count × sw` columns wider,
///   from where that run starts: tabs and spaces under `noexpandtab`, spaces under `expandtab`.
/// - `<`: the white space from the left edge up to the first non-blank shrinks by `count × sw` columns, or
///   to nothing; what of it still fits stays as it was, and a `<Tab>` that no longer fits becomes spaces.
///
/// The cursor goes to the top row at the byte column the block's top-left character had (nvim's
/// `block_col`).
fn plan_block_shift(
    st: &EditorState,
    b: &[u8],
    block: &Block,
    left: bool,
    count: u32,
    hint: GroupHint,
) -> Plan {
    let ind = st.view.indent;
    let ts = ind.tab_width.max(1);
    let total = ind.shift_width().saturating_mul(count.max(1) as usize);
    let lo = block.lo;
    let is_white = |c: u8| c == b' ' || c == b'\t';
    let mut edits: Vec<Edit> = Vec::new();
    for row in &block.rows {
        let (ls, le) = (row.ls, row.le);
        if ls == le || row.short(lo) {
            continue;
        }
        // The characters that start before the left edge (a straddler included) and, among them, where the
        // run of white space they end with starts (`ws`, at virtual column `ws_v`).
        let (mut i, mut v) = (ls, 0usize);
        let (mut ws, mut ws_v) = (ls, 0usize);
        let (mut prev, mut prev_v) = (ls, 0usize);
        while i < le && v < lo {
            let (j, w) = range::cluster_cells(b, i, le, v, ts);
            if !is_white(b[i]) {
                (ws, ws_v) = (j, v + w);
            }
            (prev, prev_v) = (i, v);
            i = j;
            v += w;
        }
        // `i` (at `v`): the first character at or right of the edge, past a straddler. `nw` (at `nw_v`): the
        // first non-blank from there.
        let (mut nw, mut nw_v) = (i, v);
        while nw < le && is_white(b[nw]) {
            let (j, w) = range::cluster_cells(b, nw, le, nw_v, ts);
            nw = j;
            nw_v += w;
        }
        // nvim's `tabstop_fromto`: the white space filling virtual columns `from..to`, tabs first under
        // `noexpandtab`.
        let blanks = |from: usize, to: usize| {
            let mut out = Vec::new();
            let mut c = from;
            if ind.style == IndentStyle::Tab {
                while (c / ts + 1) * ts <= to {
                    out.push(b'\t');
                    c = (c / ts + 1) * ts;
                }
            }
            out.extend(std::iter::repeat_n(b' ', to - c));
            out
        };
        let (from, to, fill) = if left {
            let dest = nw_v - total.min(nw_v.saturating_sub(lo));
            // Keep what stands from the edge (from a straddler the edge cuts) while it fits before `dest`.
            let (mut k, mut kv) = if v > lo { (prev, prev_v) } else { (i, v) };
            while k < nw && kv < dest {
                let (j, w) = range::cluster_cells(b, k, le, kv, ts);
                if kv + w > dest {
                    break;
                }
                k = j;
                kv += w;
            }
            (k, nw, vec![b' '; dest - kv])
        } else if v > lo && b[prev] != b'\t' {
            // The edge cuts a wide character: the shift goes in before it, its tabs laid out as from
            // column 0 (nvim's `ws_vcol = 0`).
            (prev, prev, blanks(0, total))
        } else {
            (ws, nw, blanks(ws_v, nw_v + total))
        };
        if b[from..to] != fill[..] {
            edits.push(Edit::replace(BytePos(from), to - from, fill));
        }
    }
    // The top row's byte column at the edge, kept on the shifted line (clamped onto it).
    let top = block.rows[0];
    let col = range::at_vcol(b, top.ls, lo, ts) - top.ls;
    let delta: isize = edits
        .iter()
        .take_while(|e| e.pos.0 < top.le)
        .map(|e| e.ins.len() as isize - e.del as isize)
        .sum();
    let new_len = (top.le - top.ls).saturating_add_signed(delta);
    let cursor = top.ls + col.min(new_len.saturating_sub(1));
    if edits.is_empty() {
        return nop(cursor, st.mode());
    }
    let edits = EditList::new(edits).expect("one edit per block row, top to bottom: disjoint");
    Plan {
        action: Action::Txn { edits, hint },
        cursor,
        mode: st.mode(),
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Blockwise (`CTRL-V`) yank/delete/change over the rectangle whose corners are the byte offsets `c1` and
/// `c2` (a Visual-block selection's anchor+cursor, or an operator's cursor+motion-target — e.g.
/// `c<C-v>{motion}`). Yank and Delete capture the per-row slices into a blockwise [`Register`]; the cursor
/// lands at the block's top-left corner (Vim). Change ALSO replicates: it deletes the block on every row
/// (capturing it blockwise), then ARMS a [`BlockInsert`] replicate session at the top-left exactly like
/// [`plan_block_insert`], so the text typed before `<Esc>` is replicated down every block row by
/// [`block_replicate`] (short rows follow the same skip rule). Visual-block `c`/`I`/`A` reach that
/// machinery through [`plan_block_insert`] directly; this arm handles the operator-forced `c<C-v>` path,
/// which is verified against nvim to match Visual-block change byte-for-byte. Dot-repeat of the
/// operator-forced block change is NOT yet wired (the same open gap as Visual-block-insert dot-repeat) and
/// is intentionally not oracle-tested; the replicate + single-undo-unit ARE.
fn block_op(b: &[u8], block: &Block, op: OpKind, hint: GroupHint) -> Plan {
    let top = block.rows[0];
    // The cursor lands on the block's top-left corner, the character covering the left edge — after a
    // delete, past the spaces a split `<Tab>` leaves there.
    let top_left = if op == OpKind::Yank {
        top.ds
    } else {
        top.ds + top.out_l
    };
    let reg = Register::blockwise_width(block.yank_text(b), block.width());
    // The `[`/`]` change-mark span for a blockwise yank: top-left corner → bottom-right corner (the end of
    // the last row's slice). An approximation of Vim's rectangular bracketing, adequate for the mark jump.
    let block_span = (top.ds, block.rows.last().map_or(top.ds, |r| r.de));
    let nop = |mode: Mode| Plan {
        action: Action::Nop,
        cursor: top_left,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };
    // One edit per row (rows on different lines are inherently disjoint): remove every character the block
    // touches, leaving the outside cells of a split `<Tab>` or wide char as spaces (Vim `op_delete`).
    let deletes = || -> Vec<Edit> {
        block
            .rows
            .iter()
            .filter(|r| r.de > r.ds)
            .map(|r| Edit::replace(BytePos(r.ds), r.de - r.ds, vec![b' '; r.out_l + r.out_r]))
            .collect()
    };
    match op {
        // Yank leaves the buffer unchanged, cursor at the block's top-left, back to Normal.
        OpKind::Yank => Plan {
            set_register: Some(RegWrite::Yank(reg, block_span)),
            ..nop(Mode::Normal)
        },
        OpKind::Delete => {
            let edits = deletes();
            if edits.is_empty() {
                // The block sits entirely past every line's end — nothing to remove.
                return nop(Mode::Normal);
            }
            let list = EditList::new(edits).expect("block-row deletes are disjoint (one per line)");
            Plan {
                action: Action::Txn { edits: list, hint },
                cursor: top_left,
                mode: Mode::Normal,
                is_edit: true,
                effects: Vec::new(),
                set_register: Some(RegWrite::Edit(reg)),
                set_anchor: None,
                set_mark: None,
            }
        }
        // Operator-forced blockwise change (`c<C-v>{motion}`): delete the block on every row (captured
        // blockwise, above), then ARM a replicate session at the top-left — the SAME machinery
        // `plan_block_insert` uses for Visual-block `c`, so the text typed before `<Esc>` is replicated
        // down every block row by `block_replicate`. Even when nothing is deleted (the block sits past
        // every line's end) we still arm the session so a typed run replicates; `is_edit` tracks whether
        // the delete transaction is non-empty. `append`/`to_eol` are false — `c` inserts at the left edge.
        OpKind::Change => {
            let session = BlockInsert {
                insert_start: top_left,
                top_left,
                top_line_start: top.ls,
                top_len: (top.le - top.ls) - (top.de - top.ds) + top.out_l + top.out_r,
                target_col: block.lo,
                rows_below: block.rows.len().saturating_sub(1),
                append: false,
                to_eol: false,
                change: true,
                width: block.width().max(1),
            };
            let list =
                EditList::new(deletes()).expect("block-row deletes are disjoint (one per line)");
            let is_edit = !list.is_empty();
            Plan {
                action: Action::BlockInsertArm {
                    edits: list,
                    hint,
                    session,
                },
                cursor: top_left,
                mode: Mode::Insert,
                is_edit,
                effects: Vec::new(),
                set_register: Some(RegWrite::Edit(reg)),
                set_anchor: None,
                set_mark: None,
            }
        }
    }
}

/// Where text inserted at virtual column `t` goes on the line starting at `ls` (Vim `block_insert`):
/// `(at, del, pre, post)` — replace `del` bytes at `at` with `pre` spaces, the text, then `post` spaces. A
/// `<Tab>` crossing `t` is split into the spaces on either side; another wide character crossing it stays
/// after the text, with `pre` spaces before. A line ending before `t` is padded out to it when `pad` (`A`).
fn block_insert_point(
    b: &[u8],
    ls: usize,
    t: usize,
    ts: usize,
    pad: bool,
) -> (usize, usize, usize, usize) {
    let le = line_end(b, ls);
    let (mut i, mut v) = (ls, 0);
    while i < le {
        if v >= t {
            return (i, 0, 0, 0);
        }
        let (j, w) = cluster_cells(b, i, le, v, ts);
        if v + w > t {
            let pre = t - v;
            return if b[i] == b'\t' {
                (i, 1, pre, w - pre)
            } else {
                (i, 0, pre, 0)
            };
        }
        i = j;
        v += w;
    }
    (le, 0, if pad { t.saturating_sub(v) } else { 0 }, 0)
}

/// Finish a blockwise insert-replicate session (`<Esc>` after `CTRL-V` `I`/`A`/`c`): take the text typed
/// on the top row since `session.insert_start` and insert it at `session.target_col` on each of the
/// `rows_below` rows beneath the top line. `append` rows pad short lines with spaces; non-append (`I`/`c`)
/// rows shorter than the target column are skipped (Vim). A newline typed on the top row aborts the
/// replicate (Vim). For `I`/`A` the cursor returns to the block's top-left corner (`session.top_left`);
/// for `c` (change) it rests one char left of the top row's typed text — the normal Insert-exit position,
/// which Vim keeps for a block change (matters only when more than one char was typed).
fn block_replicate(b: &[u8], cur: usize, session: BlockInsert, hint: GroupHint, ts: usize) -> Plan {
    let nop = |cursor: usize| Plan {
        action: Action::Nop,
        cursor,
        mode: Mode::Normal,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };
    let mut start = session.insert_start;
    // Insert left the block's top line (a `<BS>` at its start joined it onto the line above): nothing is
    // replicated, as Vim's `op_insert` gives up when the cursor's line changed, and the cursor steps left
    // as a plain `<Esc>` does. `insert_start` and `top_left` no longer point into that line.
    let cur_ls = line_start(b, cur);
    let tls = session.top_line_start;
    if session.change && cur_ls > tls && line_start(b, tls) == tls {
        return block_change_split(b, cur, &session, hint, ts);
    }
    if cur_ls != tls {
        return nop(motion::prev_boundary(b, cur).max(cur_ls));
    }
    // The cursor rests at the block's top-left when the session ends — for `I`/`A`, and for a `c` where the
    // typed run is empty/aborted (nothing to sit after). A non-empty block CHANGE instead rests one char
    // left of the top row's typed text (Vim's normal Insert-exit), computed once `typed` is known below.
    let end_cursor = session.top_left.min(b.len());
    // A `<BS>` past the insert start and wider text typed over it (`c<BS><BS>é`) can leave `start` inside a
    // char. nvim copies the net bytes added from the start column regardless and writes a split char onto
    // the rows below (verified vs nvim v0.12.6); here the copy starts at the next char instead (#1149).
    while start < cur && start < b.len() && (b[start] & 0xC0) == 0x80 {
        start += 1;
    }
    // The text typed on the top row. Empty (or containing a newline) → nothing to replicate.
    if cur <= start || b[start..cur].contains(&b'\n') {
        return nop(end_cursor);
    }
    let typed = &b[start..cur];
    let rest_cursor = if session.change {
        motion::prev_boundary(b, cur)
    } else {
        end_cursor
    };

    let mut edits: Vec<Edit> = Vec::new();
    let mut rs = session.top_line_start;
    for _ in 0..session.rows_below {
        let le = line_end(b, rs);
        if le >= b.len() {
            break; // no further line to replicate onto
        }
        rs = le + 1;
        let row_le = line_end(b, rs);
        if session.to_eol {
            // `$A`: append at THIS row's own end (ragged), never a fixed column.
            edits.push(Edit::insert(BytePos(row_le), typed.to_vec()));
            continue;
        }
        // Non-append + short row: skipped (Vim leaves lines shorter than the block's left edge untouched).
        if !session.append && line_cells(b, rs, ts) < session.target_col {
            continue;
        }
        let (at, del, pre, post) =
            block_insert_point(b, rs, session.target_col, ts, session.append);
        let mut ins = vec![b' '; pre];
        ins.extend_from_slice(typed);
        ins.extend(std::iter::repeat_n(b' ', post));
        edits.push(Edit::replace(BytePos(at), del, ins));
    }
    if edits.is_empty() {
        return nop(rest_cursor);
    }
    let list = EditList::new(edits).expect("block-replicate inserts are disjoint (one per line)");
    Plan {
        action: Action::Txn { edits: list, hint },
        cursor: rest_cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// A block change (`c`/`C`) whose Insert broke the top row with a `<CR>` (#1142), as nvim's `op_change`:
/// the text the top row GREW by (its length now less its length when Insert began), taken from the insert
/// column, goes into the lines BELOW the top row by line number, the rows the block had: the lines the
/// `<CR>` opened come first, so `x<CR>y` over two rows writes the `x` into the `y` line too. A line that
/// ends before the block's left column is skipped. The cursor stays where `<Esc>` leaves it, at its byte
/// column.
fn block_change_split(
    b: &[u8],
    cur: usize,
    session: &BlockInsert,
    hint: GroupHint,
    ts: usize,
) -> Plan {
    let cur_ls = line_start(b, cur);
    let rest = motion::prev_boundary(b, cur).max(cur_ls);
    let tls = session.top_line_start;
    let tle = line_end(b, tls);
    let at = session.insert_start.clamp(tls, tle);
    let grew = (tle - tls).saturating_sub(session.top_len);
    let mut end = (at + grew).min(tle);
    // nvim copies bytes; a character the cut would split is left out.
    while end > at && end < tle && (b[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    if end <= at {
        return nop(rest, Mode::Normal);
    }
    let text = &b[at..end];
    let mut edits: Vec<Edit> = Vec::new();
    let mut rs = tls;
    for _ in 0..session.rows_below {
        let le = line_end(b, rs);
        if le >= b.len() {
            break;
        }
        rs = le + 1;
        if line_cells(b, rs, ts) < session.target_col {
            continue;
        }
        edits.push(Edit::insert(
            BytePos(range::want_col(b, rs, session.target_col, ts)),
            text.to_vec(),
        ));
    }
    if edits.is_empty() {
        return nop(rest, Mode::Normal);
    }
    // The lines above the cursor's grew; its own line keeps the cursor's byte column.
    let shift: usize = edits
        .iter()
        .filter(|e| e.pos.0 < cur_ls)
        .map(|e| e.ins.len())
        .sum();
    let list = EditList::new(edits).expect("one insert per line, top to bottom: disjoint");
    Plan {
        action: Action::Txn { edits: list, hint },
        cursor: rest + shift,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Blockwise paste (`p`/`P` of a `CTRL-V` register, nvim `do_put`): drop the register's rows as a
/// rectangle. Row `i` lands on the line `i` rows below the cursor's, at the virtual column of the cursor's
/// first cell for `P` and one past its last cell for `p`. A line shorter than that column is padded with
/// spaces; a `<Tab>` crossing it is split into spaces around the block (another wide character crossing it
/// moves right, as in Vim); rows past the last line open new lines. The cursor ends at the block's top-left.
/// `{count}p` repeats each row `count` times horizontally. With `pad`, a row narrower than the block is
/// filled with spaces to the block's width between copies and before any text that follows it, so that text
/// stays in one column; `zp`/`zP` paste without (`pad` false).
fn paste_block(
    b: &[u8],
    cur: usize,
    reg: &Register,
    after: bool,
    count: usize,
    pad: bool,
    ts: usize,
) -> Plan {
    let units: Vec<&[u8]> = reg.text().split(|&c| c == b'\n').collect();
    let block_width = reg.block_width();
    let cls = line_start(b, cur);
    let (v0, v1) = vcols_at(b, cur, ts);
    let col = if after && cur < line_end(b, cls) {
        v1 + 1
    } else {
        v0
    };
    // The cursor's line is real even when it is the empty slot after a final newline (`Go<Esc>` in a
    // buffer without one, #1080): the first row lands in it. Lines opened past the end then follow it with
    // a separator before each; after a terminated last line each new line carries its own newline (#695).
    let in_end_slot = cls == b.len();
    let terminated = !in_end_slot && b.last() == Some(&b'\n');
    let mut edits: Vec<Edit> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut cursor = cur;
    let mut line = Some(cls);
    for (i, unit) in units.iter().enumerate() {
        let fill = if pad {
            block_width.saturating_sub(text_cells(unit, ts))
        } else {
            0
        };
        // Row `i` repeated `count` times; the fill goes between copies, and after the last one only when
        // text follows it on its line.
        let build = |shortline: bool| {
            let mut out = Vec::new();
            for j in 0..count {
                out.extend_from_slice(unit);
                if j + 1 < count || !shortline {
                    out.extend(std::iter::repeat_n(b' ', fill));
                }
            }
            out
        };
        let Some(ls) = line else {
            let mut row = vec![b' '; col];
            row.extend(build(true));
            if terminated {
                tail.extend(row);
                tail.push(b'\n');
            } else {
                tail.push(b'\n');
                tail.extend(row);
            }
            continue;
        };
        let le = line_end(b, ls);
        let (mut p, mut prev, mut v, mut w) = (ls, ls, 0, 0);
        while v < col && p < le {
            prev = p;
            let (j, cw) = cluster_cells(b, p, le, v, ts);
            p = j;
            w = cw;
            v += w;
        }
        let (mut at, mut del, mut pre, mut post) = (p, 0, 0, 0);
        if v < col {
            pre = col - v; // line too short: pad with spaces
        } else if v > col {
            // The last character crosses the column: split a `<Tab>` into spaces on both sides; anything
            // else moves to after the block.
            post = v - col;
            pre = w - post;
            at = prev;
            if b[prev] == b'\t' {
                del = p - prev;
            } else {
                post = 0;
            }
        }
        let shortline = v < col || (v == col && p >= le);
        let mut ins = vec![b' '; pre];
        ins.extend(build(shortline));
        ins.extend(std::iter::repeat_n(b' ', post));
        if i == 0 {
            cursor = at + pre;
        }
        if del > 0 || !ins.is_empty() {
            edits.push(Edit::replace(BytePos(at), del, ins));
        }
        line = (le + 1 < b.len()).then_some(le + 1);
    }
    if !tail.is_empty() {
        // Merge into a bottom-most edit already sitting at the buffer end (avoids two inserts at one
        // position, which the disjoint-edit invariant forbids); otherwise append as its own edit.
        match edits.last_mut() {
            Some(last) if last.end().0 == b.len() => last.ins.extend_from_slice(&tail),
            _ => edits.push(Edit::insert(BytePos(b.len()), tail)),
        }
    }
    if edits.is_empty() {
        return Plan {
            action: Action::Nop,
            cursor: cursor.min(b.len()),
            mode: Mode::Normal,
            is_edit: false,
            effects: Vec::new(),
            set_register: None,
            set_anchor: None,
            set_mark: None,
        };
    }
    let list = EditList::new(edits).expect("block paste edits are disjoint (one per line)");
    Plan {
        action: Action::Txn {
            edits: list,
            hint: GroupHint::BreakBefore,
        },
        cursor,
        mode: Mode::Normal,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    }
}

/// Whether `o`/`O`/`<CR>` indent the line they open: `'autoindent'` or `'smartindent'`, under the Vim
/// profile (an Emacs `newline` never indents).
fn auto_indents(st: &EditorState) -> bool {
    st.view.caret == CaretGravity::OnChar
        && (st.view.indent.autoindent || st.view.indent.smartindent)
}

/// Where an Insert caret at `cur` lands after its line's leading whitespace (`old_ws` bytes from `ls`) is
/// rebuilt as `new_ws` bytes: it moves by the change in length (riding with the text, nvim also inside the
/// indent), never before the line start.
fn reindented_caret(cur: usize, ls: usize, old_ws: usize, new_ws: usize) -> usize {
    (cur + new_ws).saturating_sub(old_ws).max(ls)
}

use super::indent::indent_cols;

/// Re-indent a linewise register's bytes for `]p`/`[p`: the first line takes `target_cols` of indent and
/// every other line shifts by the same column delta (Vim). Indent is rebuilt in the editor's style (spaces,
/// or tabs + a spaces remainder); a blank line stays blank. Input/output both end each line with `\n`.
fn reindent_register(
    text: &[u8],
    target_cols: usize,
    tab_width: usize,
    style: IndentStyle,
) -> Vec<u8> {
    let tw = tab_width.max(1);
    let first_end = text.iter().position(|&c| c == b'\n').unwrap_or(text.len());
    let (first_cols, _) = indent_cols(&text[..first_end], tw);
    let delta = target_cols as isize - first_cols as isize;
    let build = |cols: usize| -> Vec<u8> {
        match style {
            IndentStyle::Space => vec![b' '; cols],
            IndentStyle::Tab => {
                let mut v = vec![b'\t'; cols / tw];
                v.extend(std::iter::repeat_n(b' ', cols % tw));
                v
            }
        }
    };
    let mut out = Vec::with_capacity(text.len());
    for line in text.split_inclusive(|&c| c == b'\n') {
        let (body, nl): (&[u8], &[u8]) = match line.strip_suffix(b"\n") {
            Some(b) => (b, b"\n"),
            None => (line, b""),
        };
        let (cols, ws) = indent_cols(body, tw);
        let content = &body[ws..];
        if content.is_empty() {
            out.extend_from_slice(nl); // a blank line stays blank (no indent)
        } else {
            let new_cols = (cols as isize + delta).max(0) as usize;
            out.extend_from_slice(&build(new_cols));
            out.extend_from_slice(content);
            out.extend_from_slice(nl);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)] // paste legitimately needs the full register + gravity + gp context
fn paste(
    b: &[u8],
    cur: usize,
    mode: Mode,
    reg: &Register,
    after: bool,
    count: u32,
    gravity: CaretGravity,
    move_after: bool,
    pad_block: bool,
    ts: usize,
) -> Plan {
    let nop = Plan {
        action: Action::Nop,
        cursor: cur,
        mode,
        is_edit: false,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };
    if reg.is_empty() {
        return nop;
    }
    // Bound the repetition so a pathological `{count}p` (e.g. a digit-spam count that saturates to
    // `u32::MAX`) cannot request a multi-gigabyte allocation ([`MAX_PUT_BYTES`]).
    let unit_len = reg.text().len().max(1);
    let count = (count.max(1) as usize).min((MAX_PUT_BYTES / unit_len).max(1));
    if reg.is_blockwise() {
        return paste_block(b, cur, reg, after, count, pad_block, ts);
    }
    // `{count}p` pastes the register `count` times (Vim); the register itself is unchanged. The repeated
    // bytes are one contiguous insert, so the cursor math below (last pasted byte) still holds.
    let repeat = |unit: &[u8]| unit.repeat(count);
    let one = |e: Edit| EditList::new(vec![e]).expect("single edit is always valid");
    // A paste run from Insert (`i_CTRL-O p`) returns to Insert; every other paste ends in Normal.
    let end_mode = if mode == Mode::Insert {
        Mode::Insert
    } else {
        Mode::Normal
    };
    let mk = |at: usize, bytes: Vec<u8>, cursor: usize| Plan {
        action: Action::Txn {
            edits: one(Edit::insert(BytePos(at), bytes)),
            hint: GroupHint::BreakBefore,
        },
        cursor,
        mode: end_mode,
        is_edit: true,
        effects: Vec::new(),
        set_register: None,
        set_anchor: None,
        set_mark: None,
    };

    if reg.is_linewise() {
        // Linewise content is normalized to end with '\n'; `{count}p` stacks that many whole-line copies.
        // `gp`/`gP` (move_after) leave the cursor on the line AFTER the pasted block instead of on it.
        let text = repeat(reg.text());
        let tlen = text.len();
        // Vim rests the cursor on the FIRST NON-BLANK of the first pasted line (not column 0). The leading
        // whitespace never contains a newline, so counting spaces/tabs stays within that first line.
        let fnb = text
            .iter()
            .take_while(|&&c| c == b' ' || c == b'\t')
            .count();
        if after {
            let le = line_end(b, cur);
            if le < b.len() {
                // Insert after the current line's newline: the stored "...\n" becomes a fresh line below.
                let c = if move_after {
                    le + 1 + tlen
                } else {
                    le + 1 + fnb
                };
                mk(le + 1, text, c)
            } else {
                // Last line has no trailing newline: prepend one and drop the stored trailing newline so no
                // dangling blank line is created. Cursor lands on the first non-blank of the pasted line.
                let mut bytes = vec![b'\n'];
                bytes.extend_from_slice(text.strip_suffix(b"\n").unwrap_or(&text));
                let end = le + bytes.len();
                mk(le, bytes, if move_after { end } else { le + 1 + fnb })
            }
        } else {
            let ls = line_start(b, cur);
            mk(ls, text, if move_after { ls + tlen } else { ls + fnb })
        }
    } else {
        // `{count}p` inserts that many copies inline. Vim's charwise-paste cursor rule splits on whether the
        // pasted text spans lines: single-line content leaves the cursor on the LAST pasted byte, but content
        // carrying a newline (e.g. a charwise Visual delete across a line boundary) leaves it on the FIRST
        // pasted byte — the start of the inserted run — not the last.
        let text = repeat(reg.text());
        let n = text.len();
        let multiline = text.contains(&b'\n');
        // Single-line charwise paste: Vim (OnChar) rests the cursor ON the LAST pasted byte; the Emacs
        // profile (BetweenChar, D-050) rests point AFTER the pasted text — Emacs `yank` leaves point past
        // what it inserted. `end` is the after-last boundary; back it up one grapheme only under OnChar: the
        // start of the text's last cluster, so `ylP` on a char with combining marks rests on its base (#1132).
        let last = if multiline || n == 0 {
            1
        } else {
            n - motion::prev_grapheme(&text, n)
        };
        let tail = |end: usize| match gravity {
            CaretGravity::BetweenChar => end,
            CaretGravity::OnChar => end.saturating_sub(last),
        };
        if after {
            // Insert after the cursor char; cursor lands on the first pasted byte for multi-line content,
            // else per gravity on/after the last pasted byte. `gp` (move_after) rests just past the paste.
            // On an empty line, or past the last char (`A<C-o>p`), there is no char to paste after: the
            // text goes in at the cursor, never across the line break (#701).
            let at = if cur < b.len() && b[cur] != b'\n' {
                next_grapheme(b, cur)
            } else {
                cur
            };
            let cursor = if move_after {
                at + n
            } else if multiline {
                at
            } else {
                tail(at + n)
            };
            mk(at, text, cursor)
        } else {
            // Insert before the cursor; cursor lands on the first pasted byte for multi-line content, else
            // per gravity on/after the last pasted byte. `gP` (move_after) rests just past the paste.
            let cursor = if move_after {
                cur + n
            } else if multiline {
                cur
            } else {
                tail(cur + n)
            };
            mk(cur, text, cursor)
        }
    }
}
