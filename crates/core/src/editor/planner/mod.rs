use super::*;
use crate::cells::Tabs;
use crate::RegKind;

mod comment;
mod edit;
mod emacs;
mod insert;
mod marks;
mod operator;
mod put;
mod search;
mod visual;

pub(crate) use comment::open_comment_line_in;
use comment::plan_open_comment;
pub(in crate::editor) use comment::{open_comment_line, open_end_comment};

use put::{paste, plan_paste_selection};

use emacs::plan_emacs;

/// The most bytes one counted repeat of text may produce (`{count}p`, `{count}gr`): 64 MiB is far beyond
/// any interactive intent, and clamping the count keeps a hostile one (a digit run that saturates) from
/// requesting a multi-gigabyte allocation (#683).
const MAX_PUT_BYTES: usize = 1 << 26;

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
            set_register: Some(RegWrite::Edit(reg)),
            ..nop(indent_end, Mode::Insert)
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
                want_col(b, line, st.view.curswant.get(), st.view.indent.tabs())
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
            set_register: Some(RegWrite::Yank(captured(b, s, e, linewise), (s, e))),
            ..nop(rest, mode)
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
    recase_bytes_with(bytes, case, recase)
}

/// [`recase_bytes`] under the case `rules` (Vim's or Emacs's).
fn recase_bytes_with(bytes: &[u8], case: WordCase, rules: fn(&str, WordCase) -> String) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.extend_from_slice(rules(chunk.valid(), case).as_bytes());
        out.extend_from_slice(chunk.invalid());
    }
    out
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

/// A Vim-profile motion that fails ([`motion::fails_with`]), bare in Normal or Visual or under an operator: the
/// command does nothing and beeps, so the frontend flushes the keys that were not typed (#1085). The
/// operator is cancelled whole (`dfz`, `cj` on the last line stays in Normal).
fn failed_motion(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    cmd: &Command,
    memo: &std::cell::OnceCell<usize>,
) -> Option<Plan> {
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
    motion::fails_with(b, cur, m, count, op, visual, memo).then(|| beep(cur, st.view.mode))
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
            // The last line's newline is never part of it (nvim: `gHX` on the last line leaves `"def"`).
            let e = e.min(crate::motion::content_end(b)).max(s);
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
    // A byte that is not UTF-8 is kept as it is, the text around it recased (#706, #1243).
    Some(recase_bytes_with(&b[s..e], case, rules))
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
    edit(
        EditList::new(edits).expect("join seams are disjoint and ordered"),
        cursor,
        mode,
        hint,
    )
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
    let resolved = display_columns(st, cmd);
    let p = plan_cmd(st, resolved.as_ref().unwrap_or(cmd));
    leave_autoindent(st, st.view.cursor, p, hint)
}

