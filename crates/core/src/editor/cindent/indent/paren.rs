//! The parenthesis branch of nvim's `get_c_indent`: a line inside an unclosed `(`.

use super::super::lex::{at, is_white, skipwhite};
use super::super::options::CinOptions;
use super::super::{Ci, Pos};
use super::Step;

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// The parenthesis branch of `get_c_indent`: inside the `(` at `paren`.
    ///
    /// The phases run in nvim's order: the indent of an earlier line that matches the same paren;
    /// when none decides it, whether the outermost paren belongs to an `if`/`for`/`while`, the
    /// paren line's own amount and the leading `(` to ignore, the column to line up with, then the
    /// amount for an unclosed paren.
    pub(super) fn paren_indent(
        &mut self,
        theline: &[u8],
        paren: Pos,
        cur_curpos: Pos,
        cur_amount: &mut i32,
    ) -> i32 {
        let o = *self.o();
        let mut s = ParenScan {
            o,
            t0: at(theline, 0),
            cur_curpos,
            our_paren_pos: paren,
            amount: -1,
            is_if_for_while: false,
            ignore_paren_col: 0,
            look_c: 0,
            cur_amount: *cur_amount,
        };
        self.paren_matching_line_amount(&mut s);
        if s.amount == -1 {
            self.paren_outermost_if_for_while(&mut s);
            self.paren_label_amount(&mut s);
            if s.t0 == b')' || s.lines_up_with_paren() {
                self.paren_line_up_col(&mut s);
            }
            if s.t0 == b')' && o.matching_paren != 0 {
                // Line up with the start of the matching paren line.
            } else if s.lines_up_with_paren() {
                if s.cur_amount != i32::MAX {
                    s.amount = s.cur_amount;
                }
            } else {
                self.paren_unclosed_amount(&mut s);
            }
        }
        *cur_amount = s.cur_amount;
        s.amount
    }

    /// `get_c_indent`, "If the matching paren is more than one line away, use the indent of a
    /// previous non-empty line that matches the same paren". Leaves `amount` at `-1` when no line
    /// decides it (or a `)` line has only lowered `cur_amount`).
    fn paren_matching_line_amount(&mut self, s: &mut ParenScan) {
        if s.t0 == b')' && s.o.paren_prev != 0 {
            s.amount = self.indent_of(self.cur.lnum - 1);
            return;
        }
        s.amount = -1;
        let mut lnum = s.cur_curpos.lnum - 1;
        while lnum > s.our_paren_pos.lnum {
            if self.paren_matching_line(s, &mut lnum) == Step::Done {
                break;
            }
            lnum -= 1;
        }
    }

    /// One pass of `paren_matching_line_amount`'s backward loop, on line `lnum`: skip a comment,
    /// preprocessor or raw-string line (nvim's `continue`), or stop at the line whose paren matches
    /// ours (nvim's `break`).
    fn paren_matching_line(&mut self, s: &mut ParenScan, lnum: &mut usize) -> Step {
        let line = self.line(*lnum);
        let mut l: &'a [u8] = &line[skipwhite(line, 0).min(line.len())..];
        if self.nocode(l, 0) {
            return Step::Next;
        }
        if self.cin_ispreproc_cont(&mut l, lnum, &mut s.amount) {
            return Step::Next;
        }
        self.cur.lnum = *lnum;
        if let Some(t) = self.ind_find_start_cors(None) {
            *lnum = t.lnum + 1;
            return Step::Next;
        }
        let mp = self.corr_ind_maxparen(s.cur_curpos);
        if let Some(t) = self.find_match_paren(mp) {
            if t == s.our_paren_pos {
                s.amount = self.indent_of(*lnum);
                if s.t0 == b')' {
                    if s.our_paren_pos.lnum != *lnum && s.cur_amount > s.amount {
                        s.cur_amount = s.amount;
                    }
                    s.amount = -1;
                }
                return Step::Done;
            }
        }
        Step::Next
    }

    /// `get_c_indent`, "Look for the outermost opening parenthesis on this line and check whether
    /// it belongs to an "if", "for" or "while"" (only with `b_ind_if_for_while`).
    fn paren_outermost_if_for_while(&mut self, s: &mut ParenScan) {
        if s.o.if_for_while == 0 {
            return;
        }
        let save = self.cur;
        let mut outermost;
        let mut t = Some(s.our_paren_pos);
        loop {
            outermost = t.expect("set");
            self.cur = outermost;
            t = self.find_match_paren(s.o.maxparen);
            if !t.is_some_and(|t| t.lnum == outermost.lnum) {
                break;
            }
        }
        self.cur = save;
        let line = self.line(outermost.lnum);
        let mut c = outermost.col;
        s.is_if_for_while = Self::cin_is_if_for_while_before_offset(line, &mut c);
    }

    /// `get_c_indent`, "Line up with line where the matching paren is": the paren line's amount
    /// past any label, and "Ignore a '(' in front of the line that has a match before our matching
    /// '('".
    fn paren_label_amount(&mut self, s: &mut ParenScan) {
        let (a, lline, lstart) = self.skip_label(s.our_paren_pos.lnum);
        s.amount = a;
        let mut look = skipwhite(lline, lstart);
        let mut look_line = lline;
        if at(lline, look) == b'(' {
            let save_lnum = self.cur.lnum;
            self.cur.lnum = s.our_paren_pos.lnum;
            let line = self.cur_line();
            // `look` is relative to the line skip_label returned, which is this line.
            let look_col = look;
            self.cur.col = look_col + 1;
            if let Some(t) = self.findmatchlimit(b')', false, i64::from(s.o.maxparen)) {
                if t.lnum == s.our_paren_pos.lnum && t.col < s.our_paren_pos.col {
                    s.ignore_paren_col = t.col + 1;
                }
            }
            self.cur.lnum = save_lnum;
            look_line = line;
            look = look_col;
        }
        s.look_c = at(look_line, look);
    }

    /// `get_c_indent`, "If we're looking at a close paren, line up right there; otherwise, line up
    /// with the next (non-white) character" (one level per open paren with
    /// `b_ind_unclosed_wrapped`), then "Find how indented the paren is, or the character after it":
    /// lowers `cur_amount` to that column.
    fn paren_line_up_col(&mut self, s: &mut ParenScan) {
        let o = s.o;
        if s.t0 != b')' {
            s.cur_amount = i32::MAX;
            let l = self.line(s.our_paren_pos.lnum);
            if o.unclosed_wrapped != 0 && self.cin_ends_in(l, b"(") {
                let mut n = 1;
                for col in 0..s.our_paren_pos.col {
                    match at(l, col) {
                        b'(' | b'{' => n += 1,
                        b')' | b'}' if n > 1 => n -= 1,
                        _ => {}
                    }
                }
                s.our_paren_pos.col = 0;
                s.amount += n * o.unclosed_wrapped;
            } else if o.unclosed_whiteok != 0 {
                s.our_paren_pos.col += 1;
            } else {
                let mut col = s.our_paren_pos.col + 1;
                while is_white(at(l, col)) {
                    col += 1;
                }
                if at(l, col) != 0 {
                    s.our_paren_pos.col = col;
                } else {
                    s.our_paren_pos.col += 1;
                }
            }
        }
        if s.our_paren_pos.col > 0 {
            let col = i32::try_from(self.vcol(s.our_paren_pos.lnum, s.our_paren_pos.col))
                .unwrap_or(i32::MAX);
            if s.cur_amount > col {
                s.cur_amount = col;
            }
        }
    }

    /// `get_c_indent`, "Add b_ind_unclosed2 for each '(' before our matching one, but ignore
    /// (void) before the line (ignore_paren_col)" and "Use b_ind_unclosed once, when the first '('
    /// is not inside braces", capped by `cur_amount` ("For a line starting with ')' use the minimum
    /// of the two positions").
    fn paren_unclosed_amount(&mut self, s: &mut ParenScan) {
        let o = s.o;
        let l = self.line(s.our_paren_pos.lnum);
        let mut col = s.our_paren_pos.col;
        while s.our_paren_pos.col > s.ignore_paren_col {
            s.our_paren_pos.col -= 1;
            match at(l, s.our_paren_pos.col) {
                b'(' => {
                    s.amount += o.unclosed2;
                    col = s.our_paren_pos.col;
                }
                b')' => {
                    s.amount -= o.unclosed2;
                    col = usize::MAX;
                }
                _ => {}
            }
        }
        if col == usize::MAX {
            s.amount += o.unclosed;
        } else {
            self.cur = Pos {
                lnum: s.our_paren_pos.lnum,
                col,
            };
            if self.find_match_paren_after_brace(o.maxparen).is_some() {
                s.amount += o.unclosed2;
            } else if s.is_if_for_while {
                s.amount += o.if_for_while;
            } else {
                s.amount += o.unclosed;
            }
        }
        if s.cur_amount < s.amount {
            s.amount = s.cur_amount;
        }
    }
}

/// The locals of the parenthesis branch of nvim's `get_c_indent`, shared by its phases. Each is set
/// where nvim sets it and read by the later phases.
struct ParenScan {
    o: CinOptions,
    /// The first byte of the line being indented.
    t0: u8,
    cur_curpos: Pos,
    /// The unclosed `(` the line is inside; its column moves as nvim's does.
    our_paren_pos: Pos,
    amount: i32,
    is_if_for_while: bool,
    ignore_paren_col: usize,
    /// The byte at nvim's `look` once the label is skipped.
    look_c: u8,
    /// The caller's `cur_amount`, written back when the branch ends.
    cur_amount: i32,
}

impl ParenScan {
    /// nvim's twice-tested "(b_ind_unclosed == 0 && is_if_for_while == 0) ||
    /// (!b_ind_unclosed_noignore && *look == '(' && ignore_paren_col == 0)": line up with the
    /// unclosed paren rather than add the unclosed amount.
    fn lines_up_with_paren(&self) -> bool {
        (self.o.unclosed == 0 && !self.is_if_for_while)
            || (self.o.unclosed_noignore == 0 && self.look_c == b'(' && self.ignore_paren_col == 0)
    }
}
