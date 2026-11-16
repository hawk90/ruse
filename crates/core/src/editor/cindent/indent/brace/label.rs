//! The brace branch's backward search, the steps that look at a line before its terminator: a
//! switch label or C++ scope declaration, the lines the search ignores, a C++ base class line and
//! a JavaScript `key:` or comma line.

use super::super::super::Ci;
use super::super::Step;
use super::{BraceScan, Lookfor};

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// `get_c_indent`, "If this is a switch() label, may line up relative to that. If this is a C++
    /// scope declaration, do the same." `None` when line `l` is neither.
    pub(super) fn brace_switch_label(&mut self, s: &mut BraceScan, l: &[u8]) -> Option<Step> {
        let o = s.o;
        let iscase = self.cin_iscase(l, 0, false);
        if !iscase && !self.cin_isscopedecl(l, 0) {
            return None;
        }
        if s.lookfor == Lookfor::CppBaseclass {
            return Some(Step::Done);
        }
        if s.whilelevel > 0 {
            return Some(Step::Next);
        }
        if s.lookfor == Lookfor::Unterm || s.lookfor == Lookfor::EnumOrInit {
            s.add_continuation();
            return Some(Step::Done);
        }
        if (iscase && s.lookfor == Lookfor::Case)
            || (iscase && s.lookfor_break)
            || (!iscase && s.lookfor == Lookfor::Scopedecl)
        {
            return Some(match self.find_start_brace() {
                Some(t) if t.lnum != s.ourscope => Step::Next,
                _ => {
                    s.amount = self.get_indent();
                    Step::Done
                }
            });
        }
        let n = self.get_indent_nolabel(self.cur.lnum);
        if s.lookfor == Lookfor::Term {
            if n != 0 {
                s.amount = n;
            }
            if !s.lookfor_break {
                return Some(Step::Done);
            }
        }
        if n != 0 {
            s.amount = n;
            let cl = self.cur_line();
            if let Some(p) = self.after_label(cl) {
                if self.cin_is_cinword(cl, p) {
                    if s.t0 == b'{' {
                        s.amount += o.open_extra;
                    } else {
                        s.amount += o.level + o.no_brace;
                    }
                }
            }
            return Some(Step::Done);
        }
        s.scope_amount = self.get_indent()
            + if iscase {
                o.case_code
            } else {
                o.scopedecl_code
            };
        s.lookfor = if o.case_break != 0 {
            Lookfor::Nobreak
        } else {
            Lookfor::Any
        };
        Some(Step::Next)
    }

    /// `get_c_indent`, "Looking for a switch() label or C++ scope declaration, ignore other lines,
    /// skip {}-blocks", "Ignore jump labels with nothing after them" and "Ignore #defines, #if, etc.
    /// Ignore comment and empty lines." `*l` is the line again afterwards.
    pub(super) fn brace_skip_ignored(
        &mut self,
        s: &mut BraceScan,
        l: &mut &'a [u8],
    ) -> Option<Step> {
        if s.lookfor == Lookfor::Case || s.lookfor == Lookfor::Scopedecl {
            if self.find_last_paren(l, b'{', b'}') {
                if let Some(t) = self.find_start_brace() {
                    self.cur.lnum = t.lnum + 1;
                    self.cur.col = 0;
                }
            }
            return Some(Step::Next);
        }
        if s.o.js == 0 && self.cin_islabel() {
            let cl = self.cur_line();
            match self.after_label(cl) {
                None => return Some(Step::Next),
                Some(p) if self.nocode(cl, p) => return Some(Step::Next),
                Some(_) => {}
            }
        }
        *l = self.cur_line();
        let mut ln = self.cur.lnum;
        let pre = self.cin_ispreproc_cont(l, &mut ln, &mut s.amount);
        self.cur.lnum = ln;
        if pre || self.nocode(l, 0) {
            return Some(Step::Next);
        }
        None
    }

    /// `get_c_indent`, "Are we at the start of a cpp base class declaration or constructor
    /// initialization?", and "only look, whether there is a cpp base class declaration or
    /// initialization before the opening brace" while looking for one.
    pub(super) fn brace_cpp_baseclass(
        &mut self,
        s: &mut BraceScan,
        l: &mut &'a [u8],
    ) -> Option<Step> {
        let mut n = false;
        if s.lookfor != Lookfor::Term && s.o.cpp_baseclass > 0 {
            n = self.cin_is_cpp_baseclass(&mut s.cache);
            *l = self.cur_line();
        }
        if n {
            if s.lookfor == Lookfor::Unterm {
                s.add_continuation();
            } else if s.t0 == b'{' {
                s.lookfor = Lookfor::Unterm;
                s.ind_continuation = 0;
                return Some(Step::Next);
            } else {
                s.amount = self.get_baseclass_amount(s.cache.1.col);
            }
            return Some(Step::Done);
        } else if s.lookfor == Lookfor::CppBaseclass {
            if self.isterminated(l, 0, true, false) != 0 {
                return Some(Step::Done);
            }
            return Some(Step::Next);
        }
        None
    }

    /// `get_c_indent`, "For Javascript we might be inside an object" (line up with a `key:` line)
    /// and the JS `Comma` search: "Line below current line is the one that starts a (possibly
    /// broken) line ending in a comma."
    pub(super) fn brace_js_key_or_comma(
        &self,
        s: &mut BraceScan,
        l: &[u8],
        terminated: u8,
    ) -> Option<Step> {
        if s.js_cur_has_key {
            s.js_cur_has_key = false;
            if s.o.js != 0 && terminated == b',' {
                s.lookfor = Lookfor::JsKey;
            }
        }
        if s.lookfor == Lookfor::JsKey && self.cin_has_js_key(l, 0) {
            s.amount = self.get_indent();
            return Some(Step::Done);
        }
        if s.lookfor == Lookfor::Comma {
            if s.try_brace.lnum >= self.cur.lnum {
                return Some(Step::Done);
            }
            if terminated == b',' {
                return Some(Step::Done);
            }
            s.amount = self.get_indent();
            if self.cur.lnum - 1 == s.ourscope {
                return Some(Step::Done);
            }
        }
        None
    }
}
