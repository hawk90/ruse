//! `%` bracket matching, `[(` / `[{` unmatched brackets and `[m` / `]M` method motions.

use super::boundary::{next_boundary, prev_boundary};
use super::Motion;

/// The matching-bracket position for `%`, or `None` if there is nothing to match. If the byte at `cursor`
/// is a bracket, its pair is found (nesting-aware, possibly on another line); otherwise the first bracket
/// forward on the current line is matched (Vim). Brackets are ASCII, so byte scanning stays on boundaries.
pub(super) fn match_bracket(b: &[u8], cursor: usize) -> Option<usize> {
    // nvim's `findmatchlimit` (#1163): quotes, character literals, escapes, comments, `#if` chains and
    // the 'lisp' rules.
    crate::findmatch::percent(b, cursor)
}

/// The `count`-th UNMATCHED bracket of one type from the cursor — Vim `[(`/`])` (parens) and `[{`/`]}`
/// (braces). `forward` scans right from `cursor+1` for an unmatched CLOSE (`])`/`]}`); otherwise it scans
/// left from `cursor-1` for an unmatched OPEN (`[(`/`[{`). Nesting is same-type: a balanced pair is skipped
/// (a `)` seen while scanning left raises the depth so its matching `(` is not counted), and other bracket
/// types are ignored — exactly as Vim's `[(` skips `{}`/`[]`. Returns `cursor` (a no-op) when there is no
/// such unmatched bracket. Brackets are ASCII → the result is always a char boundary.
pub(super) fn unmatched_bracket(
    b: &[u8],
    cursor: usize,
    open: u8,
    close: u8,
    forward: bool,
    count: u32,
) -> usize {
    let mut remaining = count.max(1);
    let mut depth = 0u32;
    if forward {
        let mut i = cursor.saturating_add(1); // strictly after the cursor (an on-cursor close does not count)
        while i < b.len() {
            if b[i] == open {
                depth += 1;
            } else if b[i] == close {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        return i;
                    }
                } else {
                    depth -= 1;
                }
            }
            i += 1;
        }
        cursor
    } else {
        let mut i = cursor.min(b.len()); // decremented before each read → starts strictly before the cursor
        while i > 0 {
            i -= 1;
            if b[i] == close {
                depth += 1;
            } else if b[i] == open {
                if depth == 0 {
                    remaining -= 1;
                    if remaining == 0 {
                        return i;
                    }
                } else {
                    depth -= 1;
                }
            }
        }
        cursor
    }
}

/// `findmatchlimit` restricted to the two uses Vim's `nv_bracket_block` makes of it for the method
/// motions: find the single enclosing UNMATCHED brace of the class the cursor is in. `[m`/`[M` search
/// BACKWARD for an unmatched `{`; `]m`/`]M` search FORWARD for an unmatched `}`. Returns `None` when
/// there is no such brace (i.e. `unmatched_bracket` reports a no-op by returning `cursor`).
fn find_enclosing_brace(b: &[u8], cursor: usize, forward: bool) -> Option<usize> {
    let t = unmatched_bracket(b, cursor, b'{', b'}', forward, 1);
    if t == cursor {
        None
    } else {
        Some(t)
    }
}

