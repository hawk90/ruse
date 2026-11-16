//! The brace branch of nvim's `get_c_indent`: a line inside a `{}` block. This module holds the
//! branch's set-up (the block's indent, a closing brace, a matched `else`/`while`, the
//! imaginary-brace and label amounts) and the backward search's driver with its scope-start ends;
//! [`label`] holds the search's switch-label and ignored-line steps, [`stmt`] its unterminated
//! and terminated statement (continuation-line) steps.

mod label;
mod stmt;

use super::super::lex::{at, cin_isbreak, skipwhite};
use super::super::options::CinOptions;
use super::super::{Ci, Pos};
use super::Step;

impl<'a, L: AsRef<[u8]>> Ci<'a, L> {
    /// The brace branch of `get_c_indent`: inside the block opened by the `{` at `brace`. Returns
    /// `i32::MIN` for "the indent of the cursor line" (a matched `if`/`do`, nvim's `goto theend`).
    ///
    /// The phases run in nvim's order; each returns the indent when it decides it, else falls through
    /// to the next: the block's own indent, a closing brace or a matched `else`/`while`, the
    /// imaginary-brace correction, the case/scope label amount, then the backward search one line at
    /// a time.
    pub(super) fn brace_indent(
        &mut self,
        theline: &[u8],
        brace: Pos,
        cur_curpos: Pos,
        cur_amount: &mut i32,
        ind_continuation: &mut i32,
    ) -> i32 {
        let o = *self.o();
        let (amount, start_brace) = self.brace_block_amount(brace);
        let mut s = BraceScan {
            o,
            t0: at(theline, 0),
            try_brace: brace,
            cur_curpos,
            ourscope: brace.lnum,
            start_brace,
            amount,
            scope_amount: 0,
            cont_amount: 0,
            added_to_amount: 0,
            whilelevel: 0,
            raw_string_start: 0,
            cache: (
                false,
                Pos {
                    lnum: usize::MAX,
                    col: 0,
                },
            ),
            lookfor: Lookfor::Initial,
            lookfor_break: false,
            lookfor_cpp_namespace: false,
            js_cur_has_key: o.js != 0 && self.cin_has_js_key(theline, 0),
            cur_amount: *cur_amount,
            ind_continuation: *ind_continuation,
        };
        if let Some(a) = self.brace_close_or_match(&mut s, theline) {
            return a;
        }
        self.brace_imaginary_amount(&mut s);
        self.brace_label_amount(&mut s, theline);
        s.scope_amount = s.amount;
        s.whilelevel = 0;
        self.cur = cur_curpos;
        while self.brace_scan_line(&mut s) == Step::Next {}
        *cur_amount = s.cur_amount;
        *ind_continuation = s.ind_continuation;
        s.amount
    }

    /// `get_c_indent`, "Now figure out how indented the line is in general": the indent of the block
    /// opened by the `{` at `brace`, and where that `{` sits. A `{` that starts its line gives its own
    /// column; one at the end of a line gives the indent of the statement it ends.
    fn brace_block_amount(&mut self, brace: Pos) -> (i32, StartBrace) {
        let o = *self.o();
        let start = self.line(brace.lnum);
        let look = skipwhite(start, 0);
        if at(start, look) == b'{' {
            let amount = i32::try_from(self.vcol(brace.lnum, brace.col)).unwrap_or(i32::MAX);
            let start_brace = if at(start, 0) == b'{' {
                StartBrace::InCol0
            } else {
                StartBrace::AtStart
            };
            return (amount, start_brace);
        }
        self.cur.lnum = brace.lnum;
        let mut lnum = brace.lnum;
        if self.find_last_paren(start, b'(', b')') {
            if let Some(t) = self.find_match_paren(o.maxparen) {
                lnum = t.lnum;
            }
        }
        let cl = self.cur_line();
        let amount = if (o.js != 0 || o.keep_case_label != 0)
            && self.cin_iscase(cl, skipwhite(cl, 0), false)
        {
            self.get_indent()
        } else if o.js != 0 {
            self.indent_of(lnum)
        } else {
            self.skip_label(lnum).0
        };
        (amount, StartBrace::AtEnd)
    }

    /// `get_c_indent`, "If we're looking at a closing brace, that's where we want to be" and "If
    /// we're looking at an "else", try to find an "if" to match it with" (and a `while` with its
    /// `do`). Returns the indent of a `}` line, `i32::MIN` for a matched `else`/`while` (nvim's `goto
    /// theend`), else `None`.
    fn brace_close_or_match(&mut self, s: &mut BraceScan, theline: &[u8]) -> Option<i32> {
        if s.t0 == b'}' {
            return Some(s.amount + s.o.close_extra);
        }
        s.lookfor = Lookfor::Initial;
        if self.cin_iselse(theline, 0) {
            s.lookfor = Lookfor::If;
        } else if self.cin_iswhileofdo(theline, 0, s.cur_curpos.lnum) {
            s.lookfor = Lookfor::Do;
        }
        if s.lookfor != Lookfor::Initial {
            self.cur.lnum = s.cur_curpos.lnum;
            if self.find_match(s.lookfor == Lookfor::If, s.ourscope) {
                return Some(i32::MIN);
            }
        }
        None
    }

