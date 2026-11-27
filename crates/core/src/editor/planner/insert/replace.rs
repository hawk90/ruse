//! What is typed (or backspaced) in Replace (`R`) and Virtual Replace (`gR`) mode, and the
//! one-shot `{count}gr{char}`.

use super::{
    auto_indents, edit, motion, next_boundary, nop, one, prev_boundary, prev_grapheme, BytePos,
    Command, Edit, EditorState, GroupHint, Mode, Plan,
};
use crate::cells::Tabs;
use crate::editor::planner::MAX_PUT_BYTES;
use crate::editor::{cluster_cells, Action};
use crate::motion::{line_end, line_start};

/// What is typed (or backspaced) in Replace and Virtual Replace mode.
pub(super) fn plan_replace_typed(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
    match cmd {
        Command::ReplaceType(c) => replace_type(b, cur, hint, c),
        Command::ReplaceBackspace => replace_backspace(st, b, cur, hint),
        Command::InsertNewline => replace_newline(st, b, cur, hint),
        Command::VirtualReplaceType(c) => plan_virtual_replace_type(st, b, cur, *c, hint),
        Command::VirtualReplaceChar(count, c) => {
            plan_virtual_replace_char(st, b, cur, *count, *c, hint)
        }
        _ => unreachable!("plan_replace_typed plans only the Replace-mode typing commands"),
    }
}

/// A char typed in Replace mode: overwrite the char under the cursor, or append at the line end.
fn replace_type(b: &[u8], cur: usize, hint: GroupHint, c: &char) -> Plan {
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes().to_vec();
    let tn = typed.len();
    let le = line_end(b, cur);
    let (edits, push) = if cur < le {
        // Overwrite the char under the cursor; remember its original bytes for `<BS>` restore.
        let nb = next_boundary(b, cur);
        (
            one(Edit::replace(BytePos(cur), nb - cur, typed)),
            vec![Some(b[cur..nb].to_vec())],
        )
    } else {
        // At end-of-line there is nothing to overwrite: append, remembered as `None` (backspace deletes).
        (one(Edit::insert(BytePos(cur), typed)), vec![None])
    };
    Plan {
        action: Action::ReplaceTxn {
            edits,
            hint,
            push,
            pop: 0,
        },
        is_edit: true,
        ..nop(cur + tn, Mode::Replace)
    }
}

/// `<CR>` in Replace (`R`) and Virtual Replace (`gR`) mode, nvim's `ins_eol` → `open_line`: the new line
/// takes the autoindent, and every char it puts before the cursor goes on the `<BS>` stack, so `<BS>`
/// joins the line back and restores what the break took (#727).
///
/// `R` splits the line at the cursor; the blanks after it go (they come back with the line break). `gR`
/// keeps the text below in place: the rest of the line is deleted and replacing goes on at the start of
/// the next line, the indent overwriting its first chars; only on the last line is a line added.
fn replace_newline(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    let mode = st.view.mode;
    let (ls, le) = (line_start(b, cur), line_end(b, cur));
    let indents = auto_indents(st);
    let pad = if indents {
        st.view
            .indent
            .open_indent(b, ls, cur, true, &st.view.format.cinwords)
            .0
    } else {
        Vec::new()
    };
    let mut ins = Vec::with_capacity(pad.len() + 1);
    ins.push(b'\n');
    ins.extend_from_slice(&pad);
    let cursor = cur + ins.len();
    // `<BS>` over the break puts back the text the edit took from `[cur, end)` other than the break
    // itself; each indent char overwrote one of the next line's chars (`gR`) or was added.
    let (end, broken, mut push): (usize, Vec<u8>, Vec<Option<Vec<u8>>>) =
        if mode == Mode::VirtualReplace && le + 1 < b.len() {
            let mut end = le + 1;
            let mut push = Vec::with_capacity(pad.len() + 1);
            push.push(None);
            for _ in &pad {
                if end < b.len() && b[end] != b'\n' {
                    let nb = next_boundary(b, end);
                    push.push(Some(b[end..nb].to_vec()));
                    end = nb;
                } else {
                    push.push(None);
                }
            }
            (end, b[cur..=le].to_vec(), push)
        } else if mode == Mode::VirtualReplace {
            (le, b[cur..le].to_vec(), vec![None; pad.len() + 1])
        } else {
            let mut after = cur;
            while indents && after < le && matches!(b[after], b' ' | b'\t') {
                after += 1;
            }
            (after, b[cur..after].to_vec(), vec![None; pad.len() + 1])
        };
    if !broken.is_empty() {
        push[0] = Some(broken);
    }
    Plan {
        action: Action::ReplaceTxn {
            edits: one(Edit::replace(BytePos(cur), end - cur, ins)),
            hint,
            push,
            pop: 0,
        },
        is_edit: true,
        ..nop(cursor, mode)
    }
}

