//! `get_c_indent`: the indent of one line, with its comment, paren, brace and top-level parts.

use super::keys::CinKey;
use super::lex::{
    at, check_linecomment, cin_isbreak, cin_iscomment, cin_isdo, cin_islinecomment, is_white,
    last_byte, ncmp, skipwhite, starts, strend,
};
use super::options::{getdigits, CinOptions};
use super::{Ci, Pos};

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

    /// The comment branch of `get_c_indent`: a line inside a `/* */` comment that started at `cpos`.
    fn comment_indent(&self, theline: &[u8], cpos: Pos, cur_curpos: Pos) -> i32 {
        let o = self.o();
        let mut cpos = cpos;
        let mut amount = i32::try_from(self.vcol(cpos.lnum, cpos.col)).unwrap_or(i32::MAX);
        let mut lead_start: Vec<u8> = Vec::new();
        let mut lead_middle: Vec<u8> = Vec::new();
        let mut lead_start_len = 2usize;
        let mut lead_middle_len = 1usize;
        let mut start_align = 0u8;
        let mut start_off = 0i32;
        let mut done = false;
        let com = self.e.comments.as_bytes();
        let mut p = 0usize;
        let lnum = self.cur.lnum;
        'entries: while p < com.len() {
            let mut align = 0u8;
            let mut off = 0i32;
            let mut what = 0u8;
            while p < com.len() && com[p] != b':' {
                let c = com[p];
                if c == b's' || c == b'e' || c == b'm' {
                    what = c;
                    p += 1;
                } else if c == b'l' || c == b'r' {
                    align = c;
                    p += 1;
                } else if c.is_ascii_digit() || c == b'-' {
                    let before = p;
                    off = i32::try_from(getdigits(com, &mut p)).unwrap_or(0);
                    if p == before {
                        p += 1; // a lone `-`: nvim would spin here
                    }
                } else {
                    p += 1;
                }
            }
            if p < com.len() && com[p] == b':' {
                p += 1;
            }
            // copy_option_part up to the next `,` (a `\,` is a literal comma).
            let mut lead_end: Vec<u8> = Vec::new();
            while p < com.len() && com[p] != b',' {
                if com[p] == b'\\' && p + 1 < com.len() && com[p + 1] == b',' {
                    p += 1;
                }
                lead_end.push(com[p]);
                p += 1;
            }
            if p < com.len() {
                p += 1;
            }
            let lead_end_len = lead_end.len();
            match what {
                b's' => {
                    lead_start.clone_from(&lead_end);
                    lead_start_len = lead_end_len;
                    start_off = off;
                    start_align = align;
                }
                b'm' => {
                    lead_middle.clone_from(&lead_end);
                    lead_middle_len = lead_end_len;
                }
                b'e' => {
                    if ncmp(theline, &lead_middle, lead_middle_len)
                        && !ncmp(theline, &lead_end, lead_end_len)
                    {
                        done = true;
                        if lnum > 1 {
                            let pl = self.line(lnum - 1);
                            let look = &pl[skipwhite(pl, 0)..];
                            if ncmp(look, &lead_start, lead_start_len) {
                                amount = self.indent_of(lnum - 1);
                            } else if ncmp(look, &lead_middle, lead_middle_len) {
                                amount = self.indent_of(lnum - 1);
                                break 'entries;
                            } else if !ncmp(
                                &self.line(cpos.lnum)[cpos.col.min(self.line(cpos.lnum).len())..],
                                &lead_start,
                                lead_start_len,
                            ) {
                                continue 'entries;
                            }
                        }
                        if start_off != 0 {
                            amount += start_off;
                        } else if start_align == b'r' {
                            amount += lead_start.len() as i32 - lead_middle.len() as i32;
                        }
                        break 'entries;
                    }
                    if !ncmp(theline, &lead_middle, lead_middle_len)
                        && ncmp(theline, &lead_end, lead_end_len)
                    {
                        amount = self.indent_of(lnum - 1);
                        if off != 0 {
                            amount += off;
                        } else if align == b'r' {
                            amount += lead_start.len() as i32 - lead_middle.len() as i32;
                        }
                        done = true;
                        break 'entries;
                    }
                }
                _ => {}
            }
        }
        if done {
        } else if at(theline, 0) == b'*' {
            amount += 1;
        } else {
            amount = -1;
            let mut l = cur_curpos.lnum - 1;
            while l > cpos.lnum {
                if !self.linewhite(l) {
                    amount = self.indent_of(l);
                    break;
                }
                l -= 1;
            }
            if amount == -1 {
                let start = self.line(cpos.lnum);
                let look = cpos.col + 2;
                if o.in_comment2 == 0 && at(start, look) != 0 {
                    cpos.col = skipwhite(start, look);
                }
                amount = i32::try_from(self.vcol(cpos.lnum, cpos.col)).unwrap_or(i32::MAX);
                if o.in_comment2 != 0 || at(start, look) == 0 {
                    amount += o.in_comment;
                }
            }
        }
        amount
    }

    /// The parenthesis branch of `get_c_indent`: inside the `(` at `paren`.
    ///
    /// The phases run in nvim's order: the indent of an earlier line that matches the same paren;
    /// when none decides it, whether the outermost paren belongs to an `if`/`for`/`while`, the
    /// paren line's own amount and the leading `(` to ignore, the column to line up with, then the
    /// amount for an unclosed paren.
    fn paren_indent(
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

    /// The brace branch of `get_c_indent`: inside the block opened by the `{` at `brace`. Returns
    /// `i32::MIN` for "the indent of the cursor line" (a matched `if`/`do`, nvim's `goto theend`).
    ///
    /// The phases run in nvim's order; each returns the indent when it decides it, else falls through
    /// to the next: the block's own indent, a closing brace or a matched `else`/`while`, the
    /// imaginary-brace correction, the case/scope label amount, then the backward search one line at
    /// a time.
    fn brace_indent(
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

    /// `get_c_indent`, "If this is a switch() label, may line up relative to that. If this is a C++
    /// scope declaration, do the same." `None` when line `l` is neither.
    fn brace_switch_label(&mut self, s: &mut BraceScan, l: &[u8]) -> Option<Step> {
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
    fn brace_skip_ignored(&mut self, s: &mut BraceScan, l: &mut &'a [u8]) -> Option<Step> {
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
    fn brace_cpp_baseclass(&mut self, s: &mut BraceScan, l: &mut &'a [u8]) -> Option<Step> {
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
    fn brace_js_key_or_comma(&self, s: &mut BraceScan, l: &[u8], terminated: u8) -> Option<Step> {
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

    /// `get_c_indent`, an unterminated line (or one ending in `,`): "If we're in the middle of a
    /// paren thing, Go back to the line that starts it so we can get the right prevailing indent",
    /// "Skip over continuation lines to find the one to get the indent from", then "If this is just
    /// above the line we are indenting, and it starts with a '{', line it up with this line."
    fn brace_unterminated(&mut self, s: &mut BraceScan, mut l: &'a [u8], terminated: u8) -> Step {
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
    fn brace_while_of_do_end(&mut self, s: &mut BraceScan) -> Step {
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
    fn brace_terminated(&mut self, s: &mut BraceScan) -> Step {
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

    /// The top-level branch of `get_c_indent`: outside any brace or parenthesis.
    fn top_level_indent(&mut self, cur_curpos: Pos, ind_continuation: i32) -> i32 {
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
                amount = self.get_indent();
                if amount == 0 {
                    amount = self.cin_first_id_amount();
                }
                if amount == 0 {
                    amount = ind_continuation;
                }
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
            let look = skipwhite(l, 0);
            if at(l, look) == b';' && self.nocode(l, look + 1) {
                let save = self.cur;
                let mut lk: &'a [u8] = l;
                while self.cur.lnum > 1 {
                    self.cur.lnum -= 1;
                    lk = self.cur_line();
                    let mut ln = self.cur.lnum;
                    let nc = self.nocode(lk, 0);
                    let pre = !nc && self.cin_ispreproc_cont(&mut lk, &mut ln, &mut amount);
                    self.cur.lnum = ln;
                    if !(nc || pre) {
                        break;
                    }
                }
                if self.cur.lnum > 0 && self.cin_ends_in(lk, b"}") {
                    break;
                }
                self.cur = save;
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

/// What one pass of the backward search decided: look at the line above (nvim's `continue`), or
/// stop with the amount found (nvim's `break`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Next,
    Done,
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
