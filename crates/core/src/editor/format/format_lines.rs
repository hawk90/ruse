//! `gq` / `gw`: nvim v0.12.6's `format_lines` (`textformat.c`), split along its phases:
//!
//! 1. classify the next line ([`FmtBuf::fmt_check_par`]: nvim's `fmt_check_par`, the comment leader it
//!    starts with and whether it can be in a paragraph; `get_number_indent` for `'formatoptions'` `n`);
//! 2. on the first line of a paragraph, take the indent of the second line
//!    ([`FmtBuf::fmt_second_indent`]: the `if (first_par_line && (do_second_indent || do_number_indent)`
//!    block); a change of comment leader ends the paragraph ([`FmtBuf::same_leader`]: `same_leader`);
//! 3. at the end of a paragraph, or on a line grown too long, format it with `internal_format`
//!    ([`FmtBuf::fmt_format_par`]: "do the formatting, without 'showmode'");
//! 4. still in the same paragraph, join the next line on ([`FmtBuf::fmt_join_next`]: the next line's
//!    leader or indent deleted, then `do_join`).

use super::{is_end_part, leader_len, prev_col, skipwhite, white};
use super::{FmtBuf, FmtOpts};

/// The state `format_lines` carries from line to line: the `'formatoptions'` flags read once, and the
/// locals nvim's loop updates.
struct Par {
    /// `fo-q`: format comments.
    do_com: bool,
    /// `fo-2`: the second line's indent.
    do_second: bool,
    /// `fo-n`: numbered lists.
    do_number: bool,
    tw: usize,
    /// A joined line longer than this is formatted before the paragraph ends.
    max_len: usize,
    second_indent: i32,
    do_com_list: bool,
    first_par_line: bool,
    need_set_indent: bool,
    force_format: bool,
}

impl FmtBuf {
    /// `gq` / `gw` over the buffer's `line_count` lines (nvim's `format_lines`). The cursor starts on
    /// line 0 and ends on the last formatted line.
    pub(crate) fn format_lines(&mut self, line_count: usize, tw: usize, o: &FmtOpts<'_>) {
        let mut p = Par {
            do_com: o.has('q'),
            do_second: o.has('2'),
            do_number: o.has('n'),
            tw,
            max_len: tw * 3,
            second_indent: -1,
            do_com_list: false,
            first_par_line: true,
            need_set_indent: true,
            force_format: false,
        };
        let do_com = p.do_com;
        let mut next_is_start_par = false;

        let (prev_not_par, _, _) = if self.before.is_some() {
            self.fmt_check_par(usize::MAX, do_com, o)
        } else {
            (true, 0, None)
        };
        let mut is_not_par = prev_not_par;
        let (mut next_is_not_par, mut next_len, mut next_k) = self.fmt_check_par(0, do_com, o);
        let mut is_end_par = is_not_par || next_is_not_par;
        let mut leader = (0usize, None::<usize>);
        let mut advance = true;
        let mut l: isize = -1;
        let mut count = line_count;
        let mut prev_is_end_par = false;
        while count != 0 {
            if advance {
                l += 1;
                prev_is_end_par = is_end_par;
                is_not_par = next_is_not_par;
                leader = (next_len, next_k);
            }
            let lu = usize::try_from(l).unwrap_or(0);
            // nvim walks the cursor down the lines, a skipped one (blank, a comment end) included.
            self.cur.0 = lu;
            let last = lu + 1 >= self.lines.len();
            if count == 1 || last {
                next_is_not_par = true;
                next_len = 0;
                next_k = None;
            } else {
                (next_is_not_par, next_len, next_k) = self.fmt_check_par(lu + 1, do_com, o);
                if p.do_number {
                    next_is_start_par =
                        self.number_indent(lu + 1, do_com, o).is_some_and(|n| n > 0);
                }
            }
            advance = true;
            is_end_par = is_not_par || next_is_not_par || next_is_start_par;
            if !is_not_par {
                self.fmt_second_indent(lu, leader.0, next_len, prev_is_end_par, o, &mut p);
                if last || !self.same_leader(lu, leader.0, leader.1, next_len, next_k, o) {
                    is_end_par = true;
                }
                if is_end_par || p.force_format {
                    let grown = self.fmt_format_par(lu, is_end_par, o, &mut p);
                    l += isize::try_from(grown).unwrap_or(0);
                }
                if !is_end_par {
                    advance = false;
                    let lu = usize::try_from(l).unwrap_or(0);
                    self.fmt_join_next(lu, next_len, o, &mut p);
                }
            }
            count -= 1;
        }
    }

    /// nvim's `fmt_check_par` for line `l` (`usize::MAX` is the line before the range): whether it can
    /// not be in a paragraph (blank after its leader, the end of a three-part comment, or a form feed),
    /// with the length of its comment leader and the part that matched.
    fn fmt_check_par(
        &self,
        l: usize,
        do_com: bool,
        o: &FmtOpts<'_>,
    ) -> (bool, usize, Option<usize>) {
        let Some(line) = (if l == usize::MAX {
            self.before.as_deref()
        } else {
            self.line(l)
        }) else {
            return (true, 0, None);
        };
        let (len, k) = if do_com {
            leader_len(line, &o.parts, false, true)
        } else {
            (0, None)
        };
        let end_leader = len > 0 && k.is_some_and(|k| is_end_part(&o.parts, k));
        let not_par =
            skipwhite(line, len) >= line.len() || end_leader || line.first() == Some(&0x0c);
        (not_par, len, k)
    }