/// Vim `]m`/`[m`/`]M`/`[M` — the method (brace-block) motions. This is a faithful port of Vim's
/// `nv_bracket_block` (`normal.c`): ruse has NO language model, so the "method" is defined purely by
/// brace structure, exactly as Vim's own algorithm defines it via `findmatchlimit` over `{`/`}`.
///
/// `forward` is true for `]m`/`]M` (search right), false for `[m`/`[M`. `end` is true for `]M`/`[M`
/// (the method END, a `}`), false for `]m`/`[m` (the method START, a `{`).
///
/// The algorithm has two phases, mirroring Vim:
///   * Phase 1 walks OUT of the cursor's nesting to the outermost enclosing brace (`new_pos`), keeping
///     the one level in (`prev_pos`), by repeatedly finding the enclosing unmatched `{` (backward cmds)
///     or `}` (forward cmds).
///   * Phase 2 does a directional char-by-char scan honoring `norm` (true for `[m`/`]M`) to land on the
///     wanted class/method boundary, jumping over other methods via their matching brace.
///
/// Returns the target byte offset, or `cursor` for a no-op / error (Vim beeps and does not move). The
/// result is always an ASCII brace position → a char boundary.
///
/// DIVERGENCE (documented non-goal): braces inside string literals and comments are treated literally.
/// nvim's `findmatch` skips them; ruse has no syntax model, so `]m` over a buffer with a `}` inside a
/// string will stop on that `}`. Verified-identical everywhere else against nvim v0.12.4.
pub(super) fn method_target(
    b: &[u8],
    cursor: usize,
    forward: bool,
    end: bool,
    count: u32,
) -> usize {
    let old_pos = cursor.min(b.len());
    // findc: '{' for the backward commands, '}' for the forward commands (Vim: cmdchar '[' vs ']').
    let findc_is_open = !forward;
    // norm is TRUE for "[m" and "]M": ((findc == '{') == (nchar == 'm')).
    // nchar == 'm' ⇔ !end. So norm = (findc_is_open == !end).
    let norm = findc_is_open == !end;

    // --- Phase 1: walk out to the outermost enclosing brace. ---
    let mut new_pos: Option<usize> = None;
    let mut prev_pos: Option<usize> = None;
    let mut cur = old_pos;
    // Vim loops 9999 times; nesting depth is bounded by buffer length, so an unbounded walk-out is fine.
    while let Some(pos) = find_enclosing_brace(b, cur, forward) {
        prev_pos = new_pos;
        cur = pos;
        new_pos = Some(pos);
    }

    // --- Phase 2: directional scan for the target boundary. ---
    let mut n = count.max(1) as i64;
    let mut pos: Option<usize>;
    if let Some(pp) = prev_pos {
        // We were inside a method: start from the brace one level inside the class.
        pos = Some(pp);
        cur = pp;
        if norm {
            n -= 1;
        }
    } else {
        pos = None;
        cur = old_pos;
    }

    while n > 0 {
        loop {
            // findc == '{' (backward cmds) scans backward; else forward. Move one char boundary.
            let moved = if findc_is_open {
                if cur == 0 {
                    None
                } else {
                    Some(prev_boundary(b, cur))
                }
            } else {
                let nx = next_boundary(b, cur);
                if nx >= b.len() {
                    None
                } else {
                    Some(nx)
                }
            };
            let Some(nc) = moved else {
                // Ran off the buffer edge.
                if pos.is_none() {
                    return old_pos; // error: nothing found, no move.
                }
                n = 0;
                break;
            };
            cur = nc;
            let c = b[cur];
            if c == b'{' || c == b'}' {
                if (c == if findc_is_open { b'{' } else { b'}' } && norm) || (n == 1 && !norm) {
                    // Found the class boundary we want, or the count-th method boundary.
                    new_pos = Some(cur);
                    pos = Some(cur);
                    n = 0;
                } else if new_pos.is_none() {
                    // Started outside any class and just crossed in — use this boundary.
                    new_pos = Some(cur);
                    pos = Some(cur);
                } else {
                    // Found the start/end of ANOTHER method: jump to its matching brace and go on.
                    match find_enclosing_brace(b, cur, forward) {
                        Some(m) => {
                            pos = Some(m);
                            cur = m;
                        }
                        None => {
                            pos = None;
                            n = 0;
                        }
                    }
                }
                break;
            }
        }
        n -= 1;
    }

    match pos {
        Some(p) => p,
        None => old_pos, // error (Vim beeps): stay put.
    }
}

/// Whether a motion is one of the method (brace-block) family, with `(forward, end)` flags.
pub(super) fn method_spec(m: Motion) -> Option<(bool, bool)> {
    match m {
        Motion::MethodStartFwd => Some((true, false)),
        Motion::MethodStartBack => Some((false, false)),
        Motion::MethodEndFwd => Some((true, true)),
        Motion::MethodEndBack => Some((false, true)),
        _ => None,
    }
}

/// The (open, close, forward) triple for an unmatched-bracket motion, or `None` for any other motion.
pub(super) fn unmatched_spec(m: Motion) -> Option<(u8, u8, bool)> {
    match m {
        Motion::UnmatchedParenBack => Some((b'(', b')', false)),
        Motion::UnmatchedParenFwd => Some((b'(', b')', true)),
        Motion::UnmatchedBraceBack => Some((b'{', b'}', false)),
        Motion::UnmatchedBraceFwd => Some((b'{', b'}', true)),
        _ => None,
    }
}

