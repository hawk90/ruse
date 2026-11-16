//! The top-level branch of nvim's `get_c_indent`: a line outside any brace or parenthesis.

use super::super::lex::{at, last_byte, skipwhite};
use super::super::{Ci, Pos};

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// The top-level branch of `get_c_indent`: outside any brace or parenthesis.
    pub(super) fn top_level_indent(&mut self, cur_curpos: Pos, ind_continuation: i32) -> i32 {
        let o = *self.o();
        let mut amount = 0;
        let mut cache = (
            false,
            Pos {
                lnum: usize::MAX,
                col: 0,
            },
        );
        self.cur = cur_curpos;
        while self.cur.lnum > 1 {
            self.cur.lnum -= 1;
            self.cur.col = 0;
            if let Some(t) = self.ind_find_start_cors(None) {
                self.cur.lnum = t.lnum + 1;
                self.cur.col = 0;
                continue;
            }
            let mut l = self.cur_line();
            let mut n = false;
            if o.cpp_baseclass != 0 {
                n = self.cin_is_cpp_baseclass(&mut cache);
                l = self.cur_line();
            }
            if n {
                amount = self.get_baseclass_amount(cache.1.col);
                break;
            }
            let mut ln = self.cur.lnum;
            if self.cin_ispreproc_cont(&mut l, &mut ln, &mut amount) {
                self.cur.lnum = ln;
                continue;
            }
            self.cur.lnum = ln;
            if self.nocode(l, 0) {
                continue;
            }
            // nvim keeps `n` at 0 when the line ends in `,` (the `\` test short-circuits).
            let comma = self.cin_ends_in(l, b",");
            if comma || last_byte(l) == b'\\' {
                amount = self.top_level_continued_amount(l, comma, ind_continuation);
                break;
            }
            if self.cin_isfuncdecl(None, cur_curpos.lnum, 0) {
                break;
            }
            let l = self.cur_line();
            if at(l, skipwhite(l, 0)) == b'}' {
                break;
            }
            if self.cin_ends_in(l, b"};") {
                break;
            }
            if self.cin_ends_in(l, b"[") {
                amount = self.get_indent() + ind_continuation;
                break;
            }
            if self.top_level_semicolon_after_brace(l, &mut amount) {
                break;
            }
            if self.cin_isfuncdecl(Some(l), self.cur.lnum, 0) {
                amount = o.param;
                break;
            }
            let mut l = l;
            if self.cin_ends_in(l, b";") {
                let pl = self.line(self.cur.lnum - 1);
                if self.cin_ends_in(pl, b",") || last_byte(pl) == b'\\' {
                    break;
                }
                l = self.cur_line();
            }
            let _ = self.find_last_paren(l, b'(', b')');
            if let Some(t) = self.find_match_paren(o.maxparen) {
                self.cur = t;
            }
            amount = self.get_indent();
            break;
        }
        amount
    }

    /// `get_c_indent`, top level, "If the previous line ends in ',', use one level of indentation"
    /// (or the line ends in `\`): "take us back to opening paren", "For a line ending in ',' that is
    /// a continuation line go back to the first line with a backslash", then that line's indent, the
    /// first identifier's column, or `ind_continuation`.
    fn top_level_continued_amount(
        &mut self,
        l: &'a [u8],
        comma: bool,
        ind_continuation: i32,
    ) -> i32 {
        let o = *self.o();
        if self.find_last_paren(l, b'(', b')') {
            if let Some(t) = self.find_match_paren(o.maxparen) {
                self.cur = t;
            }
        }
        while comma && self.cur.lnum > 1 {
            let pl = self.line(self.cur.lnum - 1);
            if last_byte(pl) != b'\\' {
                break;
            }
            self.cur.lnum -= 1;
            self.cur.col = 0;
        }
        let mut amount = self.get_indent();
        if amount == 0 {
            amount = self.cin_first_id_amount();
        }
        if amount == 0 {
            amount = ind_continuation;
        }
        amount
    }

    /// `get_c_indent`, top level, "Find a line only has a semicolon that belongs to a previous line
    /// ending in '}', e.g. before an #endif. Don't increase indent then." `true` is nvim's `break`;
    /// otherwise the cursor is restored (a preprocessor line skipped on the way may still have set
    /// `amount`, as in nvim).
    fn top_level_semicolon_after_brace(&mut self, l: &'a [u8], amount: &mut i32) -> bool {
        let look = skipwhite(l, 0);
        if at(l, look) == b';' && self.nocode(l, look + 1) {
            let save = self.cur;
            let mut lk: &'a [u8] = l;
            while self.cur.lnum > 1 {
                self.cur.lnum -= 1;
                lk = self.cur_line();
                let mut ln = self.cur.lnum;
                let nc = self.nocode(lk, 0);
                let pre = !nc && self.cin_ispreproc_cont(&mut lk, &mut ln, amount);
                self.cur.lnum = ln;
                if !(nc || pre) {
                    break;
                }
            }
            if self.cur.lnum > 0 && self.cin_ends_in(lk, b"}") {
                return true;
            }
            self.cur = save;
        }
        false
    }
}