    /// nvim's `same_leader` for lines `l` and `l + 1`.
    fn same_leader(
        &self,
        l: usize,
        len1: usize,
        k1: Option<usize>,
        len2: usize,
        k2: Option<usize>,
        o: &FmtOpts<'_>,
    ) -> bool {
        if len1 == 0 {
            return len2 == 0;
        }
        let line1 = self.line(l).unwrap_or(&[]);
        if let Some(k) = k1 {
            for f in o.parts[k].flags.bytes() {
                match f {
                    b'f' => return len2 == 0,
                    b'e' => return false,
                    b's' => {
                        if line1.len() <= len1 {
                            return false;
                        }
                        if len2 == 0 {
                            return false;
                        }
                        return k2.is_some_and(|k| o.parts[k].has('m'));
                    }
                    _ => {}
                }
            }
        }
        let line2 = self.line(l + 1).unwrap_or(&[]);
        let mut i1 = skipwhite(line1, 0);
        let mut i2 = 0;
        while i2 < len2 {
            if !white(line2[i2]) {
                if line1.get(i1) != Some(&line2[i2]) {
                    break;
                }
                i1 += 1;
            } else {
                while line1.get(i1).copied().is_some_and(white) {
                    i1 += 1;
                }
            }
            i2 += 1;
        }
        i2 == len2 && i1 == len1
    }

    /// The `if (first_par_line && (do_second_indent || do_number_indent) && prev_is_end_par && …)` block
    /// of nvim's `format_lines`: on the first line `lu` of a paragraph, the indent the second line keeps
    /// (`fo-2`) or the numbered-list indent (`fo-n`), and whether it follows a comment leader.
    fn fmt_second_indent(
        &self,
        lu: usize,
        leader_len: usize,
        next_len: usize,
        prev_is_end_par: bool,
        o: &FmtOpts<'_>,
        p: &mut Par,
    ) {
        let has_next_buffer_line = lu + 1 < self.lines.len() || self.after.is_some();
        if !(p.first_par_line
            && (p.do_second || p.do_number)
            && prev_is_end_par
            && has_next_buffer_line)
        {
            return;
        }
        let next_empty = self.line(lu + 1).is_none_or(<[u8]>::is_empty);
        if p.do_second && !next_empty {
            if leader_len == 0 && next_len == 0 {
                p.second_indent = i32::try_from(self.get_indent(lu + 1, o)).unwrap_or(0);
            } else {
                p.second_indent = i32::try_from(next_len).unwrap_or(0);
                p.do_com_list = true;
            }
        } else if p.do_number {
            p.second_indent = self
                .number_indent(lu, p.do_com, o)
                .map_or(-1, |n| i32::try_from(n).unwrap_or(i32::MAX));
            if leader_len != 0 || next_len != 0 {
                p.do_com_list = true;
            }
        }
    }

    /// nvim's "do the formatting, without 'showmode'" step of `format_lines`: line `lu` ends a paragraph
    /// (`is_end_par`) or grew too long, so re-indent it, put the cursor on its last non-blank and run
    /// `internal_format`. Returns how many lines the wrap added.
    fn fmt_format_par(
        &mut self,
        lu: usize,
        is_end_par: bool,
        o: &FmtOpts<'_>,
        p: &mut Par,
    ) -> usize {
        if p.need_set_indent {
            let ind = self.get_indent(lu, o);
            self.cur = (lu, 0);
            self.set_indent(lu, ind, o);
        }
        // The cursor on the last non-blank.
        let line = &self.lines[lu];
        let mut col = if line.is_empty() {
            0
        } else {
            prev_col(line, line.len())
        };
        while col > 0 && line.get(col).is_some_and(|c| c.is_ascii_whitespace()) {
            col = prev_col(line, col);
        }
        self.cur = (lu, col);
        let before = self.lines.len();
        self.internal_format(
            p.tw,
            p.second_indent,
            true,
            p.do_com,
            p.do_com && p.do_com_list,
            None,
            o,
        );
        let grown = self.lines.len() - before;
        p.second_indent = -1;
        p.need_set_indent = is_end_par;
        if is_end_par {
            p.first_par_line = true;
        }
        p.force_format = false;
        grown
    }

    /// nvim's "still in same paragraph, so join the lines together" step of `format_lines`: delete the
    /// comment leader (or, with a second-line indent, the indent) of line `lu + 1`, then `do_join` it
    /// onto `lu`.
    fn fmt_join_next(&mut self, lu: usize, next_len: usize, o: &FmtOpts<'_>, p: &mut Par) {
        if next_len > 0 {
            self.del_start(lu + 1, next_len);
        } else if p.second_indent > 0 {
            let n = skipwhite(&self.lines[lu + 1], 0);
            if n > 0 {
                self.del_start(lu + 1, n);
            }
        }
        self.join2(lu, o);
        p.first_par_line = false;
        p.force_format = self.lines[lu].len() > p.max_len;
    }
}
