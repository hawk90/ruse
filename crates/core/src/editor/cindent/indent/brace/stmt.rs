//! The brace branch's backward search, the steps chosen by how the line above ends: an
//! unterminated line (or one ending in `,`, nvim's continuation lines), the `while` that ends a
//! `do`, and a terminated statement with its `term_again` walk.

use super::super::super::lex::{
    at, cin_isbreak, cin_iscomment, cin_isdo, last_byte, skipwhite, strend,
};
use super::super::super::Ci;
use super::super::Step;
use super::{BraceScan, Lookfor};

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// `get_c_indent`, an unterminated line (or one ending in `,`): "If we're in the middle of a
    /// paren thing, Go back to the line that starts it so we can get the right prevailing indent",
    /// "Skip over continuation lines to find the one to get the indent from", then "If this is just
    /// above the line we are indenting, and it starts with a '{', line it up with this line."
    pub(super) fn brace_unterminated(
        &mut self,
        s: &mut BraceScan,
        mut l: &'a [u8],
        terminated: u8,
    ) -> Step {
        let o = s.o;
        if s.lookfor != Lookfor::EnumOrInit
            && (at(l, skipwhite(l, 0)) == b'[' || last_byte(l) == b'[')
        {
            s.amount += s.ind_continuation;
        }
        let _ = self.find_last_paren(l, b'(', b')');
        let mp = self.corr_ind_maxparen(s.cur_curpos);
        let mut trypos = self.find_match_paren(mp);
        if trypos.is_some_and(|t| (t.lnum, t.col) < (s.try_brace.lnum, s.try_brace.col)) {
            trypos = None;
        }
        l = self.cur_line();
        if trypos.is_none() && terminated == b',' {
            if self.find_last_paren(l, b'{', b'}') {
                trypos = self.find_start_brace();
            }
            l = self.cur_line();
        }
        if let Some(t) = trypos {
            self.cur = t;
            l = self.cur_line();
            if self.cin_iscase(l, 0, false) || self.cin_isscopedecl(l, 0) {
                self.cur.lnum += 1;
                self.cur.col = 0;
                return Step::Next;
            }
        }
        if terminated == b',' {
            while self.cur.lnum > 1 {
                let pl = self.line(self.cur.lnum - 1);
                if last_byte(pl) != b'\\' {
                    break;
                }
                self.cur.lnum -= 1;
                self.cur.col = 0;
            }
            l = self.cur_line();
        }
        let mut lstart = 0usize;
        if o.js != 0 {
            s.cur_amount = self.get_indent();
        } else {
            let (a, ll, ls) = self.skip_label(self.cur.lnum);
            s.cur_amount = a;
            l = ll;
            lstart = ls;
        }
        if terminated != b',' && s.lookfor != Lookfor::Term && s.t0 == b'{' {
            s.amount = s.cur_amount;
            if at(l, skipwhite(l, lstart)) != b'{' {
                s.amount += o.open_extra;
            }
            if o.cpp_baseclass != 0 && o.js == 0 {
                s.lookfor = Lookfor::CppBaseclass;
                return Step::Next;
            }
            return Step::Done;
        }
        if self.cin_is_cinword(l, lstart) || self.cin_iselse(l, skipwhite(l, lstart)) {
            self.brace_unterminated_after_cinword(s)
        } else {
            self.brace_unterminated_other(s, terminated)
        }
    }

    /// `get_c_indent`, "Check if we are after an "if", "while", etc. Also allow "   } else"": line up
    /// with the conditional, or, when looking for a terminated line, skip to the `do` or the `if` of
    /// an `else`.
    fn brace_unterminated_after_cinword(&mut self, s: &mut BraceScan) -> Step {
        let o = s.o;
        if s.lookfor == Lookfor::Unterm || s.lookfor == Lookfor::EnumOrInit {
            s.add_continuation();
            return Step::Done;
        }
        s.amount = s.cur_amount;
        if s.t0 == b'{' {
            s.amount += o.open_extra;
        }
        if s.lookfor != Lookfor::Term {
            s.amount += o.level + o.no_brace;
            return Step::Done;
        }
        let cl = self.cur_line();
        let ls = skipwhite(cl, 0);
        if cin_isdo(cl, ls) {
            if s.whilelevel == 0 {
                return Step::Done;
            }
            s.whilelevel -= 1;
        }
        if self.cin_iselse(cl, ls) && s.whilelevel == 0 {
            if at(cl, ls) == b'}' {
                self.cur.col = ls + 1;
            }
            match self.find_start_brace() {
                None => return Step::Done,
                Some(t) => {
                    if !self.find_match(true, t.lnum) {
                        return Step::Done;
                    }
                }
            }
        }
        Step::Next
    }

    /// `get_c_indent`, "If we're below an unterminated line that is not an "if" or something, we may
    /// line up with this line or add something for a continuation line": two unterminated lines in a
    /// row, two lines ending in `,`, or the first unterminated line (where an enum or initializer, or
    /// the JS `[` list, starts its own search).
    fn brace_unterminated_other(&mut self, s: &mut BraceScan, terminated: u8) -> Step {
        let o = s.o;
        if s.lookfor == Lookfor::Unterm {
            if terminated == b',' {
                s.amount += s.ind_continuation;
            }
            return Step::Done;
        }
        if s.lookfor == Lookfor::EnumOrInit {
            if terminated == b',' {
                if o.cpp_baseclass == 0 {
                    return Step::Done;
                }
                s.lookfor = Lookfor::CppBaseclass;
                return Step::Next;
            }
            if s.amount > s.cur_amount {
                s.amount = s.cur_amount;
            }
            return Step::Next;
        }
        let cl = self.cur_line();
        s.amount = s.cur_amount;
        let n = strend(cl, 0);
        if o.js != 0
            && terminated == b','
            && (at(cl, skipwhite(cl, 0)) == b']' || (n >= 2 && cl[n - 2] == b']'))
        {
            return Step::Done;
        }
        if s.lookfor == Lookfor::Initial && terminated == b',' {
            if o.js != 0 {
                if cin_iscomment(cl, skipwhite(cl, 0)) {
                    return Step::Done;
                }
                s.lookfor = Lookfor::Comma;
                if let Some(t) = self.find_match_char(b'[', o.maxparen) {
                    if t.lnum == self.cur.lnum - 1 {
                        return Step::Done;
                    }
                    s.ourscope = t.lnum;
                }
            } else {
                s.lookfor = Lookfor::EnumOrInit;
                s.cont_amount = self.cin_first_id_amount();
            }
        } else {
            if s.lookfor == Lookfor::Initial && last_byte(cl) == b'\\' {
                s.cont_amount = self.cin_get_equal_amount(self.cur.lnum);
            }
            if s.lookfor != Lookfor::Term
                && s.lookfor != Lookfor::JsKey
                && s.lookfor != Lookfor::Comma
                && s.raw_string_start != self.cur.lnum
            {
                s.lookfor = Lookfor::Unterm;
            }
        }
        Step::Next
    }

    /// `get_c_indent`, "Check if we are after a while (cond); If so: Ignore until the matching
    /// "do"."
    pub(super) fn brace_while_of_do_end(&mut self, s: &mut BraceScan) -> Step {
        if s.lookfor == Lookfor::Unterm || s.lookfor == Lookfor::EnumOrInit {
            s.add_continuation();
            return Step::Done;
        }
        if s.whilelevel == 0 {
            s.lookfor = Lookfor::Term;
            s.amount = self.get_indent();
            if s.t0 == b'{' {
                s.amount += s.o.open_extra;
            }
        }
        s.whilelevel += 1;
        Step::Next
    }

    /// `get_c_indent`, "We are after a "normal" statement": skip a single `break` before a switch
    /// label, handle a "do {" line, add the continuation amount below a terminated line, or stop at
    /// a second terminated line; else look further back (`term_again`).
    pub(super) fn brace_terminated(&mut self, s: &mut BraceScan) -> Step {
        if s.lookfor == Lookfor::Nobreak {
            let cl = self.cur_line();
            if cin_isbreak(cl, skipwhite(cl, 0)) {
                s.lookfor = Lookfor::Any;
                return Step::Next;
            }
        }
        if s.whilelevel > 0 {
            let cl = self.cur_line();
            let sk = self.skipcomment(cl, 0);
            if cin_isdo(cl, sk) {
                s.amount = self.get_indent();
                s.whilelevel -= 1;
                return Step::Next;
            }
        }
        if s.lookfor == Lookfor::Unterm || s.lookfor == Lookfor::EnumOrInit {
            s.add_continuation();
            return Step::Done;
        }
        if s.lookfor == Lookfor::Term {
            if !s.lookfor_break && s.whilelevel == 0 {
                return Step::Done;
            }
            return Step::Next;
        }
        self.brace_term_again(s)
    }

    /// `get_c_indent`'s `term_again` label: "First line above the one we're indenting is terminated.
    /// To know what needs to be done look further backward for a terminated line", skipping a
    /// matched paren, an `else` (to its `if`) and a `{}` block (to its start).
    fn brace_term_again(&mut self, s: &mut BraceScan) -> Step {
        let o = s.o;
        loop {
            let l = self.cur_line();
            if self.find_last_paren(l, b'(', b')') {
                if let Some(t) = self.find_match_paren(o.maxparen) {
                    self.cur = t;
                    let l = self.cur_line();
                    if self.cin_iscase(l, 0, false) || self.cin_isscopedecl(l, 0) {
                        self.cur.lnum += 1;
                        self.cur.col = 0;
                        return Step::Next;
                    }
                }
            }
            let cl = self.cur_line();
            let iscase = o.keep_case_label != 0 && self.cin_iscase(cl, 0, false);
            let (a, ll, ls) = self.skip_label(self.cur.lnum);
            s.amount = a;
            if s.t0 == b'{' {
                s.amount += o.open_extra;
            }
            let li = skipwhite(ll, ls);
            if at(ll, li) == b'{' {
                s.amount -= o.open_extra;
            }
            s.lookfor = if iscase { Lookfor::Any } else { Lookfor::Term };
            if s.lookfor == Lookfor::Term
                && at(ll, li) != b'}'
                && self.cin_iselse(ll, li)
                && s.whilelevel == 0
            {
                match self.find_start_brace() {
                    None => return Step::Done,
                    Some(t) => {
                        if !self.find_match(true, t.lnum) {
                            return Step::Done;
                        }
                    }
                }
                return Step::Next;
            }
            let l = self.cur_line();
            if self.find_last_paren(l, b'{', b'}') {
                if let Some(t) = self.find_start_brace() {
                    self.cur = t;
                    let cl = self.cur_line();
                    let sk = self.skipcomment(cl, 0);
                    if at(cl, sk) == b'}' || !self.cin_iselse(cl, sk) {
                        continue; // term_again
                    }
                    self.cur.lnum += 1;
                    self.cur.col = 0;
                }
            }
            return Step::Next;
        }
    }
}
