//! Auto-wrap: nvim v0.12.6's `internal_format` (`textformat.c`), split along its phases:
//!
//! 1. with `'noautoindent'`, mark a blank under the cursor so it is not a break point
//!    ([`FmtBuf::mark_blank_under_cursor`], the `save_char` dance, undone by
//!    [`FmtBuf::restore_blank_under_cursor`]);
//! 2. per pass: stop once the line fits ([`fits`]: "find column of textwidth border"), pick the comment
//!    leader to keep, then find the break ([`find_break`]: "find position to break at");
//! 3. split the line just before the margin: `open_line` re-inserts the leader on the new line;
//! 4. on the first break, give the new line the second-line indent ([`FmtBuf::second_line_indent`]: the
//!    `if (first_line)` block), then put the cursor back on the text it was on.

use super::{char_at, col_at_cell, leader_len, next_col, prev_col, shift_col, vcol, white};
use super::{FmtBuf, FmtOpts};
use crate::cells::Tabs;
use crate::editor::range::cluster_cells;

/// Whether the cursor line already fits `tw` with `c` (or the char under the cursor) at `startcol`.
fn fits(line: &[u8], startcol: usize, c: Option<char>, tw: usize, ts: Tabs) -> bool {
    let ch_cells = |ch: char| {
        let mut tmp = [0u8; 4];
        let s = ch.encode_utf8(&mut tmp);
        cluster_cells(s.as_bytes(), 0, s.len(), vcol(line, startcol, ts), ts).1
    };
    let under = c.or_else(|| char_at(line, startcol));
    let cells = under.map_or(1, ch_cells);
    vcol(line, startcol, ts) + cells <= tw
}

/// The column the line breaks at, scanning back from `startcol` (nvim's "find position to break at"
/// loop): the start of the word after the last blank run at or before `wantcol`, or failing that the
/// first one after it. `0` is no break point.
fn find_break(
    line: &[u8],
    startcol: usize,
    c: Option<char>,
    leader_len: usize,
    wantcol: usize,
    o: &FmtOpts<'_>,
) -> usize {
    let mut cur = startcol;
    let mut foundcol = 0usize;
    loop {
        let cc = if cur == startcol && c.is_some() {
            c
        } else {
            char_at(line, cur)
        };
        let is_white = |cc: Option<char>| matches!(cc, Some(' ' | '\t'));
        if is_white(cc) {
            let mut cc = cc;
            let mut wcc = 0;
            while cur > 0 && is_white(cc) {
                cur = prev_col(line, cur);
                cc = char_at(line, cur);
                if wcc < 2 {
                    wcc += 1;
                }
            }
            if cur == 0 && is_white(cc) {
                break; // only blanks before the text
            }
            if o.has('p') && cc == Some('.') && wcc < 2 {
                continue;
            }
            if cur < leader_len {
                break;
            }
            if o.has('1') {
                if cur == 0 || cur <= leader_len {
                    break;
                }
                let col = cur;
                cur = prev_col(line, cur);
                if is_white(char_at(line, cur)) {
                    continue; // a one-letter word: look further back
                }
                cur = col;
            }
            cur = next_col(line, cur);
            foundcol = cur;
            if cur <= wantcol {
                break;
            }
        }
        if cur == 0 {
            break;
        }
        cur = prev_col(line, cur);
    }
    foundcol
}

