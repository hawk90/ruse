//! Vertical moves and line positions: `j` / `k`, first and last non-blank, line jumps, the phantom
//! slot after a final newline.

use crate::cells::Tabs;
use crate::pos::raw::{line_end, line_start};

use super::boundary::{at_bol, at_col, col_of, next_boundary, prev_boundary, prev_grapheme};

pub(super) fn up(b: &[u8], cur: usize) -> usize {
    let ls = line_start(b, cur);
    if ls == 0 {
        cur
    } else {
        at_col(b, line_start(b, ls - 1), col_of(b, ls, cur))
    }
}

/// The byte offset just past the last line that holds content. A buffer ending in `\n` has that newline as
/// the last line's TERMINATOR, not the start of a new empty line (Vim's model), so the trailing (phantom)
/// slot at `b.len()` is excluded. Vertical / `G` / `+`-family motions never step past this — `G` on
/// `"abc\n"` lands on `abc`, not on a blank line below it. Equals `b.len()` when there is no trailing `\n`.
pub(crate) fn content_end(b: &[u8]) -> usize {
    if b.last() == Some(&b'\n') {
        b.len() - 1
    } else {
        b.len()
    }
}

/// Pull an on-char (Vim Normal/Visual) caret off the phantom slot after a buffer's final newline (#677).
/// That slot is not a line in Vim's model, so a cursor left there by a motion running off the end (`w`,
/// `}`, `)`, `g_`, …), a linewise delete of the last line, or an undo would make the next typed text land
/// on an extra unterminated line. It moves onto the last line's last char (its newline when that line is
/// empty), where Vim leaves it. Any other position, and every position of an unterminated buffer, is
/// returned unchanged.
#[must_use]
pub fn off_phantom(b: &[u8], pos: crate::pos::BytePos) -> crate::pos::BytePos {
    crate::pos::BytePos(off_phantom_at(b, pos.0))
}

/// [`off_phantom`] over a bare offset, for the core's own motions.
#[must_use]
pub(crate) fn off_phantom_at(b: &[u8], pos: usize) -> usize {
    if pos < b.len() || b.last() != Some(&b'\n') {
        return pos;
    }
    let le = b.len() - 1;
    if !at_bol(b, le) {
        prev_grapheme(b, le)
    } else {
        le
    }
}

pub(super) fn down(b: &[u8], cur: usize) -> usize {
    let le = line_end(b, cur);
    // Stop at the last content line: `le >= content_end` covers both a no-newline last line (`le == len`)
    // and a trailing-newline last line (`le == len-1`), so `j` never lands on the phantom empty slot.
    if le >= content_end(b) {
        cur
    } else {
        at_col(b, le + 1, col_of(b, line_start(b, cur), cur))
    }
}

/// Vertical motion (`j`/`k`) that honours a STICKY desired column (`want`, Vim `curswant`) instead of the
/// cursor's current column, so moving through a shorter interior line does not collapse the column: `want`
/// is clamped to each line's length by [`at_col`] but never forgotten. `want == usize::MAX` (set by `$`)
/// rides each line's end. `down` picks the direction; `count` repeats. `want` is a virtual (screen) column,
/// so a `<Tab>` (`ts`) or a wide char counts its cells, as nvim's `curswant` does (#1090).
pub(crate) fn vmove(
    b: &[u8],
    cursor: usize,
    count: u32,
    down: bool,
    want: usize,
    ts: Tabs,
) -> usize {
    let mut c = cursor.min(b.len());
    for _ in 0..count.max(1) {
        if down {
            let le = line_end(b, c);
            if le >= content_end(b) {
                break; // don't descend onto the phantom line after a trailing '\n'
            }
            c = crate::editor::want_col(b, le + 1, want, ts);
        } else {
            let ls = line_start(b, c);
            if ls == 0 {
                break;
            }
            c = crate::editor::want_col(b, line_start(b, ls - 1), want, ts);
        }
    }
    c
}

/// First non-blank (non-space, non-tab) byte position on the line containing `line_start_pos`, or the line
/// start itself if the whole line is blank. Where `gg`/`G` land (Vim).
pub(crate) fn first_non_blank(b: &[u8], line_start_pos: usize) -> usize {
    let ls = line_start(b, line_start_pos);
    let le = line_end(b, ls);
    let mut i = ls;
    while i < le && (b[i] == b' ' || b[i] == b'\t') {
        i = next_boundary(b, i);
    }
    i
}

