//! Comment-leader continuation: the leader an opened line (`o`/`O`/`<CR>`) continues, and the end of a
//! three-piece comment that `formatoptions` closes (#1350).

use super::{edit, indent_cols, one};
use crate::editor::format;
use crate::motion::{line_end, line_start};
use crate::{BytePos, CaretGravity, Command, Edit, EditorState, GroupHint, Mode, Plan, View};

/// The line a comment-continuing open makes (#618): `o`/`O` under `'formatoptions'` `o` and an Insert
/// `<CR>` under `r` on a line that starts with a `'comments'` leader (nvim's `open_line` with
/// `OPENLINE_DO_COM`). The new line's leading text (indent and leader) and, for `<CR>`, how far the text
/// after the cursor starts (`'autoindent'` drops the blanks there). `None` when the line is no comment, so
/// the plain open applies; also for a finished C comment under `'autoindent'`, which lines up with the
/// line that opened it.
pub(in crate::editor) fn open_comment_line(
    st: &EditorState,
    cmd: &Command,
) -> Option<(Vec<u8>, bool)> {
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
        ind.tabs(),
    );
    let (old_cols, old_len) = indent_cols(saved, ind.tabs());
    let mut leader = lead.leader;
    let newindent = if leader.is_empty() {
        // A finished C comment: line up with the line holding its `/*`.
        let ce = lead.comment_end?;
        if !(indent_on && saved[ce..].starts_with(b"*/")) {
            return None;
        }
        let open = b[..ls + ce].windows(2).rposition(|w| w == b"/*")?;
        let ols = line_start(b, open);
        indent_cols(&b[ols..line_end(b, ols)], ind.tabs()).0
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
pub(in crate::editor) fn open_end_comment(st: &EditorState, cmd: &Command) -> Option<u8> {
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

/// Plan an open that continues a comment ([`open_comment_line`]), or `None`.
pub(super) fn plan_open_comment(
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
