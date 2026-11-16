//! `get_c_indent`: the indent of one line. This module is nvim's dispatch (the early returns for a
//! raw string, a `#` line, a jump label and a line comment, then the choice of branch) and the
//! top-level tail; each branch lives in a child module, in nvim's order: [`comment`] (inside a
//! `/* */` comment), [`paren`] (inside an unclosed `(`), [`brace`] (inside a `{}` block) and
//! [`top_level`] (outside both).

use super::keys::CinKey;
use super::lex::{
    at, check_linecomment, cin_iscomment, cin_islinecomment, last_byte, skipwhite, starts, strend,
};
use super::{Ci, Pos};

mod brace;
mod comment;
mod paren;
mod top_level;

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// `get_c_indent` for line `lnum`, the cursor at byte `col` (it matters in Insert mode, where a `)`
    /// under the cursor is cut off first). `-1` keeps the line as it is (inside a raw string).
    pub(crate) fn get_c_indent(&mut self, lnum: usize, col: usize, insert: bool) -> i32 {
        self.insert = insert;
        self.cur = Pos { lnum, col };
        let o = *self.o();
        let cur_curpos = self.cur;
        if lnum == 1 {
            return 0;
        }
        let mut linecopy: Vec<u8> = self.line(lnum).to_vec();
        let n = strend(&linecopy, 0);
        linecopy.truncate(n);
        if insert && col < linecopy.len() && linecopy[col] == b')' {
            linecopy.truncate(col);
        }
        let theline: Vec<u8> = linecopy[skipwhite(&linecopy, 0)..].to_vec();
        let theline = &theline[..];
        let t0 = at(theline, 0);
        self.cur.col = 0;
        let original_line_islabel = self.cin_islabel();
        let mut amount: i32;
        let mut cur_amount: i32 = i32::MAX;
        let mut ind_continuation = o.continuation;

        let comment_pos = self.ind_find_start_comment();
        if let Some(r) = self.find_start_rawstring(o.maxcomment) {
            if comment_pos.is_none_or(|c| r < c) {
                return -1;
            }
        }
        // #defines and so on go at the left when in 'cinkeys'.
        if t0 == b'#'
            && (at(&linecopy, 0) == b'#' || self.in_cinkeys(CinKey::Char(b'#'), b' ', true))
        {
            let d = skipwhite(theline, 1);
            if o.pragma == 0 || !starts(theline, d, b"pragma") {
                return o.hash_comment.max(0);
            }
        }
        if original_line_islabel && o.js == 0 && o.jump_label < 0 {
            return 0;
        }
        if cin_islinecomment(theline, 0) {
            let mut trypos = self.find_line_comment();
            if trypos.is_none() && self.cur.lnum > 1 {
                if let Some(c) = check_linecomment(self.line(self.cur.lnum - 1)) {
                    trypos = Some(Pos {
                        lnum: self.cur.lnum - 1,
                        col: c,
                    });
                }
            }
            if let Some(t) = trypos {
                return i32::try_from(self.vcol(t.lnum, t.col)).unwrap_or(i32::MAX);
            }
        }
        if !cin_iscomment(theline, 0) {
            if let Some(cpos) = comment_pos {
                return self.comment_indent(theline, cpos, cur_curpos).max(0);
            }
        }
        // A `]` that has a match.
        if at(theline, skipwhite(theline, 0)) == b']' {
            if let Some(t) = self.find_match_char(b'[', o.maxparen) {
                return self.indent_of(t.lnum).max(0);
            }
        }
        // Inside parentheses or braces?
        let mut trypos = self.find_match_paren(o.maxparen);
        let mut try_brace: Option<Pos> = None;
        let inside = if trypos.is_some() && o.java == 0 {
            true
        } else {
            try_brace = self.find_start_brace();
            try_brace.is_some() || trypos.is_some()
        };
        if inside {
            if let (Some(t), Some(b)) = (trypos, try_brace) {
                if (t.lnum, t.col) < (b.lnum, b.col) {
                    trypos = None;
                } else {
                    try_brace = None;
                }
            }
            if let Some(t) = trypos {
                amount = self.paren_indent(theline, t, cur_curpos, &mut cur_amount);
                if cin_iscomment(theline, 0) {
                    amount += o.comment;
                }
            } else {
                let b = try_brace.expect("inside braces");
                amount = self.brace_indent(
                    theline,
                    b,
                    cur_curpos,
                    &mut cur_amount,
                    &mut ind_continuation,
                );
                if amount == i32::MIN {
                    // `goto theend` from inside (a matched `if`/`do`).
                    return self.get_indent().max(0);
                }
                if cin_iscomment(theline, 0) {
                    amount += o.comment;
                }
                if o.jump_label > 0 && original_line_islabel {
                    amount -= o.jump_label;
                }
            }
            return amount.max(0);
        }
        // Top level.
        if t0 == b'{' {
            return o.first_open.max(0);
        }
        if cur_curpos.lnum < self.count()
            && !self.nocode(theline, 0)
            && !theline.contains(&b'{')
            && !theline.contains(&b'}')
            && !self.cin_ends_in(theline, b":")
            && !self.cin_ends_in(theline, b",")
            && self.cin_isfuncdecl(None, cur_curpos.lnum + 1, cur_curpos.lnum + 1)
            && self.isterminated(theline, 0, false, true) == 0
        {
            return o.func_type.max(0);
        }
        amount = self.top_level_indent(cur_curpos, ind_continuation);
        if cin_iscomment(theline, 0) {
            amount += o.comment;
        }
        if cur_curpos.lnum > 1 && last_byte(self.line(cur_curpos.lnum - 1)) == b'\\' {
            let ca = self.cin_get_equal_amount(cur_curpos.lnum - 1);
            if ca > 0 {
                amount = ca;
            } else if ca == 0 {
                amount += ind_continuation;
            }
        }
        amount.max(0)
    }
}

/// What one pass of the backward search decided: look at the line above (nvim's `continue`), or
/// stop with the amount found (nvim's `break`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Next,
    Done,
}