impl FmtBuf {
    /// Break the cursor line until it fits `tw` (nvim's `internal_format`). `c` is the character about to
    /// be typed at the cursor (`None` when only formatting, `gq`). `format` is `INSCHAR_FORMAT`;
    /// `do_com` / `com_list` are `INSCHAR_DO_COM` / `INSCHAR_COM_LIST`. `insstart_long` is `'fo'` `l`'s
    /// "the line was too long when this insert started".
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn internal_format(
        &mut self,
        tw: usize,
        mut second_indent: i32,
        format: bool,
        mut do_com: bool,
        com_list: bool,
        c: Option<char>,
        o: &FmtOpts<'_>,
    ) {
        let ts = o.ind.tabs();
        let mut first_line = true;
        let mut no_leader = false;
        let save_char = self.mark_blank_under_cursor(o);
        loop {
            let (l, startcol) = self.cur;
            let line = &self.lines[l];
            if fits(line, startcol, c, tw, ts) {
                break;
            }
            if no_leader {
                do_com = false;
            } else if !format && o.has('c') {
                do_com = true;
            }
            let leader_len = if do_com {
                leader_len(line, &o.parts, false, true).0
            } else {
                0
            };
            if leader_len == 0 {
                no_leader = true;
            }
            if !format && leader_len == 0 && !o.has('t') {
                break;
            }
            if startcol == 0 {
                break;
            }
            let foundcol = find_break(line, startcol, c, leader_len, col_at_cell(line, tw, ts), o);
            if foundcol == 0 {
                break;
            }
            // Move the cursor to the break: the blanks there go, the text from `startcol` on keeps its
            // offset from the word start.
            let mut after = foundcol;
            while after < line.len() && white(line[after]) {
                after += 1;
            }
            let keep = startcol.saturating_sub(after);
            self.cur = (l, foundcol);
            let old_indent = 0;
            let did_com = self.open_line(
                do_com,
                com_list,
                if com_list { second_indent } else { old_indent },
                o,
            );
            if did_com {
                no_leader = false;
            }
            if first_line {
                if !com_list {
                    second_indent = self.second_line_indent(l, second_indent, leader_len, o);
                }
                first_line = false;
            }
            let (nl, nc) = self.cur;
            self.cur = (nl, (nc + keep).min(self.lines[nl].len()));
        }
        self.restore_blank_under_cursor(save_char);
    }

    /// With `'noautoindent'` a blank under the cursor is not a break point: mark it while breaking
    /// (nvim's `save_char`). Returns the blank, for [`Self::restore_blank_under_cursor`].
    fn mark_blank_under_cursor(&mut self, o: &FmtOpts<'_>) -> Option<u8> {
        if o.ind.autoindent {
            return None;
        }
        let (l, col) = self.cur;
        let ch = *self.lines[l].get(col)?;
        if !white(ch) {
            return None;
        }
        self.lines[l][col] = b'x';
        Some(ch)
    }

    /// Put back the blank [`Self::mark_blank_under_cursor`] marked, under the cursor's new place.
    fn restore_blank_under_cursor(&mut self, save_char: Option<u8>) {
        if let Some(ch) = save_char {
            let (l, col) = self.cur;
            if col < self.lines[l].len() {
                self.lines[l][col] = ch;
            }
        }
    }

    /// The second-line indent the first new line takes (nvim's `if (first_line)` block, not under
    /// `INSCHAR_COM_LIST`): `'fo'` `n` reads it from the numbered line `l` when none was given; padding
    /// after a comment leader, else the line's indent. Returns `second_indent` as updated.
    fn second_line_indent(
        &mut self,
        l: usize,
        mut second_indent: i32,
        leader_len: usize,
        o: &FmtOpts<'_>,
    ) -> i32 {
        if second_indent < 0 && o.has('n') {
            second_indent = self
                .number_indent(l, true, o)
                .map_or(-1, |n| i32::try_from(n).unwrap_or(i32::MAX));
        }
        if second_indent >= 0 {
            let si = usize::try_from(second_indent).unwrap_or(0);
            if leader_len > 0 && si > leader_len {
                let (nl, nc) = self.cur;
                let pad = si - leader_len;
                self.lines[nl].splice(nc..nc, std::iter::repeat_n(b' ', pad));
                shift_col(&mut self.saved, nl, nc, pad as isize);
                self.cur.1 += pad;
            } else {
                let nl = self.cur.0;
                self.set_indent(nl, si, o);
            }
        }
        second_indent
    }
}