    /// `get_c_indent`, "First set amount for when we don't find anything": a `{` in column 0 takes
    /// the imaginary left-margin brace, one at the end of a line the imaginary indent (plus the
    /// namespace or `extern "C"` amount), else `b_ind_open_extra` is compensated for.
    fn brace_imaginary_amount(&mut self, s: &mut BraceScan) {
        let o = s.o;
        if s.start_brace == StartBrace::InCol0 {
            s.amount = o.open_left_imag;
            s.lookfor_cpp_namespace = true;
        } else if s.start_brace == StartBrace::AtStart && s.lookfor_cpp_namespace {
            s.lookfor_cpp_namespace = true;
        } else if s.start_brace == StartBrace::AtEnd {
            s.amount += o.open_imag;
            let cl = self.cur_line();
            let l = skipwhite(cl, 0);
            if self.cin_is_cpp_namespace(cl, l) {
                s.amount += o.cpp_namespace;
            } else if self.cin_is_cpp_extern_c(cl, l) {
                s.amount += o.cpp_extern_c;
            }
        } else {
            s.amount -= o.open_extra;
            if s.amount < 0 {
                s.amount = 0;
            }
        }
    }

    /// `get_c_indent`, "it's a switch() label" / "private:, ..." / "b_ind_level from start of
    /// block": what the backward search looks for, and the amount the line gets for it.
    fn brace_label_amount(&self, s: &mut BraceScan, theline: &[u8]) {
        let o = s.o;
        s.lookfor_break = false;
        if self.cin_iscase(theline, 0, false) {
            s.lookfor = Lookfor::Case;
            s.amount += o.case;
        } else if self.cin_isscopedecl(theline, 0) {
            s.lookfor = Lookfor::Scopedecl;
            s.amount += o.scopedecl;
        } else {
            if o.case_break != 0 && cin_isbreak(theline, 0) {
                s.lookfor_break = true;
            }
            s.lookfor = Lookfor::Initial;
            s.amount += o.level;
        }
    }

    /// One pass of `get_c_indent`'s "Search backwards. If we find something we recognize, line up
    /// with that." loop, on the line above the cursor. Its phases run in nvim's order.
    fn brace_scan_line(&mut self, s: &mut BraceScan) -> Step {
        self.cur.lnum -= 1;
        self.cur.col = 0;
        if self.cur.lnum <= s.ourscope {
            return self.brace_scope_start(s);
        }
        if let Some(t) = self.ind_find_start_cors(Some(&mut s.raw_string_start)) {
            self.cur.lnum = t.lnum + 1;
            self.cur.col = 0;
            return Step::Next;
        }
        let mut l = self.cur_line();
        if let Some(step) = self.brace_switch_label(s, l) {
            return step;
        }
        if let Some(step) = self.brace_skip_ignored(s, &mut l) {
            return step;
        }
        if let Some(step) = self.brace_cpp_baseclass(s, &mut l) {
            return step;
        }
        let terminated = self.isterminated(l, 0, false, true);
        if let Some(step) = self.brace_js_key_or_comma(s, l, terminated) {
            return step;
        }
        if terminated == 0 || (s.lookfor != Lookfor::Unterm && terminated == b',') {
            self.brace_unterminated(s, l, terminated)
        } else if self.cin_iswhileofdo_end(terminated) {
            self.brace_while_of_do_end(s)
        } else {
            self.brace_terminated(s)
        }
    }

    /// `get_c_indent`, "If we went all the way back to the start of our scope, line up with it".
    fn brace_scope_start(&mut self, s: &mut BraceScan) -> Step {
        match s.lookfor {
            Lookfor::EnumOrInit => self.brace_scope_start_enum_or_init(s),
            Lookfor::Unterm => {
                s.add_continuation();
                Step::Done
            }
            _ => self.brace_scope_start_namespace(s),
        }
    }

