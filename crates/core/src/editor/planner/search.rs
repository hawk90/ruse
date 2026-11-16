//! Search: `n`/`N`, `/`/`?`, `gn`, `*`/`#`, `gd`, `[i` and `[I`, and their helpers (#1350).
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::{apply_op, beep, captured, delete_quirk, edit_yank, nop, one, plan_op_over_span};
use crate::editor::{
    exclusive_linewise, landing_key, match_spans, search_bwd, search_fwd, search_spans, step_char,
    walk_count, OpSpan, RegWrite,
};
use crate::motion::{self, prev_boundary};
use crate::{
    BytePos, Command, Edit, EditorState, GroupHint, MarkOp, Mode, Plan, SearchOffset, SearchOp,
    SelectKind,
};

pub(super) fn plan(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, cmd: &Command) -> Plan {
    match cmd {
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
        _ => unreachable!("planner::search plans only its own commands"),
    }
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
            set_anchor: Some(s),
            ..nop(
                motion::prev_grapheme(b, e),
                Mode::Visual {
                    kind: SelectKind::Charwise,
                },
            )
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
                set_register: Some(RegWrite::Yank(reg, (s, e))),
                ..nop(s, st.view.mode)
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
        // A byte that is not UTF-8 is spelled as the regex engine's escape char (#1243).
        let text = crate::pattern::spell(&b[s..e]);
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
    let word = crate::pattern::spell(&b[s..e]);
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
