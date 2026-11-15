//! The Emacs profile's commands: the depth-1 region ops (D-027) and the distinct Emacs commands (D-051),
//! planned by [`plan_emacs`]. Moved out of `planner/mod.rs` (#1350).

use super::*;

/// A non-editing move (like [`nop`]) that also SETS the Emacs mark at `mark`. Shared by the mark commands
/// (`SetMark` / `ExchangePointMark` / `EmacsMarkWord` / `EmacsBufferEdge`).
fn nop_mark(cursor: usize, mode: Mode, mark: usize) -> Plan {
    Plan {
        set_mark: Some(MarkWrite::Set(mark)),
        ..nop(cursor, mode)
    }
}

fn edit_kill(edits: EditList, cursor: usize, mode: Mode, hint: GroupHint, reg: Register) -> Plan {
    Plan {
        action: Action::Txn { edits, hint },
        is_edit: true,
        set_register: Some(RegWrite::KillAppend(reg)),
        ..nop(cursor, mode)
    }
}

/// The first index `<= from` such that `b[i..from]` is all horizontal whitespace (scan left).
fn hspace_start(b: &[u8], from: usize) -> usize {
    let mut s = from;
    while s > 0 && is_hspace(b[s - 1]) {
        s -= 1;
    }
    s
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

/// Plan the Emacs-profile commands split out of [`plan`]: the depth-1 region ops (D-027 —
/// SetMark / KillRegion / CopyRegion / ExchangePointMark) and the distinct Emacs commands (D-051).
/// The core stays profile-agnostic; the profile decides which of these its keymap resolves to.
pub(super) fn plan_emacs(
    st: &EditorState,
    b: &[u8],
    cur: usize,
    hint: GroupHint,
    cmd: &Command,
) -> Plan {
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
                    set_register: Some(RegWrite::Yank(reg, (s, e))),
                    ..nop(cur, st.view.mode)
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
                st.view.indent.tabs(),
            );
            if !matches!(p.action, Action::Nop) {
                p.set_mark = Some(MarkWrite::Set(cur));
            }
            p
        }
        Command::EmacsKillLine => kill_line(st, b, cur, hint),
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
                    is_edit: true,
                    set_register: Some(RegWrite::KillPrepend(reg)),
                    ..nop(target, st.view.mode)
                }
            }
        }
        Command::EmacsTransposeChars => transpose_chars(st, b, cur, hint),
        Command::EmacsTransposeWords => transpose_words(st, b, cur, hint),
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
                        is_edit: true,
                        ..nop(cursor, st.view.mode)
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
        Command::EmacsCaseRegion { case } => case_region(st, b, cur, hint, case),
        Command::EmacsDeleteIndentation => delete_indentation(st, b, cur, hint),
        // `M-<` / `M->` (Emacs beginning/end-of-buffer, D-051): move point to the ABSOLUTE buffer start/end
        // (not Vim `gg`/`G`'s first-non-blank line) and PUSH the mark at the old point.
        Command::EmacsBufferEdge { start } => {
            nop_mark(if *start { 0 } else { b.len() }, st.view.mode, cur)
        }
        _ => unreachable!("plan_emacs handles only Emacs-profile commands"),
    }
}

/// `C-k` (Emacs kill-line, D-051): kill from point into the register. With text before the line end,
/// kill that text (NOT the newline). With point already at the line end, kill the terminating newline
/// instead — joining the next line up. At end-of-buffer (no newline to take) it is inert. This is the
/// no-prefix `kill-whole-line`-nil default; unlike Vim `D` (`Delete(1, LineEnd)`) it never no-ops at EOL.
fn kill_line(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
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

/// `C-t` (Emacs transpose-chars, D-051): swap the char before point with the char at point, then
/// advance point past the pair. `j` is the right char (it slides left), `i = j - 1` the left char;
/// at end of line Emacs steps back one so the two chars ENDING the line are transposed. Inert when the
/// line has no pair to transpose (point at buffer/line start, or a one-char line). No kill-ring write.
fn transpose_chars(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
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
            is_edit: true,
            ..nop(j + 1, st.view.mode)
        },
        _ => nop(cur, st.view.mode),
    }
}

/// `M-t` (Emacs transpose-words, D-051): swap the word before/around point with the following word,
/// preserving the separator, and leave point past the moved second word. Mirrors Emacs `transpose-subr`
/// for `forward-word`: `start1` = backward-word, `end1` = forward-word from there, `end2` =
/// forward-word again, `start2` = backward-word from there. Inert without a real pair. No kill write.
fn transpose_words(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
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

/// `M-^` (Emacs delete-indentation / join-line, D-051): join this line to the previous one — delete
/// the preceding newline plus the previous line's trailing whitespace and this line's leading
/// indentation, then fix up whitespace to ONE space, or NONE when the join lands at beginning-of-line
/// (an empty previous line). Point rests at the join. No kill-ring write. Distinct from Vim `J`.
fn delete_indentation(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint) -> Plan {
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

/// `C-x C-u` / `C-x C-l` (Emacs upcase-/downcase-/capitalize-region, D-051): recase the active region
/// `[min(point,mark), max)`. When the recasing changes the length (`ß` → `SS`), point at the region
/// end moves with the text (casefiddle.c adjusts it), while the mark keeps its CHARACTER offset
/// (`replace_range_2` leaves markers alone): Emacs 31.1 upcases "straße" with point at 0 and the
/// mark at 6 to "STRASSE" with the mark still at 6. Inert without a mark. No kill write.
fn case_region(st: &EditorState, b: &[u8], cur: usize, hint: GroupHint, case: &WordCase) -> Plan {
    match active_region(st, cur) {
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
                            new_end + tail.char_indices().nth(past).map_or(tail.len(), |(i, _)| i)
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
                    is_edit: true,
                    set_mark,
                    ..nop(follow(cur), st.view.mode)
                }
            }
            None => nop(cur, st.view.mode),
        },
        None => nop(cur, st.view.mode),
    }
}
