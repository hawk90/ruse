//! What is typed in Insert mode: a char (smartindent realignment, comment ending, auto-wrap),
//! `<CR>`, a line opened at an indent level, and an auto-closer.

use super::{
    auto_indents, edit, fmt_opts, format, insert_text, line_end, line_start, motion, one, BytePos,
    CaretGravity, Command, Edit, EditorState, GroupHint, Mode, OpenKind, Plan,
};

/// What is typed in Insert mode: a char, `<CR>`, an opened line at an indent level, an auto-closer.
pub(super) fn plan_typed(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
    match cmd {
        Command::InsertChar(c) => insert_char(st, b, cur, hint, c),
        Command::InsertNewline if auto_indents(st) && st.view.mode == Mode::Insert => {
            insert_newline_indented(st, b, cur, hint)
        }
        Command::InsertNewline => insert_text(cur, b"\n".to_vec(), Mode::Insert, hint),
        Command::OpenLineIndent { kind, level } => {
            open_line_at_level(st, b, cur, hint, kind, level)
        }
        Command::InsertCloser { ch } => insert_closer(b, cur, hint, ch),
        _ => unreachable!("plan_typed plans only the Insert-mode typing commands"),
    }
}

/// A char typed in Insert mode, with smartindent realignment, comment ending and auto-wrap.
fn insert_char(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, c: &char) -> Plan {
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

/// An Insert `<CR>` under `'autoindent'`/`'smartindent'` (Vim profile): the blanks after the caret
/// go, the new line takes the indent of the text before it, and a line holding only the indent the
/// last open gave it is emptied (nvim's `open_line` with `did_ai`).
fn insert_newline_indented(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    let (mut pad, _) = st
        .view
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

/// `o`/`O`/`<CR>` with a tree-suggested indent (F-015 Phase 2): open a line whose leading whitespace
/// is `level × unit`, cursor after it. `Above` inserts `<indent>\n` before the line (cursor on the new
/// line above); `Below`/`Split` insert `\n<indent>` (cursor on the new line below / after the split).
/// `level: 0` degrades to a plain open, so this also serves as the non-tree fallback.
fn open_line_at_level(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    kind: &OpenKind,
    level: &usize,
) -> Plan {
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

/// A closer (`}`/`)`/`]`) typed as the line's sole leading content realigns the line to its matching
/// opener's indent, then inserts the closer — smartindent-like (F-015 Phase 3a). Deterministic bytes
/// bracket-match; with content already before the cursor, or no matching opener, it is a plain insert.
fn insert_closer(b: &[u8], cur: usize, hint: GroupHint, ch: &char) -> Plan {
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