/// Shared by Replace (`R`) and Virtual Replace (`gR`) — the restore stack is identical; the mode is
/// preserved so `<BS>` stays in whichever replace mode is active.
///
/// In the indent `'smarttab'` steps back to the previous shiftwidth stop as in Insert (nvim's `ins_bs`),
/// taking back every char it crosses; a char typed before the run began (a caret jump, the session start)
/// is crossed but kept.
fn replace_backspace(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
    let stack = &st.view.replace_stack;
    if stack.is_empty() {
        // At the session start `<BS>` only moves the cursor left (Vim does not restore past the start).
        let ls = line_start(b, cur);
        let c = if cur > ls { prev_grapheme(b, cur) } else { cur };
        return nop(c, st.view.mode);
    }
    let ls = line_start(b, cur);
    let from = match st.view.indent.backspace(b, ls, cur, st.view.typed_space) {
        // A step that lands between two stops would retype spaces; that rare case takes one char.
        Some((from, 0)) => from,
        _ => prev_boundary(b, cur),
    };
    // The chars in `[from, cur)`, last first, each paired with the newest entry not yet used.
    let mut back = Vec::new();
    let mut at = cur;
    while at > from {
        let start = prev_boundary(b, at);
        back.push(start..at);
        at = start;
    }
    let pop = back.len().min(stack.len());
    let mut restored = Vec::new();
    for (k, span) in back.iter().enumerate().rev() {
        match stack.len().checked_sub(k + 1) {
            Some(i) => restored.extend_from_slice(stack[i].as_deref().unwrap_or_default()),
            None => restored.extend_from_slice(&b[span.clone()]),
        }
    }
    let to = if st.view.mode == Mode::VirtualReplace {
        vreplace_unpad(b, ls, from, cur, &restored, st.view.indent.tabs())
    } else {
        cur
    };
    Plan {
        action: Action::ReplaceTxn {
            edits: one(Edit::replace(BytePos(from), to - from, restored)),
            hint,
            push: Vec::new(),
            pop,
        },
        is_edit: true,
        ..nop(from, st.view.mode)
    }
}

/// The cells `text` takes when it starts at virtual column `v`.
fn cells_from(text: &[u8], v: usize, ts: Tabs) -> usize {
    let (mut i, mut at) = (0, v);
    while i < text.len() {
        let (j, w) = cluster_cells(text, i, text.len(), at, ts);
        at += w;
        i = j;
    }
    at - v
}

/// Where the Virtual Replace `<BS>` that puts `restored` back in place of `[from, cur)` ends: past the
/// spaces after `cur` that the typed text added to fill the cells of the wider text it took, so the text
/// after it keeps its column (nvim's `replace_do_bs`, #1269).
fn vreplace_unpad(
    b: &[u8],
    ls: usize,
    from: usize,
    cur: usize,
    restored: &[u8],
    ts: Tabs,
) -> usize {
    let tab = b[from..cur].contains(&b'\t') || restored.contains(&b'\t');
    let v = if tab {
        motion::vcol_of(b, ls, from, ts)
    } else {
        0
    };
    let mut typed = cells_from(&b[from..cur], v, ts);
    let put_back = cells_from(restored, v, ts);
    let le = line_end(b, cur);
    let mut to = cur;
    while put_back > typed && to < le && b[to] == b' ' {
        to += 1;
        typed += 1;
    }
    to
}