#[cfg(test)]
mod method_motion_tests {
    //! Vim `]m`/`[m`/`]M`/`[M` — method (brace-block) motions. Every expected byte offset is pinned to
    //! nvim v0.12.4 (captured headless: cursor set by byte offset, `normal! <keys>`, cursor read back as a
    //! byte offset). A faithful port of Vim's `nv_bracket_block`; see `method_target`.
    use crate::motion::{target, Motion};

    // A class with two methods. Byte map (nvim-verified landing offsets reference these):
    //   byte  0 R1: "class Foo {"      → open-brace at byte 10
    //   byte 12 R2: "    void a() {"   → open-brace at byte 25
    //   byte 27 R3: "        x;"       → `x` at byte 35
    //   byte 38 R4: "    }"            → close-brace at byte 42
    //   byte 44 R5: "    void b() {"   → open-brace at byte 57
    //   byte 59 R6: "        y;"
    //   byte 70 R7: "    }"            → close-brace at byte 74
    //   byte 76 R8: "}"                → close-brace at byte 76
    const CLASS: &[u8] =
        b"class Foo {\n    void a() {\n        x;\n    }\n    void b() {\n        y;\n    }\n}\n";
    // A flat sequence of top-level `{}` blocks.
    //   byte  0 R1: "void a() {"  → open-brace at byte 9
    //   byte 11 R2: "    x;"
    //   byte 18 R3: "}"           → close-brace at byte 18
    //   byte 20 R4: "void b() {"  → open-brace at byte 29
    //   byte 38 R6: "}"
    const FLAT: &[u8] = b"void a() {\n    x;\n}\nvoid b() {\n    y;\n}\n";

    fn t(b: &[u8], cur: usize, m: Motion, count: u32) -> usize {
        target(b, cur, m, count)
    }

    #[test]
    fn class_forward_start_walks_every_open_then_the_top_close() {
        // `]m` visits each open-brace in forward order, then (no more open-brace) the class's top-level close-brace.
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 1),
            10,
            "]m → class open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 2),
            25,
            "2]m → method a open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 3),
            57,
            "3]m → method b open-brace"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodStartFwd, 4),
            76,
            "4]m → class close close-brace"
        );
        // A count past the last boundary fails the whole motion → no move (Vim beeps).
        assert_eq!(t(CLASS, 0, Motion::MethodStartFwd, 5), 0, "5]m → no-op");
    }

    #[test]
    fn class_forward_end_walks_every_close_then_stops() {
        // `]M` visits each method-end close-brace, plus the top-level open-brace as the class boundary.
        assert_eq!(
            t(CLASS, 0, Motion::MethodEndFwd, 1),
            10,
            "]M → class open-brace (class boundary)"
        );
        assert_eq!(
            t(CLASS, 0, Motion::MethodEndFwd, 2),
            42,
            "2]M → method a close-brace"
        );
    }

    #[test]
    fn from_inside_a_method_both_directions() {
        // Cursor on `x` (byte 35), inside method a's body.
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartFwd, 1),
            57,
            "]m → method b open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartBack, 1),
            25,
            "[m → method a open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodEndFwd, 1),
            42,
            "]M → method a close-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodEndBack, 1),
            10,
            "[M → class open-brace"
        );
        assert_eq!(
            t(CLASS, 35, Motion::MethodStartBack, 2),
            10,
            "2[m → class open-brace"
        );
    }

    #[test]
    fn flat_top_level_blocks() {
        // Flat blocks: `]m` reaches the first open-brace, then that block's top-level close-brace, then fails (Vim treats
        // the level-0 close-brace as end-of-class, so it does not roll on into the next block).
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 1),
            9,
            "]m → first open-brace"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 2),
            18,
            "2]m → its top-level close-brace"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodStartFwd, 3),
            0,
            "3]m → no-op (past class end)"
        );
        assert_eq!(
            t(FLAT, 0, Motion::MethodEndFwd, 1),
            9,
            "]M → first open-brace (class boundary)"
        );
    }

    #[test]
    fn no_braces_is_a_noop() {
        let b = b"hello world\nno braces here\n";
        assert_eq!(t(b, 0, Motion::MethodStartFwd, 1), 0);
        assert_eq!(t(b, 0, Motion::MethodEndBack, 1), 0);
        assert_eq!(t(b, 5, Motion::MethodStartBack, 1), 5);
    }
}