/// The last non-blank char of the line containing `pos` (Vim `g_`), or the line start when the line is
/// blank/empty. Lands ON the char (like `$`), not past it.
pub(super) fn last_non_blank(b: &[u8], pos: usize) -> usize {
    let ls = line_start(b, pos);
    let le = line_end(b, ls);
    let mut i = le;
    while i > ls {
        let p = prev_grapheme(b, i);
        if b[p] != b' ' && b[p] != b'\t' {
            return p;
        }
        i = p;
    }
    ls
}

/// Byte start of the `n`-th line (1-based), clamped to the last line.
pub(crate) fn nth_line_start(b: &[u8], n: u32) -> usize {
    let target = n.max(1) - 1; // 0-based
    let mut seen = 0u32;
    let mut ls = 0usize;
    while seen < target {
        let le = line_end(b, ls);
        if le >= content_end(b) {
            break; // no more content lines; clamp to the last real one (never the trailing phantom)
        }
        ls = le + 1;
        seen += 1;
    }
    ls
}

/// Byte start of the last content line (the trailing phantom slot after a final `\n` is not a line).
pub(crate) fn last_line_start(b: &[u8]) -> usize {
    line_start(b, content_end(b))
}

/// The start of the line `n` lines below `pos`'s line, stopping at the last line. A trailing newline
/// does not open a further line (the terminator model).
pub(super) fn lines_down(b: &[u8], pos: usize, n: u32) -> usize {
    let mut ls = line_start(b, pos);
    for _ in 0..n {
        let le = line_end(b, ls);
        if le + 1 >= b.len() {
            break;
        }
        ls = le + 1;
    }
    ls
}

/// One bare `<Space>` step (`'whichwrap'` `s`): the next char on the line, else the first char of the next
/// line (an empty line is a stop of its own). The buffer's last char stays put.
pub(super) fn wrap_right(b: &[u8], c: usize) -> usize {
    let le = line_end(b, c);
    let n = next_boundary(b, c);
    if n < le {
        n
    } else if le + 1 < b.len() {
        le + 1
    } else {
        c
    }
}

/// One bare `<BS>` step (`'whichwrap'` `b`): the previous char on the line, else the last char of the
/// previous line (its newline when that line is empty). The buffer's first char stays put.
pub(super) fn wrap_left(b: &[u8], c: usize) -> usize {
    let ls = line_start(b, c);
    if c > ls {
        return prev_boundary(b, c);
    }
    if ls == 0 {
        return c;
    }
    let prev_nl = ls - 1;
    if prev_nl > line_start(b, prev_nl) {
        prev_boundary(b, prev_nl)
    } else {
        prev_nl
    }
}

#[cfg(test)]
mod gunderscore_column_tests {
    //! Vim `g_` (last non-blank) and `|` (go to column).
    use crate::motion::{char_span, target, Motion};

    #[test]
    fn g_underscore_lands_on_last_non_blank() {
        let b = b"  foo bar  \nx\n"; // line 0: 'r' is the last non-blank at index 8
        assert_eq!(target(b, 0, Motion::LineLastNonBlank, 1), 8);
        assert_eq!(
            target(b, 8, Motion::LineLastNonBlank, 1),
            8,
            "idempotent on the char"
        );
        // `dg_` is inclusive: from 'f' (index 2) through 'r' → "foo bar".
        assert_eq!(char_span(b, 2, Motion::LineLastNonBlank, 1), (2, 9));
    }

    #[test]
    fn pipe_goes_to_the_count_th_column() {
        let b = b"abcdef\n";
        assert_eq!(target(b, 0, Motion::Column, 1), 0, "1| = column 1");
        assert_eq!(target(b, 0, Motion::Column, 5), 4, "5| = column 5");
        // Past the line end clamps to the last column.
        assert_eq!(target(b, 0, Motion::Column, 99), 6);
        // `d5|` from column 1 spans the ordered pair [0, 4).
        assert_eq!(char_span(b, 0, Motion::Column, 5), (0, 4));
    }