/// What Virtual Replace takes for `typed`, typed at `p` (virtual column `v`) of a line ending at `le`
/// (nvim's `ins_char_bytes` under VREPLACE, #1269): the end of the chars whose cells it covers, and the
/// spaces that fill the cells the last of them leaves. A `<Tab>` that reaches past the typed char stays (it
/// shrinks), so typing over a tab inserts before it until its last cell.
fn vreplace_span(
    line: &[u8],
    p: usize,
    le: usize,
    v: usize,
    typed: &[u8],
    ts: Tabs,
) -> (usize, usize) {
    let want = v + cells_from(typed, v, ts);
    let (mut end, mut at, mut pad) = (p, v, 0);
    while end < le && at < want {
        let (j, w) = cluster_cells(line, end, le, at, ts);
        at += w;
        if at > want && line[end] == b'\t' {
            break;
        }
        end = j;
        pad += at.saturating_sub(want);
    }
    (end, pad)
}

/// `gR`-mode typing (Virtual Replace): the typed char takes the chars its cells cover, spaces filling what
/// a wider char leaves; over a multi-column tab it goes in before the tab (which shrinks) until the tab's
/// last column; at end-of-line it is appended. What it took is remembered for `<BS>` ([`vreplace_span`]).
fn plan_virtual_replace_type(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    c: char,
    hint: GroupHint,
) -> Plan {
    let mut buf = [0u8; 4];
    let typed = c.encode_utf8(&mut buf).as_bytes();
    let tn = typed.len();
    let (ls, le) = (line_start(b, cur), line_end(b, cur));
    let ts = st.view.indent.tabs();
    // The virtual column matters only to a tab, typed or taken: skip the scan from the line start without
    // one (a keystroke on a long line stays cheap).
    let (mut end, mut pad) = vreplace_span(b, cur, le, 0, typed, ts);
    if typed == b"\t" || b[cur..(end + 1).min(le)].contains(&b'\t') {
        let v = motion::vcol_of(b, ls, cur, ts);
        (end, pad) = vreplace_span(b, cur, le, v, typed, ts);
    }
    let mut ins = typed.to_vec();
    ins.resize(tn + pad, b' ');
    // Nothing taken (end-of-line, a tab that shrinks): `<BS>` deletes the typed char.
    let push = vec![(end > cur).then(|| b[cur..end].to_vec())];
    Plan {
        action: Action::ReplaceTxn {
            edits: one(Edit::replace(BytePos(cur), end - cur, ins)),
            hint,
            push,
            pop: 0,
        },
        is_edit: true,
        ..nop(cur + tn, Mode::VirtualReplace)
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
    // A count of 0 never comes from a key (`0` is a motion; the engine sends at least 1), but a replayed
    // trace can carry one: plan it as 1, like the other counted commands, so the loop below always runs
    // and the cursor arithmetic after it cannot underflow (#1347).
    let count = count.max(1);
    let tabw = st.view.indent.tabs();
    let ls = line_start(b, cur);
    let le = line_end(b, cur);
    // Work on an owned copy of the whole line so tab virtual-column math sees the evolving text; positions
    // are relative to `ls`. Only bytes from the cursor onward are ever touched.
    let mut line: Vec<u8> = b[ls..le].to_vec();
    let start = cur - ls; // cursor offset within `line`
    let mut p = start;
    let mut v = motion::vcol_of(&line, 0, p, tabw);
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
        }
        // Take what the typed char covers, a tab shrinking and spaces filling a cut wider char, as `gR`
        // does ([`vreplace_span`]); the next one goes on at the first of those spaces.
        let (end, pad) = vreplace_span(&line, p, line.len(), v, typed, tabw);
        let spaces = std::iter::repeat_n(b' ', pad);
        line.splice(p..end, typed.iter().copied().chain(spaces));
        v += cells_from(typed, v, tabw);
        p += tn;
    }
    // One replace of the line tail from the cursor; everything before `start` is unchanged.
    let new_tail = line[start..].to_vec();
    // Vim leaves Replace nudging left one boundary → the cursor rests on the LAST typed char at `ls + p - tn`
    // (`count >= 1` and p advances by at least one `tn` per iteration, so this never precedes the line start).
    let cursor = ls + p - tn;
    edit(
        one(Edit::replace(BytePos(cur), le - cur, new_tail)),
        cursor,
        Mode::Normal,
        hint,
    )
}