/// `cmd` with a `{count}|` or `{count}gM` motion resolved in DISPLAY columns (nvim's `nv_pipe` / `nv_g_cmd`
/// `gM`: a `<Tab>` spans to its tabstop, a wide char two cells and a byte that is not UTF-8 its four-cell
/// `<ff>`, #1251), or `None` when it has neither. The motion layer counts clusters and knows no tabstop, so
/// the landing is found here and handed back as the `|` count that steps that many clusters onto it: `|`
/// lands on the char covering the column (the last char past the line end), `gM` on the one at `count`
/// percent of the line's cells.
fn display_columns(st: &EditorState, cmd: &Command) -> Option<Command> {
    let (count, m) = match cmd {
        Command::Move(n, m)
        | Command::Delete(n, m)
        | Command::Change(n, m)
        | Command::Yank(n, m) => (*n, *m),
        Command::OpForced { count, motion, .. } | Command::CaseMotion { count, motion, .. } => {
            (*count, *motion)
        }
        _ => return None,
    };
    if !matches!(m, Motion::Column | Motion::MidLine) {
        return None;
    }
    let b = st.bytes();
    let ts = st.view.indent.tabs();
    let ls = line_start(b, st.view.cursor);
    let n = count.max(1) as usize;
    let vcol = match m {
        Motion::Column => n - 1,
        // nvim takes a count over 100 as a bare `gM`.
        _ if n > 100 => range::line_cells(b, ls, ts) / 2,
        _ => range::line_cells(b, ls, ts).saturating_mul(n) / 100,
    };
    // The char covering `vcol`, or `None` past the line's last cell. Only the cells up to it are read, so
    // `5000|` on a huge line costs 5000 cells.
    // The line's end is found by the walk, not scanned for first: a huge line costs the cells crossed.
    let (mut i, mut v, mut last) = (ls, 0, ls);
    let mut hit = None;
    while i < b.len() && b[i] != b'\n' {
        // A cluster never crosses a line break, but `\r\n` would be one: bound a CR to itself.
        let bound = if b[i] == b'\r' { i + 1 } else { b.len() };
        let (j, w) = cluster_cells(b, i, bound, v, ts);
        if vcol < v + w {
            hit = Some(i);
            break;
        }
        last = i;
        i = j;
        v += w;
    }
    // Past the end Normal stops on the last char; Visual may go one past it (nvim's `coladvance`
    // `one_more`): `v10|` on `abc` takes the line break.
    let at = match hit {
        Some(at) => at,
        None if st.view.mode.selection().is_some() => i,
        None => last,
    };
    let mut steps = 0usize;
    let mut i = ls;
    while i < at {
        i = motion::next_boundary(b, i);
        steps += 1;
    }
    let col = u32::try_from(steps + 1).unwrap_or(u32::MAX);
    let mut out = cmd.clone();
    match &mut out {
        Command::Move(n, m)
        | Command::Delete(n, m)
        | Command::Change(n, m)
        | Command::Yank(n, m) => {
            *n = col;
            *m = Motion::Column;
        }
        Command::OpForced { count, motion, .. } | Command::CaseMotion { count, motion, .. } => {
            *count = col;
            *motion = Motion::Column;
        }
        _ => {}
    }
    Some(out)
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
    // The bare motion's target, shared by the failure check and the `Move` arm (#1231).
    let memo = std::cell::OnceCell::new();
    if let Some(p) = failed_motion(st, b, cur, cmd, &memo) {
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
            motion::vmove(
                b,
                cur,
                1,
                false,
                st.view.curswant.get(),
                st.view.indent.tabs(),
            ),
            st.view.mode,
        ),
        Command::MoveDown => nop(
            motion::vmove(
                b,
                cur,
                1,
                true,
                st.view.curswant.get(),
                st.view.indent.tabs(),
            ),
            st.view.mode,
        ),
        Command::EnterInsert
        | Command::InsertAtLastInsert
        | Command::EnterInsertAfter
        | Command::InsertLineStart
        | Command::InsertColumnZero
        | Command::AppendLineEnd
        | Command::OpenBelow
        | Command::OpenAbove
        | Command::EnterNormal
        | Command::EnterTerminal
        | Command::EnterTerminalNormal
        | Command::EnterReplace
        | Command::ReplaceType(..)
        | Command::ReplaceBackspace
        | Command::EnterVirtualReplace
        | Command::VirtualReplaceType(..)
        | Command::VirtualReplaceChar(..)
        | Command::InsertChar(..)
        | Command::InsertRegister(..)
        | Command::InsertEval(..)
        | Command::InsertPaste(..)
        | Command::ReplacePaste(..)
        | Command::EmacsPaste(..)
        | Command::InsertDeleteWordBack
        | Command::InsertDeleteToLineStart
        | Command::CompleteWord { .. }
        | Command::InsertIndent
        | Command::InsertDedent
        | Command::InsertDeleteIndent
        | Command::InsertDeleteIndentRestore
        | Command::InsertStartColumnLine { .. }
        | Command::InsertCaretStep { .. }
        | Command::InsertTab
        | Command::InsertNewline
        | Command::OpenLineIndent { .. }
        | Command::InsertCloser { .. } => insert::plan(st, b, cur, hint, cmd),
        Command::DeleteBack
        | Command::DeleteUnder(..)
        | Command::DeleteForward(..)
        | Command::ReplaceChar(..)
        | Command::ToggleCase(..)
        | Command::JoinLines(..)
        | Command::JoinLinesNoSpace(..)
        | Command::IncrementNumber(..)
        | Command::IncrementSelection { .. } => edit::plan(st, b, cur, hint, cmd),
        Command::GotoLastChange
        | Command::GotoLastChangeLine
        | Command::GotoChangeMarkStart
        | Command::GotoChangeMarkEnd
        | Command::GotoChangeMarkStartLine
        | Command::GotoChangeMarkEndLine
        | Command::GotoVisualMarkStart
        | Command::GotoVisualMarkEnd
        | Command::GotoVisualMarkStartLine
        | Command::GotoVisualMarkEndLine
        | Command::GotoContextMark
        | Command::GotoContextMarkLine
        | Command::GotoOlderChange
        | Command::GotoNewerChange
        | Command::GotoOlderJump
        | Command::GotoNewerJump
        | Command::SetNamedMark(..)
        | Command::GotoNamedMark(..)
        | Command::GotoNamedMarkLine(..)
        | Command::OpToMark { .. } => marks::plan(st, b, cur, hint, cmd),
        // `CTRL-G u`: break the undo group. A pure nop (is_edit = false), so `commit` sets
        // `last_was_edit = false` and the NEXT edit's `GroupHint` becomes `BreakBefore` — a fresh undo
        // group starts here mid-insert-session (Vim `i_CTRL-G_u`). Cursor and mode are untouched.
        Command::BreakUndo => nop(cur, st.view.mode),
        Command::Move(..)
        | Command::Delete(..)
        | Command::Change(..)
        | Command::Yank(..)
        | Command::CaseMotion { .. }
        | Command::OpForced { .. }
        | Command::ShiftRight(..)
        | Command::ShiftLeft(..)
        | Command::ShiftMotion { .. }
        | Command::Reindent { .. }
        | Command::Format { .. }
        | Command::Comment { .. }
        | Command::SetIndents { .. }
        | Command::SetIndentCols { .. }
        | Command::InsertReindent => operator::plan(st, b, cur, hint, &memo, cmd),
        Command::Paste { .. }
        | Command::PasteBlockInner { .. }
        | Command::PasteIndent { .. }
        | Command::SetRegister(..)
        | Command::SetExprRegister(..) => put::plan(st, b, cur, cmd),
        Command::EnterVisual { .. }
        | Command::EnterSelect { .. }
        | Command::ReselectVisual
        | Command::ReplaceSelection(..)
        | Command::BlockInsert(..)
        | Command::SwapSelectionEnds
        | Command::YankSelection
        | Command::DeleteSelection
        | Command::ChangeSelection
        | Command::VisualOp(..)
        | Command::CaseSelection(..)
        | Command::ReplaceSelectionChar(..)
        | Command::PasteSelection { .. } => visual::plan(st, b, cur, hint, cmd),
        Command::SearchNext(..)
        | Command::SearchPrev(..)
        | Command::Search { .. }
        | Command::SearchObject { .. }
        | Command::SearchWordUnder { .. }
        | Command::GotoDeclaration { .. }
        | Command::ShowKeywordLines { .. }
        | Command::GotoFirstMatch(..) => search::plan(st, b, cur, hint, cmd),
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
            ..nop(cur, undo_resume_mode)
        },
        Command::Redo => Plan {
            action: Action::Redo,
            ..nop(cur, undo_resume_mode)
        },
        Command::UndoOlder => Plan {
            action: Action::UndoChrono { older: true },
            ..nop(cur, undo_resume_mode)
        },
        // `U` — a change in its own right (`u` undoes it), so it is an edit; nothing to restore is a no-op.
        Command::LineUndo if st.doc.can_line_undo() => Plan {
            action: Action::LineUndo,
            is_edit: true,
            ..nop(cur, Mode::Normal)
        },
        Command::LineUndo => nop(cur, st.view.mode),
        Command::UndoNewer => Plan {
            action: Action::UndoChrono { older: false },
            ..nop(cur, undo_resume_mode)
        },
        Command::Save => Plan {
            effects: vec![Effect::Save],
            ..nop(cur, st.view.mode)
        },
        Command::Quit => Plan {
            effects: vec![Effect::Quit],
            ..nop(cur, st.view.mode)
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
    edit(edits, first_cursor, st.mode(), hint)
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
            } => mark_line_target(b, st.view.named_mark(name)?.0),
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

    if edits.is_empty() {
        // Nothing to indent/unindent (e.g. `<<` at column 0, or `>>` on an empty line): a pure cursor move.
        return nop(cursor, st.mode());
    }
    let edits = EditList::new(edits)
        .expect("shift edits sit at distinct line starts, so they are disjoint");
    edit(edits, cursor, st.mode(), hint)
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
    let nop = |mode: Mode| nop(top_left, mode);
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
            edit_yank(list, top_left, Mode::Normal, hint, reg)
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
                is_edit,
                set_register: Some(RegWrite::Edit(reg)),
                ..self::nop(top_left, Mode::Insert)
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
    ts: Tabs,
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

use super::indent::indent_cols;