    #[test]
    fn gm_lands_at_the_percent_char_of_the_line() {
        // Landings VERIFIED against nvim v0.12.4 (`gM` = 50% by char count; 0-based index = 50*L/100).
        // "abcdefghij" (L=10): bare gM (count 50) → index 5 ('f').
        let b = b"abcdefghij\n";
        assert_eq!(target(b, 0, Motion::MidLine, 50), 5, "gM = middle char");
        assert_eq!(target(b, 9, Motion::MidLine, 50), 5, "gM ignores start col");
        // `{count}gM` as a percentage (nvim: 25gM→col3/idx2, 100gM→last/idx9, 10gM→idx1, 1gM→idx0).
        assert_eq!(target(b, 0, Motion::MidLine, 25), 2, "25gM");
        assert_eq!(
            target(b, 0, Motion::MidLine, 100),
            9,
            "100gM clamps to last char"
        );
        assert_eq!(target(b, 0, Motion::MidLine, 10), 1, "10gM");
        assert_eq!(target(b, 0, Motion::MidLine, 1), 0, "1gM");
        // Odd length floors: "abcde" (L=5) → 50*5/100 = 2 (nvim col 3).
        let odd = b"abcde\n";
        assert_eq!(
            target(odd, 0, Motion::MidLine, 50),
            2,
            "gM on a 5-char line"
        );
        // Single char / empty line: stay put at the line start.
        assert_eq!(target(b"a\n", 0, Motion::MidLine, 50), 0);
        assert_eq!(target(b"\n", 0, Motion::MidLine, 50), 0, "empty line");
        // `dgM` is exclusive charwise (nvim: `0dgM` on "abcdefghij" → "fghij" = span [0,5); `$dgM` → [5,9)).
        assert_eq!(char_span(b, 0, Motion::MidLine, 50), (0, 5));
        assert_eq!(char_span(b, 9, Motion::MidLine, 50), (5, 9));
        // gM counts leading whitespace too: "    abcdef" (L=10) → idx 5 (nvim col 6).
        assert_eq!(target(b"    abcdef\n", 0, Motion::MidLine, 50), 5);
    }
}

#[cfg(test)]
mod line_first_non_blank_motion_tests {
    //! Vim `+` / `<CR>` (down), `-` (up), `_` (count-1 down) — line motions landing on the first
    //! non-blank of the target line. (Operator linewise-ness is covered in `editor::tests`.)
    use crate::motion::{target, Motion};

    // "a\n  bc\n   d\nef\n": line starts 0,2,7,12; first-non-blanks 0,4,10,12.
    const B: &[u8] = b"a\n  bc\n   d\nef\n";

    #[test]
    fn plus_goes_count_lines_down_to_first_non_blank() {
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 1),
            4,
            "+ -> line 1 'b'"
        );
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 2),
            10,
            "2+ -> line 2 'd'"
        );
        // Past the last line clamps to the last CONTENT line ('ef' at byte 12). The trailing '\n' is a
        // terminator, not a navigable empty line (Vim), so `+` never lands on the phantom slot at byte 15.
        assert_eq!(
            target(B, 0, Motion::DownFirstNonBlank, 99),
            12,
            "clamp at last content line"
        );
    }

    #[test]
    fn minus_goes_count_lines_up_to_first_non_blank() {
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 1),
            10,
            "- -> line 2 'd'"
        );
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 3),
            0,
            "3- -> line 0 'a'"
        );
        // Past the first line clamps to line 0.
        assert_eq!(
            target(B, 12, Motion::UpFirstNonBlank, 99),
            0,
            "clamp at first line"
        );
    }

    #[test]
    fn underscore_is_count_minus_one_lines_down() {
        // `_` alone == `^` (first non-blank of the current line), no vertical move.
        assert_eq!(
            target(B, 5, Motion::LineUnderscore, 1),
            target(B, 5, Motion::LineFirstNonBlank, 1),
            "_ == ^"
        );
        // `2_` moves one line down; `3_` two lines down.
        assert_eq!(
            target(B, 4, Motion::LineUnderscore, 2),
            10,
            "2_ -> one line down"
        );
        assert_eq!(
            target(B, 0, Motion::LineUnderscore, 3),
            10,
            "3_ -> two lines down"
        );
    }
}
