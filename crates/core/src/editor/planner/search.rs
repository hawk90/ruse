//! Search: `n`/`N`, `/`/`?`, `gn`, `*`/`#`, `gd`, `[i` and `[I`, and their helpers (#1350).
//!
//! Split out of [`super::plan_cmd`] (#1349); the arms are as they were there.

use super::*;

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