    /// `get_c_indent`, "We reached end of scope: If looking for a enum or structure initialization go
    /// further back": past the scope start, until the declaration the initializer belongs to.
    fn brace_scope_start_enum_or_init(&mut self, s: &mut BraceScan) -> Step {
        let o = s.o;
        if self.cur.lnum == 0 || (self.cur.lnum as i64) < s.ourscope as i64 - i64::from(o.maxparen)
        {
            if s.cont_amount > 0 {
                s.amount = s.cont_amount;
            } else if o.js == 0 {
                s.amount += s.ind_continuation;
            }
            return Step::Done;
        }
        if let Some(t) = self.ind_find_start_cors(None) {
            self.cur.lnum = t.lnum + 1;
            self.cur.col = 0;
            return Step::Next;
        }
        let mut l = self.cur_line();
        let mut ln = self.cur.lnum;
        if self.cin_ispreproc_cont(&mut l, &mut ln, &mut s.amount) {
            self.cur.lnum = ln;
            return Step::Next;
        }
        self.cur.lnum = ln;
        if self.nocode(l, 0) {
            return Step::Next;
        }
        let terminated = self.isterminated(l, 0, false, true);
        if s.start_brace != StartBrace::InCol0 || !self.cin_isfuncdecl(Some(l), self.cur.lnum, 0) {
            if terminated == b',' {
                return Step::Done;
            }
            if terminated != b';' && self.cin_isinit() {
                return Step::Done;
            }
            if terminated == 0 || terminated == b'{' {
                return Step::Next;
            }
        }
        if terminated != b';' {
            let mut t = None;
            if self.find_last_paren(l, b'(', b')') {
                t = self.find_match_paren(o.maxparen);
            }
            if t.is_none() && self.find_last_paren(l, b'{', b'}') {
                t = self.find_start_brace();
            }
            if let Some(t) = t {
                self.cur.lnum = t.lnum + 1;
                self.cur.col = 0;
                return Step::Next;
            }
        }
        s.add_continuation();
        Step::Done
    }

    /// `get_c_indent`, the scope start reached while not in an initializer: line up with the scope,
    /// and, "Looking for C++ namespace, need to look further back", add the namespace or `extern
    /// "C"` amount.
    fn brace_scope_start_namespace(&mut self, s: &mut BraceScan) -> Step {
        let o = s.o;
        if s.lookfor != Lookfor::Term
            && s.lookfor != Lookfor::CppBaseclass
            && s.lookfor != Lookfor::Comma
        {
            s.amount = s.scope_amount;
            if s.t0 == b'{' {
                s.amount += o.open_extra;
                s.added_to_amount = o.open_extra;
            }
        }
        if s.lookfor_cpp_namespace {
            if self.cur.lnum == s.ourscope {
                return Step::Next;
            }
            if self.cur.lnum == 0 || (self.cur.lnum as i64) < s.ourscope as i64 - 20 {
                return Step::Done;
            }
            if let Some(t) = self.ind_find_start_cors(None) {
                self.cur.lnum = t.lnum + 1;
                self.cur.col = 0;
                return Step::Next;
            }
            let mut l = self.cur_line();
            let mut ln = self.cur.lnum;
            if self.cin_ispreproc_cont(&mut l, &mut ln, &mut s.amount) {
                self.cur.lnum = ln;
                return Step::Next;
            }
            self.cur.lnum = ln;
            if self.cin_is_cpp_namespace(l, 0) {
                s.amount += o.cpp_namespace - s.added_to_amount;
                return Step::Done;
            } else if self.cin_is_cpp_extern_c(l, 0) {
                s.amount += o.cpp_extern_c - s.added_to_amount;
                return Step::Done;
            }
            if self.nocode(l, 0) {
                return Step::Next;
            }
        }
        Step::Done
    }
}

/// What the brace branch's backward search looks for (nvim's `LOOKFOR_*`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookfor {
    Initial,
    If,
    Do,
    Case,
    Any,
    Term,
    Unterm,
    Scopedecl,
    Nobreak,
    CppBaseclass,
    EnumOrInit,
    JsKey,
    Comma,
}

/// Where the `{` that opens the block sits (nvim's `BRACE_IN_COL0`, `BRACE_AT_START`,
/// `BRACE_AT_END`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartBrace {
    InCol0,
    AtStart,
    AtEnd,
}

/// The locals of the brace branch of nvim's `get_c_indent`, shared by its phases. Each is set where
/// nvim sets it and read by the later phases, so no phase recomputes another's value.
struct BraceScan {
    o: CinOptions,
    /// The first byte of the line being indented.
    t0: u8,
    /// The `{` that opens the block (nvim's `tryposBrace` copy).
    try_brace: Pos,
    cur_curpos: Pos,
    ourscope: usize,
    start_brace: StartBrace,
    amount: i32,
    scope_amount: i32,
    cont_amount: i32,
    added_to_amount: i32,
    whilelevel: i32,
    raw_string_start: usize,
    cache: (bool, Pos),
    lookfor: Lookfor,
    lookfor_break: bool,
    lookfor_cpp_namespace: bool,
    js_cur_has_key: bool,
    /// The caller's `cur_amount`, written back when the search ends.
    cur_amount: i32,
    /// The caller's `ind_continuation`, written back when the search ends.
    ind_continuation: i32,
}

impl BraceScan {
    /// nvim's repeated "if (cont_amount > 0) amount = cont_amount; else amount +=
    /// ind_continuation;".
    fn add_continuation(&mut self) {
        if self.cont_amount > 0 {
            self.amount = self.cont_amount;
        } else {
            self.amount += self.ind_continuation;
        }
    }
}
